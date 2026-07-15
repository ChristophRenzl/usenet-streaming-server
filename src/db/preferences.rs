//! Quality preferences for the single default user (user_id = 1).

use serde::{Deserialize, Serialize};
use sqlx::SqlitePool;
use utoipa::ToSchema;

use crate::{error::AppResult, release::parse::Resolution};

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct Preferences {
    pub preferred_resolution: Resolution,
    pub max_resolution: Resolution,
    /// Series-specific preferred resolution: TV episodes rank against this
    /// when set (e.g. 2160p movies but 1080p episodes); `None` falls back to
    /// `preferred_resolution`. Defaulted so older clients that omit the field
    /// keep working.
    #[serde(default)]
    pub preferred_resolution_tv: Option<Resolution>,
    /// Series-specific maximum resolution; `None` falls back to
    /// `max_resolution`.
    #[serde(default)]
    pub max_resolution_tv: Option<Resolution>,
    pub preferred_video_codecs: Vec<String>,
    pub preferred_audio_codecs: Vec<String>,
    pub max_size_bytes: Option<i64>,
    /// Preferred audio language: an ISO 639-1 code / language name
    /// (`en`, `de`, `german`, ...) or the special value `original`, which
    /// resolves to each title's TMDB original language at ranking time
    /// (e.g. Japanese audio for anime).
    pub language: String,
    /// Boost terms — releases containing these score higher (not required).
    pub allowed_terms: Vec<String>,
    /// Hard-exclude terms — case-insensitive substring match on the title.
    pub blocked_terms: Vec<String>,
    /// Rank larger releases (more bitrate) above smaller ones within the same
    /// quality tier. Defaulted so older clients that omit the field keep
    /// working.
    #[serde(default)]
    pub prefer_larger_releases: bool,
    /// Allow Dolby-Vision-only releases. When off, DV releases without an
    /// HDR10 fallback are rejected in ranking and a DV profile 5 stream that
    /// still slips through is tone-mapped instead of served as DV.
    #[serde(default = "default_true")]
    pub allow_dolby_vision: bool,
}

fn default_true() -> bool {
    true
}

impl Preferences {
    /// The preferences a given media type should be ranked against: TV uses
    /// its own resolution overrides when set, falling back to the movie/global
    /// values. The preferred value is clamped to the effective max so a partial
    /// override (say `max_resolution_tv = 1080p` with a 2160p global preferred)
    /// can never produce an inconsistent pair.
    pub fn for_media_type(mut self, tv: bool) -> Self {
        if tv {
            if let Some(preferred) = self.preferred_resolution_tv {
                self.preferred_resolution = preferred;
            }
            if let Some(max) = self.max_resolution_tv {
                self.max_resolution = max;
            }
        }
        if self.preferred_resolution > self.max_resolution {
            self.preferred_resolution = self.max_resolution;
        }
        self
    }
}

#[derive(sqlx::FromRow)]
struct PreferencesRow {
    preferred_resolution: String,
    max_resolution: String,
    preferred_resolution_tv: Option<String>,
    max_resolution_tv: Option<String>,
    preferred_video_codecs: String,
    preferred_audio_codecs: String,
    max_size_bytes: Option<i64>,
    language: String,
    allowed_terms: String,
    blocked_terms: String,
    prefer_larger_releases: bool,
    allow_dolby_vision: bool,
}

const USER_ID: i64 = 1;

pub async fn get(pool: &SqlitePool) -> AppResult<Preferences> {
    let row: PreferencesRow = sqlx::query_as(
        "SELECT preferred_resolution, max_resolution, preferred_resolution_tv,
                max_resolution_tv, preferred_video_codecs,
                preferred_audio_codecs, max_size_bytes, language, allowed_terms, blocked_terms,
                prefer_larger_releases, allow_dolby_vision
         FROM preferences WHERE user_id = ?",
    )
    .bind(USER_ID)
    .fetch_one(pool)
    .await?;

    Ok(Preferences {
        preferred_resolution: parse_resolution(&row.preferred_resolution)?,
        max_resolution: parse_resolution(&row.max_resolution)?,
        preferred_resolution_tv: parse_optional_resolution(row.preferred_resolution_tv.as_deref())?,
        max_resolution_tv: parse_optional_resolution(row.max_resolution_tv.as_deref())?,
        preferred_video_codecs: parse_terms(&row.preferred_video_codecs)?,
        preferred_audio_codecs: parse_terms(&row.preferred_audio_codecs)?,
        max_size_bytes: row.max_size_bytes,
        language: row.language,
        allowed_terms: parse_terms(&row.allowed_terms)?,
        blocked_terms: parse_terms(&row.blocked_terms)?,
        prefer_larger_releases: row.prefer_larger_releases,
        allow_dolby_vision: row.allow_dolby_vision,
    })
}

