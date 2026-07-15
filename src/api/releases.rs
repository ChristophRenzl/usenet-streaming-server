//! Release search: TMDB id → indexer fan-out → parsed & ranked candidates.
//! The resolution pipeline is shared with the streaming session creation.

use axum::{
    extract::{Query, State},
    Json,
};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::{
    db,
    error::{AppError, AppResult},
    indexer::{self, SearchQuery},
    release::{
        parse::Resolution,
        rank::{rank, RankedRelease},
        verify,
    },
    state::AppState,
    tmdb::models::{MediaType, TvShow},
};

use super::metadata::tmdb_client;

/// What to find releases for: a movie or one episode of a show.
#[derive(Debug, Clone, Copy)]
pub struct ReleaseTarget {
    pub tmdb_id: i64,
    pub media_type: MediaType,
    pub season: Option<u32>,
    pub episode: Option<u32>,
}

impl ReleaseTarget {
    /// Validate the season/episode combination for the media type.
    pub fn validated(self) -> AppResult<Self> {
        if self.media_type == MediaType::Tv && (self.season.is_none() || self.episode.is_none()) {
            return Err(AppError::BadRequest(
                "season and episode are required for type=tv".into(),
            ));
        }
        Ok(self)
    }
}

/// Full resolution pipeline: TMDB details → indexer fan-out → parse + rank
/// against the stored preferences. Used by `GET /releases`,
/// `POST /stream/sessions` and `POST /downloads`.
///
/// `max_resolution` is the optional per-request device capability cap: the
/// lower of it and the stored preference max wins (see
/// [`crate::release::rank::rank`]).
pub async fn resolve_candidates(
    state: &AppState,
    target: &ReleaseTarget,
    max_resolution: Option<Resolution>,
    ignore_blocked_terms: bool,
) -> AppResult<Vec<RankedRelease>> {
    let indexers = db::indexers::list_enabled(&state.db).await?;
    if indexers.is_empty() {
        return Err(AppError::BadRequest(
            "no enabled indexers configured; add one via POST /api/v1/settings/indexers".into(),
        ));
    }

    let tmdb = tmdb_client(state).await?;
    let (queries, item) = match target.media_type {
        MediaType::Movie => {
            let movie = tmdb.movie_details(target.tmdb_id).await?;
            let query = match movie.imdb_id.clone() {
                Some(imdb_id) => SearchQuery::MovieByImdb { imdb_id },
                None => SearchQuery::Raw {
                    query: match movie.year {
                        Some(year) => format!("{} {year}", movie.title),
                        None => movie.title.clone(),
                    },
                },
            };
            (
                vec![query],
                ItemInfo {
                    // TMDB rarely lacks movie runtimes; assume a compact 100
                    // minutes when it does so the bandwidth gate stays armed
                    // (underestimating runtime overestimates bitrate — the
                    // safe direction for "will it stream").
                    runtime_secs: Some(movie.runtime_minutes.map_or(100.0 * 60.0, |m| m as f64 * 60.0)),
                    title: movie.title,
                    year: movie.year,
                    tvdb_id: None,
                    imdb_id: movie.imdb_id,
                    original_language: movie.original_language,
                    absolute_episode: None,
                },
            )
        }
        MediaType::Tv => {
            let (season, episode) = target
                .season
                .zip(target.episode)
                .expect("validated TV target");
            let show = tmdb.tv_details(target.tmdb_id).await?;
            // `tv_search_queries` already fans out tvsearch + SxxExx + (for
            // anime) the absolute-episode-number query. The absolute number
            // is also kept on `ItemInfo` for candidate verification below.
            let queries = tv_search_queries(&show, season, episode);
            let absolute = absolute_episode(&show, season, episode);
            (
                queries,
                ItemInfo {
                    // Prestige shows (HBO etc.) often have an empty
                    // episode_run_time on TMDB; assume 40 minutes so huge
                    // remuxes still get bitrate-checked.
                    runtime_secs: Some(
                        show.episode_runtime_minutes.map_or(40.0 * 60.0, |m| m as f64 * 60.0),
                    ),
                    title: show.title,
                    year: show.year,
                    tvdb_id: show.tvdb_id,
                    imdb_id: show.imdb_id,
                    original_language: show.original_language,
                    absolute_episode: absolute,
                },
            )
        }
    };

    let raw = indexer::search_many(&state.http, indexers, &queries).await;
    // Series can carry their own resolution preferences (e.g. 2160p movies
    // but 1080p episodes); rank against the pair effective for this media type.
    let mut prefs = db::preferences::get(&state.db)
        .await?
        .for_media_type(target.media_type == MediaType::Tv);
    // Pre-release escape hatch: drop the blocked terms so CAM/TS/HDCAM rips are
    // no longer rejected — for brand-new titles whose only releases are those.
    if ignore_blocked_terms {
        prefs.blocked_terms.clear();
    }
    // Releases whose average bitrate exceeds what the connection has proven
    // it can sustain would stall no matter the buffer; reject them up front.
    let gate = match (item.runtime_secs, state.nntp_pool.observed_bps()) {
        (Some(runtime_secs), Some(available_bps)) => Some(crate::release::rank::StreamGate {
            runtime_secs,
            available_bps,
        }),
        _ => None,
    };
    let mut ranked = rank(
        raw,
        &prefs,
        max_resolution,
        item.original_language.as_deref(),
        gate,
    );

    // Reject candidates that contradict the requested title (wrong show,
    // wrong year, wrong episode). They stay in the list with a reason so
    // pickers can show them and a manual guid pin can still override.
    let expected = verify::Expected {
        title: &item.title,
        year: item.year,
        tvdb_id: item.tvdb_id,
        imdb_id: item.imdb_id.as_deref(),
        season: target.season,
        episode: target.episode,
        absolute_episode: item.absolute_episode,
    };
    for candidate in &mut ranked {
        if candidate.rejected.is_none() {
            if let Some(reason) = verify::mismatch_reason(&candidate.raw, &expected) {
                candidate.rejected = Some(reason);
            }
        }
    }
    // Releases the user marked as bad are rejected too, so automatic
    // selection skips them; they stay listed (with the reason) and a manual
    // guid pin still overrides — like every other rejection.
    apply_blacklist(
        &mut ranked,
        &db::release_blacklist::titles(&state.db).await?,
    );
    // Stable: keeps rank order within the accepted and rejected groups.
    ranked.sort_by_key(|c| c.rejected.is_some());
    Ok(ranked)
}

