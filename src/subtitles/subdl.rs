//! SubDL fallback provider (subdl.com).
//!
//! Used only when OpenSubtitles cannot deliver — no result for the language,
//! a download failure, or the daily download quota (free accounts: 20/day).
//! SubDL needs its own (free) API key from <https://subdl.com/panel/api>,
//! stored as the `subdl_api_key` app setting.
//!
//! Subtitles are delivered as ZIP archives containing the SRT; the first
//! text entry is extracted and decoded.

use std::io::Read;

use serde::Deserialize;

use super::opensubtitles::SubtitleQuery;
use crate::error::{AppError, AppResult};

/// API host (search).
pub const DEFAULT_API_BASE: &str = "https://api.subdl.com";
/// Download host for the zip paths the API returns.
pub const DEFAULT_DL_BASE: &str = "https://dl.subdl.com";

/// One SubDL search hit, reduced to what the fallback needs.
#[derive(Debug, Clone, PartialEq)]
pub struct SubdlResult {
    /// Zip path on the download host (e.g. `/subtitle/123-456.zip`).
    pub url: String,
    /// Release/description string, when provided.
    pub release_name: Option<String>,
    /// Lower-cased ISO 639-1 language code.
    pub language: String,
    pub hearing_impaired: bool,
}

pub struct SubdlClient {
    http: reqwest::Client,
    api_key: String,
    api_base: String,
    dl_base: String,
}

impl SubdlClient {
    pub fn new(http: reqwest::Client, api_key: impl Into<String>) -> Self {
        Self {
            http,
            api_key: api_key.into(),
            api_base: DEFAULT_API_BASE.to_string(),
            dl_base: DEFAULT_DL_BASE.to_string(),
        }
    }

    /// Search subtitles for one language of the query. Results are filtered
    /// to the requested season/episode for shows.
    pub async fn search(
        &self,
        query: &SubtitleQuery,
        language: &str,
    ) -> AppResult<Vec<SubdlResult>> {
        let mut params: Vec<(&str, String)> = vec![
            ("api_key", self.api_key.clone()),
            ("tmdb_id", query.tmdb_id.to_string()),
            ("languages", language.to_uppercase()),
        ];
        if let (Some(season), Some(episode)) = (query.season, query.episode) {
            params.push(("type", "tv".into()));
            params.push(("season_number", season.to_string()));
            params.push(("episode_number", episode.to_string()));
        } else {
            params.push(("type", "movie".into()));
        }
        let response = self
            .http
            .get(format!("{}/api/v1/subtitles", self.api_base))
            .query(&params)
            .send()
            .await
            .map_err(|e| AppError::Upstream(format!("SubDL search: {e}")))?;
        let status = response.status();
        if !status.is_success() {
            return Err(AppError::Upstream(format!(
                "SubDL search returned HTTP {status}"
            )));
        }
        let body: SearchResponse = response
            .json()
            .await
            .map_err(|e| AppError::Upstream(format!("SubDL search response: {e}")))?;
        if !body.status {
            return Err(AppError::Upstream(format!(
                "SubDL rejected the search: {}",
                body.error.unwrap_or_else(|| "unknown error".into())
            )));
        }
        Ok(parse_results(body, query, language))
    }

    /// Download a search hit's zip and extract the subtitle text.
    pub async fn download(&self, url_path: &str) -> AppResult<String> {
        let response = self
            .http
            .get(format!("{}{}", self.dl_base, url_path))
            .send()
            .await
            .map_err(|e| AppError::Upstream(format!("SubDL download: {e}")))?;
        let status = response.status();
        if !status.is_success() {
            return Err(AppError::Upstream(format!(
                "SubDL download returned HTTP {status}"
            )));
        }
        let bytes = response
            .bytes()
            .await
            .map_err(|e| AppError::Upstream(format!("SubDL download body: {e}")))?;
        extract_subtitle_text(&bytes)
    }
}

