//! Key/value app settings stored in the `app_settings` table.

use sqlx::SqlitePool;

use crate::error::AppResult;

/// Key under which the TMDB API key is stored.
pub const TMDB_API_KEY: &str = "tmdb_api_key";

/// Key under which the OpenSubtitles API key is stored. Optional: subtitles
/// are a best-effort feature and playback works without it.
pub const OPENSUBTITLES_API_KEY: &str = "opensubtitles_api_key";

/// Key under which an optional OpenSubtitles account username is stored.
/// Logging in lifts the anonymous per-IP download quota.
pub const OPENSUBTITLES_USERNAME: &str = "opensubtitles_username";

/// Key under which the OpenSubtitles account password is stored (paired with
/// [`OPENSUBTITLES_USERNAME`]).
pub const OPENSUBTITLES_PASSWORD: &str = "opensubtitles_password";

/// Key under which a rotated server API key is stored. When set, requests may
/// authenticate with either this value or the bootstrap key from the config
/// file / environment (the latter stays valid as a recovery path).
pub const API_KEY_OVERRIDE: &str = "api_key_override";

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

/// Remove a setting so it reads back as "not set" (`get` returns `None`).
/// Idempotent: deleting a missing key is a no-op.
pub async fn delete(pool: &SqlitePool, key: &str) -> AppResult<()> {
    sqlx::query("DELETE FROM app_settings WHERE key = ?")
        .bind(key)
        .execute(pool)
        .await?;
    Ok(())
}
