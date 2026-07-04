//! Download-job rows: lifecycle status, progress and the final file path.
//! All rows belong to the single default user (id 1), like watch history.

use serde::Serialize;
use sqlx::SqlitePool;
use utoipa::ToSchema;

use crate::error::AppResult;

/// One download job as stored in the database.
#[derive(Debug, Clone, sqlx::FromRow, Serialize, ToSchema)]
pub struct Download {
    /// UUID (v4), assigned at creation.
    pub id: String,
    pub tmdb_id: i64,
    /// `movie` or `tv`.
    pub media_type: String,
    pub season: Option<i64>,
    pub episode: Option<i64>,
    pub release_title: String,
    pub nzb_url: String,
    /// `pending`, `downloading`, `complete`, `failed` or `cancelled`.
    pub status: String,
    pub progress_bytes: i64,
    pub total_bytes: Option<i64>,
    /// Absolute path of the finished file (set when `status == "complete"`).
    pub file_path: Option<String>,
    /// Failure reason when `status == "failed"`.
    pub error: Option<String>,
    pub created_at: String,
    pub updated_at: String,
}

/// Data for a new `pending` row.
pub struct NewDownload<'a> {
    pub id: &'a str,
    pub tmdb_id: i64,
    pub media_type: &'a str,
    pub season: Option<u32>,
    pub episode: Option<u32>,
    pub release_title: &'a str,
    pub nzb_url: &'a str,
}

pub async fn insert(pool: &SqlitePool, new: &NewDownload<'_>) -> AppResult<Download> {
    sqlx::query(
        "INSERT INTO downloads
             (id, user_id, tmdb_id, media_type, season, episode, release_title, nzb_url)
         VALUES (?, 1, ?, ?, ?, ?, ?, ?)",
    )
    .bind(new.id)
    .bind(new.tmdb_id)
    .bind(new.media_type)
    .bind(new.season)
    .bind(new.episode)
    .bind(new.release_title)
    .bind(new.nzb_url)
    .execute(pool)
    .await?;
    get(pool, new.id)
        .await?
        .ok_or_else(|| crate::error::AppError::Internal(anyhow::anyhow!("row vanished on insert")))
}

pub async fn list(pool: &SqlitePool) -> AppResult<Vec<Download>> {
    Ok(sqlx::query_as(
        "SELECT id, tmdb_id, media_type, season, episode, release_title, nzb_url,
                status, progress_bytes, total_bytes, file_path, error, created_at, updated_at
         FROM downloads WHERE user_id = 1
         ORDER BY created_at DESC, rowid DESC",
    )
    .fetch_all(pool)
    .await?)
}