pub async fn set(pool: &SqlitePool, prefs: &Preferences) -> AppResult<()> {
    sqlx::query(
        "UPDATE preferences SET preferred_resolution = ?, max_resolution = ?,
             preferred_resolution_tv = ?, max_resolution_tv = ?,
             preferred_video_codecs = ?, preferred_audio_codecs = ?, max_size_bytes = ?,
             language = ?, allowed_terms = ?, blocked_terms = ?,
             prefer_larger_releases = ?, allow_dolby_vision = ?,
             updated_at = datetime('now')
         WHERE user_id = ?",
    )
    .bind(prefs.preferred_resolution.to_string())
    .bind(prefs.max_resolution.to_string())
    .bind(prefs.preferred_resolution_tv.map(|r| r.to_string()))
    .bind(prefs.max_resolution_tv.map(|r| r.to_string()))
    .bind(to_json(&prefs.preferred_video_codecs)?)
    .bind(to_json(&prefs.preferred_audio_codecs)?)
    .bind(prefs.max_size_bytes)
    .bind(&prefs.language)
    .bind(to_json(&prefs.allowed_terms)?)
    .bind(to_json(&prefs.blocked_terms)?)
    .bind(prefs.prefer_larger_releases)
    .bind(prefs.allow_dolby_vision)
    .bind(USER_ID)
    .execute(pool)
    .await?;
    Ok(())
}

fn parse_resolution(s: &str) -> AppResult<Resolution> {
    s.parse()
        .map_err(|e: String| anyhow::anyhow!("invalid resolution in preferences: {e}").into())
}

fn parse_optional_resolution(s: Option<&str>) -> AppResult<Option<Resolution>> {
    s.map(parse_resolution).transpose()
}

fn parse_terms(json: &str) -> AppResult<Vec<String>> {
    serde_json::from_str(json)
        .map_err(|e| anyhow::anyhow!("invalid JSON list in preferences: {e}").into())
}

fn to_json(terms: &[String]) -> AppResult<String> {
    serde_json::to_string(terms).map_err(|e| anyhow::anyhow!("serializing terms: {e}").into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn prefs() -> Preferences {
        Preferences {
            preferred_resolution: Resolution::R1080p,
            max_resolution: Resolution::R2160p,
            preferred_resolution_tv: None,
            max_resolution_tv: None,
            preferred_video_codecs: vec![],
            preferred_audio_codecs: vec![],
            max_size_bytes: None,
            language: "en".into(),
            allowed_terms: vec![],
            blocked_terms: vec![],
            prefer_larger_releases: false,
            allow_dolby_vision: true,
        }
    }

    #[test]
    fn movies_ignore_tv_overrides() {
        let mut p = prefs();
        p.preferred_resolution_tv = Some(Resolution::R720p);
        p.max_resolution_tv = Some(Resolution::R720p);
        let effective = p.for_media_type(false);
        assert_eq!(effective.preferred_resolution, Resolution::R1080p);
        assert_eq!(effective.max_resolution, Resolution::R2160p);
    }

    #[test]
    fn tv_uses_overrides_when_set() {
        let mut p = prefs();
        p.preferred_resolution_tv = Some(Resolution::R720p);
        p.max_resolution_tv = Some(Resolution::R1080p);
        let effective = p.for_media_type(true);
        assert_eq!(effective.preferred_resolution, Resolution::R720p);
        assert_eq!(effective.max_resolution, Resolution::R1080p);
    }

    #[test]
    fn tv_without_overrides_falls_back_to_global() {
        let effective = prefs().for_media_type(true);
        assert_eq!(effective.preferred_resolution, Resolution::R1080p);
        assert_eq!(effective.max_resolution, Resolution::R2160p);
    }

    #[test]
    fn partial_override_clamps_preferred_to_effective_max() {
        // Global preferred 2160p, TV max 1080p, no TV preferred: the pair must
        // stay consistent (preferred ≤ max) after applying the override.
        let mut p = prefs();
        p.preferred_resolution = Resolution::R2160p;
        p.max_resolution_tv = Some(Resolution::R1080p);
        let effective = p.for_media_type(true);
        assert_eq!(effective.preferred_resolution, Resolution::R1080p);
        assert_eq!(effective.max_resolution, Resolution::R1080p);
    }
}
