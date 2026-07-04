//! Key/value app settings stored in the `app_settings` table.

use sqlx::SqlitePool;

use crate::error::AppResult;

/// Key under which the TMDB API key is stored.
pub const TMDB_API_KEY: &str = "tmdb_api_key";

pub async fn get(pool: &SqlitePool, key: &str) -> AppResult<Option<String>> {
    let value: Option<(String,)> = sqlx::query_as("SELECT value FROM app_settings WHERE key = ?")
        .bind(key)
        .fetch_optional(pool)
        .await?;
    Ok(value.map(|(v,)| v))
}

pub async fn set(pool: &SqlitePool, key: &str, value: &str) -> AppResult<()> {
    sqlx::query(
        "INSERT INTO app_settings (key, value) VALUES (?, ?)
         ON CONFLICT(key) DO UPDATE SET value = excluded.value",
    )
    .bind(key)
    .bind(value)
    .execute(pool)
    .await?;
    Ok(())
}
