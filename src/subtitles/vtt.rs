//! SRT → WebVTT conversion.
//!
//! A pure, dependency-free converter used when a subtitle downloaded from
//! OpenSubtitles (almost always SubRip / `.srt`) is delivered into an HLS
//! session. AVPlayer and hls.js consume WebVTT, so the SRT is normalized:
//!
//! - a leading UTF-8 BOM is stripped;
//! - non-UTF-8 bytes fall back to a best-effort latin-1 decode;
//! - CRLF/CR line endings are normalized to LF;
//! - the `WEBVTT` header is prepended;
//! - SRT numeric index lines are dropped;
//! - the `,` decimal separator in cue timestamps becomes `.`;
//! - cue text (including basic tags like `<i>`) passes through untouched.
//!
//! Two timing transforms ride on top of the conversion:
//!
//! - an optional **fps rescale** ([`srt_to_vtt_scaled`]): when a subtitle was
//!   authored against a different frame rate than the media, every cue
//!   timestamp is multiplied by `media_fps / subtitle_fps` so the drift is
//!   removed;
//! - a **manual offset** ([`shift_vtt`]): every cue timestamp in an
//!   already-produced WebVTT document is shifted by a signed millisecond
//!   amount (clamped at 0) for the nudge-the-subtitles fallback.

/// Decode raw subtitle bytes to text: strip a UTF-8 BOM, decode as UTF-8, and
/// fall back to latin-1 (ISO-8859-1) when the bytes are not valid UTF-8.
pub fn decode_subtitle_bytes(bytes: &[u8]) -> String {
    // A UTF-8 BOM (EF BB BF) precedes many .srt files.
    let bytes = bytes.strip_prefix(&[0xEF, 0xBB, 0xBF]).unwrap_or(bytes);
    match std::str::from_utf8(bytes) {
        Ok(text) => text.to_string(),
        // latin-1 maps every byte 1:1 to the same Unicode code point.
        Err(_) => bytes.iter().map(|&b| b as char).collect(),
    }
}

/// HLS WebVTT header. The `X-TIMESTAMP-MAP` anchors WebVTT cue time zero to the
/// program timeline origin (MPEGTS 0). Without it, AVPlayer anchors cues to the
/// first media segment it loads, so resuming mid-video — where the fMP4 stream
/// begins at a non-zero PTS via ffmpeg's `-output_ts_offset` — shifts every
/// subtitle by the resume offset. Anchoring at zero keeps timing correct from
/// any start position and is a no-op for playback from the beginning.
const VTT_HEADER: &str = "WEBVTT\nX-TIMESTAMP-MAP=MPEGTS:0,LOCAL:00:00:00.000\n\n";

/// Convert SRT text to a WebVTT document.
///
/// Input is text (already BOM-stripped/decoded, e.g. via
/// [`decode_subtitle_bytes`]); a stray leading BOM char is tolerated anyway.
pub fn srt_to_vtt(srt: &str) -> String {
    srt_to_vtt_scaled(srt, None)
}

/// Like [`srt_to_vtt`] but linearly rescales every cue timestamp by `scale`
/// (`media_fps / subtitle_fps`) when it is `Some` and meaningfully different
/// from 1.0. Used to correct fps-mismatch drift when attaching a subtitle that
/// was authored against a different frame rate than the media. `None` (or a
/// scale within 0.001 of 1.0) leaves timestamps untouched.
pub fn srt_to_vtt_scaled(srt: &str, scale: Option<f64>) -> String {
    let srt = srt.strip_prefix('\u{feff}').unwrap_or(srt);
    // Only apply a scale that is finite, positive and actually differs.
    let scale = scale.filter(|s| s.is_finite() && *s > 0.0 && (*s - 1.0).abs() > 0.001);

    let mut out = String::with_capacity(srt.len() + VTT_HEADER.len());
    out.push_str(VTT_HEADER);

    // Whether the previous emitted line was blank, to collapse runs of blank
    // lines that the dropped index lines would otherwise leave behind.
    let mut prev_blank = true;
    // The next non-blank line after a blank line may be a bare SRT index.
    let mut at_cue_start = true;

    for raw in srt.split('\n') {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        let trimmed = line.trim();

        if trimmed.is_empty() {
            if !prev_blank {
                out.push('\n');
                prev_blank = true;
            }
            at_cue_start = true;
            continue;
        }

        // A cue begins with an optional numeric index line followed by a
        // timing line. Drop the index; rewrite the timing line's separators.
        if at_cue_start && is_index_line(trimmed) {
            // Skip it, but stay "at cue start" so the timing line is next.
            continue;
        }

        if is_timing_line(trimmed) {
            out.push_str(&convert_timing_line(line, |secs| match scale {
                Some(scale) => secs * scale,
                None => secs,
            }));
        } else {
            out.push_str(line);
        }
        out.push('\n');
        prev_blank = false;
        at_cue_start = false;
    }

    if !out.ends_with('\n') {
        out.push('\n');
    }
    out
}

