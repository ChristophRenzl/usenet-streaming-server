//! Watch-history rows. The streaming layer only records that a session was
//! started (and returns any stored resume position); position updates come
//! from a dedicated history endpoint in a later milestone.

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
    pub indexer_id: i64,
    pub nzb_url: &'a str,
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
            indexer_id: 1,
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
    async fn episodes_are_tracked_separately() {
        let pool = pool().await;
        for episode in [1u32, 2u32] {
            let start = SessionStart {
                tmdb_id: 7,
                media_type: "tv",
                season: Some(1),
                episode: Some(episode),
                release_title: "Show.S01",
                indexer_id: 1,
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
