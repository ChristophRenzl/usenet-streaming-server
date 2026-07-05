//! Watchlist rows for the single default user: TMDB items saved for later,
//! denormalized (title, artwork, overview) so listing needs no TMDB calls.

use sqlx::SqlitePool;

use crate::error::{AppError, AppResult};

const USER_ID: i64 = 1;

/// One watchlist row as stored.
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct WatchlistEntry {
    pub id: i64,
    pub tmdb_id: i64,
    pub media_type: String,
    pub title: String,
    pub year: Option<i64>,
    pub poster_url: Option<String>,
    pub backdrop_url: Option<String>,
    pub overview: Option<String>,
    pub vote_average: Option<f64>,
    pub added_at: String,
}

/// Denormalized TMDB details for a new watchlist row.
#[derive(Debug, Clone)]
pub struct NewWatchlistEntry {
    pub tmdb_id: i64,
    /// `movie` or `tv`.
    pub media_type: String,
    pub title: String,
    pub year: Option<i32>,
    pub poster_url: Option<String>,
    pub backdrop_url: Option<String>,
    pub overview: Option<String>,
    pub vote_average: Option<f64>,
}

/// The stored row for an item, when present.
pub async fn get(
    pool: &SqlitePool,
    tmdb_id: i64,
    media_type: &str,
) -> AppResult<Option<WatchlistEntry>> {
    Ok(sqlx::query_as(
        "SELECT id, tmdb_id, media_type, title, year, poster_url,
                backdrop_url, overview, vote_average, added_at
         FROM watchlist WHERE user_id = ? AND tmdb_id = ? AND media_type = ?",
    )
    .bind(USER_ID)
    .bind(tmdb_id)
    .bind(media_type)
    .fetch_optional(pool)
    .await?)
}

/// Insert the entry unless it already exists (the unique index has no NULL
/// columns, so `ON CONFLICT` is reliable here). Returns the stored row and
/// whether this call created it.
pub async fn add(
    pool: &SqlitePool,
    entry: &NewWatchlistEntry,
) -> AppResult<(WatchlistEntry, bool)> {
    let result = sqlx::query(
        "INSERT INTO watchlist
             (user_id, tmdb_id, media_type, title, year, poster_url,
              backdrop_url, overview, vote_average)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
         ON CONFLICT (user_id, tmdb_id, media_type) DO NOTHING",
    )
    .bind(USER_ID)
    .bind(entry.tmdb_id)
    .bind(&entry.media_type)
    .bind(&entry.title)
    .bind(entry.year)
    .bind(&entry.poster_url)
    .bind(&entry.backdrop_url)
    .bind(&entry.overview)
    .bind(entry.vote_average)
    .execute(pool)
    .await?;

    let created = result.rows_affected() > 0;
    let row = get(pool, entry.tmdb_id, &entry.media_type)
        .await?
        .ok_or_else(|| {
            AppError::Internal(anyhow::anyhow!("watchlist row vanished after insert"))
        })?;
    Ok((row, created))
}

/// All watchlist rows, newest first.
pub async fn list(pool: &SqlitePool) -> AppResult<Vec<WatchlistEntry>> {
    Ok(sqlx::query_as(
        "SELECT id, tmdb_id, media_type, title, year, poster_url,
                backdrop_url, overview, vote_average, added_at
         FROM watchlist WHERE user_id = ?
         ORDER BY added_at DESC, id DESC",
    )
    .bind(USER_ID)
    .fetch_all(pool)
    .await?)
}

/// Delete one item. Returns false when it was not on the list.
pub async fn delete(pool: &SqlitePool, tmdb_id: i64, media_type: &str) -> AppResult<bool> {
    let result =
        sqlx::query("DELETE FROM watchlist WHERE user_id = ? AND tmdb_id = ? AND media_type = ?")
            .bind(USER_ID)
            .bind(tmdb_id)
            .bind(media_type)
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

    fn entry(tmdb_id: i64, media_type: &str, title: &str) -> NewWatchlistEntry {
        NewWatchlistEntry {
            tmdb_id,
            media_type: media_type.into(),
            title: title.into(),
            year: Some(2010),
            poster_url: Some("https://img/p.jpg".into()),
            backdrop_url: None,
            overview: Some("plot".into()),
            vote_average: Some(8.4),
        }
    }

    #[tokio::test]
    async fn add_is_idempotent_per_item() {
        let pool = pool().await;
        let (row, created) = add(&pool, &entry(27205, "movie", "Inception"))
            .await
            .unwrap();
        assert!(created);
        assert_eq!(row.title, "Inception");
        assert_eq!(row.year, Some(2010));

        // Re-adding does not duplicate and keeps the original row.
        let (again, created) = add(&pool, &entry(27205, "movie", "Renamed")).await.unwrap();
        assert!(!created);
        assert_eq!(again.id, row.id);
        assert_eq!(again.title, "Inception");

        // The same tmdb_id with a different media type is a separate item.
        let (_, created) = add(&pool, &entry(27205, "tv", "Inception The Series"))
            .await
            .unwrap();
        assert!(created);
        assert_eq!(list(&pool).await.unwrap().len(), 2);
    }

    #[tokio::test]
    async fn list_is_newest_first_and_delete_reports_absence() {
        let pool = pool().await;
        add(&pool, &entry(1, "movie", "First")).await.unwrap();
        add(&pool, &entry(2, "tv", "Second")).await.unwrap();

        let all = list(&pool).await.unwrap();
        assert_eq!(all.len(), 2);
        // Same added_at second is possible; the id tiebreak puts the newer first.
        assert_eq!(all[0].title, "Second");
        assert_eq!(all[1].title, "First");

        assert!(get(&pool, 1, "movie").await.unwrap().is_some());
        assert!(get(&pool, 1, "tv").await.unwrap().is_none());

        assert!(delete(&pool, 1, "movie").await.unwrap());
        assert!(!delete(&pool, 1, "movie").await.unwrap());
        assert_eq!(list(&pool).await.unwrap().len(), 1);
    }
}