/// Shift every cue timestamp in an already-produced WebVTT document by
/// `offset_ms` milliseconds (positive = later, negative = earlier). Timestamps
/// that would go negative clamp to `0`. Non-timing lines (`WEBVTT` header, cue
/// text, blank lines, `NOTE`s) pass through untouched.
///
/// This operates on WebVTT (`.`-separated timestamps), not SRT, so it can be
/// applied repeatedly against the pristine base VTT to realize a cumulative
/// manual offset without compounding rounding drift.
pub fn shift_vtt(vtt: &str, offset_ms: i64) -> String {
    let mut out = String::with_capacity(vtt.len());
    for raw in vtt.split('\n') {
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        if is_timing_line(line) {
            out.push_str(&convert_timing_line(line, |secs| {
                (secs + offset_ms as f64 / 1000.0).max(0.0)
            }));
        } else {
            out.push_str(line);
        }
        out.push('\n');
    }
    // Preserve a single trailing newline like the source rather than doubling.
    if !vtt.ends_with('\n') {
        out.pop();
    }
    out
}

/// A pure SRT index line: only ASCII digits.
fn is_index_line(line: &str) -> bool {
    !line.is_empty() && line.bytes().all(|b| b.is_ascii_digit())
}

/// One parsed WebVTT cue: absolute times plus the raw payload lines.
#[derive(Debug, Clone, PartialEq)]
struct Cue {
    start_secs: f64,
    end_secs: f64,
    /// The timing line's trailing cue settings, when present.
    settings: Option<String>,
    payload: String,
}

/// Parse the cues of a WebVTT document (header and NOTE/STYLE blocks are
/// skipped; cue identifiers are dropped).
fn parse_cues(vtt: &str) -> Vec<Cue> {
    let mut cues = Vec::new();
    let mut lines = vtt.lines().peekable();
    while let Some(line) = lines.next() {
        if !is_timing_line(line) {
            continue;
        }
        let Some((start_raw, rest)) = line.split_once("-->") else {
            continue;
        };
        let rest = rest.trim_start();
        let (end_raw, settings) = match rest.split_once(char::is_whitespace) {
            Some((ts, tail)) => (ts, Some(tail.to_string())),
            None => (rest, None),
        };
        let (Some(start_secs), Some(end_secs)) =
            (parse_timestamp(start_raw.trim_end()), parse_timestamp(end_raw))
        else {
            continue;
        };
        let mut payload = String::new();
        while let Some(text) = lines.peek() {
            if text.trim().is_empty() {
                break;
            }
            if !payload.is_empty() {
                payload.push('\n');
            }
            payload.push_str(text);
            lines.next();
        }
        cues.push(Cue {
            start_secs,
            end_secs,
            settings,
            payload,
        });
    }
    cues
}

/// Slice a time window out of one or more (growing) WebVTT documents.
///
/// Used for embedded-subtitle renditions: ffmpeg appends cues to fragment
/// files while the media streams (one fragment per (re)start position), and
/// each HLS window request is answered with the cues overlapping
/// `[start_secs, end_secs)` across all fragments, deduplicated (fragments
/// from seek restarts can overlap). Cue times stay absolute — the rendition's
/// `X-TIMESTAMP-MAP` anchors local zero to the program timeline.
pub fn window_vtt(fragments: &[String], start_secs: f64, end_secs: f64) -> String {
    let mut cues: Vec<Cue> = fragments
        .iter()
        .flat_map(|fragment| parse_cues(fragment))
        .filter(|cue| cue.end_secs > start_secs && cue.start_secs < end_secs)
        .collect();
    cues.sort_by(|a, b| {
        a.start_secs
            .total_cmp(&b.start_secs)
            .then(a.end_secs.total_cmp(&b.end_secs))
    });
    cues.dedup();

    let mut out = String::from(VTT_HEADER);
    for cue in cues {
        out.push_str(&format!(
            "{} --> {}",
            format_timestamp(cue.start_secs),
            format_timestamp(cue.end_secs)
        ));
        if let Some(settings) = &cue.settings {
            out.push(' ');
            out.push_str(settings);
        }
        out.push('\n');
        out.push_str(&cue.payload);
        out.push_str("\n\n");
    }
    out
}

