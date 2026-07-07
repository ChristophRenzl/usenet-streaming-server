//! OpenSubtitles integration: subtitle search/download and the WebVTT + HLS
//! wiring that delivers external subtitles into a playback session so tvOS
//! AVPlayer offers them natively.

pub mod hash;
pub mod hls;
pub mod opensubtitles;
pub mod vtt;

use std::sync::Arc;

pub use hash::osdb_hash;
pub use hls::{embedded_subtitle_playlist, embedded_window_name, EMBEDDED_WINDOW_SECS};
pub use hls::{master_playlist, subtitle_media_playlist, SubtitleTrack, SUBTITLE_GROUP};
pub use opensubtitles::{
    is_token_rejected, DownloadLink, OpenSubtitlesClient, SubtitleDownload, SubtitleQuery,
    SubtitleResult,
};
pub use vtt::{decode_subtitle_bytes, shift_vtt, srt_to_vtt, srt_to_vtt_scaled, window_vtt};

use crate::error::{AppError, AppResult};
use crate::stream::Session;

/// Production OpenSubtitles REST API v1 base URL. Injectable in
/// [`OpenSubtitlesClient::new`] for tests.
pub const DEFAULT_BASE_URL: &str = "https://api.opensubtitles.com/api/v1";

/// Cached OpenSubtitles user JWT, shared across requests. Logging in with an
/// account lifts the anonymous download quota; the token stays valid for
/// about a day, so it is fetched once and reused until it is rejected or the
/// stored credentials change.
#[derive(Clone, Default)]
pub struct TokenCache(Arc<tokio::sync::Mutex<Option<String>>>);

impl TokenCache {
    pub async fn get(&self) -> Option<String> {
        self.0.lock().await.clone()
    }

    pub async fn set(&self, token: Option<String>) {
        *self.0.lock().await = token;
    }
}

/// A human-friendly display name for a subtitle track from its ISO 639-1 code.
/// Covers the common cases; unknown codes fall back to the upper-cased code.
pub fn language_display_name(code: &str) -> String {
    let name = match code.to_ascii_lowercase().as_str() {
        "en" => "English",
        "de" => "German",
        "fr" => "French",
        "es" => "Spanish",
        "it" => "Italian",
        "pt" | "pt-pt" => "Portuguese",
        "pt-br" => "Portuguese (Brazil)",
        "zh-cn" => "Chinese (Simplified)",
        "zh-tw" => "Chinese (Traditional)",
        "nl" => "Dutch",
        "sv" => "Swedish",
        "no" => "Norwegian",
        "da" => "Danish",
        "fi" => "Finnish",
        "pl" => "Polish",
        "cs" => "Czech",
        "ru" => "Russian",
        "uk" => "Ukrainian",
        "ja" => "Japanese",
        "ko" => "Korean",
        "zh" => "Chinese",
        "ar" => "Arabic",
        "he" => "Hebrew",
        "tr" => "Turkish",
        "hi" => "Hindi",
        "el" => "Greek",
        "hu" => "Hungarian",
        "ro" => "Romanian",
        "th" => "Thai",
        "vi" => "Vietnamese",
        "id" => "Indonesian",
        _ => return code.to_ascii_uppercase(),
    };
    name.to_string()
}

