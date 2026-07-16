//! Persistent stream cache: everything streamed from NNTP is also written to
//! a dedicated cache directory (an auto-created download job tagged
//! `origin = 'cache'`), so later playback of the same movie/episode comes
//! straight from disk — for any user, the cache is global.
//!
//! Budgeting happens before a cache job starts writing: entries are evicted
//! LRU-first (by `last_played_at`, falling back to `created_at`) until the
//! new file fits under the configured size cap *and* the cache volume keeps
//! at least [`FREE_DISK_FLOOR_BYTES`] free. Entries whose release is playing
//! right now are never evicted.

use std::collections::HashSet;
use std::path::Path;

use uuid::Uuid;

use crate::db::{self, downloads::Download};
use crate::download::DownloadJob;
use crate::error::{AppError, AppResult};
use crate::nzb::{MainContent, Nzb};
use crate::release::rank::RankedRelease;
use crate::state::AppState;

/// Decimal gigabyte, matching how the admin UI counts sizes.
pub const GB: u64 = 1_000_000_000;

/// Default size cap when no `stream_cache_max_gb` setting is stored:
/// 5000 GB = 5 TB.
pub const DEFAULT_MAX_GB: u64 = 5000;

/// Eviction also kicks in when the cache volume would drop below this much
/// free space, regardless of the size cap.
pub const FREE_DISK_FLOOR_BYTES: u64 = 100 * GB;

/// Whether the stream cache is enabled (admin setting, default ON).
pub async fn enabled(pool: &sqlx::SqlitePool) -> bool {
    db::settings::get(pool, db::settings::STREAM_CACHE_ENABLED)
        .await
        .ok()
        .flatten()
        .as_deref()
        != Some("false")
}

/// The configured size cap in bytes (admin setting, default
/// [`DEFAULT_MAX_GB`]).
pub async fn max_cache_bytes(pool: &sqlx::SqlitePool) -> u64 {
    db::settings::get(pool, db::settings::STREAM_CACHE_MAX_GB)
        .await
        .ok()
        .flatten()
        .and_then(|v| v.trim().parse::<u64>().ok())
        .filter(|gb| *gb > 0)
        .unwrap_or(DEFAULT_MAX_GB)
        .saturating_mul(GB)
}

/// Free bytes on the volume holding `path` (nearest existing ancestor is
/// probed when the directory does not exist yet). `None` when the platform
/// has no statvfs or the probe fails — the free-disk floor is then skipped.
#[cfg(unix)]
pub fn free_disk_bytes(path: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;

    let mut probe = path;
    while !probe.exists() {
        probe = match probe.parent() {
            Some(parent) if !parent.as_os_str().is_empty() => parent,
            _ => Path::new("."),
        };
        if probe == Path::new(".") {
            break;
        }
    }
    let c_path = std::ffi::CString::new(probe.as_os_str().as_bytes()).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    let rc = unsafe { libc::statvfs(c_path.as_ptr(), &mut stat) };
    (rc == 0).then(|| (stat.f_bavail as u64).saturating_mul(stat.f_frsize as u64))
}

#[cfg(not(unix))]
pub fn free_disk_bytes(_path: &Path) -> Option<u64> {
    None
}

// ---- Eviction ---------------------------------------------------------------

/// One completed cache entry as seen by the eviction selector.
#[derive(Debug, Clone)]
pub struct EvictionCandidate {
    pub id: String,
    pub bytes: u64,
    /// Last cache hit (SQLite UTC text, lexicographically ordered).
    pub last_played_at: Option<String>,
    pub created_at: String,
    /// True when a live session is playing this entry's release — such
    /// entries are never evicted.
    pub playing: bool,
}

impl EvictionCandidate {
    /// LRU key: last playback, falling back to creation time.
    fn last_used(&self) -> &str {
        self.last_played_at.as_deref().unwrap_or(&self.created_at)
    }
}