/// Pull the first text-subtitle entry (`.srt`/`.ass`/`.ssa`/`.vtt`) out of the
/// zip and decode it to text.
fn extract_subtitle_text(zip_bytes: &[u8]) -> AppResult<String> {
    let mut archive = zip::ZipArchive::new(std::io::Cursor::new(zip_bytes))
        .map_err(|e| AppError::Upstream(format!("SubDL zip: {e}")))?;
    for index in 0..archive.len() {
        let mut entry = archive
            .by_index(index)
            .map_err(|e| AppError::Upstream(format!("SubDL zip entry: {e}")))?;
        let name = entry.name().to_ascii_lowercase();
        if !(name.ends_with(".srt")
            || name.ends_with(".ass")
            || name.ends_with(".ssa")
            || name.ends_with(".vtt"))
        {
            continue;
        }
        let mut bytes = Vec::with_capacity(entry.size() as usize);
        entry
            .read_to_end(&mut bytes)
            .map_err(|e| AppError::Upstream(format!("SubDL zip read: {e}")))?;
        return Ok(super::decode_subtitle_bytes(&bytes));
    }
    Err(AppError::Upstream(
        "SubDL zip contained no subtitle file".into(),
    ))
}

fn parse_results(body: SearchResponse, query: &SubtitleQuery, language: &str) -> Vec<SubdlResult> {
    let wanted = language.to_ascii_lowercase();
    body.subtitles
        .into_iter()
        .filter_map(|s| {
            let url = s.url?;
            // For shows, a hit tagged with another episode is a mismatch;
            // untagged hits (season packs) are kept.
            if let (Some(want), Some(got)) = (query.episode, s.episode) {
                if want != got {
                    return None;
                }
            }
            let lang = s
                .language
                .map(|l| l.to_ascii_lowercase())
                .unwrap_or_else(|| wanted.clone());
            if !lang.starts_with(&wanted) {
                return None;
            }
            Some(SubdlResult {
                url,
                release_name: s.release_name.or(s.name),
                language: wanted.clone(),
                hearing_impaired: s.hi.unwrap_or(false),
            })
        })
        .collect()
}

#[derive(Debug, Deserialize)]
struct SearchResponse {
    #[serde(default)]
    status: bool,
    error: Option<String>,
    #[serde(default)]
    subtitles: Vec<RawSubtitle>,
}

#[derive(Debug, Deserialize)]
struct RawSubtitle {
    release_name: Option<String>,
    name: Option<String>,
    /// ISO code, upper-cased by SubDL ("EN").
    language: Option<String>,
    url: Option<String>,
    episode: Option<u32>,
    hi: Option<bool>,
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Write;

    fn zip_with(name: &str, content: &[u8]) -> Vec<u8> {
        let mut cursor = std::io::Cursor::new(Vec::new());
        {
            let mut writer = zip::ZipWriter::new(&mut cursor);
            writer
                .start_file::<_, ()>(name, zip::write::FileOptions::default())
                .expect("start file");
            writer.write_all(content).expect("write");
            writer.finish().expect("finish");
        }
        cursor.into_inner()
    }

    #[test]
    fn extracts_srt_from_zip() {
        let zip = zip_with(
            "Some.Movie.2024.srt",
            b"1\n00:00:01,000 --> 00:00:02,000\nHi\n",
        );
        let text = extract_subtitle_text(&zip).expect("extract");
        assert!(text.contains("Hi"));
    }

    #[test]
    fn zip_without_subtitles_errors() {
        let zip = zip_with("readme.txt", b"nope");
        assert!(extract_subtitle_text(&zip).is_err());
    }

    #[test]
    fn parses_and_filters_results() {
        let body: SearchResponse = serde_json::from_str(
            r#"{
                "status": true,
                "subtitles": [
                    {"release_name": "Show.S01E02.1080p", "language": "EN",
                     "url": "/subtitle/1-2.zip", "episode": 2, "hi": false},
                    {"release_name": "Show.S01E03.1080p", "language": "EN",
                     "url": "/subtitle/1-3.zip", "episode": 3},
                    {"release_name": "Show.S01E02.GERMAN", "language": "DE",
                     "url": "/subtitle/1-4.zip", "episode": 2},
                    {"release_name": "No url", "language": "EN"}
                ]
            }"#,
        )
        .expect("parse");
        let query = SubtitleQuery {
            tmdb_id: 42,
            season: Some(1),
            episode: Some(2),
            languages: vec!["en".into()],
            moviehash: None,
        };
        let results = parse_results(body, &query, "en");
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].url, "/subtitle/1-2.zip");
        assert_eq!(results[0].language, "en");
    }
}