/// Convert an already-downloaded SRT subtitle to WebVTT and attach it to
/// `session` as an HLS subtitle rendition:
///
/// 1. SRT→VTT convert, optionally rescaling cue times by `fps_scale`
///    (`media_fps / subtitle_fps`) to correct fps-mismatch drift;
/// 2. write `sub_<lang>_<n>.vtt` and its single-segment media playlist
///    `sub_<lang>_<n>.m3u8` into the session temp dir;
/// 3. record a [`SubtitleTrack`] on the session (so the master playlist route
///    starts advertising it), keeping the pristine VTT so a later manual
///    offset re-shifts from the base.
///
/// Returns the recorded track. Language is validated to an ISO code so it is
/// safe to embed in filenames matched by the session serving route.
pub async fn attach_subtitle(
    session: &Arc<Session>,
    language: &str,
    srt: &str,
    make_default: bool,
    fps_scale: Option<f64>,
) -> AppResult<SubtitleTrack> {
    let lang = normalize_language(language)?;
    let vtt = srt_to_vtt_scaled(srt, fps_scale);

    // Sequence number: 1-based per language; also the stable track key.
    let n = session.subtitle_count_for(&lang) + 1;
    let key = format!("{lang}_{n}");
    let vtt_name = format!("sub_{key}.vtt");
    let playlist_name = format!("sub_{key}.m3u8");

    let duration = session.info().duration_secs;
    let media_playlist = subtitle_media_playlist(&vtt_name, duration);

    tokio::fs::write(session.temp_dir.join(&vtt_name), vtt.as_bytes())
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("writing subtitle vtt: {e}")))?;
    tokio::fs::write(
        session.temp_dir.join(&playlist_name),
        media_playlist.as_bytes(),
    )
    .await
    .map_err(|e| AppError::Internal(anyhow::anyhow!("writing subtitle playlist: {e}")))?;

    // Only the first attached track defaults, unless explicitly requested.
    let default = make_default && session.subtitle_tracks().is_empty();
    let track = SubtitleTrack {
        language: lang.clone(),
        name: language_display_name(&lang),
        playlist_name,
        vtt_name,
        key,
        base_vtt: vtt,
        offset_ms: 0,
        default,
    };
    session.add_subtitle_track(track.clone());
    Ok(track)
}

/// Re-emit a track's WebVTT shifted by `offset_ms` (absolute, relative to the
/// pristine base VTT — not cumulative), overwriting `sub_<key>.vtt` in the
/// session temp dir. The HLS playlist URI is unchanged, so a player just
/// reloads the rendition. Returns the updated track (with `offset_ms` stored),
/// or `None` when no track with `key` exists on the session.
pub async fn set_subtitle_offset(
    session: &Arc<Session>,
    key: &str,
    offset_ms: i64,
) -> AppResult<Option<SubtitleTrack>> {
    let Some(track) = session.subtitle_track_by_key(key) else {
        return Ok(None);
    };
    let shifted = vtt::shift_vtt(&track.base_vtt, offset_ms);
    tokio::fs::write(session.temp_dir.join(&track.vtt_name), shifted.as_bytes())
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("rewriting subtitle vtt: {e}")))?;
    let updated = session.set_subtitle_offset(key, offset_ms);
    Ok(updated)
}

/// Validate + normalize a language code to lower case: a 2–3 letter ISO code
/// with an optional regional suffix (`en`, `ger`, `pt-br`, `zh-cn`, ...).
/// This is the value embedded into subtitle filenames, so it must not contain
/// anything the session serving route's filename allowlist would reject.
pub fn normalize_language(language: &str) -> AppResult<String> {
    let lang = language.trim().to_ascii_lowercase();
    let (base, region) = match lang.split_once('-') {
        Some((base, region)) => (base, Some(region)),
        None => (lang.as_str(), None),
    };
    let ok_part =
        |part: &str| (2..=4).contains(&part.len()) && part.bytes().all(|b| b.is_ascii_lowercase());
    let valid = base.len() <= 3 && ok_part(base) && region.is_none_or(ok_part);
    if valid {
        Ok(lang)
    } else {
        Err(AppError::BadRequest(format!(
            "invalid subtitle language code '{language}' (expected an ISO code like 'en' or 'pt-br')"
        )))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn language_normalization() {
        assert_eq!(normalize_language(" EN ").unwrap(), "en");
        assert_eq!(normalize_language("ger").unwrap(), "ger");
        assert_eq!(normalize_language("PT-BR").unwrap(), "pt-br");
        assert_eq!(normalize_language("zh-cn").unwrap(), "zh-cn");
        assert!(normalize_language("english").is_err());
        assert!(normalize_language("e").is_err());
        assert!(normalize_language("e1").is_err());
        assert!(normalize_language("../x").is_err());
        assert!(normalize_language("pt-").is_err());
        assert!(normalize_language("pt-brasil").is_err());
        assert!(normalize_language("-br").is_err());
    }

    #[test]
    fn known_and_unknown_language_names() {
        assert_eq!(language_display_name("en"), "English");
        assert_eq!(language_display_name("DE"), "German");
        assert_eq!(language_display_name("xx"), "XX");
    }
}
