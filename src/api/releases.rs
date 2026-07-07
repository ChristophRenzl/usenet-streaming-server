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
        rank::{rank, sort_candidates, RankedRelease},
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
                (vec![query], Vec::new()),
                ItemInfo {
                    title: movie.title,
                    year: movie.year,
                    tvdb_id: None,
                    imdb_id: movie.imdb_id,
                    original_language: movie.original_language,
                    absolute_episode: None,
                    episode_air_year: None,
                    episode_title: None,
                },
            )
        }
        MediaType::Tv => {
            let (season, episode) = target
                .season
                .zip(target.episode)
                .expect("validated TV target");
            let show = tmdb.tv_details(target.tmdb_id).await?;
            // Primary lookup plus quota-guarded fallbacks (SxxExx text and,
            // for anime, absolute-episode-number queries). The absolute
            // number is also kept on `ItemInfo` for candidate verification.
            let queries = tv_search_queries(&show, season, episode);
            let absolute = absolute_episode(&show, season, episode);
            // The episode's TMDB title separates same-name shows (remakes,
            // spin-offs) when release names carry an episode title, and its
            // air year keeps air-date-tagged releases of current episodes
            // from being mistaken for remakes. Optional context: resolution
            // proceeds without it.
            let mut episode_title = None;
            let mut episode_air_year = None;
            if let Ok(details) = tmdb.episode_details(target.tmdb_id, season, episode).await {
                episode_air_year = details
                    .air_date
                    .as_deref()
                    .and_then(|d| d.get(0..4))
                    .and_then(|y| y.parse().ok());
                episode_title = details.title;
            }
            (
                queries,
                ItemInfo {
                    title: show.title,
                    year: show.year,
                    tvdb_id: show.tvdb_id,
                    imdb_id: show.imdb_id,
                    original_language: show.original_language,
                    absolute_episode: absolute,
                    episode_air_year,
                    episode_title,
                },
            )
        }
    };
    let (primary, fallbacks) = queries;

    // Series can carry their own resolution preferences (e.g. 2160p movies
    // but 1080p episodes); rank against the pair effective for this media type.
    let prefs = db::preferences::get(&state.db)
        .await?
        .for_media_type(target.media_type == MediaType::Tv);
    let expected = verify::Expected {
        title: &item.title,
        year: item.year,
        tvdb_id: item.tvdb_id,
        imdb_id: item.imdb_id.as_deref(),
        season: target.season,
        episode: target.episode,
        absolute_episode: item.absolute_episode,
        episode_air_year: item.episode_air_year,
        episode_title: item.episode_title.as_deref(),
    };

    // Rank, then reject candidates that contradict the requested title
    // (wrong show, wrong year, wrong episode) — they stay in the list with a
    // reason so pickers can show them and a manual guid pin can override —
    // and apply episode-level score adjustments (episode-title match,
    // unnumbered recaps/packs) before the final ordering.
    let process = |raw: Vec<indexer::RawRelease>| -> Vec<RankedRelease> {
        let mut ranked = rank(
            raw,
            &prefs,
            max_resolution,
            item.original_language.as_deref(),
        );
        for candidate in &mut ranked {
            if candidate.rejected.is_none() {
                candidate.rejected = verify::mismatch_reason(&candidate.raw, &expected);
            }
            if candidate.rejected.is_none() {
                candidate.score += verify::score_adjustment(&candidate.raw, &expected);
            }
        }
        sort_candidates(&mut ranked);
        ranked
    };

    let raw = indexer::search_many(&state.http, indexers.clone(), &primary).await;
    let mut ranked = process(raw);

    if !fallbacks.is_empty() && needs_fallback(&ranked, &prefs, max_resolution) {
        let more = indexer::search_many(&state.http, indexers, &fallbacks).await;
        if !more.is_empty() {
            let combined = indexer::dedupe(ranked.into_iter().map(|c| c.raw).chain(more));
            ranked = process(combined);
        }
    }
    Ok(ranked)
}

/// TMDB metadata of the searched item, kept for post-search verification.
struct ItemInfo {
    title: String,
    year: Option<i32>,
    tvdb_id: Option<i64>,
    imdb_id: Option<String>,
    original_language: Option<String>,
    absolute_episode: Option<u32>,
    episode_air_year: Option<i32>,
    episode_title: Option<String>,
}

/// Build the indexer search strategies for one TV episode, split into the
/// primary lookup and the fallback batch. Every query burns per-indexer API
/// quota (real indexers rate-limit and temporarily ban), so the fallbacks
/// only run when the primary results are sparse — see [`needs_fallback`].
///
/// Primary: `tvsearch` by TVDB id + season + episode — the canonical
/// scene-numbered lookup (a `"{title} SxxExx"` free-text query when no TVDB
/// id is known).
///
/// Fallbacks:
/// 1. The `"{title} SxxExx"` free-text query, for indexers that do not index
///    the TVDB mapping.
/// 2. For **anime only** (original language Japanese), `"{title} {absolute}"`
///    free-text queries using the *absolute* episode number, in plain and
///    zero-padded forms. Anime is released and indexed by a running episode
///    count (`One Piece - 1100`, `One Piece - 0036`, `E0629`) rather than
///    `SxxExx`, so scene-numbered lookups return nothing for it. Skipped when
///    the absolute number cannot be derived from TMDB's season data.
pub fn tv_search_queries(
    show: &TvShow,
    season: u32,
    episode: u32,
) -> (Vec<SearchQuery>, Vec<SearchQuery>) {
    let text_query = SearchQuery::Raw {
        query: format!("{} S{season:02}E{episode:02}", show.title),
    };
    let (primary, mut fallbacks) = match show.tvdb_id {
        Some(tvdb_id) => (
            SearchQuery::TvByTvdb {
                tvdb_id,
                season,
                episode,
            },
            vec![text_query],
        ),
        None => (text_query, Vec::new()),
    };
    // Anime is numbered absolutely, not SxxExx. Gate on Japanese original
    // language and require enough season data to compute the running count.
    if show.original_language.as_deref() == Some("ja") {
        if let Some(absolute) = absolute_episode(show, season, episode) {
            let mut forms = vec![
                format!("{absolute}"),
                format!("{absolute:03}"),
                format!("{absolute:04}"),
            ];
            forms.dedup();
            for form in forms {
                fallbacks.push(SearchQuery::Raw {
                    query: format!("{} {form}", show.title),
                });
            }
        }
    }
    (vec![primary], fallbacks)
}