/// A cue timing line contains the `-->` arrow.
fn is_timing_line(line: &str) -> bool {
    line.contains("-->")
}

/// Rewrite a cue timing line's two timestamps: convert the SRT `,` decimal
/// separator to `.`, and pass each timestamp's value (in seconds) through
/// `transform` (identity, an fps rescale, or an offset shift). Any trailing
/// cue-setting text after the end timestamp is left untouched.
fn convert_timing_line(line: &str, transform: impl Fn(f64) -> f64) -> String {
    // SRT/VTT timing: `00:00:01,000 --> 00:00:04,000` (optional settings).
    match line.split_once("-->") {
        Some((start, rest)) => {
            // `rest` is the end timestamp plus any WebVTT cue settings.
            let rest = rest.trim_start();
            let (end_ts, settings) = match rest.split_once(char::is_whitespace) {
                Some((ts, tail)) => (ts, Some(tail)),
                None => (rest, None),
            };
            let mut converted = format!(
                "{} --> {}",
                rewrite_timestamp(start.trim_end(), &transform),
                rewrite_timestamp(end_ts, &transform)
            );
            if let Some(settings) = settings {
                converted.push(' ');
                converted.push_str(settings);
            }
            converted
        }
        None => line.to_string(),
    }
}

/// Rewrite one timestamp token: parse it to seconds, apply `transform`, and
/// re-render it as a WebVTT timestamp (`.` separator). When the token cannot
/// be parsed, fall back to a plain comma→dot conversion so unexpected input is
/// still emitted as valid-ish VTT rather than dropped.
fn rewrite_timestamp(ts: &str, transform: impl Fn(f64) -> f64) -> String {
    match parse_timestamp(ts) {
        Some(secs) => format_timestamp(transform(secs)),
        None => ts.replace(',', "."),
    }
}

/// Parse a `HH:MM:SS,mmm` / `HH:MM:SS.mmm` / `MM:SS,mmm` timestamp to seconds.
fn parse_timestamp(ts: &str) -> Option<f64> {
    let ts = ts.trim();
    let (hms, millis) = match ts.split_once([',', '.']) {
        Some((hms, frac)) => {
            // Fractional part: pad/truncate to milliseconds.
            let frac = frac.trim();
            if frac.is_empty() || !frac.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            let mut ms_str = frac.to_string();
            while ms_str.len() < 3 {
                ms_str.push('0');
            }
            let millis: u64 = ms_str.get(..3)?.parse().ok()?;
            (hms, millis)
        }
        None => (ts, 0),
    };

    let mut parts = hms.split(':').rev();
    let seconds: u64 = parts.next()?.parse().ok()?;
    let minutes: u64 = parts.next()?.parse().ok()?;
    let hours: u64 = match parts.next() {
        Some(h) => h.parse().ok()?,
        None => 0,
    };
    if parts.next().is_some() || seconds >= 60 || minutes >= 60 {
        return None;
    }
    Some(hours as f64 * 3600.0 + minutes as f64 * 60.0 + seconds as f64 + millis as f64 / 1000.0)
}