/// Pick entries to evict (oldest-last-used first, skipping playing ones) so
/// that after writing `incoming_bytes`:
///
/// - total cache usage stays within `max_bytes`, and
/// - the cache volume keeps at least `free_floor_bytes` free (when
///   `free_disk_bytes` is known).
///
/// Returns the ids to delete, in eviction order. The selection may be
/// insufficient when too much is playing or the cache is simply too small —
/// the caller re-checks the budget after deleting.
pub fn select_evictions(
    candidates: &[EvictionCandidate],
    used_bytes: u64,
    incoming_bytes: u64,
    max_bytes: u64,
    free_disk_bytes: Option<u64>,
    free_floor_bytes: u64,
) -> Vec<String> {
    let needed_for_cap = (used_bytes.saturating_add(incoming_bytes)).saturating_sub(max_bytes);
    let needed_for_disk = free_disk_bytes
        .map(|free| (free_floor_bytes.saturating_add(incoming_bytes)).saturating_sub(free))
        .unwrap_or(0);
    let needed = needed_for_cap.max(needed_for_disk);
    if needed == 0 {
        return Vec::new();
    }

    let mut evictable: Vec<&EvictionCandidate> =
        candidates.iter().filter(|c| !c.playing).collect();
    evictable.sort_by(|a, b| {
        a.last_used()
            .cmp(b.last_used())
            .then_with(|| a.created_at.cmp(&b.created_at))
            .then_with(|| a.id.cmp(&b.id))
    });

    let mut freed: u64 = 0;
    let mut victims = Vec::new();
    for candidate in evictable {
        if freed >= needed {
            break;
        }
        freed = freed.saturating_add(candidate.bytes);
        victims.push(candidate.id.clone());
    }
    victims
}

/// Release titles currently being played by any live session.
fn playing_titles(state: &AppState) -> HashSet<String> {
    state
        .sessions
        .snapshot()
        .iter()
        .map(|s| s.release_title.clone())
        .collect()
}

/// Delete a cache entry: its file (best-effort) and its row.
async fn remove_entry(state: &AppState, entry: &Download) -> AppResult<()> {
    if let Some(path) = entry.file_path.as_deref() {
        if let Err(error) = tokio::fs::remove_file(path).await {
            if error.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(%path, %error, "failed to delete cached file");
            }
        }
    }
    db::downloads::delete(&state.db, &entry.id).await?;
    Ok(())
}

/// Make room for `incoming_bytes` in the cache: evict completed entries
/// LRU-first until the size cap and the free-disk floor are both satisfied.
/// Errors when the budget cannot be met (entry larger than the cap, disk
/// nearly full, everything else playing) — the caller (a cache job) then
/// fails without affecting playback.
pub async fn ensure_capacity(state: &AppState, incoming_bytes: u64) -> AppResult<()> {
    let max_bytes = max_cache_bytes(&state.db).await;
    let cache_dir = state.config.storage.cache_path();
    let playing = playing_titles(state);

    let entries = db::downloads::cache_entries(&state.db).await?;
    let completed: Vec<&Download> = entries
        .iter()
        .filter(|e| e.status == "complete")
        .collect();
    let candidates: Vec<EvictionCandidate> = completed
        .iter()
        .map(|e| EvictionCandidate {
            id: e.id.clone(),
            bytes: e.total_bytes.unwrap_or(e.progress_bytes).max(0) as u64,
            last_played_at: e.last_played_at.clone(),
            created_at: e.created_at.clone(),
            playing: playing.contains(&e.release_title),
        })
        .collect();

    let (used_bytes, _) = db::downloads::cache_usage(&state.db).await?;
    let free = free_disk_bytes(&cache_dir);
    let victims = select_evictions(
        &candidates,
        used_bytes,
        incoming_bytes,
        max_bytes,
        free,
        FREE_DISK_FLOOR_BYTES,
    );

    let mut freed: u64 = 0;
    for id in &victims {
        let Some(entry) = completed.iter().find(|e| &e.id == id) else {
            continue;
        };
        freed = freed.saturating_add(entry.total_bytes.unwrap_or(entry.progress_bytes).max(0) as u64);
        tracing::info!(
            entry = %entry.id,
            release = %entry.release_title,
            "evicting cache entry to make room"
        );
        remove_entry(state, entry).await?;
    }

    let used_after = used_bytes.saturating_sub(freed);
    if used_after.saturating_add(incoming_bytes) > max_bytes {
        return Err(AppError::Internal(anyhow::anyhow!(
            "stream cache full: {incoming_bytes} bytes incoming, {used_after} in use, cap {max_bytes}"
        )));
    }
    if let Some(free) = free {
        let free_after = free.saturating_add(freed);
        if free_after < FREE_DISK_FLOOR_BYTES.saturating_add(incoming_bytes) {
            return Err(AppError::Internal(anyhow::anyhow!(
                "cache volume nearly full: {free_after} bytes would remain, floor {FREE_DISK_FLOOR_BYTES}"
            )));
        }
    }
    Ok(())
}

// ---- Auto-caching a streamed release -----------------------------------------

