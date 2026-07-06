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
}

#[derive(Debug, Deserialize)]
struct ProbeDoc {
    format: Option<ProbeFormat>,
    #[serde(default)]
    streams: Vec<ProbeStream>,
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

    Ok(ProbeResult {
        duration_secs,
        video_codec,
        audio_codec,
        audio_channels,
        fps,
        video_range,
    })
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
    }
}