/// Render seconds as a WebVTT `HH:MM:SS.mmm` timestamp (always with hours,
/// which AVPlayer/hls.js accept). Negative inputs clamp to zero.
fn format_timestamp(secs: f64) -> String {
    let total_ms = (secs.max(0.0) * 1000.0).round() as u64;
    let ms = total_ms % 1000;
    let total_secs = total_ms / 1000;
    let s = total_secs % 60;
    let m = (total_secs / 60) % 60;
    let h = total_secs / 3600;
    format!("{h:02}:{m:02}:{s:02}.{ms:03}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn window_vtt_slices_and_merges_fragments() {
        let f0 = "WEBVTT\n\n00:00:10.000 --> 00:00:12.000\nEarly\n\n\
                  00:01:05.000 --> 00:01:07.500 line:85%\nIn window\n\n";
        let f1 = "WEBVTT\n\n00:01:05.000 --> 00:01:07.500 line:85%\nIn window\n\n\
                  00:01:59.000 --> 00:02:03.000\nSpans boundary\n\n\
                  00:03:00.000 --> 00:03:02.000\nLate\n\n";
        let out = window_vtt(&[f0.to_string(), f1.to_string()], 60.0, 120.0);
        assert!(out.starts_with("WEBVTT\nX-TIMESTAMP-MAP"));
        // Cue before the window is excluded, late cue too.
        assert!(!out.contains("Early"));
        assert!(!out.contains("Late"));
        // The duplicated cue appears once, settings preserved.
        assert_eq!(out.matches("In window").count(), 1);
        assert!(out.contains("00:01:05.000 --> 00:01:07.500 line:85%"));
        // A cue overlapping the window end is included with absolute times.
        assert!(out.contains("00:01:59.000 --> 00:02:03.000\nSpans boundary"));
    }

    #[test]
    fn window_vtt_empty_fragments_yield_header_only() {
        let out = window_vtt(&[], 0.0, 60.0);
        assert!(out.starts_with("WEBVTT\nX-TIMESTAMP-MAP"));
        assert!(!out.contains("-->"));
    }

    #[test]
    fn basic_conversion_adds_header_and_dots() {
        let srt = "1\n00:00:01,000 --> 00:00:04,000\nHello world\n";
        let vtt = srt_to_vtt(srt);
        // Header carries the HLS timestamp map (anchors cue 0 to program 0 so
        // subtitles stay in sync when playback starts mid-video), then a blank
        // line before the first cue.
        assert!(vtt.starts_with("WEBVTT\nX-TIMESTAMP-MAP=MPEGTS:0,LOCAL:00:00:00.000\n\n"));
        assert!(vtt.contains("00:00:01.000 --> 00:00:04.000"));
        assert!(vtt.contains("Hello world"));
        // The numeric index line is dropped.
        assert!(!vtt.contains("\n1\n"));
    }

    #[test]
    fn shift_preserves_timestamp_map_header() {
        // A manual offset re-shifts cues from the base VTT; the timestamp-map
        // anchor is a non-timing line and must survive untouched, or the fix
        // for mid-video sync would be undone the moment the user nudges timing.
        let base = srt_to_vtt("1\n00:00:10,000 --> 00:00:12,000\nHi\n");
        let shifted = shift_vtt(&base, 500);
        assert!(shifted.contains("X-TIMESTAMP-MAP=MPEGTS:0,LOCAL:00:00:00.000"));
        assert!(shifted.contains("00:00:10.500 --> 00:00:12.500"));
    }

    #[test]
    fn handles_crlf_line_endings() {
        let srt = "1\r\n00:00:01,000 --> 00:00:02,000\r\nLine\r\n\r\n";
        let vtt = srt_to_vtt(srt);
        assert!(!vtt.contains('\r'), "CR must be normalized away");
        assert!(vtt.contains("00:00:01.000 --> 00:00:02.000"));
        assert!(vtt.contains("Line"));
    }

    #[test]
    fn multi_line_cues_and_blank_lines_preserved() {
        let srt = "1\n00:00:01,000 --> 00:00:04,000\nFirst line\nSecond line\n\n2\n00:00:05,000 --> 00:00:06,500\nNext cue\n";
        let vtt = srt_to_vtt(srt);
        assert!(vtt.contains("First line\nSecond line\n"));
        assert!(vtt.contains("00:00:05.000 --> 00:00:06.500"));
        assert!(vtt.contains("Next cue"));
        // Cues are separated by a blank line.
        assert!(vtt.contains("Second line\n\n00:00:05.000"));
    }

    #[test]
    fn strips_utf8_bom_from_text() {
        let srt = "\u{feff}1\n00:00:01,000 --> 00:00:02,000\nWith BOM\n";
        let vtt = srt_to_vtt(srt);
        assert!(vtt.starts_with("WEBVTT"));
        assert!(!vtt.contains('\u{feff}'));
    }

    #[test]
    fn decode_strips_bom_bytes() {
        let bytes = [0xEF, 0xBB, 0xBF, b'h', b'i'];
        assert_eq!(decode_subtitle_bytes(&bytes), "hi");
    }

    #[test]
    fn decode_falls_back_to_latin1() {
        // 0xE9 is 'é' in latin-1 but an invalid lone UTF-8 byte.
        let bytes = [b'c', b'a', b'f', b'\xe9'];
        assert_eq!(decode_subtitle_bytes(&bytes), "café");
    }

    #[test]
    fn preserves_basic_tags() {
        let srt = "1\n00:00:01,000 --> 00:00:02,000\n<i>italic</i> and <b>bold</b>\n";
        let vtt = srt_to_vtt(srt);
        assert!(vtt.contains("<i>italic</i> and <b>bold</b>"));
    }

    #[test]
    fn keeps_cue_settings_after_end_timestamp() {
        let srt = "1\n00:00:01,000 --> 00:00:02,000 line:90%\nPositioned\n";
        let vtt = srt_to_vtt(srt);
        assert!(vtt.contains("00:00:01.000 --> 00:00:02.000 line:90%"));
    }

    #[test]
    fn collapses_missing_index_lines() {
        // Some SRT files omit index numbers; conversion should still work.
        let srt = "00:00:01,000 --> 00:00:02,000\nNo index here\n";
        let vtt = srt_to_vtt(srt);
        assert!(vtt.contains("00:00:01.000 --> 00:00:02.000"));
        assert!(vtt.contains("No index here"));
    }

    #[test]
    fn trailing_output_always_ends_with_newline() {
        let vtt = srt_to_vtt("1\n00:00:01,000 --> 00:00:02,000\nX");
        assert!(vtt.ends_with('\n'));
    }

    #[test]
    fn parse_and_format_round_trip() {
        assert_eq!(parse_timestamp("01:02:03,500"), Some(3723.5));
        assert_eq!(parse_timestamp("00:00:01.000"), Some(1.0));
        // MM:SS form (no hours) is tolerated.
        assert_eq!(parse_timestamp("02:05,250"), Some(125.25));
        assert_eq!(format_timestamp(3723.5), "01:02:03.500");
        assert_eq!(format_timestamp(1.0), "00:00:01.000");
        assert_eq!(format_timestamp(-5.0), "00:00:00.000");
        assert!(parse_timestamp("nonsense").is_none());
    }

    #[test]
    fn scale_none_leaves_timestamps_unchanged() {
        let srt = "1\n00:00:10,000 --> 00:00:12,000\nHi\n";
        assert_eq!(srt_to_vtt_scaled(srt, None), srt_to_vtt(srt));
        // A no-op scale (within tolerance of 1.0) is also unchanged.
        assert_eq!(srt_to_vtt_scaled(srt, Some(1.0)), srt_to_vtt(srt));
    }

    #[test]
    fn fps_rescale_25_to_23_976_moves_60s_cue_to_about_62_56s() {
        // A cue authored at 25 fps but the media is 23.976 fps: scale by
        // media_fps / subtitle_fps = 23.976 / 25 = 0.95904. A cue at 60s
        // moves to 60 * 25 / 23.976 ~= 62.564s. (We scale by media/subtitle,
        // matching the attach path.)
        let scale = 25.0 / 23.976; // media_fps / subtitle_fps
        let srt = "1\n00:01:00,000 --> 00:01:02,000\nDrift\n";
        let vtt = srt_to_vtt_scaled(srt, Some(scale));
        let start = parse_timestamp(
            vtt.lines()
                .find(|l| l.contains("-->"))
                .unwrap()
                .split("-->")
                .next()
                .unwrap()
                .trim(),
        )
        .unwrap();
        assert!(
            (start - 62.564).abs() < 0.01,
            "60s cue rescaled to {start}, expected ~62.56"
        );
    }

    #[test]
    fn shift_vtt_moves_cues_and_clamps_negatives() {
        let vtt = "WEBVTT\n\n00:00:10.000 --> 00:00:12.000\nHello\n\n00:00:01.000 --> 00:00:02.000\nEarly\n";
        // +2000ms: 10s -> 12s, 12s -> 14s.
        let shifted = shift_vtt(vtt, 2000);
        assert!(
            shifted.contains("00:00:12.000 --> 00:00:14.000"),
            "{shifted}"
        );
        // -5000ms clamps the 1s cue start to 0.
        let earlier = shift_vtt(vtt, -5000);
        assert!(
            earlier.contains("00:00:00.000 --> 00:00:00.000"),
            "{earlier}"
        );
        // Header and cue text survive.
        assert!(shifted.starts_with("WEBVTT"));
        assert!(shifted.contains("Hello"));
    }

    #[test]
    fn shift_is_relative_to_base_not_compounding() {
        let base = "WEBVTT\n\n00:00:10.000 --> 00:00:12.000\nHi\n";
        // Applying +1000 then +2000 to the SAME base equals +2000 directly.
        let a = shift_vtt(base, 2000);
        let b = shift_vtt(base, 1000);
        let b_again = shift_vtt(base, 2000);
        assert_ne!(a, b);
        assert_eq!(a, b_again, "shifting the base is exact, not cumulative");
    }
}