/// Reject candidates whose title was blacklisted ("marked as bad" from the
/// player). Earlier rejection reasons win; blacklisting only fills in.
fn apply_blacklist(
    candidates: &mut [RankedRelease],
    blacklisted: &std::collections::HashSet<String>,
) {
    if blacklisted.is_empty() {
        return;
    }
    for candidate in candidates {
        if candidate.rejected.is_none() && blacklisted.contains(&candidate.raw.title) {
            candidate.rejected = Some("marked as bad".into());
        }
    }
}

/// TMDB metadata of the searched item, kept for post-search verification.
struct ItemInfo {
    title: String,
    year: Option<i32>,
    tvdb_id: Option<i64>,
    imdb_id: Option<String>,
    original_language: Option<String>,
    absolute_episode: Option<u32>,
    /// Nominal runtime, for bitrate estimation in the bandwidth gate.
    runtime_secs: Option<f64>,
}

/// Build the set of indexer search strategies for one TV episode. Their
/// results are merged and deduped downstream, so issuing several is cheap and
/// only widens coverage:
///
/// 1. `tvsearch` by TVDB id + season + episode (when a TVDB id is known) — the
///    canonical scene-numbered lookup.
/// 2. A `"{title} SxxExx"` free-text query — always, as a fallback for
///    indexers that do not index the TVDB mapping.
/// 3. For **anime only** (original language Japanese), a `"{title} {absolute}"`
///    free-text query using the *absolute* episode number. Anime is released
///    and indexed by a running episode count (e.g. `One Piece - 1100`) rather
///    than `SxxExx`, so scene-numbered lookups return nothing for it. Skipped
///    when the absolute number cannot be derived from TMDB's season data.
pub fn tv_search_queries(show: &TvShow, season: u32, episode: u32) -> Vec<SearchQuery> {
    let mut queries = Vec::new();
    if let Some(tvdb_id) = show.tvdb_id {
        queries.push(SearchQuery::TvByTvdb {
            tvdb_id,
            season,
            episode,
        });
    }
    queries.push(SearchQuery::Raw {
        query: format!("{} S{season:02}E{episode:02}", show.title),
    });
    // Anime is numbered absolutely, not SxxExx. Gate on Japanese original
    // language and require enough season data to compute the running count.
    if show.original_language.as_deref() == Some("ja") {
        if let Some(absolute) = absolute_episode(show, season, episode) {
            queries.push(SearchQuery::Raw {
                query: format!("{} {absolute}", show.title),
            });
        }
    }
    queries
}

