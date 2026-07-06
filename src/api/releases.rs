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
    tmdb::models::{MediaType, SeasonSummary},
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
                vec![query],
                ItemInfo {
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
            let mut queries = vec![match show.tvdb_id {
                Some(tvdb_id) => SearchQuery::TvByTvdb {
                    tvdb_id,
                    season,
                    episode,
                },
                None => SearchQuery::Raw {
                    query: format!("{} S{season:02}E{episode:02}", show.title),
                },
            }];
            // Anime is mostly indexed with absolute episode numbering
            // ("One Piece - 0901"), which tvsearch/SxxEyy queries miss
            // entirely for long-running shows. Fan out an extra free-text
            // search by the absolute number for Japanese titles.
            let absolute_episode = absolute_episode_number(&show.seasons, season, episode);
            if show.original_language.as_deref() == Some("ja") {
                if let Some(absolute) = absolute_episode {
                    queries.push(SearchQuery::Raw {
                        query: format!("{} {absolute}", show.title),
                    });
                }
            }
            (
                queries,
                ItemInfo {
                    title: show.title,
                    year: show.year,
                    tvdb_id: show.tvdb_id,
                    imdb_id: show.imdb_id,
                    original_language: show.original_language,
                    absolute_episode,
                },
            )
        }
    };

    let searches = queries
        .iter()
        .map(|query| indexer::search_all(&state.http, indexers.clone(), query));
    let mut seen = std::collections::HashSet::new();
    let raw: Vec<_> = futures::future::join_all(searches)
        .await
        .into_iter()
        .flatten()
        .filter(|release| seen.insert(release.guid.clone()))
        .collect();

    let prefs = db::preferences::get(&state.db).await?;
    let mut ranked = rank(
        raw,
        &prefs,
        max_resolution,
        item.original_language.as_deref(),
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
    // Stable: keeps rank order within the accepted and rejected groups.
    ranked.sort_by_key(|c| c.rejected.is_some());
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
}

/// Absolute episode number across seasons (anime-style numbering): the
/// requested episode plus the episode counts of all prior regular seasons.
/// None when any prior season's episode count is unknown.
fn absolute_episode_number(seasons: &[SeasonSummary], season: u32, episode: u32) -> Option<u32> {
    if season == 0 {
        return None;
    }
    let mut absolute = episode;
    for summary in seasons {
        if summary.season_number >= 1 && summary.season_number < season {
            absolute += u32::try_from(summary.episode_count?).ok()?;
        }
    }
    Some(absolute)
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
    fn absolute_episode_numbering_sums_prior_regular_seasons() {
        let season = |number: u32, episodes: Option<i64>| SeasonSummary {
            season_number: number,
            title: None,
            episode_count: episodes,
            air_date: None,
            poster_url: None,
        };
        let seasons = vec![
            season(0, Some(12)), // specials never count
            season(1, Some(61)),
            season(2, Some(16)),
            season(3, Some(30)),
        ];
        assert_eq!(absolute_episode_number(&seasons, 1, 5), Some(5));
        assert_eq!(absolute_episode_number(&seasons, 3, 2), Some(79));
        assert_eq!(absolute_episode_number(&seasons, 0, 1), None);
        // Unknown prior count → cannot compute.
        let unknown = vec![season(1, None), season(2, Some(10))];
        assert_eq!(absolute_episode_number(&unknown, 3, 1), None);
    }

    #[test]
    fn guid_pin_ignores_the_preferred_title() {
        let candidates = vec![candidate("A", 30, None), candidate("B", 20, None)];
        let picked = pick_candidates(&candidates, Some("guid-B"), Some("A"), 5).unwrap();
        assert_eq!(picked.len(), 1);
        assert_eq!(picked[0].raw.title, "B");
    }
}
