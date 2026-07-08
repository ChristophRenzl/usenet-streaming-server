//! Watch-history rows: session-start upserts (returning the stored resume
//! position), position updates from the history API, listing and deletion.

use sqlx::SqlitePool;

use crate::error::AppResult;

/// TMDB metadata captured best-effort at session start. All fields are
/// optional: a failed lookup stores NULLs, and updates never overwrite a
/// previously stored value with NULL (COALESCE in the UPDATE).
#[derive(Debug, Clone, Default)]
pub struct MediaMeta {
    /// Movie or show title.
    pub title: Option<String>,
    pub poster_url: Option<String>,
    pub backdrop_url: Option<String>,
    /// Episode title (tv only).
    pub episode_title: Option<String>,
    /// Episode still image (tv only).
    pub still_url: Option<String>,
}

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
    pub meta: MediaMeta,
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
    pub title: Option<String>,
    pub poster_url: Option<String>,
    pub backdrop_url: Option<String>,
    pub episode_title: Option<String>,
    pub still_url: Option<String>,
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
pub async fn record_session_start(
    pool: &SqlitePool,
    user_id: i64,
    start: &SessionStart<'_>,
) -> AppResult<f64> {
    let existing: Option<(i64, f64)> = sqlx::query_as(
        "SELECT id, position_secs FROM watch_history
         WHERE user_id = ? AND tmdb_id = ? AND media_type = ?
           AND season IS ? AND episode IS ?",
    )
    .bind(user_id)
    .bind(start.tmdb_id)
    .bind(start.media_type)
    .bind(start.season)
    .bind(start.episode)
    .fetch_optional(pool)
    .await?;

    match existing {
        Some((id, position)) => {
            // COALESCE keeps previously stored metadata when a later session
            // start could not fetch it (TMDB hiccup).
            sqlx::query(
                "UPDATE watch_history
                 SET release_title = ?, indexer_id = ?, nzb_url = ?, duration_secs = ?,
                     title = COALESCE(?, title),
                     poster_url = COALESCE(?, poster_url),
                     backdrop_url = COALESCE(?, backdrop_url),
                     episode_title = COALESCE(?, episode_title),
                     still_url = COALESCE(?, still_url),
                     watched_at = datetime('now')
                 WHERE id = ?",
            )
            .bind(start.release_title)
            .bind(start.indexer_id)
            .bind(start.nzb_url)
            .bind(start.duration_secs)
            .bind(&start.meta.title)
            .bind(&start.meta.poster_url)
            .bind(&start.meta.backdrop_url)
            .bind(&start.meta.episode_title)
            .bind(&start.meta.still_url)
            .bind(id)
            .execute(pool)
            .await?;
            Ok(position)
        }
        None => {
            sqlx::query(
                "INSERT INTO watch_history
                     (user_id, tmdb_id, media_type, season, episode, release_title,
                      indexer_id, nzb_url, duration_secs, title, poster_url,
                      backdrop_url, episode_title, still_url)
                 VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?, ?)",
            )
            .bind(user_id)
            .bind(start.tmdb_id)
            .bind(start.media_type)
            .bind(start.season)
            .bind(start.episode)
            .bind(start.release_title)
            .bind(start.indexer_id)
            .bind(start.nzb_url)
            .bind(start.duration_secs)
            .bind(&start.meta.title)
            .bind(&start.meta.poster_url)
            .bind(&start.meta.backdrop_url)
            .bind(&start.meta.episode_title)
            .bind(&start.meta.still_url)
            .execute(pool)
            .await?;
            Ok(0.0)
        }
    }
}

/// All history rows, most recently watched first.
pub async fn list(pool: &SqlitePool, user_id: i64) -> AppResult<Vec<HistoryEntry>> {
    Ok(sqlx::query_as(
        "SELECT id, tmdb_id, media_type, season, episode, release_title,
                position_secs, duration_secs, watched_at,
                title, poster_url, backdrop_url, episode_title, still_url
         FROM watch_history WHERE user_id = ?
         ORDER BY watched_at DESC, id DESC",
    )
    .bind(user_id)
    .fetch_all(pool)
    .await?)
}