/// Compute the absolute episode number: the running count across all prior
/// regular seasons plus this episode. Season 0 (specials) is ignored. Returns
/// `None` when the target season's data is missing, so the caller can skip the
/// absolute-number strategy gracefully.
pub fn absolute_episode(show: &TvShow, season: u32, episode: u32) -> Option<u32> {
    // The target season must be present in TMDB's data; without it we cannot
    // trust the running count.
    if !show.seasons.iter().any(|s| s.season_number == season) {
        return None;
    }
    let prior: u32 = show
        .seasons
        .iter()
        .filter(|s| s.season_number >= 1 && s.season_number < season)
        .map(|s| s.episode_count.unwrap_or(0).max(0) as u32)
        .sum();
    Some(prior + episode)
}

/// Pick the candidates to actually try: the guid-pinned release when given,
/// otherwise the top `max_attempts` accepted ones in rank order. When
/// `preferred_title` names an accepted candidate (the release last watched
/// for this item), it is moved to the front so resuming reuses the same
/// release. Shared by session creation and download jobs.
pub fn pick_candidates(
    candidates: &[RankedRelease],
    release_guid: Option<&str>,
    preferred_title: Option<&str>,
    max_attempts: usize,
) -> AppResult<Vec<RankedRelease>> {
    let to_try: Vec<RankedRelease> = match release_guid {
        Some(guid) => {
            let chosen = candidates
                .iter()
                .find(|c| c.raw.guid == guid)
                .cloned()
                .ok_or_else(|| AppError::NotFound(format!("release with guid '{guid}'")))?;
            vec![chosen]
        }
        None => {
            let mut accepted: Vec<RankedRelease> = candidates
                .iter()
                .filter(|c| c.rejected.is_none())
                .take(max_attempts)
                .cloned()
                .collect();
            // The last-watched release wins over the ranking, even when it
            // fell below the top `max_attempts` cut in a fresh search.
            if let Some(title) = preferred_title {
                if let Some(index) = accepted.iter().position(|c| c.raw.title == title) {
                    accepted[..=index].rotate_right(1);
                } else if let Some(preferred) = candidates
                    .iter()
                    .find(|c| c.rejected.is_none() && c.raw.title == title)
                {
                    accepted.insert(0, preferred.clone());
                    accepted.truncate(max_attempts);
                }
            }
            accepted
        }
    };
    if to_try.is_empty() {
        return Err(AppError::NoRelease(
            "no accepted release candidates (all were rejected by preferences)".into(),
        ));
    }
    Ok(to_try)
}

#[derive(Debug, Deserialize, IntoParams)]
pub struct ReleasesParams {
    /// TMDB id of the movie or show.
    pub tmdb_id: i64,
    /// Media type: `movie` or `tv`.
    #[serde(rename = "type")]
    pub media_type: MediaType,
    /// Season number (required for `tv`).
    pub season: Option<u32>,
    /// Episode number (required for `tv`).
    pub episode: Option<u32>,
    /// Device capability cap (`480p`, `720p`, `1080p`, `2160p`): releases
    /// above the lower of this and the stored preference max are rejected,
    /// and the best supported resolution ranks first.
    pub max_resolution: Option<Resolution>,
    /// When `true`, ignore the stored blocked terms so pre-release/low-quality
    /// rips (CAM/TS/…) are not rejected — the "allow pre-release" escape hatch.
    #[serde(default)]
    pub ignore_blocked_terms: bool,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ReleasesResponse {
    pub candidates: Vec<RankedRelease>,
}

/// Find release candidates for a movie or episode across all enabled
/// indexers, parsed and ranked against the stored quality preferences.
#[utoipa::path(get, path = "/releases", tag = "releases",
    params(ReleasesParams),
    responses(
        (status = 200, body = ReleasesResponse),
        (status = 400, description = "Bad parameters, no indexers or TMDB key missing"),
        (status = 404, description = "Unknown TMDB id"),
    ))]
