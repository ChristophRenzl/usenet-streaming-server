//! Weighted in-memory cache for decoded yEnc segments.
//!
//! moka's `try_get_with` already coalesces concurrent fetches of the same
//! key (single-flight), so a separate in-flight map is unnecessary; errors
//! are not cached, so failed fetches are retried on the next demand read.

use std::sync::Arc;

use bytes::Bytes;
use moka::future::Cache;

use crate::error::{AppError, AppResult};

/// One decoded segment plus the yEnc metadata needed to place and reuse it
/// without re-decoding.
#[derive(Debug, Clone)]
pub struct CachedSegment {
    /// 0-based offset of this part within its file (from `=ypart`).
    pub begin: u64,
    /// Whole-file size (from `=ybegin size=`).
    pub file_size: u64,
    /// `name=` from `=ybegin`.
    pub file_name: Arc<str>,
    pub data: Bytes,
}

/// Shared decoded-segment cache, keyed by message-id. Cheap to clone.
#[derive(Clone)]
pub struct SegmentCache {
    cache: Cache<String, CachedSegment>,
}

impl SegmentCache {
    /// `max_bytes` should come from `CacheConfig::memory_bytes`.
    pub fn new(max_bytes: u64) -> Self {
        let cache = Cache::builder()
            .max_capacity(max_bytes)
            .weigher(|key: &String, value: &CachedSegment| {
                let approx = value.data.len() + key.len() + 64;
                u32::try_from(approx).unwrap_or(u32::MAX)
            })
            .build();
        Self { cache }
    }

    /// Get a decoded segment, running `fetch` on a miss. Concurrent callers
    /// for the same message-id share a single fetch.
    pub async fn get_or_fetch<F>(&self, message_id: &str, fetch: F) -> AppResult<CachedSegment>
    where
        F: std::future::Future<Output = AppResult<CachedSegment>> + Send,
    {
        self.cache
            .try_get_with(message_id.to_string(), fetch)
            .await
            .map_err(unwrap_shared_error)
    }

    pub fn contains(&self, message_id: &str) -> bool {
        self.cache.contains_key(message_id)
    }
}

/// moka shares one error among coalesced waiters as `Arc<AppError>`; unwrap
/// it when we are the only owner, otherwise reconstruct preserving the
/// variants downstream code matches on.
fn unwrap_shared_error(err: Arc<AppError>) -> AppError {
    Arc::try_unwrap(err).unwrap_or_else(|shared| match &*shared {
        AppError::MissingSegment(id) => AppError::MissingSegment(id.clone()),
        AppError::CompressedRarUnsupported => AppError::CompressedRarUnsupported,
        AppError::EncryptedRarUnsupported => AppError::EncryptedRarUnsupported,
        other => AppError::Upstream(other.to_string()),
    })
}
