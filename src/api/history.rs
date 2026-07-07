//! Watch history: list watched items, upsert playback positions (directly
//! or via a per-session convenience endpoint) and delete entries.

use axum::extract::{Path, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use crate::{
    db::{self, watch_history::HistoryEntry},
    error::{AppError, AppResult},
    state::AppState,
    tmdb::models::MediaType,
};

/// One watched item with the stored resume position.
#[derive(Debug, Serialize, ToSchema)]
pub struct HistoryItem {
    pub id: i64,
    pub tmdb_id: i64,
    /// `movie` or `tv`.
    pub media_type: String,
    pub season: Option<i64>,
    pub episode: Option<i64>,
    /// Release last used to play this item (when played through us).
    pub release_title: Option<String>,
    pub position_secs: f64,
    pub duration_secs: Option<f64>,
    /// UTC timestamp of the last position update / session start.
    pub watched_at: String,
    /// 0–100, when the duration is known.
    pub percent_watched: Option<f64>,
    /// Movie or show title, captured at session start (missing on rows
    /// recorded before metadata capture existed).
    pub title: Option<String>,
    pub poster_url: Option<String>,
    pub backdrop_url: Option<String>,
    /// Episode title (tv only).
    pub episode_title: Option<String>,
    /// Episode still image (tv only).
    pub still_url: Option<String>,
}

impl From<HistoryEntry> for HistoryItem {
    fn from(entry: HistoryEntry) -> Self {
        let percent_watched = entry
            .duration_secs
            .filter(|d| *d > 0.0)
            .map(|d| (entry.position_secs / d * 100.0).clamp(0.0, 100.0));
        Self {
            id: entry.id,
            tmdb_id: entry.tmdb_id,
            media_type: entry.media_type,
            season: entry.season,
            episode: entry.episode,
            release_title: entry.release_title,
            position_secs: entry.position_secs,
            duration_secs: entry.duration_secs,
            watched_at: entry.watched_at,
            percent_watched,
            title: entry.title,
            poster_url: entry.poster_url,
            backdrop_url: entry.backdrop_url,
            episode_title: entry.episode_title,
            still_url: entry.still_url,
        }
    }
}

/// The watch history, most recently watched first.
#[utoipa::path(get, path = "/history", tag = "history",
    responses((status = 200, body = [HistoryItem])))]
pub async fn list_history(State(state): State<AppState>) -> AppResult<Json<Vec<HistoryItem>>> {
    let entries = db::watch_history::list(&state.db).await?;
    Ok(Json(entries.into_iter().map(Into::into).collect()))
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct UpdateHistoryRequest {
    /// TMDB id of the movie or show.
    pub tmdb_id: i64,
    /// `movie` or `tv`.
    pub media_type: MediaType,
    /// Season number (required for `tv`).
    pub season: Option<u32>,
    /// Episode number (required for `tv`).
    pub episode: Option<u32>,
    /// Playback position in seconds.
    pub position_secs: f64,
    /// Total duration in seconds; only overwrites the stored value when set.
    pub duration_secs: Option<f64>,
}

/// Store a playback position (upsert per item) and bump `watched_at`.
#[utoipa::path(post, path = "/history", tag = "history",
    request_body = UpdateHistoryRequest,
    responses((status = 200, body = HistoryItem), (status = 400)))]
pub async fn update_history(
    State(state): State<AppState>,
    Json(request): Json<UpdateHistoryRequest>,
) -> AppResult<Json<HistoryItem>> {
    if request.media_type == MediaType::Tv
        && (request.season.is_none() || request.episode.is_none())
    {
        return Err(AppError::BadRequest(
            "season and episode are required for media_type=tv".into(),
        ));
    }
    let position_secs = validated_position(request.position_secs)?;
    let duration_secs = match request.duration_secs {
        Some(d) if !d.is_finite() || d < 0.0 => {
            return Err(AppError::BadRequest(
                "duration_secs must be finite and non-negative".into(),
            ))
        }
        other => other,
    };
    let entry = db::watch_history::upsert_position(
        &state.db,
        &db::watch_history::PositionUpdate {
            tmdb_id: request.tmdb_id,
            media_type: request.media_type.as_str(),
            season: request.season,
            episode: request.episode,
            position_secs,
            duration_secs,
        },
    )
    .await?;
    // A finished position (the manual "mark watched") propagates to Trakt's
    // watch history, best-effort — like the Jellyfin plugin.
    if let Some(item) = trakt_item(
        request.tmdb_id,
        request.media_type,
        request.season,
        request.episode,
    ) {
        let finished = entry
            .duration_secs
            .filter(|d| *d > 0.0)
            .is_some_and(|d| entry.position_secs >= d * 0.95);
        if finished {
            super::trakt::spawn_history_write(&state, item, true);
        }
    }
    Ok(Json(entry.into()))
}