pub async fn get(pool: &SqlitePool, id: &str) -> AppResult<Option<Download>> {
    Ok(sqlx::query_as(
        "SELECT id, tmdb_id, media_type, season, episode, release_title, nzb_url,
                status, progress_bytes, total_bytes, file_path, error, created_at, updated_at
         FROM downloads WHERE user_id = 1 AND id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?)
}

/// Most recent completed downloads for one item (newest first). Callers
/// still have to check that the file exists on disk.
pub async fn completed_for_item(
    pool: &SqlitePool,
    tmdb_id: i64,
    media_type: &str,
    season: Option<u32>,
    episode: Option<u32>,
) -> AppResult<Vec<Download>> {
    Ok(sqlx::query_as(
        "SELECT id, tmdb_id, media_type, season, episode, release_title, nzb_url,
                status, progress_bytes, total_bytes, file_path, error, created_at, updated_at
         FROM downloads
         WHERE user_id = 1 AND tmdb_id = ? AND media_type = ?
           AND season IS ? AND episode IS ?
           AND status = 'complete' AND file_path IS NOT NULL
         ORDER BY updated_at DESC, rowid DESC",
    )
    .bind(tmdb_id)
    .bind(media_type)
    .bind(season)
    .bind(episode)
    .fetch_all(pool)
    .await?)
}

pub async fn mark_downloading(pool: &SqlitePool, id: &str, total_bytes: i64) -> AppResult<()> {
    sqlx::query(
        "UPDATE downloads
         SET status = 'downloading', total_bytes = ?, updated_at = datetime('now')
         WHERE id = ?",
    )
    .bind(total_bytes)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn set_progress(pool: &SqlitePool, id: &str, progress_bytes: i64) -> AppResult<()> {
    sqlx::query(
        "UPDATE downloads SET progress_bytes = ?, updated_at = datetime('now') WHERE id = ?",
    )
    .bind(progress_bytes)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn mark_complete(pool: &SqlitePool, id: &str, file_path: &str) -> AppResult<()> {
    sqlx::query(
        "UPDATE downloads
         SET status = 'complete', progress_bytes = COALESCE(total_bytes, progress_bytes),
             file_path = ?, error = NULL, updated_at = datetime('now')
         WHERE id = ?",
    )
    .bind(file_path)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn mark_failed(pool: &SqlitePool, id: &str, error: &str) -> AppResult<()> {
    sqlx::query(
        "UPDATE downloads
         SET status = 'failed', error = ?, updated_at = datetime('now')
         WHERE id = ?",
    )
    .bind(error)
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

pub async fn mark_cancelled(pool: &SqlitePool, id: &str) -> AppResult<()> {
    sqlx::query(
        "UPDATE downloads
         SET status = 'cancelled', updated_at = datetime('now')
         WHERE id = ?",
    )
    .bind(id)
    .execute(pool)
    .await?;
    Ok(())
}

/// Delete the row. Returns false when the id is unknown.
pub async fn delete(pool: &SqlitePool, id: &str) -> AppResult<bool> {
    let result = sqlx::query("DELETE FROM downloads WHERE user_id = 1 AND id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

/// Startup recovery: jobs that were `pending`/`downloading` when the server
/// stopped can never finish, so mark them failed. Returns the number of
/// rows touched.
pub async fn recover_interrupted(pool: &SqlitePool) -> AppResult<u64> {
    let result = sqlx::query(
        "UPDATE downloads
         SET status = 'failed', error = 'interrupted by server restart',
             updated_at = datetime('now')
         WHERE status IN ('pending', 'downloading')",
    )
    .execute(pool)
    .await?;
    Ok(result.rows_affected())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool() -> SqlitePool {
        crate::db::connect(":memory:").await.expect("db")
    }

    fn new_row(id: &str) -> NewDownload<'_> {
        NewDownload {
            id,
            tmdb_id: 42,
            media_type: "movie",
            season: None,
            episode: None,
            release_title: "Movie.2026.1080p-TEST",
            nzb_url: "https://indexer/x.nzb",
        }
    }

    #[tokio::test]
    async fn lifecycle_and_listing() {
        let pool = pool().await;
        let row = insert(&pool, &new_row("a")).await.unwrap();
        assert_eq!(row.status, "pending");
        assert_eq!(row.progress_bytes, 0);

        mark_downloading(&pool, "a", 1000).await.unwrap();
        set_progress(&pool, "a", 400).await.unwrap();
        let row = get(&pool, "a").await.unwrap().unwrap();
        assert_eq!(row.status, "downloading");
        assert_eq!((row.progress_bytes, row.total_bytes), (400, Some(1000)));

        mark_complete(&pool, "a", "/tmp/movie.mkv").await.unwrap();
        let row = get(&pool, "a").await.unwrap().unwrap();
        assert_eq!(row.status, "complete");
        assert_eq!(row.progress_bytes, 1000, "complete snaps progress to total");
        assert_eq!(row.file_path.as_deref(), Some("/tmp/movie.mkv"));

        let found = completed_for_item(&pool, 42, "movie", None, None)
            .await
            .unwrap();
        assert_eq!(found.len(), 1);
        assert!(completed_for_item(&pool, 42, "tv", None, None)
            .await
            .unwrap()
            .is_empty());

        insert(&pool, &new_row("b")).await.unwrap();
        let all = list(&pool).await.unwrap();
        assert_eq!(all.len(), 2);
        assert_eq!(all[0].id, "b", "newest first");

        assert!(delete(&pool, "b").await.unwrap());
        assert!(!delete(&pool, "b").await.unwrap());
    }

    #[tokio::test]
    async fn interrupted_jobs_are_failed_on_recovery() {
        let pool = pool().await;
        insert(&pool, &new_row("p")).await.unwrap();
        insert(&pool, &new_row("d")).await.unwrap();
        mark_downloading(&pool, "d", 10).await.unwrap();
        insert(&pool, &new_row("c")).await.unwrap();
        mark_complete(&pool, "c", "/tmp/c.mkv").await.unwrap();

        assert_eq!(recover_interrupted(&pool).await.unwrap(), 2);
        for id in ["p", "d"] {
            let row = get(&pool, id).await.unwrap().unwrap();
            assert_eq!(row.status, "failed");
            assert_eq!(row.error.as_deref(), Some("interrupted by server restart"));
        }
        let row = get(&pool, "c").await.unwrap().unwrap();
        assert_eq!(row.status, "complete");
    }
}
