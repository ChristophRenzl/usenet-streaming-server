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
    color_transfer: Option<String>,
}

/// Run `ffprobe -v quiet -print_format json -show_format -show_streams`
/// against `url` with a 20s timeout. Fails when no video stream is found
/// (nothing we could remux).
pub async fn probe_url(ffprobe_path: &str, url: &str) -> AppResult<ProbeResult> {
    let child = tokio::process::Command::new(ffprobe_path)
        .args([
            "-v",
            "quiet",
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
        ])
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
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
        return Err(AppError::Upstream(format!(
            "ffprobe exited with {} (is the media readable?)",
            output.status
        )));
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
    let mut color_transfer = None;
    for stream in &doc.streams {
        match stream.codec_type.as_deref() {
            Some("video") if !has_video => {
                has_video = true;
                video_codec = stream.codec_name.clone();
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
                    {"codec_type": "video", "codec_name": "h264"},
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
        // No color transfer reported → SDR.
        assert_eq!(result.video_range, "SDR");
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