/// Fire-and-forget entry point called when an NNTP streaming session starts:
/// persist the streamed release into the cache by spawning a cache-originated
/// download job. Best-effort — any problem is logged and playback is never
/// affected. The job runs independently of the session, so it keeps
/// downloading while playback is paused and completes the whole file.
#[allow(clippy::too_many_arguments)]
pub async fn cache_streamed_release(
    state: &AppState,
    tmdb_id: i64,
    media_type: &str,
    season: Option<u32>,
    episode: Option<u32>,
    candidate: &RankedRelease,
    nzb: Nzb,
    main: MainContent,
) {
    if !enabled(&state.db).await {
        return;
    }
    match try_cache(state, tmdb_id, media_type, season, episode, candidate, nzb, main).await {
        Ok(Some(id)) => {
            tracing::info!(cache_entry = %id, release = %candidate.raw.title, "caching streamed release");
        }
        Ok(None) => {
            tracing::debug!(release = %candidate.raw.title, "release already cached or downloading; not re-caching");
        }
        Err(error) => {
            tracing::warn!(release = %candidate.raw.title, %error, "failed to start cache job");
        }
    }
}

#[allow(clippy::too_many_arguments)]
async fn try_cache(
    state: &AppState,
    tmdb_id: i64,
    media_type: &str,
    season: Option<u32>,
    episode: Option<u32>,
    candidate: &RankedRelease,
    nzb: Nzb,
    main: MainContent,
) -> AppResult<Option<Uuid>> {
    // Dedupe: a live (pending/downloading/complete) entry of this exact
    // release — cache or user download — already serves/will serve from
    // disk. A `complete` row whose file vanished from disk does not count;
    // dead cache rows are dropped so the release can be re-cached.
    let mut already_covered = false;
    for existing in
        db::downloads::live_entries_for_release(&state.db, &candidate.raw.title).await?
    {
        if existing.status != "complete" {
            already_covered = true;
            continue;
        }
        let exists = match existing.file_path.as_deref() {
            Some(path) => tokio::fs::try_exists(path).await.unwrap_or(false),
            None => false,
        };
        if exists {
            already_covered = true;
        } else if existing.origin == "cache" {
            tracing::info!(
                entry = %existing.id,
                release = %existing.release_title,
                "dropping cache row whose file vanished from disk"
            );
            db::downloads::delete(&state.db, &existing.id).await?;
        }
    }
    if already_covered {
        return Ok(None);
    }

    // A different release of the same movie/episode supersedes the stale
    // cache entries (e.g. after a blacklist or an explicit release pin):
    // delete them, unless they are being played right now.
    let playing = playing_titles(state);
    for stale in
        db::downloads::cache_entries_for_item(&state.db, tmdb_id, media_type, season, episode)
            .await?
    {
        if stale.release_title == candidate.raw.title || playing.contains(&stale.release_title) {
            continue;
        }
        if let Ok(uuid) = Uuid::parse_str(&stale.id) {
            // A still-running job cleans up its partial file on cancel.
            state.downloads.cancel(&uuid).await;
        }
        tracing::info!(
            entry = %stale.id,
            release = %stale.release_title,
            "removing stale cache entry superseded by a new release"
        );
        remove_entry(state, &stale).await?;
    }

    let id = Uuid::new_v4();
    db::downloads::insert(
        &state.db,
        &db::downloads::NewDownload {
            id: &id.to_string(),
            tmdb_id,
            media_type,
            season,
            episode,
            release_title: &candidate.raw.title,
            nzb_url: &candidate.raw.nzb_url,
            origin: "cache",
        },
    )
    .await?;
    state
        .downloads
        .spawn(state.clone(), id, DownloadJob::cache(nzb, main));
    Ok(Some(id))
}

// ---- Admin operations ---------------------------------------------------------

/// Clear the cache: cancel running cache jobs and delete every entry (file +
/// row), skipping entries whose release is playing right now. Returns how
/// many entries were removed.
pub async fn clear(state: &AppState) -> AppResult<u64> {
    let playing = playing_titles(state);
    let mut removed = 0;
    for entry in db::downloads::cache_entries(&state.db).await? {
        if playing.contains(&entry.release_title) {
            tracing::info!(entry = %entry.id, release = %entry.release_title, "skipping cache entry in use");
            continue;
        }
        if let Ok(uuid) = Uuid::parse_str(&entry.id) {
            state.downloads.cancel(&uuid).await;
        }
        remove_entry(state, &entry).await?;
        removed += 1;
    }
    Ok(removed)
}

