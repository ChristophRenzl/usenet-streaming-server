//! HLS wiring for external WebVTT subtitles.
//!
//! An attached subtitle becomes a single-segment WebVTT rendition:
//!
//! - the `.vtt` holds the whole subtitle (cue times already relative to the
//!   program, since the source SRT covers the entire movie/episode);
//! - a tiny per-subtitle media playlist (`sub_<lang>_<n>.m3u8`) references that
//!   one `.vtt` as a single `#EXTINF:<duration>` segment and is closed with
//!   `#EXT-X-ENDLIST` (VOD);
//! - the master playlist gains an `#EXT-X-MEDIA:TYPE=SUBTITLES` entry per track
//!   and the video variant references `SUBTITLES="subs"` so AVPlayer offers the
//!   track natively.

/// Subtitle group id used across the master playlist.
pub const SUBTITLE_GROUP: &str = "subs";

/// When the media duration is unknown, use a target duration large enough to
/// span any realistic movie/episode.
const FALLBACK_DURATION_SECS: f64 = 86_400.0;

/// One attached subtitle rendition recorded on the session.
#[derive(Debug, Clone, PartialEq)]
pub struct SubtitleTrack {
    /// ISO 639-1 language code (`en`, `de`, ...).
    pub language: String,
    /// Human-readable track name shown in the player (`English`, ...).
    pub name: String,
    /// Media-playlist filename in the session dir (`sub_en_1.m3u8`).
    pub playlist_name: String,
    /// WebVTT filename in the session dir (`sub_en_1.vtt`).
    pub vtt_name: String,
    /// Stable per-language track key used to address this track in the offset
    /// endpoint (`en_1`, the `<lang>_<n>` slug from the filenames). Unique
    /// within a session.
    pub key: String,
    /// The pristine WebVTT (after fps rescale but before any manual offset),
    /// kept so a manual offset re-shifts from the base rather than compounding.
    pub base_vtt: String,
    /// Current cumulative manual offset in milliseconds (positive = later).
    pub offset_ms: i64,
    /// Whether this is the auto-selected default track.
    pub default: bool,
}

/// Build the single-segment WebVTT media playlist for one subtitle track.
/// `duration_secs` should be the media duration; when `None`, a large
/// target duration is used so the single segment still spans the program.
pub fn subtitle_media_playlist(vtt_name: &str, duration_secs: Option<f64>) -> String {
    let duration = duration_secs
        .filter(|d| d.is_finite() && *d > 0.0)
        .unwrap_or(FALLBACK_DURATION_SECS);
    // Target duration must be an integer >= the longest segment.
    let target = duration.ceil() as u64;
    format!(
        "#EXTM3U\n\
         #EXT-X-VERSION:7\n\
         #EXT-X-TARGETDURATION:{target}\n\
         #EXT-X-MEDIA-SEQUENCE:0\n\
         #EXT-X-PLAYLIST-TYPE:VOD\n\
         #EXTINF:{duration:.3},\n\
         {vtt_name}\n\
         #EXT-X-ENDLIST\n"
    )
}

/// Render the HLS master playlist for a session with `tracks` subtitle
/// renditions. Mirrors the inline master in `api::stream` (single video
/// variant pointing at `media.m3u8`) but adds `#EXT-X-MEDIA:TYPE=SUBTITLES`
/// rows and a `SUBTITLES="subs"` attribute on the variant when any track
/// exists.
pub fn master_playlist(tracks: &[SubtitleTrack]) -> String {
    let mut out = String::from("#EXTM3U\n#EXT-X-VERSION:7\n");

    for track in tracks {
        out.push_str(&format!(
            "#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"{group}\",NAME=\"{name}\",\
             LANGUAGE=\"{lang}\",AUTOSELECT=YES,DEFAULT={default},\
             FORCED=NO,URI=\"{uri}\"\n",
            group = SUBTITLE_GROUP,
            name = escape_attr(&track.name),
            lang = escape_attr(&track.language),
            default = if track.default { "YES" } else { "NO" },
            uri = track.playlist_name,
        ));
    }

    out.push_str("#EXT-X-STREAM-INF:BANDWIDTH=20000000");
    if !tracks.is_empty() {
        out.push_str(&format!(",SUBTITLES=\"{SUBTITLE_GROUP}\""));
    }
    out.push_str("\nmedia.m3u8\n");
    out
}

/// Escape a value for an HLS quoted-string attribute (drop the quote/CR/LF
/// characters the grammar forbids inside a quoted-string).
fn escape_attr(value: &str) -> String {
    value
        .chars()
        .filter(|&c| c != '"' && c != '\r' && c != '\n')
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn track(lang: &str, n: u32, default: bool) -> SubtitleTrack {
        SubtitleTrack {
            language: lang.into(),
            name: super::super::language_display_name(lang),
            playlist_name: format!("sub_{lang}_{n}.m3u8"),
            vtt_name: format!("sub_{lang}_{n}.vtt"),
            key: format!("{lang}_{n}"),
            base_vtt: String::new(),
            offset_ms: 0,
            default,
        }
    }

    #[test]
    fn media_playlist_is_single_segment_vod() {
        let pl = subtitle_media_playlist("sub_en_1.vtt", Some(120.5));
        assert!(pl.starts_with("#EXTM3U"));
        assert!(pl.contains("#EXT-X-TARGETDURATION:121"));
        assert!(pl.contains("#EXT-X-PLAYLIST-TYPE:VOD"));
        assert!(pl.contains("#EXTINF:120.500,\nsub_en_1.vtt\n"));
        assert!(pl.trim_end().ends_with("#EXT-X-ENDLIST"));
    }

    #[test]
    fn media_playlist_falls_back_when_duration_unknown() {
        let pl = subtitle_media_playlist("sub_en_1.vtt", None);
        assert!(pl.contains("#EXT-X-TARGETDURATION:86400"));
        assert!(pl.contains("sub_en_1.vtt"));
    }

    #[test]
    fn master_without_tracks_matches_plain_variant() {
        let master = master_playlist(&[]);
        assert!(master.contains("#EXT-X-STREAM-INF:BANDWIDTH=20000000\nmedia.m3u8\n"));
        assert!(!master.contains("SUBTITLES"));
    }

    #[test]
    fn master_with_tracks_adds_media_and_variant_attribute() {
        let master = master_playlist(&[track("en", 1, true), track("de", 1, false)]);
        assert!(master.contains(
            "#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",NAME=\"English\",LANGUAGE=\"en\""
        ));
        assert!(master.contains("DEFAULT=YES"));
        assert!(master.contains("URI=\"sub_en_1.m3u8\""));
        assert!(master.contains("LANGUAGE=\"de\""));
        assert!(master.contains("#EXT-X-STREAM-INF:BANDWIDTH=20000000,SUBTITLES=\"subs\""));
    }
}
