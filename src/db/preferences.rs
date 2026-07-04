//! Quality preferences for the single default user (user_id = 1).

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use utoipa::ToSchema;

use crate::{error::AppResult, release::parse::Resolution};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct Preferences {
    pub preferred_resolution: Resolution,
    pub max_resolution: Resolution,
    pub preferred_video_codecs: Vec<String>,
    pub preferred_audio_codecs: Vec<String>,
    pub max_size_bytes: Option<i64>,
    pub language: String,
    /// Boost terms — releases containing these score higher (not required).
    pub allowed_terms: Vec<String>,
    /// Hard-exclude terms — case-insensitive substring match on the title.
    pub blocked_terms: Vec<String>,
}

#[derive(sqlx::FromRow)]
struct PreferencesRow {
    preferred_resolution: String,
    max_resolution: String,
    preferred_video_codecs: String,
    preferred_audio_codecs: String,
    max_size_bytes: Option<i64>,
    language: String,
    allowed_terms: String,
    blocked_terms: String,
}

const USER_ID: i64 = 1;

pub async fn get(pool: &SqlitePool) -> AppResult<Preferences> {
    let row: PreferencesRow = sqlx::query_as(
        "SELECT preferred_resolution, max_resolution, preferred_video_codecs,
                preferred_audio_codecs, max_size_bytes, language, allowed_terms, blocked_terms
         FROM preferences WHERE user_id = ?",
    )
    .bind(USER_ID)
    .fetch_one(pool)
    .await?;

    Ok(Preferences {
        preferred_resolution: parse_resolution(&row.preferred_resolution)?,
        max_resolution: parse_resolution(&row.max_resolution)?,
        preferred_video_codecs: parse_terms(&row.preferred_video_codecs)?,
        preferred_audio_codecs: parse_terms(&row.preferred_audio_codecs)?,
        max_size_bytes: row.max_size_bytes,
        language: row.language,
        allowed_terms: parse_terms(&row.allowed_terms)?,
        blocked_terms: parse_terms(&row.blocked_terms)?,
    })
}

pub async fn set(pool: &SqlitePool, prefs: &Preferences) -> AppResult<()> {
    sqlx::query(
        "UPDATE preferences SET preferred_resolution = ?, max_resolution = ?,
             preferred_video_codecs = ?, preferred_audio_codecs = ?, max_size_bytes = ?,
             language = ?, allowed_terms = ?, blocked_terms = ?, updated_at = datetime('now')
         WHERE user_id = ?",
    )
    .bind(prefs.preferred_resolution.to_string())
    .bind(prefs.max_resolution.to_string())
    .bind(to_json(&prefs.preferred_video_codecs)?)
    .bind(to_json(&prefs.preferred_audio_codecs)?)
    .bind(prefs.max_size_bytes)
    .bind(&prefs.language)
    .bind(to_json(&prefs.allowed_terms)?)
    .bind(to_json(&prefs.blocked_terms)?)
    .bind(USER_ID)
    .execute(pool)
    .await?;
    Ok(())
}

fn parse_resolution(s: &str) -> AppResult<Resolution> {
    s.parse()
        .map_err(|e: String| anyhow::anyhow!("invalid resolution in preferences: {e}").into())
}

fn parse_terms(json: &str) -> AppResult<Vec<String>> {
    serde_json::from_str(json)
        .map_err(|e| anyhow::anyhow!("invalid JSON list in preferences: {e}").into())
}

fn to_json(terms: &[String]) -> AppResult<String> {
    serde_json::to_string(terms).map_err(|e| anyhow::anyhow!("serializing terms: {e}").into())
}
