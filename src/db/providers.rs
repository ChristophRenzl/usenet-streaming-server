//! CRUD repository for NNTP provider configurations.

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use utoipa::ToSchema;

use crate::error::AppResult;

#[derive(Debug, Clone, Serialize, sqlx::FromRow, ToSchema)]
pub struct Provider {
    pub id: i64,
    pub name: String,
    pub host: String,
    pub port: u16,
    pub use_tls: bool,
    pub username: Option<String>,
    pub password: Option<String>,
    pub max_connections: i64,
    pub priority: i64,
    pub enabled: bool,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct ProviderInput {
    pub name: String,
    pub host: String,
    #[serde(default = "default_port")]
    pub port: u16,
    #[serde(default = "default_true")]
    pub use_tls: bool,
    pub username: Option<String>,
    pub password: Option<String>,
    #[serde(default = "default_max_connections")]
    pub max_connections: i64,
    #[serde(default)]
    pub priority: i64,
    #[serde(default = "default_true")]
    pub enabled: bool,
}

fn default_true() -> bool {
    true
}

fn default_port() -> u16 {
    563
}

fn default_max_connections() -> i64 {
    10
}

pub async fn list(pool: &SqlitePool) -> AppResult<Vec<Provider>> {
    let rows = sqlx::query_as(
        "SELECT id, name, host, port, use_tls, username, password, max_connections, priority,
                enabled
         FROM nntp_providers ORDER BY priority DESC, id",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn get(pool: &SqlitePool, id: i64) -> AppResult<Option<Provider>> {
    let row = sqlx::query_as(
        "SELECT id, name, host, port, use_tls, username, password, max_connections, priority,
                enabled
         FROM nntp_providers WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

pub async fn create(pool: &SqlitePool, input: &ProviderInput) -> AppResult<Provider> {
    let row = sqlx::query_as(
        "INSERT INTO nntp_providers
             (name, host, port, use_tls, username, password, max_connections, priority, enabled)
         VALUES (?, ?, ?, ?, ?, ?, ?, ?, ?)
         RETURNING id, name, host, port, use_tls, username, password, max_connections, priority,
                   enabled",
    )
    .bind(&input.name)
    .bind(&input.host)
    .bind(input.port)
    .bind(input.use_tls)
    .bind(&input.username)
    .bind(&input.password)
    .bind(input.max_connections)
    .bind(input.priority)
    .bind(input.enabled)
    .fetch_one(pool)
    .await?;
    Ok(row)
}

pub async fn update(
    pool: &SqlitePool,
    id: i64,
    input: &ProviderInput,
) -> AppResult<Option<Provider>> {
    let row = sqlx::query_as(
        "UPDATE nntp_providers SET name = ?, host = ?, port = ?, use_tls = ?, username = ?,
             password = ?, max_connections = ?, priority = ?, enabled = ?
         WHERE id = ?
         RETURNING id, name, host, port, use_tls, username, password, max_connections, priority,
                   enabled",
    )
    .bind(&input.name)
    .bind(&input.host)
    .bind(input.port)
    .bind(input.use_tls)
    .bind(&input.username)
    .bind(&input.password)
    .bind(input.max_connections)
    .bind(input.priority)
    .bind(input.enabled)
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

pub async fn delete(pool: &SqlitePool, id: i64) -> AppResult<bool> {
    let result = sqlx::query("DELETE FROM nntp_providers WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}
