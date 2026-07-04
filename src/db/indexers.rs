//! CRUD repository for Newznab indexer configurations.

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use utoipa::ToSchema;

use crate::error::AppResult;

#[derive(Debug, Clone, Serialize, sqlx::FromRow, ToSchema)]
pub struct Indexer {
    pub id: i64,
    pub name: String,
    pub base_url: String,
    pub api_key: String,
    pub enabled: bool,
    pub priority: i64,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct IndexerInput {
    pub name: String,
    pub base_url: String,
    pub api_key: String,
    #[serde(default = "default_true")]
    pub enabled: bool,
    #[serde(default)]
    pub priority: i64,
}

fn default_true() -> bool {
    true
}

pub async fn list(pool: &SqlitePool) -> AppResult<Vec<Indexer>> {
    let rows = sqlx::query_as(
        "SELECT id, name, base_url, api_key, enabled, priority
         FROM indexers ORDER BY priority DESC, id",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn list_enabled(pool: &SqlitePool) -> AppResult<Vec<Indexer>> {
    let rows = sqlx::query_as(
        "SELECT id, name, base_url, api_key, enabled, priority
         FROM indexers WHERE enabled = 1 ORDER BY priority DESC, id",
    )
    .fetch_all(pool)
    .await?;
    Ok(rows)
}

pub async fn get(pool: &SqlitePool, id: i64) -> AppResult<Option<Indexer>> {
    let row = sqlx::query_as(
        "SELECT id, name, base_url, api_key, enabled, priority FROM indexers WHERE id = ?",
    )
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

pub async fn create(pool: &SqlitePool, input: &IndexerInput) -> AppResult<Indexer> {
    let row = sqlx::query_as(
        "INSERT INTO indexers (name, base_url, api_key, enabled, priority)
         VALUES (?, ?, ?, ?, ?)
         RETURNING id, name, base_url, api_key, enabled, priority",
    )
    .bind(&input.name)
    .bind(&input.base_url)
    .bind(&input.api_key)
    .bind(input.enabled)
    .bind(input.priority)
    .fetch_one(pool)
    .await?;
    Ok(row)
}

pub async fn update(
    pool: &SqlitePool,
    id: i64,
    input: &IndexerInput,
) -> AppResult<Option<Indexer>> {
    let row = sqlx::query_as(
        "UPDATE indexers SET name = ?, base_url = ?, api_key = ?, enabled = ?, priority = ?
         WHERE id = ?
         RETURNING id, name, base_url, api_key, enabled, priority",
    )
    .bind(&input.name)
    .bind(&input.base_url)
    .bind(&input.api_key)
    .bind(input.enabled)
    .bind(input.priority)
    .bind(id)
    .fetch_optional(pool)
    .await?;
    Ok(row)
}

pub async fn delete(pool: &SqlitePool, id: i64) -> AppResult<bool> {
    let result = sqlx::query("DELETE FROM indexers WHERE id = ?")
        .bind(id)
        .execute(pool)
        .await?;
    Ok(result.rows_affected() > 0)
}