pub async fn find_releases(
    State(state): State<AppState>,
    Query(params): Query<ReleasesParams>,
) -> AppResult<Json<ReleasesResponse>> {
    let target = ReleaseTarget {
        tmdb_id: params.tmdb_id,
        media_type: params.media_type,
        season: params.season,
        episode: params.episode,
    }
    .validated()?;
    let candidates = resolve_candidates(
        &state,
        &target,
        params.max_resolution,
        params.ignore_blocked_terms,
    )
    .await?;
    Ok(Json(ReleasesResponse { candidates }))
}

#[derive(Debug, Serialize, ToSchema)]
pub struct BlacklistResponse {
    pub entries: Vec<db::release_blacklist::BlacklistedRelease>,
}

/// All blacklisted releases ("marked as bad"), newest first.
#[utoipa::path(get, path = "/releases/blacklist", tag = "releases",
    responses((status = 200, body = BlacklistResponse)))]
pub async fn list_blacklist(State(state): State<AppState>) -> AppResult<Json<BlacklistResponse>> {
    Ok(Json(BlacklistResponse {
        entries: db::release_blacklist::list(&state.db).await?,
    }))
}

/// Un-blacklist one release so automatic selection may pick it again.
#[utoipa::path(delete, path = "/releases/blacklist/{id}", tag = "releases",
    params(("id" = i64, Path, description = "Blacklist entry id")),
    responses((status = 204), (status = 404)))]
