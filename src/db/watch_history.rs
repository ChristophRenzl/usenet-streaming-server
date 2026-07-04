//! Watch-history rows: session-start upserts (returning the stored resume
//! position), position updates from the history API, listing and deletion.

use sqlx::SqlitePool;

use crate::error::AppResult;

/// Data recorded when a playback session starts.
pub struct SessionStart<'a> {
    pub tmdb_id: i64,
    /// `movie` or `tv`.
    pub media_type: &'a str,
    pub season: Option<u32>,
    pub episode: Option<u32>,
    pub release_title: &'a str,
    /// None for disk playback of a finished download.
    pub indexer_id: Option<i64>,
    pub nzb_url: &'a str,
    pub duration_secs: Option<f64>,
}

/// One watch-history row as stored.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct HistoryEntry {
    pub id: i64,
    pub tmdb_id: i64,
    pub media_type: String,
    pub season: Option<i64>,
    pub episode: Option<i64>,
    pub release_title: Option<String>,
    pub position_secs: f64,
    pub duration_secs: Option<f64>,
    pub watched_at: String,
}

/// A position update from a client (history API or the per-session
/// convenience endpoint).
pub struct PositionUpdate<'a> {
    pub tmdb_id: i64,
    pub media_type: &'a str,
    pub season: Option<u32>,
    pub episode: Option<u32>,
    pub position_secs: f64,
    /// Only overwrites the stored duration when present.
    pub duration_secs: Option<f64>,
}

