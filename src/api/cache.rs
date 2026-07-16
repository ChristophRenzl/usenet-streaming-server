//! Stream-cache administration: usage/entry stats and clearing. The
//! enable/disable toggle and the size cap live in `PUT /settings/app`
//! (`stream_cache_enabled`, `stream_cache_max_gb`).

use axum::extract::State;
use axum::Json;
use serde::Serialize;
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::{db, error::AppResult, state::AppState, stream_cache};

/// Current state of the persistent stream cache.
#[derive(Debug, Serialize, ToSchema)]
pub struct CacheStats {
    /// Whether new streams are cached (admin setting, default on).
    pub enabled: bool,
    /// Configured size cap in bytes.
    pub max_bytes: u64,
    /// Bytes currently used (completed entries in full, running ones by
    /// their progress).
    pub used_bytes: u64,
    /// Live cache entries (completed + currently writing).
    pub entry_count: u64,
    /// Absolute cache directory.
    pub cache_dir: String,
    /// Free bytes on the cache volume, when known. Eviction keeps at least
    /// 100 GB free here.
    pub free_disk_bytes: Option<u64>,
}

/// Stream-cache usage: size, entry count and the cache location.
#[utoipa::path(get, path = "/cache", tag = "cache",
    responses((status = 200, body = CacheStats)))]
pub async fn cache_stats(State(state): State<AppState>) -> AppResult<Json<CacheStats>> {
    let (used_bytes, entry_count) = db::downloads::cache_usage(&state.db).await?;
    let cache_dir = state.config.storage.cache_path();
    Ok(Json(CacheStats {
        enabled: stream_cache::enabled(&state.db).await,
        max_bytes: stream_cache::max_cache_bytes(&state.db).await,
        used_bytes,
        entry_count,
        free_disk_bytes: stream_cache::free_disk_bytes(&cache_dir),
        cache_dir: cache_dir.to_string_lossy().into_owned(),
    }))
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ClearCacheResponse {
    /// Number of cache entries removed. Entries whose release is being
    /// played right now are kept.
    pub removed: u64,
}

/// Clear the stream cache: cancel running cache writes and delete every
/// entry (file + database row). Entries currently being played are skipped.
#[utoipa::path(delete, path = "/cache", tag = "cache",
    responses((status = 200, body = ClearCacheResponse)))]
pub async fn clear_cache(State(state): State<AppState>) -> AppResult<Json<ClearCacheResponse>> {
    let removed = stream_cache::clear(&state).await?;
    tracing::info!(removed, "stream cache cleared");
    Ok(Json(ClearCacheResponse { removed }))
}

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(cache_stats, clear_cache))
}
