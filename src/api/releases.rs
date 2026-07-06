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
    },
    state::AppState,
    tmdb::models::MediaType,
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
    let (query, original_language) = match target.media_type {
        MediaType::Movie => {
            let movie = tmdb.movie_details(target.tmdb_id).await?;
            let query = match movie.imdb_id {
                Some(imdb_id) => SearchQuery::MovieByImdb { imdb_id },
                None => SearchQuery::Raw {
                    query: match movie.year {
                        Some(year) => format!("{} {year}", movie.title),
                        None => movie.title,
                    },
                },
            };
            (query, movie.original_language)
        }
        MediaType::Tv => {
            let (season, episode) = target
                .season
                .zip(target.episode)
                .expect("validated TV target");
            let show = tmdb.tv_details(target.tmdb_id).await?;
            let query = match show.tvdb_id {
                Some(tvdb_id) => SearchQuery::TvByTvdb {
                    tvdb_id,
                    season,
                    episode,
                },
                None => SearchQuery::Raw {
                    query: format!("{} S{season:02}E{episode:02}", show.title),
                },
            };
            (query, show.original_language)
        }
    };

    let raw = indexer::search_all(&state.http, indexers, &query).await;
    let prefs = db::preferences::get(&state.db).await?;
    Ok(rank(
        raw,
        &prefs,
        max_resolution,
        original_language.as_deref(),
    ))
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