/// Accepted-candidate count below which the primary search counts as
/// "sparse" and the fallback queries are worth their indexer API quota.
const SPARSE_RESULTS_THRESHOLD: usize = 10;

/// Whether to spend quota on the fallback queries: the primary results are
/// sparse AND none of them delivers the preferred resolution. A rich primary
/// result without the preferred resolution (say dozens of candidates, all
/// 1080p, preference 2160p) means the quality simply doesn't exist — more
/// text queries won't conjure it.
fn needs_fallback(
    ranked: &[RankedRelease],
    prefs: &db::preferences::Preferences,
    device_cap: Option<Resolution>,
) -> bool {
    let mut preferred = prefs.preferred_resolution.min(prefs.max_resolution);
    if let Some(cap) = device_cap {
        preferred = preferred.min(cap);
    }
    let mut accepted = 0usize;
    let mut has_preferred = false;
    for candidate in ranked.iter().filter(|c| c.rejected.is_none()) {
        accepted += 1;
        has_preferred |= candidate.parsed.resolution == Some(preferred);
    }
    accepted < SPARSE_RESULTS_THRESHOLD && !has_preferred
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
    let candidates = resolve_candidates(&state, &target, params.max_resolution).await?;
    Ok(Json(ReleasesResponse { candidates }))
}

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(find_releases))
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
    fn anime_show_issues_absolute_number_fallbacks() {
        let s = show(
            "One Piece",
            "ja",
            vec![season(1, Some(1099)), season(2, Some(50))],
        );
        let (primary, fallbacks) = tv_search_queries(&s, 2, 1);
        // Primary: tvsearch. Fallbacks: SxxExx + absolute-number forms
        // ("One Piece 1100" — 4-digit, so no extra padded variants).
        assert_eq!(primary.len(), 1);
        assert!(matches!(primary[0], SearchQuery::TvByTvdb { .. }));
        assert_eq!(fallbacks.len(), 2, "fallbacks were: {fallbacks:?}");
        assert!(matches!(&fallbacks[0], SearchQuery::Raw { query } if query == "One Piece S02E01"));
        assert!(matches!(&fallbacks[1], SearchQuery::Raw { query } if query == "One Piece 1100"));
    }

    #[test]
    fn anime_absolute_fallbacks_include_zero_padded_forms() {
        let s = show(
            "One Piece",
            "ja",
            vec![season(1, Some(30)), season(2, Some(50))],
        );
        let (_, fallbacks) = tv_search_queries(&s, 2, 6);
        // Absolute 36: releases carry "36", "036" and "0036" style numbers.
        let raw: Vec<&str> = fallbacks
            .iter()
            .filter_map(|q| match q {
                SearchQuery::Raw { query } => Some(query.as_str()),
                _ => None,
            })
            .collect();
        assert_eq!(
            raw,
            [
                "One Piece S02E06",
                "One Piece 36",
                "One Piece 036",
                "One Piece 0036"
            ]
        );
    }

    #[test]
    fn non_anime_show_does_not_issue_absolute_number_query() {
        let s = show(
            "Breaking Bad",
            "en",
            vec![season(1, Some(7)), season(2, Some(13))],
        );
        let (primary, fallbacks) = tv_search_queries(&s, 2, 1);
        // tvsearch primary + SxxExx fallback only; no absolute strategy.
        assert_eq!(primary.len(), 1);
        assert_eq!(fallbacks.len(), 1);
        assert!(
            matches!(&fallbacks[0], SearchQuery::Raw { query } if query == "Breaking Bad S02E01")
        );
    }

    #[test]
    fn anime_without_season_data_skips_absolute_query_gracefully() {
        let s = show("Mystery Anime", "ja", vec![]);
        let (primary, fallbacks) = tv_search_queries(&s, 1, 1);
        // Only tvsearch + SxxExx; absolute skipped since season data is absent.
        assert_eq!(primary.len(), 1);
        assert_eq!(fallbacks.len(), 1);
        assert!(!fallbacks
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
    fn guid_pin_ignores_the_preferred_title() {
        let candidates = vec![candidate("A", 30, None), candidate("B", 20, None)];
        let picked = pick_candidates(&candidates, Some("guid-B"), Some("A"), 5).unwrap();
        assert_eq!(picked.len(), 1);
        assert_eq!(picked[0].raw.title, "B");
    }
}