/// Remove one item from the history.
#[utoipa::path(delete, path = "/history/{id}", tag = "history",
    params(("id" = i64, Path, description = "History entry id")),
    responses((status = 204), (status = 404)))]
pub async fn delete_history(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> AppResult<StatusCode> {
    // Grab the row first: unwatching something that was finished also
    // removes it from the Trakt history (best-effort), mirroring the add.
    let entry = db::watch_history::get(&state.db, id).await?;
    if db::watch_history::delete(&state.db, id).await? {
        if let Some(entry) = entry {
            let finished = entry
                .duration_secs
                .filter(|d| *d > 0.0)
                .is_some_and(|d| entry.position_secs >= d * 0.95);
            let media_type = if entry.media_type == "tv" {
                MediaType::Tv
            } else {
                MediaType::Movie
            };
            if finished {
                if let Some(item) = trakt_item(
                    entry.tmdb_id,
                    media_type,
                    entry.season.map(|s| s as u32),
                    entry.episode.map(|e| e as u32),
                ) {
                    super::trakt::spawn_history_write(&state, item, false);
                }
            }
        }
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AppError::NotFound(format!("history entry {id}")))
    }
}

/// The Trakt reference for a history item; None for a tv row without both
/// season and episode numbers.
fn trakt_item(
    tmdb_id: i64,
    media_type: MediaType,
    season: Option<u32>,
    episode: Option<u32>,
) -> Option<crate::trakt::ScrobbleItem> {
    match media_type {
        MediaType::Movie => Some(crate::trakt::ScrobbleItem::Movie { tmdb_id }),
        MediaType::Tv => Some(crate::trakt::ScrobbleItem::Episode {
            show_tmdb_id: tmdb_id,
            season: season?,
            episode: episode?,
        }),
    }
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct SessionPositionRequest {
    /// Current playback position in seconds.
    pub position_secs: f64,
}

/// Convenience for players: report the playback position of a live session.
/// The item identity and duration come from the session itself.
#[utoipa::path(put, path = "/stream/{session_id}/position", tag = "history",
    params(("session_id" = Uuid, Path, description = "Session id")),
    request_body = SessionPositionRequest,
    responses((status = 200, body = HistoryItem), (status = 404)))]
pub async fn update_session_position(
    State(state): State<AppState>,
    Path(session_id): Path<Uuid>,
    Json(request): Json<SessionPositionRequest>,
) -> AppResult<Json<HistoryItem>> {
    let session = state
        .sessions
        .get(&session_id)
        .ok_or_else(|| AppError::NotFound(format!("session {session_id}")))?;
    session.touch();
    let position_secs = validated_position(request.position_secs)?;
    let entry = db::watch_history::upsert_position(
        &state.db,
        &db::watch_history::PositionUpdate {
            tmdb_id: session.tmdb_id,
            media_type: session.media_type.as_str(),
            season: session.season,
            episode: session.episode,
            position_secs,
            duration_secs: session.info().duration_secs,
        },
    )
    .await?;
    Ok(Json(entry.into()))
}

fn validated_position(position_secs: f64) -> AppResult<f64> {
    if !position_secs.is_finite() || position_secs < 0.0 {
        return Err(AppError::BadRequest(
            "position_secs must be finite and non-negative".into(),
        ));
    }
    Ok(position_secs)
}

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(list_history, update_history))
        .routes(routes!(delete_history))
        .routes(routes!(update_session_position))
}
