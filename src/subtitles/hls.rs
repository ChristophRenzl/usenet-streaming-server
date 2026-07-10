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
    /// Automatic alignment against the embedded track's release-accurate cue
    /// timing: `None` = not attempted yet, `Some(ms)` = attempted (0 when the
    /// track was already in sync or no confident estimate existed). Applied
    /// on top of the manual offset when the VTT is emitted.
    pub auto_offset_ms: Option<i64>,
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

/// Window length for embedded-subtitle renditions. Embedded subs are
/// extracted *while* the media streams, so their VTT grows over time — a
/// single-segment rendition (like external subs use) would be fetched once
/// at start and miss every later cue. Short windows make the player fetch
/// cues lazily near the playhead, by which point extraction has long passed.
pub const EMBEDDED_WINDOW_SECS: f64 = 60.0;

/// Window-file name for one embedded-subtitle window (`sub_emb_en_w0004.vtt`).
pub fn embedded_window_name(language: &str, window: u64) -> String {
    format!("sub_emb_{language}_w{window:04}.vtt")
}

/// Media playlist for an embedded-subtitle rendition: fixed-length windows
/// covering the whole program, listed upfront (mirroring the video playlist's
/// list-everything style). Window contents are sliced from the growing VTT at
/// request time.
pub fn embedded_subtitle_playlist(language: &str, duration_secs: Option<f64>) -> String {
    let duration = duration_secs
        .filter(|d| d.is_finite() && *d > 0.0)
        .unwrap_or(FALLBACK_DURATION_SECS);
    let mut out = format!(
        "#EXTM3U\n\
         #EXT-X-VERSION:7\n\
         #EXT-X-TARGETDURATION:{target}\n\
         #EXT-X-MEDIA-SEQUENCE:0\n\
         #EXT-X-PLAYLIST-TYPE:VOD\n",
        target = EMBEDDED_WINDOW_SECS as u64,
    );
    let windows = (duration / EMBEDDED_WINDOW_SECS).ceil().max(1.0) as u64;
    for window in 0..windows {
        let remaining = duration - window as f64 * EMBEDDED_WINDOW_SECS;
        let length = remaining.min(EMBEDDED_WINDOW_SECS);
        out.push_str(&format!(
            "#EXTINF:{length:.3},\n{name}\n",
            name = embedded_window_name(language, window),
        ));
    }
    out.push_str("#EXT-X-ENDLIST\n");
    out
}

/// Variant-level facts for the single `#EXT-X-STREAM-INF` row.
#[derive(Debug, Clone, Default)]
pub struct MasterVariant {
    /// Peak bandwidth in bits/s; `None` falls back to a generous default.
    pub bandwidth_bps: Option<i64>,
    /// RFC 6381 `CODECS` value; omitted when unknown.
    pub codecs: Option<String>,
    /// Served video dimensions for `RESOLUTION`; omitted when unknown.
    pub resolution: Option<(i64, i64)>,
    /// `SDR`, `PQ` or `HLG`.
    pub video_range: String,
}

/// `BANDWIDTH` fallback when the probe reported no bitrate. Generous on
/// purpose: an underestimate makes AVPlayer buffer too little for remuxes.
const DEFAULT_BANDWIDTH_BPS: i64 = 20_000_000;