pub async fn delete_blacklist_entry(
    State(state): State<AppState>,
    axum::extract::Path(id): axum::extract::Path<i64>,
) -> AppResult<axum::http::StatusCode> {
    if db::release_blacklist::delete(&state.db, id).await? {
        Ok(axum::http::StatusCode::NO_CONTENT)
    } else {
        Err(AppError::NotFound(format!("blacklist entry {id}")))
    }
}

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(find_releases))
        .routes(routes!(list_blacklist))
        .routes(routes!(delete_blacklist_entry))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::release::parse::parse_release_name;
    use crate::tmdb::models::SeasonSummary;

    fn season(number: u32, episode_count: Option<i64>) -> SeasonSummary {
        SeasonSummary {
            season_number: number,
            title: Some(format!("Season {number}")),
            episode_count,
            air_date: None,
            poster_url: None,
        }
    }

    fn show(title: &str, original_language: &str, seasons: Vec<SeasonSummary>) -> TvShow {
        TvShow {
            episode_runtime_minutes: None,
            tmdb_id: 1,
            media_type: MediaType::Tv,
            imdb_id: None,
            tvdb_id: Some(81797),
            title: title.into(),
            year: None,
            overview: None,
            poster_url: None,
            backdrop_url: None,
            vote_average: None,
            original_language: Some(original_language.into()),
            trailer_youtube_key: None,
            seasons,
            cast: vec![],
        }
    }

    #[test]
    fn absolute_episode_sums_prior_regular_seasons_plus_episode() {
        // Specials (season 0) are ignored; seasons 1 & 2 (12 + 13) precede the
        // target season 3 episode 1 → 12 + 13 + 1 = 26.
        let s = show(
            "One Piece",
            "ja",
            vec![
                season(0, Some(5)),
                season(1, Some(12)),
                season(2, Some(13)),
                season(3, Some(20)),
            ],
        );
        assert_eq!(absolute_episode(&s, 3, 1), Some(26));
        // Season 1 episode 4 has no prior regular seasons → 4.
        assert_eq!(absolute_episode(&s, 1, 4), Some(4));
    }

    #[test]
    fn absolute_episode_is_none_when_target_season_missing() {
        let s = show("One Piece", "ja", vec![season(1, Some(12))]);
        assert_eq!(absolute_episode(&s, 5, 1), None);
    }

    #[test]
    fn anime_show_issues_absolute_number_query() {
        let s = show(
            "One Piece",
            "ja",
            vec![season(1, Some(1099)), season(2, Some(50))],
        );
        let queries = tv_search_queries(&s, 2, 1);
        // tvsearch + SxxExx + absolute-number ("One Piece 1100").
        assert_eq!(queries.len(), 3);
        assert!(matches!(queries[0], SearchQuery::TvByTvdb { .. }));
        assert!(
            matches!(&queries[2], SearchQuery::Raw { query } if query == "One Piece 1100"),
            "queries were: {queries:?}"
        );
    }

    #[test]
    fn non_anime_show_does_not_issue_absolute_number_query() {
        let s = show(
            "Breaking Bad",
            "en",
            vec![season(1, Some(7)), season(2, Some(13))],
        );
        let queries = tv_search_queries(&s, 2, 1);
        // tvsearch + SxxExx only; no absolute-number strategy.
        assert_eq!(queries.len(), 2);
        assert!(queries
            .iter()
            .all(|q| !matches!(q, SearchQuery::Raw { query } if !query.contains('S'))));
    }

    #[test]
    fn anime_without_season_data_skips_absolute_query_gracefully() {
        let s = show("Mystery Anime", "ja", vec![]);
        let queries = tv_search_queries(&s, 1, 1);
        // Only tvsearch + SxxExx; absolute skipped since season data is absent.
        assert_eq!(queries.len(), 2);
        assert!(!queries
            .iter()
            .any(|q| matches!(q, SearchQuery::Raw { query } if query == "Mystery Anime 1")));
    }

    fn candidate(title: &str, score: i64, rejected: Option<&str>) -> RankedRelease {
        RankedRelease {
            raw: crate::indexer::RawRelease {
                title: title.into(),
                guid: format!("guid-{title}"),
                nzb_url: format!("https://x/{title}.nzb"),
                size_bytes: None,
                posted_at: None,
                indexer_id: 1,
                indexer_name: "test".into(),
                tvdb_id: None,
                imdb_id: None,
                file_count: None,
            },
            parsed: parse_release_name(title),
            score,
            rejected: rejected.map(Into::into),
        }
    }

    #[test]
    fn preferred_title_moves_to_front_keeping_rank_order_behind_it() {
        let candidates = vec![
            candidate("A", 30, None),
            candidate("B", 20, None),
            candidate("C", 10, None),
        ];
        let picked = pick_candidates(&candidates, None, Some("B"), 5).unwrap();
        let titles: Vec<&str> = picked.iter().map(|c| c.raw.title.as_str()).collect();
        assert_eq!(titles, ["B", "A", "C"]);
    }

    #[test]
    fn preferred_title_below_the_attempt_cut_is_still_tried_first() {
        let candidates = vec![
            candidate("A", 30, None),
            candidate("B", 20, None),
            candidate("C", 10, None),
        ];
        let picked = pick_candidates(&candidates, None, Some("C"), 2).unwrap();
        let titles: Vec<&str> = picked.iter().map(|c| c.raw.title.as_str()).collect();
        assert_eq!(titles, ["C", "A"]);
    }

    #[test]
    fn unknown_or_rejected_preferred_title_changes_nothing() {
        let candidates = vec![
            candidate("A", 30, None),
            candidate("B", 20, Some("blocked term")),
            candidate("C", 10, None),
        ];
        for preferred in [Some("Gone"), Some("B"), None] {
            let picked = pick_candidates(&candidates, None, preferred, 5).unwrap();
            let titles: Vec<&str> = picked.iter().map(|c| c.raw.title.as_str()).collect();
            assert_eq!(titles, ["A", "C"]);
        }
    }

    #[test]
    fn blacklist_rejects_by_title_without_overwriting_reasons() {
        let mut candidates = vec![
            candidate("A", 30, None),
            candidate("B", 20, Some("blocked term")),
            candidate("C", 10, None),
        ];
        let blacklisted: std::collections::HashSet<String> =
            ["A".to_string(), "B".to_string()].into();
        apply_blacklist(&mut candidates, &blacklisted);
        assert_eq!(candidates[0].rejected.as_deref(), Some("marked as bad"));
        // An earlier rejection reason is kept.
        assert_eq!(candidates[1].rejected.as_deref(), Some("blocked term"));
        assert_eq!(candidates[2].rejected, None);

        // Automatic selection now skips the blacklisted title...
        let picked = pick_candidates(&candidates, None, None, 5).unwrap();
        assert_eq!(picked.len(), 1);
        assert_eq!(picked[0].raw.title, "C");
        // ...but a manual guid pin still overrides.
        let pinned = pick_candidates(&candidates, Some("guid-A"), None, 5).unwrap();
        assert_eq!(pinned[0].raw.title, "A");
    }

    #[test]
    fn guid_pin_ignores_the_preferred_title() {
        let candidates = vec![candidate("A", 30, None), candidate("B", 20, None)];
        let picked = pick_candidates(&candidates, Some("guid-B"), Some("A"), 5).unwrap();
        assert_eq!(picked.len(), 1);
        assert_eq!(picked[0].raw.title, "B");
    }
}
