//! User accounts and their login tokens.
//!
//! User 1 is the built-in server owner/admin (created by the very first
//! migration): API-key requests act as this user, so single-user setups and
//! the Apple clients keep working without any login. Additional users get a
//! username + password (argon2) and authenticate via `POST /auth/login`,
//! which mints a bearer token stored here.

use serde::Serialize;
use sqlx::SqlitePool;
use utoipa::ToSchema;

use crate::error::{AppError, AppResult};

/// One user account, safe to expose over the API (no hash).
#[derive(Debug, Clone, Serialize, ToSchema, sqlx::FromRow)]
pub struct User {
    pub id: i64,
    pub name: String,
    pub is_admin: bool,
    /// Whether a password is set — the owner starts without one and cannot
    /// log in by name until an admin sets it.
    pub has_password: bool,
}

/// Per-user playback aggregates for the admin Users page. `watch_time_secs`
/// sums the stored playback positions across all history rows — a proxy for
/// total time watched (rewatches and abandoned positions count once);
/// `last_activity` is the newest history update (position reports touch it
/// every few seconds during playback).
#[derive(Debug, Clone, sqlx::FromRow)]
pub struct WatchStats {
    pub user_id: i64,
    pub watch_time_secs: f64,
    pub last_activity: Option<String>,
}

pub async fn watch_stats(pool: &SqlitePool) -> AppResult<Vec<WatchStats>> {
    sqlx::query_as(
        "SELECT user_id,
                COALESCE(SUM(position_secs), 0.0) AS watch_time_secs,
                MAX(watched_at) AS last_activity
         FROM watch_history GROUP BY user_id",
    )
    .fetch_all(pool)
    .await
    .map_err(AppError::Database)
}

pub async fn list(pool: &SqlitePool) -> AppResult<Vec<User>> {
    sqlx::query_as(
        "SELECT id, name, is_admin,
                password_hash IS NOT NULL AND password_hash != '' AS has_password
         FROM users ORDER BY id",
    )
    .fetch_all(pool)
    .await
    .map_err(AppError::Database)
}

pub async fn get(pool: &SqlitePool, id: i64) -> AppResult<Option<User>> {
    sqlx::query_as(
        "SELECT id, name, is_admin,
                password_hash IS NOT NULL AND password_hash != '' AS has_password
         FROM users WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await
    .map_err(AppError::Database)
}

/// The stored password hash for a (case-insensitive) username, with the
/// user's id — the login handler verifies against it.
pub async fn credentials(
    pool: &SqlitePool,
    name: &str,
) -> AppResult<Option<(i64, Option<String>)>> {
    sqlx::query_as("SELECT id, password_hash FROM users WHERE LOWER(name) = LOWER(?)")
        .bind(name)
        .fetch_optional(pool)
        .await
        .map_err(AppError::Database)
}

/// Create a user; fails with a readable error on duplicate names.
pub async fn create(
    pool: &SqlitePool,
    name: &str,
    password_hash: &str,
    is_admin: bool,
) -> AppResult<User> {
    let result = sqlx::query("INSERT INTO users (name, password_hash, is_admin) VALUES (?, ?, ?)")
        .bind(name)
        .bind(password_hash)
        .bind(is_admin)
        .execute(pool)
        .await;
    match result {
        Ok(done) => Ok(get(pool, done.last_insert_rowid())
            .await?
            .expect("just inserted")),
        Err(sqlx::Error::Database(e)) if e.is_unique_violation() => Err(AppError::BadRequest(
            format!("a user named '{name}' already exists"),
        )),
        Err(e) => Err(AppError::Database(e)),
    }
}

pub async fn set_password_hash(pool: &SqlitePool, id: i64, hash: &str) -> AppResult<bool> {
    let result = sqlx::query("UPDATE users SET password_hash = ? WHERE id = ?")
        .bind(hash)
        .bind(id)
        .execute(pool)
        .await
        .map_err(AppError::Database)?;
    Ok(result.rows_affected() > 0)
}

/// Delete a user together with their personal data (tokens cascade).
pub async fn delete(pool: &SqlitePool, id: i64) -> AppResult<bool> {
    sqlx::query("DELETE FROM watch_history WHERE user_id = ?")
        .bind(id)
        .execute(pool)
        .await
        .map_err(AppError::Database)?;
    sqlx::query("DELETE FROM watchlist WHERE user_id = ?")
        .bind(id)
        .execute(pool)
        .await
        .map_err(AppError::Database)?;
    sqlx::query("DELETE FROM user_tokens WHERE user_id = ?")
        .bind(id)
        .execute(pool)
        .await
        .map_err(AppError::Database)?;
    let result = sqlx::query("DELETE FROM users WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await
        .map_err(AppError::Database)?;
    Ok(result.rows_affected() > 0)
}

// ---- Tokens -----------------------------------------------------------------

pub async fn insert_token(
    pool: &SqlitePool,
    token: &str,
    user_id: i64,
    device: Option<&str>,
) -> AppResult<()> {
    sqlx::query("INSERT INTO user_tokens (token, user_id, device) VALUES (?, ?, ?)")
        .bind(token)
        .bind(user_id)
        .bind(device)
        .execute(pool)
        .await
        .map_err(AppError::Database)?;
    Ok(())
}

/// The user a bearer token belongs to (updating its recency), or `None`.
pub async fn token_user(pool: &SqlitePool, token: &str) -> AppResult<Option<User>> {
    let user: Option<User> = sqlx::query_as(
        "SELECT id, name, is_admin,
                password_hash IS NOT NULL AND password_hash != '' AS has_password
         FROM users
         WHERE id = (SELECT user_id FROM user_tokens WHERE token = ?)",
    )
    .bind(token)
    .fetch_optional(pool)
    .await
    .map_err(AppError::Database)?;
    if user.is_some() {
        sqlx::query("UPDATE user_tokens SET last_used_at = datetime('now') WHERE token = ?")
            .bind(token)
            .execute(pool)
            .await
            .map_err(AppError::Database)?;
    }
    Ok(user)
}

/// Invalidate every login token of one user (used on password reset so
/// existing sessions cannot continue with the old credential).
pub async fn delete_tokens_for_user(pool: &SqlitePool, user_id: i64) -> AppResult<()> {
    sqlx::query("DELETE FROM user_tokens WHERE user_id = ?")
        .bind(user_id)
        .execute(pool)
        .await
        .map_err(AppError::Database)?;
    Ok(())
}

pub async fn delete_token(pool: &SqlitePool, token: &str) -> AppResult<()> {
    sqlx::query("DELETE FROM user_tokens WHERE token = ?")
        .bind(token)
        .execute(pool)
        .await
        .map_err(AppError::Database)?;
    Ok(())
}
