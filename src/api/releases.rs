//! Release search: TMDB id → indexer fan-out → parsed & ranked candidates.

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
    // Validate parameters before touching TMDB or the indexers.
    let tv_target = match params.media_type {
        MediaType::Tv => Some(params.season.zip(params.episode).ok_or_else(|| {
            AppError::BadRequest("season and episode are required for type=tv".into())
        })?),
        MediaType::Movie => None,
    };

    let indexers = db::indexers::list_enabled(&state.db).await?;
    if indexers.is_empty() {
        return Err(AppError::BadRequest(
            "no enabled indexers configured; add one via POST /api/v1/settings/indexers".into(),
        ));
    }

    let tmdb = tmdb_client(&state).await?;
    let query = match params.media_type {
        MediaType::Movie => {
            let movie = tmdb.movie_details(params.tmdb_id).await?;
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
            let (season, episode) = tv_target.expect("validated above");
            let show = tmdb.tv_details(params.tmdb_id).await?;
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
    let candidates = rank(raw, &prefs);
    Ok(Json(ReleasesResponse { candidates }))
}

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(find_releases))
}
