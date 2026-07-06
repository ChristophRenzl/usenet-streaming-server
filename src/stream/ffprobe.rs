//! Media probing via `ffprobe` against the internal loopback URL.

use std::process::Stdio;
use std::time::Duration;

use serde::Deserialize;

use crate::error::{AppError, AppResult};

const PROBE_TIMEOUT: Duration = Duration::from_secs(20);

/// The subset of ffprobe output the streaming layer cares about.
#[derive(Debug, Clone, Default)]
pub struct ProbeResult {
    pub duration_secs: Option<f64>,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    pub audio_channels: Option<i64>,
    /// Frames per second of the first video stream, when reported. Used to
    /// correct fps-mismatch drift when attaching external subtitles.
    pub fps: Option<f64>,
    /// HLS `VIDEO-RANGE` value derived from the color transfer: `PQ`
    /// (HDR10/DV), `HLG`, or `SDR`.
    pub video_range: String,
    /// Embedded chapter markers, in file order. Empty for the vast majority of
    /// releases (most have no chapters). Powers the client's "Skip Intro".
    pub chapters: Vec<Chapter>,
    /// End time (seconds) of the detected intro/opening chapter, when the
    /// release has one titled like an intro that starts within the first few
    /// minutes. `None` when there is no such chapter (the client then falls
    /// back to its own heuristic).
    pub intro_end_secs: Option<f64>,
}

/// One embedded chapter marker.
#[derive(Debug, Clone, PartialEq)]
pub struct Chapter {
    pub start_secs: f64,
    pub end_secs: f64,
    pub title: Option<String>,
}

/// A detected intro chapter must start no later than this into the file, so a
/// mid-episode chapter merely named "OP reprise" is not mistaken for the
/// opening.
const INTRO_MAX_START_SECS: f64 = 300.0;

#[derive(Debug, Deserialize)]
struct ProbeDoc {
    format: Option<ProbeFormat>,
    #[serde(default)]
    streams: Vec<ProbeStream>,
    #[serde(default)]
    chapters: Vec<ProbeChapter>,
}

#[derive(Debug, Deserialize)]
struct ProbeChapter {
    /// Chapter start/end are seconds as strings (`start_time`, `end_time`).
    start_time: Option<String>,
    end_time: Option<String>,
    #[serde(default)]
    tags: ProbeChapterTags,
}

