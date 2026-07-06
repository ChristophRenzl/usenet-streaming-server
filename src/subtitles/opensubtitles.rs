//! Async OpenSubtitles REST API v1 client.
//!
//! The base URL is constructor-injected so tests can point at a wiremock
//! server. Every request carries the consumer `Api-Key` header (stored in
//! `app_settings` under `opensubtitles_api_key`). Downloads may additionally
//! send a user JWT obtained via [`OpenSubtitlesClient::login`] — logging in
//! with an OpenSubtitles account lifts the anonymous download quota.
//!
//! Secrets never appear in logs: error messages are built from status codes,
//! not from URLs, headers or bodies.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::error::{AppError, AppResult};

use super::vtt::decode_subtitle_bytes;

/// User-Agent required by the OpenSubtitles API (they reject blank ones).
const USER_AGENT: &str = concat!("usenet-streaming-server/", env!("CARGO_PKG_VERSION"));

/// Marker message for "the user JWT was rejected" errors, so callers can drop
/// a cached token, log in again and retry once.
const TOKEN_REJECTED: &str = "OpenSubtitles rejected the login token";

/// True when `error` is the download-with-token 401 produced by this client
/// (i.e. a cached login token has expired and a fresh login may fix it).
pub fn is_token_rejected(error: &AppError) -> bool {
    matches!(error, AppError::Upstream(message) if message.starts_with(TOKEN_REJECTED))
}

/// One ranked subtitle candidate returned by [`OpenSubtitlesClient::search`].
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, ToSchema)]
pub struct SubtitleResult {
    /// OpenSubtitles `data[].id` (the subtitle entry id, a string).
    pub id: String,
    /// ISO 639-1 language code (`en`, `de`, ...), lower-cased.
    pub language: String,
    /// Human release/description string, when the API provides one.
    pub release_name: Option<String>,
    /// The downloadable file id — pass this to `download`/the attach endpoint.
    pub file_id: i64,
    /// Whether this is a hearing-impaired (SDH) subtitle.
    pub hearing_impaired: bool,
    /// Times this subtitle has been downloaded (popularity signal).
    pub download_count: i64,
    /// Whether the subtitle was produced by machine translation.
    pub ai_translated: bool,
    /// True when this subtitle was matched by the media's moviehash — it was
    /// timed against *this* exact release, so it needs no fps/offset
    /// correction and ranks first.
    pub moviehash_match: bool,
    /// Frame rate the subtitle was authored at, when reported. Used to correct
    /// fps-mismatch drift for non-hash-matched subtitles.
    pub fps: Option<f64>,
}

/// A resolved download link plus the account's remaining daily quota.
#[derive(Debug, Clone)]
pub struct DownloadLink {
    /// Temporary CDN URL serving the raw subtitle bytes.
    pub url: String,
    /// Downloads left today, when the API reports it.
    pub remaining_quota: Option<i64>,
}

/// A fully downloaded subtitle: decoded text plus quota info.
#[derive(Debug, Clone)]
pub struct SubtitleDownload {
    /// Decoded subtitle text (SRT).
    pub text: String,
    /// Downloads left today, when the API reports it.
    pub remaining_quota: Option<i64>,
}

pub struct OpenSubtitlesClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
}

// ---- Raw API shapes ---------------------------------------------------------

#[derive(Debug, Deserialize)]
struct RawSearchResponse {
    #[serde(default)]
    data: Vec<RawSubtitle>,
}

#[derive(Debug, Deserialize)]
struct RawSubtitle {
    id: Option<String>,
    attributes: Option<RawAttributes>,
}

#[derive(Debug, Deserialize)]
struct RawAttributes {
    language: Option<String>,
    release: Option<String>,
    #[serde(default)]
    download_count: i64,
    #[serde(default)]
    hearing_impaired: bool,
    #[serde(default)]
    ai_translated: bool,
    #[serde(default)]
    moviehash_match: bool,
    /// OpenSubtitles reports fps as a JSON number; accept string too.
    #[serde(default, deserialize_with = "de_opt_f64")]
    fps: Option<f64>,
    #[serde(default)]
    files: Vec<RawFile>,
}

/// Deserialize an optional fps that OpenSubtitles may send as a number, a
/// numeric string, `0` (meaning "unknown") or `null`.
fn de_opt_f64<'de, D>(deserializer: D) -> Result<Option<f64>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    let value = serde_json::Value::deserialize(deserializer)?;
    let fps = match value {
        serde_json::Value::Number(n) => n.as_f64(),
        serde_json::Value::String(s) => s.trim().parse::<f64>().ok(),
        _ => None,
    };
    // OpenSubtitles uses 0 to mean "no fps recorded".
    Ok(fps.filter(|f| f.is_finite() && *f > 0.0))
}

