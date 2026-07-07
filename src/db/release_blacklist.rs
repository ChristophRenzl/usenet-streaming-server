//! Releases the user marked as bad (broken A/V sync, wrong content, ...).
//! Keyed by exact release title — the same underlying release is listed by
//! several indexers under different guids, but its title is stable — so one
//! flag hides it everywhere.

use std::collections::HashSet;

use serde::Serialize;
use sqlx::SqlitePool;
use utoipa::ToSchema;

use crate::error::AppResult;

/// One blacklisted release as stored.
#[derive(Debug, Clone, sqlx::FromRow, Serialize, ToSchema)]
pub struct BlacklistedRelease {
    pub id: i64,
    pub title: String,
    /// What was being played when it was flagged (context only).
    pub tmdb_id: Option<i64>,
    pub media_type: Option<String>,
    pub season: Option<i64>,
    pub episode: Option<i64>,
    pub reason: Option<String>,
    pub added_at: String,
}

/// Context recorded alongside a newly blacklisted title.
#[derive(Debug, Clone, Default)]
pub struct NewBlacklistedRelease<'a> {
    pub title: &'a str,
    pub tmdb_id: Option<i64>,
    pub media_type: Option<&'a str>,
    pub season: Option<u32>,
    pub episode: Option<u32>,
    pub reason: Option<&'a str>,
}

/// Insert the title unless it is already blacklisted. Returns whether this
/// call created the entry.
pub async fn add(pool: &SqlitePool, entry: &NewBlacklistedRelease<'_>) -> AppResult<bool> {
    let result = sqlx::query(
        "INSERT INTO release_blacklist (title, tmdb_id, media_type, season, episode, reason)
         VALUES (?, ?, ?, ?, ?, ?)
         ON CONFLICT (title) DO NOTHING",
    )
    .bind(entry.title)
    .bind(entry.tmdb_id)
    .bind(entry.media_type)
    .bind(entry.season)
    .bind(entry.episode)
    .bind(entry.reason)
    .execute(pool)
    .await?;
    Ok(result.rows_affected() > 0)
}

/// All blacklisted titles, for candidate filtering during ranking.
pub async fn titles(pool: &SqlitePool) -> AppResult<HashSet<String>> {
    let rows: Vec<(String,)> = sqlx::query_as("SELECT title FROM release_blacklist")
        .fetch_all(pool)
        .await?;
    Ok(rows.into_iter().map(|(title,)| title).collect())
}

/// Whether one title is blacklisted (used by the disk-playback shortcut).
pub async fn contains(pool: &SqlitePool, title: &str) -> AppResult<bool> {
    let row: Option<(i64,)> = sqlx::query_as("SELECT id FROM release_blacklist WHERE title = ?")
        .bind(title)
        .fetch_optional(pool)
        .await?;
    Ok(row.is_some())
}

/// All entries, newest first (management listing).
pub async fn list(pool: &SqlitePool) -> AppResult<Vec<BlacklistedRelease>> {
    Ok(sqlx::query_as(
        "SELECT id, title, tmdb_id, media_type, season, episode, reason, added_at
         FROM release_blacklist ORDER BY added_at DESC, id DESC",
    )
    .fetch_all(pool)
    .await?)
}

/// Remove one entry (un-blacklist). Returns false when the id is unknown.
pub async fn delete(pool: &SqlitePool, id: i64) -> AppResult<bool> {
    let result = sqlx::query("DELETE FROM release_blacklist WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool() -> SqlitePool {
        crate::db::connect(":memory:").await.expect("db")
    }

    #[tokio::test]
    async fn add_is_idempotent_per_title() {
        let pool = pool().await;
        let entry = NewBlacklistedRelease {
            title: "Show.S01E01.1080p.WEB.H264-BAD",
            tmdb_id: Some(42),
            media_type: Some("tv"),
            season: Some(1),
            episode: Some(1),
            reason: Some("audio out of sync"),
        };
        assert!(add(&pool, &entry).await.unwrap());
        assert!(!add(&pool, &entry).await.unwrap());

        let entries = list(&pool).await.unwrap();
        assert_eq!(entries.len(), 1);
        assert_eq!(entries[0].title, "Show.S01E01.1080p.WEB.H264-BAD");
        assert_eq!(entries[0].reason.as_deref(), Some("audio out of sync"));

        assert!(contains(&pool, "Show.S01E01.1080p.WEB.H264-BAD")
            .await
            .unwrap());
        assert!(!contains(&pool, "Other.Release").await.unwrap());
    }

    #[tokio::test]
    async fn titles_and_delete_round_trip() {
        let pool = pool().await;
        for title in ["A", "B"] {
            add(
                &pool,
                &NewBlacklistedRelease {
                    title,
                    ..Default::default()
                },
            )
            .await
            .unwrap();
        }
        let all = titles(&pool).await.unwrap();
        assert!(all.contains("A") && all.contains("B"));

        let id = list(&pool).await.unwrap()[0].id;
        assert!(delete(&pool, id).await.unwrap());
        assert!(!delete(&pool, id).await.unwrap());
        assert_eq!(titles(&pool).await.unwrap().len(), 1);
    }
}
