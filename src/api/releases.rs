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
    release::rank::{rank, RankedRelease},
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
/// against the stored preferences. Used by `GET /releases` and by
/// `POST /stream/sessions`.
pub async fn resolve_candidates(
    state: &AppState,
    target: &ReleaseTarget,
) -> AppResult<Vec<RankedRelease>> {
    let indexers = db::indexers::list_enabled(&state.db).await?;
    if indexers.is_empty() {
        return Err(AppError::BadRequest(
            "no enabled indexers configured; add one via POST /api/v1/settings/indexers".into(),
        ));
    }

    let tmdb = tmdb_client(state).await?;
    let query = match target.media_type {
        MediaType::Movie => {
            let movie = tmdb.movie_details(target.tmdb_id).await?;
            match movie.imdb_id {
                Some(imdb_id) => SearchQuery::MovieByImdb { imdb_id },
                None => SearchQuery::Raw {
                    query: match movie.year {
                        Some(year) => format!("{} {year}", movie.title),
                        None => movie.title,
                    },
                },
            }
        }
        MediaType::Tv => {
            let (season, episode) = target
                .season
                .zip(target.episode)
                .expect("validated TV target");
            let show = tmdb.tv_details(target.tmdb_id).await?;
            match show.tvdb_id {
                Some(tvdb_id) => SearchQuery::TvByTvdb {
                    tvdb_id,
                    season,
                    episode,
                },
                None => SearchQuery::Raw {
                    query: format!("{} S{season:02}E{episode:02}", show.title),
                },
            }
        }
    };

    let raw = indexer::search_all(&state.http, indexers, &query).await;
    let prefs = db::preferences::get(&state.db).await?;
    Ok(rank(raw, &prefs))
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
    let candidates = resolve_candidates(&state, &target).await?;
    Ok(Json(ReleasesResponse { candidates }))
}

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(find_releases))
}
