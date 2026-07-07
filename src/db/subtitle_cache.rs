//! Cache of downloaded OpenSubtitles files, keyed by OpenSubtitles file id.
//!
//! Downloads are the quota-limited OpenSubtitles operation (20/day on free
//! accounts) while searches are free — so the search still runs per session
//! (it picks the best release match), but a file that was ever downloaded is
//! served from here forever after.

use sqlx::SqlitePool;

use crate::error::{AppError, AppResult};

/// Rows beyond this are pruned oldest-`last_used_at`-first on insert. At a
/// typical ~50 KB per subtitle this bounds the table around 50 MB.
const MAX_ROWS: i64 = 1000;

/// The cached subtitle text for a file id, updating its recency. `None` on a
/// cache miss.
pub async fn get(pool: &SqlitePool, file_id: i64) -> AppResult<Option<String>> {
    let row: Option<(String,)> =
        sqlx::query_as("SELECT srt FROM subtitle_cache WHERE file_id = ?")
            .bind(file_id)
            .fetch_optional(pool)
            .await
            .map_err(AppError::Database)?;
    if row.is_some() {
        sqlx::query("UPDATE subtitle_cache SET last_used_at = datetime('now') WHERE file_id = ?")
            .bind(file_id)
            .execute(pool)
            .await
            .map_err(AppError::Database)?;
    }
    Ok(row.map(|(srt,)| srt))
}

/// Store (or refresh) a downloaded subtitle, pruning least-recently-used
/// rows beyond the cap.
pub async fn put(pool: &SqlitePool, file_id: i64, srt: &str) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO subtitle_cache (file_id, srt) VALUES (?, ?)
         ON CONFLICT(file_id) DO UPDATE SET srt = excluded.srt,
             last_used_at = datetime('now')",
    )
    .bind(file_id)
    .bind(srt)
    .execute(pool)
    .await
    .map_err(AppError::Database)?;
    sqlx::query(
        "DELETE FROM subtitle_cache WHERE file_id NOT IN
             (SELECT file_id FROM subtitle_cache ORDER BY last_used_at DESC LIMIT ?)",
    )
    .bind(MAX_ROWS)
    .execute(pool)
    .await
    .map_err(AppError::Database)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool() -> SqlitePool {
        let pool = SqlitePool::connect("sqlite::memory:").await.expect("pool");
        sqlx::migrate!().run(&pool).await.expect("migrations");
        pool
    }

    #[tokio::test]
    async fn round_trip_and_miss() {
        let pool = pool().await;
        assert_eq!(get(&pool, 42).await.expect("get"), None);
        put(&pool, 42, "1\n00:00:01,000 --> 00:00:02,000\nHi\n")
            .await
            .expect("put");
        assert!(get(&pool, 42)
            .await
            .expect("get")
            .expect("hit")
            .contains("Hi"));
    }

    #[tokio::test]
    async fn upsert_replaces_text() {
        let pool = pool().await;
        put(&pool, 7, "old").await.expect("put");
        put(&pool, 7, "new").await.expect("put");
        assert_eq!(get(&pool, 7).await.expect("get").as_deref(), Some("new"));
    }
}