pub async fn get(pool: &SqlitePool, user_id: i64, id: i64) -> AppResult<Option<HistoryEntry>> {
    Ok(sqlx::query_as(
        "SELECT id, tmdb_id, media_type, season, episode, release_title,
                position_secs, duration_secs, watched_at,
                title, poster_url, backdrop_url, episode_title, still_url
         FROM watch_history WHERE user_id = ? AND id = ?",
    )
    .bind(user_id)
    .bind(id)
    .fetch_optional(pool)
    .await?)
}

/// Upsert a playback position and bump `watched_at`. Same manual NULL-safe
/// upsert as [`record_session_start`] (see the comment there). Returns the
/// stored row.
pub async fn upsert_position(
    pool: &SqlitePool,
    user_id: i64,
    update: &PositionUpdate<'_>,
) -> AppResult<HistoryEntry> {
    let existing: Option<(i64,)> = sqlx::query_as(
        "SELECT id FROM watch_history
         WHERE user_id = ? AND tmdb_id = ? AND media_type = ?
           AND season IS ? AND episode IS ?",
    )
    .bind(user_id)
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
             VALUES (?, ?, ?, ?, ?, ?, ?)",
        )
        .bind(user_id)
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
    get(pool, user_id, id)
        .await?
        .ok_or_else(|| crate::error::AppError::Internal(anyhow::anyhow!("row vanished on upsert")))
}

/// Mark an item watched from a remote sync (Trakt import).
///
/// Rows that are already effectively finished are left untouched (returns
/// false); rows with real progress get `position = duration`; missing rows
/// are inserted with the 1/1 "watched without playing" sentinel. `watched_at`
/// is the remote timestamp (normalized RFC3339), so Continue Watching's
/// ordering reflects when it was actually watched — not when it synced.
pub async fn mark_watched_remote(
    pool: &SqlitePool,
    user_id: i64,
    tmdb_id: i64,
    media_type: &str,
    season: Option<u32>,
    episode: Option<u32>,
    watched_at: Option<&str>,
) -> AppResult<bool> {
    // "2024-01-01T20:15:00.000Z" -> "2024-01-01 20:15:00" (the table's format).
    let normalized = watched_at.map(|w| {
        w.replace('T', " ")
            .trim_end_matches('Z')
            .split('.')
            .next()
            .unwrap_or_default()
            .to_string()
    });

    let existing: Option<(i64, f64, Option<f64>)> = sqlx::query_as(
        "SELECT id, position_secs, duration_secs FROM watch_history
         WHERE user_id = ? AND tmdb_id = ? AND media_type = ?
           AND season IS ? AND episode IS ?",
    )
    .bind(user_id)
    .bind(tmdb_id)
    .bind(media_type)
    .bind(season)
    .bind(episode)
    .fetch_optional(pool)
    .await?;

    match existing {
        Some((_, position, Some(duration))) if duration > 0.0 && position >= duration * 0.95 => {
            Ok(false) // already watched locally — nothing to do.
        }
        Some((id, _, duration)) => {
            let target = duration.filter(|d| *d > 0.0).unwrap_or(1.0);
            sqlx::query(
                "UPDATE watch_history
                 SET position_secs = ?, duration_secs = COALESCE(duration_secs, 1.0),
                     watched_at = COALESCE(?, watched_at)
                 WHERE id = ?",
            )
            .bind(target)
            .bind(&normalized)
            .bind(id)
            .execute(pool)
            .await?;
            Ok(true)
        }
        None => {
            sqlx::query(
                "INSERT INTO watch_history
                     (user_id, tmdb_id, media_type, season, episode,
                      position_secs, duration_secs, watched_at)
                 VALUES (?, ?, ?, ?, ?, 1.0, 1.0, COALESCE(?, datetime('now')))",
            )
            .bind(user_id)
            .bind(tmdb_id)
            .bind(media_type)
            .bind(season)
            .bind(episode)
            .bind(&normalized)
            .execute(pool)
            .await?;
            Ok(true)
        }
    }
}

