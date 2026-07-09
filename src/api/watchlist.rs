//! Watchlist: save movies/TV shows for later. Rows carry denormalized TMDB
//! details (fetched once on add), so listing renders with zero TMDB calls.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::{
    db::{self, watchlist::WatchlistEntry},
    error::{AppError, AppResult},
    state::AppState,
    tmdb::models::MediaType,
};

use super::metadata::tmdb_client;

/// One saved item with its denormalized TMDB details.
#[derive(Debug, Serialize, ToSchema)]
pub struct WatchlistItem {
    pub id: i64,
    pub tmdb_id: i64,
    /// `movie` or `tv`.
    pub media_type: String,
    pub title: String,
    pub year: Option<i64>,
    pub poster_url: Option<String>,
    pub backdrop_url: Option<String>,
    pub overview: Option<String>,
    pub vote_average: Option<f64>,
    /// UTC timestamp when the item was added.
    pub added_at: String,
}

impl From<WatchlistEntry> for WatchlistItem {
    fn from(entry: WatchlistEntry) -> Self {
        Self {
            id: entry.id,
            tmdb_id: entry.tmdb_id,
            media_type: entry.media_type,
            title: entry.title,
            year: entry.year,
            poster_url: entry.poster_url,
            backdrop_url: entry.backdrop_url,
            overview: entry.overview,
            vote_average: entry.vote_average,
            added_at: entry.added_at,
        }
    }
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct AddWatchlistRequest {
    /// TMDB id of the movie or show.
    pub tmdb_id: i64,
    /// `movie` or `tv`.
    pub media_type: MediaType,
}

/// Add an item. The server fetches title/artwork/overview from TMDB and
/// stores them with the row. Idempotent: re-adding an item returns the
/// existing row with 200 instead of 201 (and skips the TMDB fetch).
#[utoipa::path(post, path = "/watchlist", tag = "watchlist",
    request_body = AddWatchlistRequest,
    responses(
        (status = 201, body = WatchlistItem, description = "Newly added"),
        (status = 200, body = WatchlistItem, description = "Was already on the watchlist"),
        (status = 400, description = "TMDB API key not configured"),
        (status = 404, description = "Unknown TMDB id"),
        (status = 502, description = "TMDB upstream error"),
    ))]
pub async fn add_to_watchlist(
    State(state): State<AppState>,
    axum::Extension(current): axum::Extension<super::auth::CurrentUser>,
    Json(request): Json<AddWatchlistRequest>,
) -> AppResult<(StatusCode, Json<WatchlistItem>)> {
    let media_type = request.media_type.as_str();
    if let Some(existing) =
        db::watchlist::get(&state.db, current.id, request.tmdb_id, media_type).await?
    {
        return Ok((StatusCode::OK, Json(existing.into())));
    }

    let client = tmdb_client(&state).await?;
    let entry = match request.media_type {
        MediaType::Movie => {
            let movie = client.movie_details(request.tmdb_id).await?;
            db::watchlist::NewWatchlistEntry {
                tmdb_id: request.tmdb_id,
                media_type: media_type.into(),
                title: movie.title,
                year: movie.year,
                poster_url: movie.poster_url,
                backdrop_url: movie.backdrop_url,
                overview: movie.overview,
                vote_average: movie.vote_average,
            }
        }
        MediaType::Tv => {
            let show = client.tv_details(request.tmdb_id).await?;
            db::watchlist::NewWatchlistEntry {
                tmdb_id: request.tmdb_id,
                media_type: media_type.into(),
                title: show.title,
                year: show.year,
                poster_url: show.poster_url,
                backdrop_url: show.backdrop_url,
                overview: show.overview,
                vote_average: show.vote_average,
            }
        }
    };

    let (row, created) = db::watchlist::add(&state.db, current.id, &entry).await?;
    let status = if created {
        StatusCode::CREATED
    } else {
        StatusCode::OK
    };
    Ok((status, Json(row.into())))
}

/// The watchlist, most recently added first.
#[utoipa::path(get, path = "/watchlist", tag = "watchlist",
    responses((status = 200, body = [WatchlistItem])))]
pub async fn list_watchlist(
    State(state): State<AppState>,
    axum::Extension(current): axum::Extension<super::auth::CurrentUser>,
) -> AppResult<Json<Vec<WatchlistItem>>> {
    let entries = db::watchlist::list(&state.db, current.id).await?;
    Ok(Json(entries.into_iter().map(Into::into).collect()))
}

#[derive(Debug, Deserialize, IntoParams)]
pub struct WatchlistStatusParams {
    /// TMDB id of the movie or show.
    pub tmdb_id: i64,
    /// `movie` or `tv`.
    pub media_type: MediaType,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct WatchlistStatus {
    pub in_watchlist: bool,
}

/// Whether an item is on the watchlist (for detail screens).
#[utoipa::path(get, path = "/watchlist/status", tag = "watchlist",
    params(WatchlistStatusParams),
    responses((status = 200, body = WatchlistStatus)))]
pub async fn watchlist_status(
    State(state): State<AppState>,
    axum::Extension(current): axum::Extension<super::auth::CurrentUser>,
    Query(params): Query<WatchlistStatusParams>,
) -> AppResult<Json<WatchlistStatus>> {
    let entry = db::watchlist::get(
        &state.db,
        current.id,
        params.tmdb_id,
        params.media_type.as_str(),
    )
    .await?;
    Ok(Json(WatchlistStatus {
        in_watchlist: entry.is_some(),
    }))
}

#[derive(Debug, Deserialize, IntoParams)]
pub struct RemoveWatchlistParams {
    /// `movie` or `tv`.
    pub media_type: MediaType,
}

/// Remove an item from the watchlist.
#[utoipa::path(delete, path = "/watchlist/{tmdb_id}", tag = "watchlist",
    params(
        ("tmdb_id" = i64, Path, description = "TMDB id of the movie or show"),
        RemoveWatchlistParams,
    ),
    responses((status = 204), (status = 404, description = "Item is not on the watchlist")))]
pub async fn remove_from_watchlist(
    State(state): State<AppState>,
    axum::Extension(current): axum::Extension<super::auth::CurrentUser>,
    Path(tmdb_id): Path<i64>,
    Query(params): Query<RemoveWatchlistParams>,
) -> AppResult<StatusCode> {
    if db::watchlist::delete(&state.db, current.id, tmdb_id, params.media_type.as_str()).await? {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AppError::NotFound(format!(
            "watchlist item {tmdb_id} ({})",
            params.media_type.as_str()
        )))
    }
}

pub fn router() -> OpenApiRouter<AppState> {
    // `/watchlist/status` is static and wins over the `/watchlist/{tmdb_id}`
    // capture in axum 0.8.
    OpenApiRouter::new()
        .routes(routes!(add_to_watchlist, list_watchlist))
        .routes(routes!(watchlist_status))
        .routes(routes!(remove_from_watchlist))
}