#[derive(Debug, Deserialize)]
struct RawFile {
    file_id: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct RawDownloadResponse {
    link: Option<String>,
    remaining: Option<i64>,
    #[serde(default)]
    message: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawLoginResponse {
    token: Option<String>,
}

impl OpenSubtitlesClient {
    pub fn new(
        http: reqwest::Client,
        base_url: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Self {
        Self {
            http,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key: api_key.into(),
        }
    }

    /// Apply the shared auth + identification headers to a request builder.
    fn authed(&self, builder: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        builder
            .header("Api-Key", &self.api_key)
            .header(reqwest::header::USER_AGENT, USER_AGENT)
            .header(reqwest::header::ACCEPT, "application/json")
    }

    /// Log in with an OpenSubtitles account and return the user JWT. Sending
    /// it on downloads lifts the anonymous per-IP quota. The token is valid
    /// for about a day; cache it (see `subtitles::TokenCache`) instead of
    /// logging in per request.
    pub async fn login(&self, username: &str, password: &str) -> AppResult<String> {
        let url = format!("{}/login", self.base_url);
        let response = self
            .authed(self.http.post(&url))
            .json(&serde_json::json!({ "username": username, "password": password }))
            .send()
            .await
            .map_err(|e| {
                AppError::Upstream(format!(
                    "OpenSubtitles login request failed: {}",
                    e.without_url()
                ))
            })?;

        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::BAD_REQUEST
        {
            return Err(AppError::Upstream(format!(
                "OpenSubtitles login failed (HTTP {}): check the configured username/password \
                 (note: the username, not the account e-mail address)",
                status.as_u16()
            )));
        }
        if !status.is_success() {
            return Err(quota_or_upstream(status, "login"));
        }
        let raw: RawLoginResponse = response.json().await.map_err(|e| {
            AppError::Upstream(format!(
                "OpenSubtitles login decode failed: {}",
                e.without_url()
            ))
        })?;
        raw.token
            .filter(|t| !t.is_empty())
            .ok_or_else(|| AppError::Upstream("OpenSubtitles login returned no token".into()))
    }

    /// Search subtitles for a movie (`tmdb_id`) or an episode
    /// (`parent_tmdb_id` + `season` + `episode`). `languages` is a comma
    /// separated ISO 639-1 list (e.g. `en,de`); it also drives ranking. When
    /// `moviehash` is set, hash-matched (release-accurate) results are found
    /// and ranked first. Results are ranked: moviehash matches first, then
    /// matching-language order, then non-AI, then higher download count.
    pub async fn search(&self, query: &SubtitleQuery) -> AppResult<Vec<SubtitleResult>> {
        let mut params: Vec<(&str, String)> = Vec::new();
        match query.season.zip(query.episode) {
            // An episode: OpenSubtitles keys on the *show* tmdb id plus S/E.
            Some((season, episode)) => {
                params.push(("parent_tmdb_id", query.tmdb_id.to_string()));
                params.push(("season_number", season.to_string()));
                params.push(("episode_number", episode.to_string()));
            }
            None => params.push(("tmdb_id", query.tmdb_id.to_string())),
        }
        if !query.languages.is_empty() {
            params.push(("languages", query.languages.join(",")));
        }
        if let Some(hash) = query.moviehash.as_deref().filter(|h| !h.is_empty()) {
            params.push(("moviehash", hash.to_string()));
        }

        let url = format!("{}/subtitles", self.base_url);
        let response = self
            .authed(self.http.get(&url))
            .query(&params)
            .send()
            .await
            .map_err(|e| {
                AppError::Upstream(format!("OpenSubtitles request failed: {}", e.without_url()))
            })?;

        let status = response.status();
        if !status.is_success() {
            return Err(quota_or_upstream(status, "search"));
        }
        let raw: RawSearchResponse = response.json().await.map_err(|e| {
            AppError::Upstream(format!(
                "OpenSubtitles response decode failed: {}",
                e.without_url()
            ))
        })?;

        let mut results: Vec<SubtitleResult> =
            raw.data.into_iter().filter_map(map_subtitle).collect();
        rank_results(&mut results, &query.languages);
        Ok(results)
    }

    /// Resolve a `file_id` to a temporary CDN link (POST /download). The
    /// subtitle is requested as SRT so the local SRT→WebVTT conversion always
    /// sees the format it expects. `token` is an optional user JWT from
    /// [`login`](Self::login) that lifts the download quota. Quota errors from
    /// the API (406/429) surface as [`AppError::Upstream`] with a clear message.
    pub async fn download_link(
        &self,
        file_id: i64,
        token: Option<&str>,
    ) -> AppResult<DownloadLink> {
        let url = format!("{}/download", self.base_url);
        let mut request = self
            .authed(self.http.post(&url))
            .header(reqwest::header::CONTENT_TYPE, "application/json")
            .json(&serde_json::json!({ "file_id": file_id, "sub_format": "srt" }));
        if let Some(token) = token {
            request = request.bearer_auth(token);
        }
        let response = request.send().await.map_err(|e| {
            AppError::Upstream(format!(
                "OpenSubtitles download request failed: {}",
                e.without_url()
            ))
        })?;

        let status = response.status();
        if status == reqwest::StatusCode::UNAUTHORIZED && token.is_some() {
            // Distinguishable so the caller can re-login once and retry.
            return Err(AppError::Upstream(format!(
                "{TOKEN_REJECTED} (HTTP 401); it may have expired"
            )));
        }
        if !status.is_success() {
            return Err(quota_or_upstream(status, "download"));
        }
        let raw: RawDownloadResponse = response.json().await.map_err(|e| {
            AppError::Upstream(format!(
                "OpenSubtitles download decode failed: {}",
                e.without_url()
            ))
        })?;
        let url = raw.link.filter(|l| !l.is_empty()).ok_or_else(|| {
            AppError::Upstream(format!(
                "OpenSubtitles download returned no link{}",
                raw.message.map(|m| format!(": {m}")).unwrap_or_default()
            ))
        })?;
        Ok(DownloadLink {
            url,
            remaining_quota: raw.remaining,
        })
    }

    /// Download a subtitle end to end: resolve the link and fetch the bytes,
    /// returning the decoded subtitle text (SRT) plus the remaining quota.
    /// BOM/latin-1 handled.
    pub async fn download_subtitle(
        &self,
        file_id: i64,
        token: Option<&str>,
    ) -> AppResult<SubtitleDownload> {
        let link = self.download_link(file_id, token).await?;
        let response = self
            .http
            .get(&link.url)
            .header(reqwest::header::USER_AGENT, USER_AGENT)
            .send()
            .await
            .map_err(|e| {
                AppError::Upstream(format!(
                    "OpenSubtitles CDN fetch failed: {}",
                    e.without_url()
                ))
            })?;
        if !response.status().is_success() {
            return Err(AppError::Upstream(format!(
                "OpenSubtitles CDN returned HTTP {}",
                response.status()
            )));
        }
        let bytes = response.bytes().await.map_err(|e| {
            AppError::Upstream(format!(
                "reading OpenSubtitles subtitle body: {}",
                e.without_url()
            ))
        })?;
        Ok(SubtitleDownload {
            text: decode_subtitle_bytes(&bytes),
            remaining_quota: link.remaining_quota,
        })
    }
}

/// Parameters for a subtitle search.
#[derive(Debug, Clone, Default)]
pub struct SubtitleQuery {
    /// For movies: the movie's TMDB id. For episodes: the *show's* TMDB id.
    pub tmdb_id: i64,
    /// Season number (episodes only).
    pub season: Option<u32>,
    /// Episode number (episodes only).
    pub episode: Option<u32>,
    /// Preferred ISO 639-1 language codes, most-preferred first.
    pub languages: Vec<String>,
    /// OpenSubtitles/OSDb moviehash of the media file, when known (session
    /// auto-attach path only). Hash-matched results rank first. The standalone
    /// tmdb search leaves this `None`.
    pub moviehash: Option<String>,
}

/// Map 406 (quota exhausted) / 429 (rate limited) to a clear upstream message;
/// everything else to a generic upstream HTTP error.
fn quota_or_upstream(status: reqwest::StatusCode, op: &str) -> AppError {
    match status.as_u16() {
        406 => AppError::Upstream(
            "OpenSubtitles daily download quota exhausted (HTTP 406); add OpenSubtitles \
             account credentials via PUT /settings/app for a higher quota, or try again \
             tomorrow"
                .into(),
        ),
        429 => AppError::Upstream(
            "OpenSubtitles rate limit hit (HTTP 429); slow down and retry".into(),
        ),
        401 | 403 => AppError::Upstream(
            "OpenSubtitles rejected the API key (HTTP 401/403); check the key".into(),
        ),
        other => AppError::Upstream(format!("OpenSubtitles {op} returned HTTP {other}")),
    }
}

fn map_subtitle(item: RawSubtitle) -> Option<SubtitleResult> {
    let id = item.id?;
    let attributes = item.attributes?;
    // Only entries with a downloadable file are usable.
    let file_id = attributes.files.iter().find_map(|f| f.file_id)?;
    Some(SubtitleResult {
        id,
        language: attributes.language.unwrap_or_default().to_ascii_lowercase(),
        release_name: attributes.release,
        file_id,
        hearing_impaired: attributes.hearing_impaired,
        download_count: attributes.download_count,
        ai_translated: attributes.ai_translated,
        moviehash_match: attributes.moviehash_match,
        fps: attributes.fps,
    })
}

/// Rank subtitles: moviehash (release-accurate) matches first, then
/// preferred-language order, then human (non-AI) over machine translations,
/// then higher download count.
fn rank_results(results: &mut [SubtitleResult], languages: &[String]) {
    let lang_rank = |lang: &str| -> usize {
        languages
            .iter()
            .position(|l| l.eq_ignore_ascii_case(lang))
            .unwrap_or(languages.len())
    };
    results.sort_by(|a, b| {
        // Hash matches first (true sorts before false via reverse).
        b.moviehash_match
            .cmp(&a.moviehash_match)
            .then(lang_rank(&a.language).cmp(&lang_rank(&b.language)))
            .then(a.ai_translated.cmp(&b.ai_translated)) // false (human) first
            .then(b.download_count.cmp(&a.download_count)) // higher first
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sub(lang: &str, ai: bool, count: i64, file_id: i64) -> SubtitleResult {
        SubtitleResult {
            id: file_id.to_string(),
            language: lang.into(),
            release_name: None,
            file_id,
            hearing_impaired: false,
            download_count: count,
            ai_translated: ai,
            moviehash_match: false,
            fps: None,
        }
    }

    #[test]
    fn ranking_prefers_language_order_then_human_then_downloads() {
        let mut results = vec![
            sub("de", false, 100, 1),
            sub("en", true, 5000, 2),
            sub("en", false, 10, 3),
            sub("en", false, 900, 4),
        ];
        rank_results(&mut results, &["en".into(), "de".into()]);
        // English first (preferred), human over AI, then by download count.
        assert_eq!(results[0].file_id, 4); // en, human, 900
        assert_eq!(results[1].file_id, 3); // en, human, 10
        assert_eq!(results[2].file_id, 2); // en, AI
        assert_eq!(results[3].file_id, 1); // de
    }

    #[test]
    fn ranking_puts_moviehash_matches_first() {
        // A German hash-matched sub outranks a preferred-language English one.
        let mut hash_match = sub("de", false, 1, 10);
        hash_match.moviehash_match = true;
        let mut results = vec![sub("en", false, 9000, 20), hash_match];
        rank_results(&mut results, &["en".into(), "de".into()]);
        assert_eq!(results[0].file_id, 10, "hash match ranks first");
        assert!(results[0].moviehash_match);
        assert_eq!(results[1].file_id, 20);
    }

    #[test]
    fn map_subtitle_requires_a_file_id() {
        let raw = RawSubtitle {
            id: Some("42".into()),
            attributes: Some(RawAttributes {
                language: Some("EN".into()),
                release: Some("Rel".into()),
                download_count: 3,
                hearing_impaired: true,
                ai_translated: false,
                moviehash_match: false,
                fps: None,
                files: vec![],
            }),
        };
        assert!(map_subtitle(raw).is_none());

        let raw = RawSubtitle {
            id: Some("42".into()),
            attributes: Some(RawAttributes {
                language: Some("EN".into()),
                release: Some("Rel".into()),
                download_count: 3,
                hearing_impaired: true,
                ai_translated: false,
                moviehash_match: true,
                fps: Some(23.976),
                files: vec![RawFile { file_id: Some(777) }],
            }),
        };
        let mapped = map_subtitle(raw).expect("mapped");
        assert_eq!(mapped.file_id, 777);
        assert_eq!(mapped.language, "en", "language is lower-cased");
        assert!(mapped.hearing_impaired);
        assert!(mapped.moviehash_match);
        assert_eq!(mapped.fps, Some(23.976));
    }

    #[test]
    fn fps_deserializes_from_number_string_and_zero() {
        #[derive(serde::Deserialize)]
        struct Holder {
            #[serde(default, deserialize_with = "de_opt_f64")]
            fps: Option<f64>,
        }
        let num: Holder = serde_json::from_str(r#"{"fps": 23.976}"#).unwrap();
        assert_eq!(num.fps, Some(23.976));
        let string: Holder = serde_json::from_str(r#"{"fps": "25.000"}"#).unwrap();
        assert_eq!(string.fps, Some(25.0));
        // 0 means "unknown".
        let zero: Holder = serde_json::from_str(r#"{"fps": 0}"#).unwrap();
        assert_eq!(zero.fps, None);
        let missing: Holder = serde_json::from_str(r#"{}"#).unwrap();
        assert_eq!(missing.fps, None);
    }
}