/// Upsert the history row for this item and return the previously stored
/// playback position (0 for a first watch).
///
/// The upsert is done manually (SELECT then UPDATE/INSERT) because the
/// table's UNIQUE index contains nullable season/episode columns and SQLite
/// treats NULLs as distinct in unique indexes, so `ON CONFLICT` would never
/// fire for movies.
pub async fn record_session_start(pool: &SqlitePool, start: &SessionStart<'_>) -> AppResult<f64> {
    let existing: Option<(i64, f64)> = sqlx::query_as(
        "SELECT id, position_secs FROM watch_history
         WHERE user_id = 1 AND tmdb_id = ? AND media_type = ?
           AND season IS ? AND episode IS ?",
    )
    .bind(start.tmdb_id)
    .bind(start.media_type)
    .bind(start.season)
    .bind(start.episode)
    .fetch_optional(pool)
    .await?;

    match existing {
        Some((id, position)) => {
            sqlx::query(
                "UPDATE watch_history
                 SET release_title = ?, indexer_id = ?, nzb_url = ?, duration_secs = ?,
                     watched_at = datetime('now')
                 WHERE id = ?",
            )
            .bind(start.release_title)
            .bind(start.indexer_id)
            .bind(start.nzb_url)
            .bind(start.duration_secs)
            .bind(id)
            .execute(pool)
            .await?;
            Ok(position)
        }
        None => {
            sqlx::query(
                "INSERT INTO watch_history
                     (user_id, tmdb_id, media_type, season, episode, release_title,
                      indexer_id, nzb_url, duration_secs)
                 VALUES (1, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(start.tmdb_id)
            .bind(start.media_type)
            .bind(start.season)
            .bind(start.episode)
            .bind(start.release_title)
            .bind(start.indexer_id)
            .bind(start.nzb_url)
            .bind(start.duration_secs)
            .execute(pool)
            .await?;
            Ok(0.0)
        }
    }
}

/// All history rows, most recently watched first.
pub async fn list(pool: &SqlitePool) -> AppResult<Vec<HistoryEntry>> {
    Ok(sqlx::query_as(
        "SELECT id, tmdb_id, media_type, season, episode, release_title,
                position_secs, duration_secs, watched_at
         FROM watch_history WHERE user_id = 1
         ORDER BY watched_at DESC, id DESC",
    )
    .fetch_all(pool)
    .await?)
}

pub async fn get(pool: &SqlitePool, id: i64) -> AppResult<Option<HistoryEntry>> {
    Ok(sqlx::query_as(
        "SELECT id, tmdb_id, media_type, season, episode, release_title,
                position_secs, duration_secs, watched_at
         FROM watch_history WHERE user_id = 1 AND id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?)
}

/// Upsert a playback position and bump `watched_at`. Same manual NULL-safe
/// upsert as [`record_session_start`] (see the comment there). Returns the
/// stored row.
pub async fn upsert_position(
    pool: &SqlitePool,
    update: &PositionUpdate<'_>,
) -> AppResult<HistoryEntry> {
    let existing: Option<(i64,)> = sqlx::query_as(
        "SELECT id FROM watch_history
         WHERE user_id = 1 AND tmdb_id = ? AND media_type = ?
           AND season IS ? AND episode IS ?",
    )
    .bind(update.tmdb_id)
    .bind(update.media_type)
    .bind(update.season)
    .bind(update.episode)
    .fetch_optional(pool)
    .await?;

    let id = match existing {
        Some((id,)) => {
            sqlx::query(
                "UPDATE watch_history
                 SET position_secs = ?, duration_secs = COALESCE(?, duration_secs),
                     watched_at = datetime('now')
                 WHERE id = ?",
            )
            .bind(update.position_secs)
            .bind(update.duration_secs)
            .bind(id)
            .execute(pool)
            .await?;
            id
        }
        None => sqlx::query(
            "INSERT INTO watch_history
                 (user_id, tmdb_id, media_type, season, episode, position_secs, duration_secs)
             VALUES (1, ?, ?, ?, ?, ?, ?)",
        )
        .bind(update.tmdb_id)
        .bind(update.media_type)
        .bind(update.season)
        .bind(update.episode)
        .bind(update.position_secs)
        .bind(update.duration_secs)
        .execute(pool)
        .await?
        .last_insert_rowid(),
    };
    get(pool, id)
        .await?
        .ok_or_else(|| crate::error::AppError::Internal(anyhow::anyhow!("row vanished on upsert")))
}

/// Delete one history row. Returns false when the id is unknown.
pub async fn delete(pool: &SqlitePool, id: i64) -> AppResult<bool> {
    let result = sqlx::query("DELETE FROM watch_history WHERE user_id = 1 AND id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

/// Stored resume position for an item, when any.
pub async fn position_secs(
    pool: &SqlitePool,
    tmdb_id: i64,
    media_type: &str,
    season: Option<u32>,
    episode: Option<u32>,
) -> AppResult<Option<f64>> {
    let row: Option<(f64,)> = sqlx::query_as(
        "SELECT position_secs FROM watch_history
         WHERE user_id = 1 AND tmdb_id = ? AND media_type = ?
           AND season IS ? AND episode IS ?",
    )
    .bind(tmdb_id)
    .bind(media_type)
    .bind(season)
    .bind(episode)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(p,)| p))
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool() -> SqlitePool {
        crate::db::connect(":memory:").await.expect("db")
    }

    #[tokio::test]
    async fn movie_rows_upsert_despite_null_season() {
        let pool = pool().await;
        let start = SessionStart {
            tmdb_id: 42,
            media_type: "movie",
            season: None,
            episode: None,
            release_title: "First.Release",
            indexer_id: Some(1),
            nzb_url: "https://x/a.nzb",
            duration_secs: Some(100.0),
        };
        assert_eq!(record_session_start(&pool, &start).await.unwrap(), 0.0);

        // Simulate progress written by the (future) history endpoint.
        sqlx::query("UPDATE watch_history SET position_secs = 33.5 WHERE tmdb_id = 42")
            .execute(&pool)
            .await
            .unwrap();

        // Starting again returns the stored position and does not duplicate.
        let again = SessionStart {
            release_title: "Second.Release",
            ..start
        };
        assert_eq!(record_session_start(&pool, &again).await.unwrap(), 33.5);
        let (count, title): (i64, String) = sqlx::query_as(
            "SELECT COUNT(*), MAX(release_title) FROM watch_history WHERE tmdb_id = 42",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(count, 1);
        assert_eq!(title, "Second.Release");

        assert_eq!(
            position_secs(&pool, 42, "movie", None, None).await.unwrap(),
            Some(33.5)
        );
        assert_eq!(
            position_secs(&pool, 42, "tv", None, None).await.unwrap(),
            None
        );
    }

    #[tokio::test]
    async fn upsert_position_updates_in_place_and_lists_newest_first() {
        let pool = pool().await;
        let movie = PositionUpdate {
            tmdb_id: 42,
            media_type: "movie",
            season: None,
            episode: None,
            position_secs: 120.0,
            duration_secs: Some(3600.0),
        };
        let entry = upsert_position(&pool, &movie).await.unwrap();
        assert_eq!(entry.position_secs, 120.0);
        assert_eq!(entry.duration_secs, Some(3600.0));

        // Second update moves the position but must not erase the duration.
        let entry = upsert_position(
            &pool,
            &PositionUpdate {
                position_secs: 240.0,
                duration_secs: None,
                ..movie
            },
        )
        .await
        .unwrap();
        assert_eq!(entry.position_secs, 240.0);
        assert_eq!(entry.duration_secs, Some(3600.0));

        // TV rows with NULL-different keys stay separate.
        upsert_position(
            &pool,
            &PositionUpdate {
                tmdb_id: 42,
                media_type: "tv",
                season: Some(1),
                episode: Some(3),
                position_secs: 10.0,
                duration_secs: None,
            },
        )
        .await
        .unwrap();

        let all = list(&pool).await.unwrap();
        assert_eq!(all.len(), 2);
        // Same watched_at second is possible; the id tiebreaker puts the
        // newer row first.
        assert_eq!(all[0].media_type, "tv");

        assert!(delete(&pool, entry.id).await.unwrap());
        assert!(!delete(&pool, entry.id).await.unwrap());
        assert_eq!(list(&pool).await.unwrap().len(), 1);
    }

    #[tokio::test]
    async fn episodes_are_tracked_separately() {
        let pool = pool().await;
        for episode in [1u32, 2u32] {
            let start = SessionStart {
                tmdb_id: 7,
                media_type: "tv",
                season: Some(1),
                episode: Some(episode),
                release_title: "Show.S01",
                indexer_id: Some(1),
                nzb_url: "https://x/e.nzb",
                duration_secs: None,
            };
            record_session_start(&pool, &start).await.unwrap();
        }
        let (count,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM watch_history WHERE tmdb_id = 7")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(count, 2);
    }
}
