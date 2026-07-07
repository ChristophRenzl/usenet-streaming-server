//! Download jobs: enqueue a release for server-side download (same
//! resolution pipeline as streaming), inspect progress, cancel or delete.

use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::Json;
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use crate::{
    db::{self, downloads::Download},
    download::DownloadJob,
    error::{AppError, AppResult},
    release::parse::Resolution,
    state::AppState,
    tmdb::models::MediaType,
};

use super::releases::{pick_candidates, resolve_candidates, ReleaseTarget};
use super::stream::{fetch_healthy_release, MAX_ATTEMPTS};

#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateDownloadRequest {
    /// TMDB id of the movie or show.
    pub tmdb_id: i64,
    /// `movie` or `tv`.
    pub media_type: MediaType,
    /// Season number (required for `tv`).
    pub season: Option<u32>,
    /// Episode number (required for `tv`).
    pub episode: Option<u32>,
    /// Pin a specific release by its indexer guid instead of automatic
    /// candidate selection.
    pub release_guid: Option<String>,
    /// Device capability cap (`480p`, `720p`, `1080p`, `2160p`): releases
    /// above the lower of this and the stored preference max are rejected,
    /// and the best supported resolution ranks first.
    pub max_resolution: Option<Resolution>,
    /// When `true`, ignore the stored blocked terms so pre-release/low-quality
    /// rips (CAM/TS/…) are candidates.
    #[serde(default)]
    pub ignore_blocked_terms: bool,
}

/// A download row plus the computed completion percentage.
#[derive(Debug, Serialize, ToSchema)]
pub struct DownloadItem {
    #[serde(flatten)]
    pub download: Download,
    /// 0–100, when the total size is known.
    pub percent: Option<f64>,
}

impl From<Download> for DownloadItem {
    fn from(download: Download) -> Self {
        let percent = match (download.status.as_str(), download.total_bytes) {
            ("complete", _) => Some(100.0),
            (_, Some(total)) if total > 0 => {
                Some((download.progress_bytes as f64 / total as f64 * 100.0).clamp(0.0, 100.0))
            }
            _ => None,
        };
        Self { download, percent }
    }
}

/// Queue a download: resolve releases like session creation, pick the first
/// healthy candidate (guid pin or rank order, up to 5 attempts), then run
/// the copy in the background.
#[utoipa::path(post, path = "/downloads", tag = "downloads",
    request_body = CreateDownloadRequest,
    responses(
        (status = 202, body = DownloadItem, description = "Job accepted and running"),
        (status = 400, description = "Bad parameters, missing indexers or TMDB key"),
        (status = 404, description = "Unknown TMDB id or release_guid"),
        (status = 422, description = "No healthy release found; details list per-candidate reasons"),
    ))]
pub async fn create_download(
    State(state): State<AppState>,
    Json(request): Json<CreateDownloadRequest>,
) -> AppResult<(StatusCode, Json<DownloadItem>)> {
    let target = ReleaseTarget {
        tmdb_id: request.tmdb_id,
        media_type: request.media_type,
        season: request.season,
        episode: request.episode,
    }
    .validated()?;

    let candidates = resolve_candidates(
        &state,
        &target,
        request.max_resolution,
        request.ignore_blocked_terms,
    )
    .await?;
    let to_try = pick_candidates(
        &candidates,
        request.release_guid.as_deref(),
        None,
        MAX_ATTEMPTS,
    )?;

    let mut failures: Vec<String> = Vec::new();
    for candidate in to_try {
        match fetch_healthy_release(&state, &candidate).await {
            Ok((nzb, main)) => {
                let id = Uuid::new_v4();
                let row = db::downloads::insert(
                    &state.db,
                    &db::downloads::NewDownload {
                        id: &id.to_string(),
                        tmdb_id: target.tmdb_id,
                        media_type: target.media_type.as_str(),
                        season: target.season,
                        episode: target.episode,
                        release_title: &candidate.raw.title,
                        nzb_url: &candidate.raw.nzb_url,
                    },
                )
                .await?;
                state
                    .downloads
                    .spawn(state.clone(), id, DownloadJob::plain(nzb, main));
                tracing::info!(download = %id, release = %candidate.raw.title, "download queued");
                return Ok((StatusCode::ACCEPTED, Json(row.into())));
            }
            Err(error) => {
                tracing::warn!(release = %candidate.raw.title, %error, "candidate failed, trying next");
                failures.push(format!("{}: {error}", candidate.raw.title));
            }
        }
    }
    Err(AppError::NoRelease(failures.join("; ")))
}

/// All download jobs, newest first.
#[utoipa::path(get, path = "/downloads", tag = "downloads",
    responses((status = 200, body = [DownloadItem])))]
pub async fn list_downloads(State(state): State<AppState>) -> AppResult<Json<Vec<DownloadItem>>> {
    let rows = db::downloads::list(&state.db).await?;
    Ok(Json(rows.into_iter().map(Into::into).collect()))
}

/// One download job with progress.
#[utoipa::path(get, path = "/downloads/{id}", tag = "downloads",
    params(("id" = Uuid, Path, description = "Download id")),
    responses((status = 200, body = DownloadItem), (status = 404)))]
pub async fn get_download(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
) -> AppResult<Json<DownloadItem>> {
    let row = db::downloads::get(&state.db, &id.to_string())
        .await?
        .ok_or_else(|| AppError::NotFound(format!("download {id}")))?;
    Ok(Json(row.into()))
}

#[derive(Debug, Deserialize, IntoParams)]
pub struct DeleteDownloadParams {
    /// Also delete the downloaded file from disk (finished downloads only).
    #[serde(default)]
    pub delete_file: bool,
}

/// Cancel or delete a download. A running job is cancelled: its partial
/// file is removed and the row is kept with status `cancelled`. A finished
/// job's row is deleted — pass `delete_file=true` to also remove the
/// downloaded file.
#[utoipa::path(delete, path = "/downloads/{id}", tag = "downloads",
    params(("id" = Uuid, Path, description = "Download id"), DeleteDownloadParams),
    responses(
        (status = 204, description = "Cancelled (row kept) or deleted"),
        (status = 404),
    ))]
pub async fn delete_download(
    State(state): State<AppState>,
    Path(id): Path<Uuid>,
    Query(params): Query<DeleteDownloadParams>,
) -> AppResult<StatusCode> {
    if state.downloads.cancel(&id).await {
        // The job cleaned up after itself; only fall through to deletion
        // when it actually finished before the cancel took effect.
        let row = db::downloads::get(&state.db, &id.to_string()).await?;
        if !matches!(row, Some(row) if row.status == "complete") {
            return Ok(StatusCode::NO_CONTENT);
        }
    }

    let row = db::downloads::get(&state.db, &id.to_string())
        .await?
        .ok_or_else(|| AppError::NotFound(format!("download {id}")))?;
    if params.delete_file {
        if let Some(path) = row.file_path.as_deref() {
            if let Err(error) = tokio::fs::remove_file(path).await {
                if error.kind() != std::io::ErrorKind::NotFound {
                    tracing::warn!(%path, %error, "failed to delete downloaded file");
                }
            }
        }
    }
    db::downloads::delete(&state.db, &id.to_string()).await?;
    Ok(StatusCode::NO_CONTENT)
}

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(create_download, list_downloads))
        .routes(routes!(get_download, delete_download))
}