/// Startup sweep: `.partial` files in the cache directory are leftovers of
/// jobs interrupted by a crash/restart (their rows are purged separately).
pub async fn sweep_stale_partials(cache_dir: &Path) {
    let Ok(mut entries) = tokio::fs::read_dir(cache_dir).await else {
        return;
    };
    while let Ok(Some(entry)) = entries.next_entry().await {
        let path = entry.path();
        if path.extension().is_some_and(|ext| ext == "partial") {
            if let Err(error) = tokio::fs::remove_file(&path).await {
                tracing::warn!(path = %path.display(), %error, "failed to remove stale cache partial");
            } else {
                tracing::info!(path = %path.display(), "removed stale cache partial");
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entry(id: &str, bytes: u64, created: &str, played: Option<&str>) -> EvictionCandidate {
        EvictionCandidate {
            id: id.into(),
            bytes,
            last_played_at: played.map(Into::into),
            created_at: created.into(),
            playing: false,
        }
    }

    fn playing(mut candidate: EvictionCandidate) -> EvictionCandidate {
        candidate.playing = true;
        candidate
    }

    const PLENTY_FREE: Option<u64> = Some(10_000 * GB);

    #[test]
    fn nothing_evicted_while_within_cap_and_floor() {
        let entries = vec![
            entry("a", 100, "2026-01-01 00:00:00", None),
            entry("b", 100, "2026-01-02 00:00:00", None),
        ];
        let victims = select_evictions(&entries, 200, 300, 1000, PLENTY_FREE, FREE_DISK_FLOOR_BYTES);
        assert!(victims.is_empty());
    }

    #[test]
    fn cap_overflow_evicts_oldest_created_first() {
        let entries = vec![
            entry("newer", 400, "2026-01-02 00:00:00", None),
            entry("oldest", 400, "2026-01-01 00:00:00", None),
            entry("newest", 400, "2026-01-03 00:00:00", None),
        ];
        // 1200 used + 300 incoming with a 1200 cap → free at least 300.
        let victims =
            select_evictions(&entries, 1200, 300, 1200, PLENTY_FREE, FREE_DISK_FLOOR_BYTES);
        assert_eq!(victims, vec!["oldest".to_string()]);

        // Needing more than one entry evicts in LRU order.
        let victims =
            select_evictions(&entries, 1200, 700, 1200, PLENTY_FREE, FREE_DISK_FLOOR_BYTES);
        assert_eq!(victims, vec!["oldest".to_string(), "newer".to_string()]);
    }

    #[test]
    fn last_played_at_wins_over_created_at() {
        // "old" was created first but played recently; "fresh" was created
        // later but never played since — "fresh" is the LRU victim.
        let entries = vec![
            entry("old", 500, "2026-01-01 00:00:00", Some("2026-06-30 20:00:00")),
            entry("fresh", 500, "2026-02-01 00:00:00", None),
        ];
        let victims =
            select_evictions(&entries, 1000, 200, 1000, PLENTY_FREE, FREE_DISK_FLOOR_BYTES);
        assert_eq!(victims, vec!["fresh".to_string()]);
    }

    #[test]
    fn free_disk_floor_triggers_eviction_below_the_cap() {
        let entries = vec![
            entry("a", 50 * GB, "2026-01-01 00:00:00", None),
            entry("b", 50 * GB, "2026-01-02 00:00:00", None),
        ];
        // Usage is far under the cap, but the volume only has 110 GB free and
        // 20 GB is incoming: 110 - 20 = 90 GB < 100 GB floor → evict "a".
        let victims = select_evictions(
            &entries,
            100 * GB,
            20 * GB,
            5000 * GB,
            Some(110 * GB),
            FREE_DISK_FLOOR_BYTES,
        );
        assert_eq!(victims, vec!["a".to_string()]);

        // Unknown free space skips the floor check entirely.
        let victims = select_evictions(
            &entries,
            100 * GB,
            20 * GB,
            5000 * GB,
            None,
            FREE_DISK_FLOOR_BYTES,
        );
        assert!(victims.is_empty());
    }

    #[test]
    fn playing_entries_are_never_evicted() {
        let entries = vec![
            playing(entry("in-use", 400, "2026-01-01 00:00:00", None)),
            entry("idle", 400, "2026-01-02 00:00:00", None),
        ];
        // Both would have to go to satisfy the budget; only the idle one may.
        let victims =
            select_evictions(&entries, 800, 800, 800, PLENTY_FREE, FREE_DISK_FLOOR_BYTES);
        assert_eq!(victims, vec!["idle".to_string()]);
    }
}