/// Render the HLS master playlist for a session with `tracks` subtitle
/// renditions. Mirrors the inline master in `api::stream` (single video
/// variant pointing at `media.m3u8`) but adds `#EXT-X-MEDIA:TYPE=SUBTITLES`
/// rows and a `SUBTITLES="subs"` attribute on the variant when any track
/// exists.
pub fn master_playlist(tracks: &[SubtitleTrack], variant: &MasterVariant) -> String {
    let mut out = String::from("#EXTM3U\n#EXT-X-VERSION:7\n");

    // External provider tracks are listed first and carry the DEFAULT flag:
    // they are single full-file VTTs that every player times correctly from
    // any resume position. The windowed embedded renditions misrender after
    // deep resumes on AVPlayer (tvOS/iOS) regardless of X-TIMESTAMP-MAP
    // form, so they stay selectable but are never auto-picked. Sessions
    // without an external track keep the embedded default as a fallback.
    let mut ordered: Vec<&SubtitleTrack> = tracks
        .iter()
        .filter(|t| !t.key.starts_with("emb_"))
        .collect();
    let externals = ordered.len();
    ordered.extend(tracks.iter().filter(|t| t.key.starts_with("emb_")));

    for (index, track) in ordered.iter().enumerate() {
        let default = if externals > 0 {
            index == 0
        } else {
            track.default
        };
        out.push_str(&format!(
            "#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"{group}\",NAME=\"{name}\",\
             LANGUAGE=\"{lang}\",AUTOSELECT=YES,DEFAULT={default},\
             FORCED=NO,URI=\"{uri}\"\n",
            group = SUBTITLE_GROUP,
            name = escape_attr(&track.name),
            lang = escape_attr(&track.language),
            default = if default { "YES" } else { "NO" },
            uri = track.playlist_name,
        ));
    }

    out.push_str(&format!(
        "#EXT-X-STREAM-INF:BANDWIDTH={}",
        variant.bandwidth_bps.unwrap_or(DEFAULT_BANDWIDTH_BPS)
    ));
    // CODECS matters beyond bookkeeping: AVPlayer refuses PQ/HLG variants it
    // cannot validate decoder support for, so HDR streams fail with
    // "unsupported URL" (-1002) when the attribute is missing.
    if let Some(codecs) = &variant.codecs {
        out.push_str(&format!(",CODECS=\"{}\"", escape_attr(codecs)));
    }
    if let Some((w, h)) = variant.resolution {
        out.push_str(&format!(",RESOLUTION={w}x{h}"));
    }
    // VIDEO-RANGE is required: AVPlayer assumes SDR when it is absent and then
    // rejects the stream once the format description turns out to be PQ/HLG.
    out.push_str(&format!(",VIDEO-RANGE={}", variant.video_range));
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
            auto_offset_ms: None,
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

    fn variant(video_range: &str) -> MasterVariant {
        MasterVariant {
            video_range: video_range.to_string(),
            ..MasterVariant::default()
        }
    }

    #[test]
    fn master_without_tracks_matches_plain_variant() {
        let master = master_playlist(&[], &variant("SDR"));
        assert!(
            master.contains("#EXT-X-STREAM-INF:BANDWIDTH=20000000,VIDEO-RANGE=SDR\nmedia.m3u8\n")
        );
        assert!(!master.contains("SUBTITLES"));
    }

    #[test]
    fn master_with_tracks_adds_media_and_variant_attribute() {
        let master = master_playlist(
            &[track("en", 1, true), track("de", 1, false)],
            &variant("PQ"),
        );
        assert!(master.contains(
            "#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"subs\",NAME=\"English\",LANGUAGE=\"en\""
        ));
        assert!(master.contains("DEFAULT=YES"));
        assert!(master.contains("URI=\"sub_en_1.m3u8\""));
        assert!(master.contains("LANGUAGE=\"de\""));
        assert!(master.contains("VIDEO-RANGE=PQ"));
        assert!(master
            .contains("#EXT-X-STREAM-INF:BANDWIDTH=20000000,VIDEO-RANGE=PQ,SUBTITLES=\"subs\""));
    }

    #[test]
    fn embedded_playlist_lists_all_windows_upfront() {
        let playlist = embedded_subtitle_playlist("en", Some(150.0));
        assert!(playlist.contains("#EXT-X-TARGETDURATION:60"));
        assert!(playlist.contains("#EXTINF:60.000,\nsub_emb_en_w0000.vtt"));
        assert!(playlist.contains("#EXTINF:60.000,\nsub_emb_en_w0001.vtt"));
        // Final partial window carries the remainder.
        assert!(playlist.contains("#EXTINF:30.000,\nsub_emb_en_w0002.vtt"));
        assert!(!playlist.contains("w0003"));
        assert!(playlist.trim_end().ends_with("#EXT-X-ENDLIST"));
    }

    #[test]
    fn master_declares_codecs_resolution_and_measured_bandwidth() {
        let master = master_playlist(
            &[],
            &MasterVariant {
                bandwidth_bps: Some(80_000_000),
                codecs: Some("hvc1.2.4.L153.B0,mp4a.40.2".to_string()),
                resolution: Some((3840, 2160)),
                video_range: "PQ".to_string(),
            },
        );
        assert!(master.contains(
            "#EXT-X-STREAM-INF:BANDWIDTH=80000000,\
             CODECS=\"hvc1.2.4.L153.B0,mp4a.40.2\",\
             RESOLUTION=3840x2160,VIDEO-RANGE=PQ\nmedia.m3u8\n"
        ));
    }
}