#[derive(Debug, Default, Deserialize)]
struct ProbeChapterTags {
    title: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ProbeFormat {
    duration: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ProbeStream {
    codec_type: Option<String>,
    codec_name: Option<String>,
    channels: Option<i64>,
    /// Rational frame rate `num/den`, e.g. `24000/1001`. `avg_frame_rate` is
    /// preferred (real average); `r_frame_rate` is the fallback base rate.
    avg_frame_rate: Option<String>,
    r_frame_rate: Option<String>,
    color_transfer: Option<String>,
}

/// Parse an ffprobe rational frame rate (`num/den`, e.g. `24000/1001`) to fps.
/// Returns `None` for missing, zero (`0/0`) or unparseable values.
fn parse_frame_rate(rate: Option<&str>) -> Option<f64> {
    let rate = rate?.trim();
    let (num, den) = rate.split_once('/')?;
    let num: f64 = num.trim().parse().ok()?;
    let den: f64 = den.trim().parse().ok()?;
    if den == 0.0 || num == 0.0 {
        return None;
    }
    let fps = num / den;
    fps.is_finite().then_some(fps)
}

/// Run `ffprobe -v error -print_format json -show_format -show_streams`
/// against `url` with a 20s timeout. On failure the error carries ffprobe's
/// own stderr so the reason is actionable. Fails when no video stream is
/// found (nothing we could remux).
pub async fn probe_url(ffprobe_path: &str, url: &str) -> AppResult<ProbeResult> {
    let child = tokio::process::Command::new(ffprobe_path)
        // `-v error` (not `quiet`) so ffprobe writes the real failure reason to
        // stderr — captured below — while the JSON still goes to stdout. This
        // turns the opaque "is the media readable?" into an actionable message
        // (e.g. "Invalid data found" vs "Server returned 5XX" for a missing
        // article vs "moov atom not found").
        .args([
            "-v",
            "error",
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
            // Chapter markers power the client's "Skip Intro"; harmless (empty)
            // for the many releases without embedded chapters.
            "-show_chapters",
        ])
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| {
            AppError::Internal(anyhow::anyhow!("spawning ffprobe ({ffprobe_path}): {e}"))
        })?;

    let output = tokio::time::timeout(PROBE_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| {
            AppError::Upstream(format!(
                "ffprobe timed out after {}s",
                PROBE_TIMEOUT.as_secs()
            ))
        })?
        .map_err(|e| AppError::Internal(anyhow::anyhow!("waiting for ffprobe: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.trim();
        // Keep the tail (the last line is usually the actionable one) bounded.
        let detail: String = detail
            .chars()
            .rev()
            .take(400)
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        return Err(AppError::Upstream(if detail.is_empty() {
            format!(
                "ffprobe exited with {} (is the media readable?)",
                output.status
            )
        } else {
            format!("ffprobe exited with {}: {detail}", output.status)
        }));
    }

    let doc: ProbeDoc = serde_json::from_slice(&output.stdout)
        .map_err(|e| AppError::Upstream(format!("unparseable ffprobe output: {e}")))?;
    parse_probe(doc)
}

fn parse_probe(doc: ProbeDoc) -> AppResult<ProbeResult> {
    let duration_secs = doc
        .format
        .and_then(|f| f.duration)
        .and_then(|d| d.parse::<f64>().ok());

    let mut has_video = false;
    let mut video_codec = None;
    let mut audio_codec = None;
    let mut audio_channels = None;
    let mut fps = None;
    let mut color_transfer = None;
    for stream in &doc.streams {
        match stream.codec_type.as_deref() {
            Some("video") if !has_video => {
                has_video = true;
                video_codec = stream.codec_name.clone();
                fps = parse_frame_rate(stream.avg_frame_rate.as_deref())
                    .or_else(|| parse_frame_rate(stream.r_frame_rate.as_deref()));
                color_transfer = stream.color_transfer.clone();
            }
            Some("audio") if audio_codec.is_none() => {
                audio_codec = stream.codec_name.clone();
                audio_channels = stream.channels;
            }
            _ => {}
        }
    }

    if !has_video {
        return Err(AppError::Upstream(
            "media contains no video stream".to_string(),
        ));
    }

    let video_range = match color_transfer.as_deref() {
        Some("smpte2084") => "PQ",
        Some("arib-std-b67") => "HLG",
        _ => "SDR",
    }
    .to_string();

    let chapters: Vec<Chapter> = doc
        .chapters
        .into_iter()
        .map(|c| Chapter {
            start_secs: c.start_time.and_then(|s| s.parse().ok()).unwrap_or(0.0),
            end_secs: c.end_time.and_then(|s| s.parse().ok()).unwrap_or(0.0),
            title: c.tags.title,
        })
        .collect();
    let intro_end_secs = detect_intro_end(&chapters);

    Ok(ProbeResult {
        duration_secs,
        video_codec,
        audio_codec,
        audio_channels,
        fps,
        video_range,
        chapters,
        intro_end_secs,
    })
}

/// Find the end of the intro/opening among the chapters: the end time of the
/// first chapter whose title looks like an intro (`intro`, `opening`, `op`, or
/// `avant`, case-insensitively) and that starts within the first
/// [`INTRO_MAX_START_SECS`]. `None` when no chapter qualifies.
fn detect_intro_end(chapters: &[Chapter]) -> Option<f64> {
    chapters
        .iter()
        .find(|c| c.start_secs <= INTRO_MAX_START_SECS && title_is_intro(c.title.as_deref()))
        .map(|c| c.end_secs)
}

/// Whether a chapter title names an intro/opening. Matches whole words so a
/// title like "Recap" is not caught by the substring "op".
fn title_is_intro(title: Option<&str>) -> bool {
    let Some(title) = title else { return false };
    let lower = title.to_ascii_lowercase();
    lower
        .split(|c: char| !c.is_alphanumeric())
        .any(|word| matches!(word, "intro" | "opening" | "op" | "avant"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_typical_probe_output() {
        let doc: ProbeDoc = serde_json::from_str(
            r#"{
                "streams": [
                    {"codec_type": "video", "codec_name": "h264", "avg_frame_rate": "24000/1001"},
                    {"codec_type": "audio", "codec_name": "ac3", "channels": 6},
                    {"codec_type": "audio", "codec_name": "aac", "channels": 2}
                ],
                "format": {"duration": "5400.123"}
            }"#,
        )
        .expect("parse");
        let result = parse_probe(doc).expect("probe");
        assert_eq!(result.duration_secs, Some(5400.123));
        assert_eq!(result.video_codec.as_deref(), Some("h264"));
        // First audio stream wins.
        assert_eq!(result.audio_codec.as_deref(), Some("ac3"));
        assert_eq!(result.audio_channels, Some(6));
        // 24000/1001 ~= 23.976 fps.
        assert!((result.fps.unwrap() - 23.976).abs() < 0.001);
        // No color transfer reported → SDR.
        assert_eq!(result.video_range, "SDR");
    }

    #[test]
    fn frame_rate_parsing_handles_rationals_and_junk() {
        assert!((parse_frame_rate(Some("25/1")).unwrap() - 25.0).abs() < 1e-9);
        assert!((parse_frame_rate(Some("24000/1001")).unwrap() - 23.976).abs() < 0.001);
        // Zero numerator/denominator and junk yield None.
        assert_eq!(parse_frame_rate(Some("0/0")), None);
        assert_eq!(parse_frame_rate(Some("30/0")), None);
        assert_eq!(parse_frame_rate(Some("nonsense")), None);
        assert_eq!(parse_frame_rate(None), None);
    }

    #[test]
    fn falls_back_to_r_frame_rate_when_avg_missing() {
        let doc: ProbeDoc = serde_json::from_str(
            r#"{"streams": [{"codec_type": "video", "codec_name": "h264", "r_frame_rate": "25/1"}]}"#,
        )
        .expect("parse");
        let result = parse_probe(doc).expect("probe");
        assert!((result.fps.unwrap() - 25.0).abs() < 1e-9);
    }

    #[test]
    fn color_transfer_maps_to_video_range() {
        for (transfer, expected) in [
            ("smpte2084", "PQ"),
            ("arib-std-b67", "HLG"),
            ("bt709", "SDR"),
        ] {
            let doc: ProbeDoc = serde_json::from_str(&format!(
                r#"{{"streams": [{{"codec_type": "video", "codec_name": "hevc", "color_transfer": "{transfer}"}}], "format": {{}}}}"#,
            ))
            .expect("parse");
            assert_eq!(parse_probe(doc).expect("probe").video_range, expected);
        }
    }

    #[test]
    fn missing_video_stream_is_an_error() {
        let doc: ProbeDoc = serde_json::from_str(
            r#"{"streams": [{"codec_type": "audio", "codec_name": "mp3"}], "format": {}}"#,
        )
        .expect("parse");
        assert!(parse_probe(doc).is_err());
    }

    #[test]
    fn tolerates_missing_fields() {
        let doc: ProbeDoc =
            serde_json::from_str(r#"{"streams": [{"codec_type": "video"}]}"#).expect("parse");
        let result = parse_probe(doc).expect("probe");
        assert_eq!(result.duration_secs, None);
        assert_eq!(result.video_codec, None);
        assert_eq!(result.audio_codec, None);
        // No chapters element → empty, and no intro marker.
        assert!(result.chapters.is_empty());
        assert_eq!(result.intro_end_secs, None);
    }

    #[test]
    fn parses_chapters_and_detects_intro() {
        let doc: ProbeDoc = serde_json::from_str(
            r#"{
                "streams": [{"codec_type": "video", "codec_name": "hevc"}],
                "format": {"duration": "1440.0"},
                "chapters": [
                    {"start_time": "0.000", "end_time": "90.000", "tags": {"title": "Intro"}},
                    {"start_time": "90.000", "end_time": "1440.000", "tags": {"title": "Episode"}}
                ]
            }"#,
        )
        .expect("parse");
        let result = parse_probe(doc).expect("probe");
        assert_eq!(result.chapters.len(), 2);
        assert_eq!(result.chapters[0].start_secs, 0.0);
        assert_eq!(result.chapters[0].end_secs, 90.0);
        assert_eq!(result.chapters[0].title.as_deref(), Some("Intro"));
        // The intro chapter's end time is the intro end.
        assert_eq!(result.intro_end_secs, Some(90.0));
    }

    #[test]
    fn no_intro_chapter_leaves_intro_end_none() {
        let doc: ProbeDoc = serde_json::from_str(
            r#"{
                "streams": [{"codec_type": "video", "codec_name": "h264"}],
                "chapters": [
                    {"start_time": "0.0", "end_time": "600.0", "tags": {"title": "Chapter 1"}},
                    {"start_time": "600.0", "end_time": "1200.0", "tags": {"title": "Chapter 2"}}
                ]
            }"#,
        )
        .expect("parse");
        let result = parse_probe(doc).expect("probe");
        assert_eq!(result.chapters.len(), 2);
        assert_eq!(result.intro_end_secs, None);
    }

    #[test]
    fn intro_detection_matches_common_titles_and_respects_start_window() {
        // Whole-word intro synonyms match.
        for title in ["Intro", "OPENING", "op", "Avant"] {
            assert!(
                title_is_intro(Some(title)),
                "'{title}' should be an intro title"
            );
        }
        // "Recap" must not match on the "op"/"cap" substring.
        assert!(!title_is_intro(Some("Recap")));
        assert!(!title_is_intro(Some("Episode")));
        assert!(!title_is_intro(None));

        // An intro-titled chapter that starts too late is not the opening.
        let late = vec![Chapter {
            start_secs: INTRO_MAX_START_SECS + 60.0,
            end_secs: INTRO_MAX_START_SECS + 120.0,
            title: Some("Opening".into()),
        }];
        assert_eq!(detect_intro_end(&late), None);
    }
}