/// Delete one history row. Returns false when the id is unknown.
pub async fn delete(pool: &SqlitePool, user_id: i64, id: i64) -> AppResult<bool> {
    let result = sqlx::query("DELETE FROM watch_history WHERE user_id = ? AND id = ?")
        .bind(user_id)
        .bind(id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}

/// Release last used to play an item, when recorded. Lets session creation
/// prefer the release the user actually watched when auto-selecting.
pub async fn last_release_title(
    pool: &SqlitePool,
    user_id: i64,
    tmdb_id: i64,
    media_type: &str,
    season: Option<u32>,
    episode: Option<u32>,
) -> AppResult<Option<String>> {
    let row: Option<(Option<String>,)> = sqlx::query_as(
        "SELECT release_title FROM watch_history
         WHERE user_id = ? AND tmdb_id = ? AND media_type = ?
           AND season IS ? AND episode IS ?",
    )
    .bind(user_id)
    .bind(tmdb_id)
    .bind(media_type)
    .bind(season)
    .bind(episode)
    .fetch_optional(pool)
    .await?;
    Ok(row.and_then(|(title,)| title))
}

/// Stored resume position for an item, when any.
pub async fn position_secs(
    pool: &SqlitePool,
    user_id: i64,
    tmdb_id: i64,
    media_type: &str,
    season: Option<u32>,
    episode: Option<u32>,
) -> AppResult<Option<f64>> {
    let row: Option<(f64,)> = sqlx::query_as(
        "SELECT position_secs FROM watch_history
         WHERE user_id = ? AND tmdb_id = ? AND media_type = ?
           AND season IS ? AND episode IS ?",
    )
    .bind(user_id)
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
            meta: MediaMeta {
                title: Some("The Movie".into()),
                backdrop_url: Some("https://img/backdrop.jpg".into()),
                ..Default::default()
            },
        };
        assert_eq!(record_session_start(&pool, 1, &start).await.unwrap(), 0.0);

        // Simulate progress written by the (future) history endpoint.
        sqlx::query("UPDATE watch_history SET position_secs = 33.5 WHERE tmdb_id = 42")
            .execute(&pool)
            .await
            .unwrap();

        // Starting again returns the stored position and does not duplicate.
        // The empty meta must not erase the previously stored metadata.
        let again = SessionStart {
            release_title: "Second.Release",
            meta: MediaMeta::default(),
            ..start
        };
        assert_eq!(record_session_start(&pool, 1, &again).await.unwrap(), 33.5);
        let (count, title): (i64, String) = sqlx::query_as(
            "SELECT COUNT(*), MAX(release_title) FROM watch_history WHERE tmdb_id = 42",
        )
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(count, 1);
        assert_eq!(title, "Second.Release");

        let entry = &list(&pool, 1).await.unwrap()[0];
        assert_eq!(entry.title.as_deref(), Some("The Movie"));
        assert_eq!(
            entry.backdrop_url.as_deref(),
            Some("https://img/backdrop.jpg")
        );

        assert_eq!(
            last_release_title(&pool, 1, 42, "movie", None, None)
                .await
                .unwrap()
                .as_deref(),
            Some("Second.Release")
        );
        assert_eq!(
            last_release_title(&pool, 1, 42, "tv", None, None)
                .await
                .unwrap(),
            None
        );

        assert_eq!(
            position_secs(&pool, 1, 42, "movie", None, None)
                .await
                .unwrap(),
            Some(33.5)
        );
        assert_eq!(
            position_secs(&pool, 1, 42, "tv", None, None).await.unwrap(),
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
        let entry = upsert_position(&pool, 1, &movie).await.unwrap();
        assert_eq!(entry.position_secs, 120.0);
        assert_eq!(entry.duration_secs, Some(3600.0));

        // Second update moves the position but must not erase the duration.
        let entry = upsert_position(
            &pool,
            1,
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
            1,
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

        let all = list(&pool, 1).await.unwrap();
        assert_eq!(all.len(), 2);
        // Same watched_at second is possible; the id tiebreaker puts the
        // newer row first.
        assert_eq!(all[0].media_type, "tv");

        assert!(delete(&pool, 1, entry.id).await.unwrap());
        assert!(!delete(&pool, 1, entry.id).await.unwrap());
        assert_eq!(list(&pool, 1).await.unwrap().len(), 1);
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
                meta: MediaMeta::default(),
            };
            record_session_start(&pool, 1, &start).await.unwrap();
        }
        let (count,): (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM watch_history WHERE tmdb_id = 7")
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(count, 2);
    }
}
