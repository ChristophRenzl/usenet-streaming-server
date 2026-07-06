//! Persistence for audio-fingerprint intro detection: per-episode chromaprint
//! fingerprints and the per-season detected intro.
//!
//! Both tables key on `(tmdb_id, season[, episode])` with non-null columns, so
//! `ON CONFLICT` upserts are reliable (unlike the movie-inclusive
//! watch-history index).

use sqlx::SqlitePool;

use crate::error::AppResult;

/// The detected intro for a season, in seconds.
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct SeasonIntro {
    pub intro_start_secs: f64,
    pub intro_end_secs: f64,
}

/// Store (or replace) an episode's fingerprint. `fingerprint` is the raw
/// little-endian BLOB from [`crate::stream::fingerprint::to_bytes`].
pub async fn upsert_episode_fingerprint(
    pool: &SqlitePool,
    tmdb_id: i64,
    season: u32,
    episode: u32,
    fingerprint: &[u8],
) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO episode_fingerprints (tmdb_id, season, episode, fingerprint)
         VALUES (?, ?, ?, ?)
         ON CONFLICT(tmdb_id, season, episode)
         DO UPDATE SET fingerprint = excluded.fingerprint,
                       created_at = datetime('now')",
    )
    .bind(tmdb_id)
    .bind(season)
    .bind(episode)
    .bind(fingerprint)
    .execute(pool)
    .await?;
    Ok(())
}

/// Load the stored fingerprint for one episode, when present.
pub async fn episode_fingerprint(
    pool: &SqlitePool,
    tmdb_id: i64,
    season: u32,
    episode: u32,
) -> AppResult<Option<Vec<u8>>> {
    let row: Option<(Vec<u8>,)> = sqlx::query_as(
        "SELECT fingerprint FROM episode_fingerprints
         WHERE tmdb_id = ? AND season = ? AND episode = ?",
    )
    .bind(tmdb_id)
    .bind(season)
    .bind(episode)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(bytes,)| bytes))
}

/// Any *other* stored fingerprint for the same season (a sibling episode), for
/// the intro comparison. Returns the first one found, excluding `episode`.
pub async fn sibling_fingerprint(
    pool: &SqlitePool,
    tmdb_id: i64,
    season: u32,
    episode: u32,
) -> AppResult<Option<Vec<u8>>> {
    let row: Option<(Vec<u8>,)> = sqlx::query_as(
        "SELECT fingerprint FROM episode_fingerprints
         WHERE tmdb_id = ? AND season = ? AND episode <> ?
         ORDER BY episode
         LIMIT 1",
    )
    .bind(tmdb_id)
    .bind(season)
    .bind(episode)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(bytes,)| bytes))
}

/// The cached detected intro for a season, when one has been computed.
pub async fn season_intro(
    pool: &SqlitePool,
    tmdb_id: i64,
    season: u32,
) -> AppResult<Option<SeasonIntro>> {
    let row: Option<(f64, f64)> = sqlx::query_as(
        "SELECT intro_start_secs, intro_end_secs FROM season_intros
         WHERE tmdb_id = ? AND season = ?",
    )
    .bind(tmdb_id)
    .bind(season)
    .fetch_optional(pool)
    .await?;
    Ok(row.map(|(intro_start_secs, intro_end_secs)| SeasonIntro {
        intro_start_secs,
        intro_end_secs,
    }))
}

/// Store (or replace) the detected intro for a season.
pub async fn upsert_season_intro(
    pool: &SqlitePool,
    tmdb_id: i64,
    season: u32,
    intro: SeasonIntro,
) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO season_intros (tmdb_id, season, intro_start_secs, intro_end_secs)
         VALUES (?, ?, ?, ?)
         ON CONFLICT(tmdb_id, season)
         DO UPDATE SET intro_start_secs = excluded.intro_start_secs,
                       intro_end_secs = excluded.intro_end_secs,
                       updated_at = datetime('now')",
    )
    .bind(tmdb_id)
    .bind(season)
    .bind(intro.intro_start_secs)
    .bind(intro.intro_end_secs)
    .execute(pool)
    .await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    async fn pool() -> SqlitePool {
        crate::db::connect(":memory:").await.expect("db")
    }

    #[tokio::test]
    async fn episode_fingerprint_round_trips_and_upserts() {
        let pool = pool().await;
        assert!(episode_fingerprint(&pool, 42, 1, 1)
            .await
            .unwrap()
            .is_none());

        let fp = vec![1u8, 2, 3, 4, 5, 6, 7, 8];
        upsert_episode_fingerprint(&pool, 42, 1, 1, &fp)
            .await
            .unwrap();
        assert_eq!(
            episode_fingerprint(&pool, 42, 1, 1).await.unwrap(),
            Some(fp.clone())
        );

        // Upsert replaces.
        let fp2 = vec![9u8, 9, 9, 9];
        upsert_episode_fingerprint(&pool, 42, 1, 1, &fp2)
            .await
            .unwrap();
        assert_eq!(
            episode_fingerprint(&pool, 42, 1, 1).await.unwrap(),
            Some(fp2)
        );
    }

    #[tokio::test]
    async fn sibling_finds_another_episode_in_the_same_season_only() {
        let pool = pool().await;
        upsert_episode_fingerprint(&pool, 7, 2, 3, &[3, 0, 0, 0])
            .await
            .unwrap();
        // No sibling yet (only this episode).
        assert!(sibling_fingerprint(&pool, 7, 2, 3).await.unwrap().is_none());

        // A different season is not a sibling.
        upsert_episode_fingerprint(&pool, 7, 1, 3, &[1, 0, 0, 0])
            .await
            .unwrap();
        assert!(sibling_fingerprint(&pool, 7, 2, 3).await.unwrap().is_none());

        // Another episode of the same season is.
        upsert_episode_fingerprint(&pool, 7, 2, 5, &[5, 0, 0, 0])
            .await
            .unwrap();
        assert_eq!(
            sibling_fingerprint(&pool, 7, 2, 3).await.unwrap(),
            Some(vec![5, 0, 0, 0])
        );
    }

    #[tokio::test]
    async fn season_intro_round_trips_and_upserts() {
        let pool = pool().await;
        assert!(season_intro(&pool, 99, 1).await.unwrap().is_none());

        upsert_season_intro(
            &pool,
            99,
            1,
            SeasonIntro {
                intro_start_secs: 2.5,
                intro_end_secs: 92.5,
            },
        )
        .await
        .unwrap();
        assert_eq!(
            season_intro(&pool, 99, 1).await.unwrap(),
            Some(SeasonIntro {
                intro_start_secs: 2.5,
                intro_end_secs: 92.5,
            })
        );

        // Upsert replaces.
        upsert_season_intro(
            &pool,
            99,
            1,
            SeasonIntro {
                intro_start_secs: 0.0,
                intro_end_secs: 80.0,
            },
        )
        .await
        .unwrap();
        assert_eq!(
            season_intro(&pool, 99, 1)
                .await
                .unwrap()
                .unwrap()
                .intro_end_secs,
            80.0
        );
    }
}
