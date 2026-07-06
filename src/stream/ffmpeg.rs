//! ffmpeg HLS remuxing: spawn, stderr capture, readiness detection and
//! VOD finalization.

use std::path::Path;
use std::process::Stdio;
use std::sync::Arc;
use std::time::Duration;

use tokio::io::{AsyncBufReadExt, BufReader};

use crate::error::{AppError, AppResult};

use super::session::{Session, SessionState};

/// Audio codecs HLS.js/AVPlayer play back without transcoding.
const COPYABLE_AUDIO: &[&str] = &["aac", "ac3", "eac3"];

/// Nominal HLS segment length. The synthetic VOD playlist, ffmpeg's
/// `-hls_time`, the forced keyframe cadence and the segment-index ↔ time
/// mapping must all agree on this.
pub const SEGMENT_SECONDS: f64 = 6.0;

/// HDR→SDR for clients that cannot render PQ/HLG. Downscale first (cheap)
/// so the linear-light tonemap runs at 1080p, not 4K — that is the
/// difference between ~0.7x and ~1.4x realtime on a 10-core box.
const TONEMAP_TO_SDR_FILTER: &str = "scale=min(iw\\,1920):-2:flags=bilinear,\
    zscale=t=linear:npl=100,tonemap=hable:desat=0,\
    zscale=p=bt709:t=bt709:m=bt709:r=tv,format=yuv420p";

/// Whether the source audio must be transcoded to AAC. `None` (no audio
/// stream) needs no transcoding.
pub fn should_transcode_audio(codec: Option<&str>) -> bool {
    match codec {
        None => false,
        Some(codec) => !COPYABLE_AUDIO.contains(&codec.to_ascii_lowercase().as_str()),
    }
}

/// Everything `spawn_hls` needs besides the session itself.
pub struct SpawnOptions<'a> {
    pub ffmpeg_path: &'a str,
    /// Loopback URL of the session's virtual file.
    pub input_url: &'a str,
    /// `-ss` input seek; 0 for a fresh start.
    pub start_secs: f64,
    pub transcode_audio: bool,
    /// Probed source video codec; decides container-level fixups like the
    /// HEVC `hvc1` tag.
    pub video_codec: Option<&'a str>,
    /// Tone-map HDR video to 1080p SDR H.264 instead of copying.
    pub tonemap_to_sdr: bool,
}

/// Spawn ffmpeg writing an fMP4 event playlist into the session's temp dir
/// and start the monitor tasks (stderr capture, readiness poll, exit
/// handling). The child is stored on the session so seek/teardown can kill
/// it.
pub async fn spawn_hls(session: &Arc<Session>, options: SpawnOptions<'_>) -> AppResult<()> {
    let dir = &session.temp_dir;
    let mut cmd = tokio::process::Command::new(options.ffmpeg_path);
    cmd.args(["-nostdin", "-y", "-seekable", "1"]);
    if options.start_secs > 0.0 {
        cmd.arg("-ss").arg(format!("{:.3}", options.start_secs));
    }
    cmd.arg("-i").arg(options.input_url);
    cmd.args(["-map", "0:v:0", "-map", "0:a:0?"]);
    if options.tonemap_to_sdr {
        cmd.args(["-vf", TONEMAP_TO_SDR_FILTER]);
        cmd.args(["-c:v", "libx264", "-preset", "veryfast", "-crf", "21"]);
        cmd.args(["-maxrate", "12M", "-bufsize", "24M"]);
        // x264 keyframes must land on the segment cadence or the hls muxer
        // cannot split.
        cmd.args(["-force_key_frames", "expr:gte(t,n_forced*6)"]);
    } else {
        cmd.args(["-c:v", "copy"]);
        // AVPlayer only decodes HEVC when the fMP4 sample entry is `hvc1`
        // AND its hvcC box carries the VPS/SPS/PPS parameter sets. Web-DL
        // MKVs often ship them in-band only (23-byte parameter-less
        // extradata), which VideoToolbox rejects (unimpErr -4, black screen
        // with audio). Round-tripping through annex-B makes the mp4 muxer
        // rebuild hvcC from the in-band parameter sets.
        if matches!(options.video_codec, Some(c) if c.eq_ignore_ascii_case("hevc")) {
            cmd.args(["-tag:v", "hvc1", "-bsf:v", "hevc_mp4toannexb"]);
        }
    }
    if options.transcode_audio {
        cmd.args(["-c:a", "aac", "-b:a", "192k", "-ac", "2"]);
    } else {
        cmd.args(["-c:a", "copy"]);
    }
    cmd.args([
        "-f",
        "hls",
        "-hls_segment_type",
        "fmp4",
        "-hls_time",
        "6",
        "-hls_playlist_type",
        "event",
        "-hls_fmp4_init_filename",
        "init.mp4",
    ]);
    if options.start_secs > 0.0 {
        // Keep segment numbering and fMP4 timestamps on the global VOD
        // timeline so a restarted ffmpeg slots into the same playlist.
        let start_number = (options.start_secs / SEGMENT_SECONDS).round() as u64;
        cmd.arg("-start_number").arg(start_number.to_string());
        cmd.arg("-output_ts_offset")
            .arg(format!("{:.3}", options.start_secs));
    }
    cmd.arg("-hls_segment_filename");
    cmd.arg(dir.join("seg_%05d.m4s"));
    cmd.arg(dir.join("media.m3u8"));
    cmd.stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .kill_on_drop(true);

    let mut child = cmd.spawn().map_err(|e| {
        AppError::Internal(anyhow::anyhow!(
            "spawning ffmpeg ({}): {e}",
            options.ffmpeg_path
        ))
    })?;
    let stderr = child.stderr.take().expect("ffmpeg stderr piped");

    let generation = session.generation();
    *session.child.lock().await = Some(child);

    tokio::spawn(watch_ready(session.clone(), generation));
    tokio::spawn(monitor(session.clone(), generation, stderr));
    Ok(())
}

/// Flip Starting -> Ready as soon as the media playlist appears.
async fn watch_ready(session: Arc<Session>, generation: u64) {
    let playlist = session.playlist_path();
    loop {
        if session.generation() != generation || !matches!(session.state(), SessionState::Starting)
        {
            return;
        }
        if tokio::fs::try_exists(&playlist).await.unwrap_or(false) {
            session.mark_ready(generation);
            return;
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
}

/// Drain stderr (kept as a bounded tail for error reporting), then reap the
/// child on natural exit: success finalizes the playlist as VOD and marks
/// the session Ended, failure records the stderr tail.
async fn monitor(session: Arc<Session>, generation: u64, stderr: tokio::process::ChildStderr) {
    let mut lines = BufReader::new(stderr).lines();
    while let Ok(Some(line)) = lines.next_line().await {
        tracing::debug!(session = %session.id, "ffmpeg: {line}");
        session.push_stderr(line);
    }

    // stderr EOF: the process is exiting. Take the child unless a seek or
    // teardown already claimed it (they kill + reap themselves).
    let child = {
        let mut slot = session.child.lock().await;
        if session.generation() == generation {
            slot.take()
        } else {
            None
        }
    };
    let Some(mut child) = child else { return };

    match child.wait().await {
        Ok(status) if status.success() => {
            // `-hls_playlist_type event` appends EXT-X-ENDLIST on a clean
            // finish; make sure it is there so players treat this as VOD.
            if let Err(e) = ensure_endlist(&session.playlist_path()).await {
                tracing::warn!(session = %session.id, error = %e, "finalizing playlist");
            }
            session.finish(generation, Ok(()));
            tracing::info!(session = %session.id, "ffmpeg finished");
        }
        Ok(status) => {
            let tail = session.stderr_tail(10);
            session.finish(
                generation,
                Err(format!("ffmpeg exited with {status}: {tail}")),
            );
            tracing::warn!(session = %session.id, %status, "ffmpeg failed");
        }
        Err(e) => {
            session.finish(generation, Err(format!("waiting for ffmpeg: {e}")));
        }
    }
}

/// Append `#EXT-X-ENDLIST` to the playlist when missing.
async fn ensure_endlist(playlist: &Path) -> std::io::Result<()> {
    let text = tokio::fs::read_to_string(playlist).await?;
    if text.contains("#EXT-X-ENDLIST") {
        return Ok(());
    }
    let mut amended = text;
    if !amended.ends_with('\n') {
        amended.push('\n');
    }
    amended.push_str("#EXT-X-ENDLIST\n");
    tokio::fs::write(playlist, amended).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn audio_transcode_decision() {
        assert!(!should_transcode_audio(None));
        assert!(!should_transcode_audio(Some("aac")));
        assert!(!should_transcode_audio(Some("AC3")));
        assert!(!should_transcode_audio(Some("eac3")));
        assert!(should_transcode_audio(Some("dts")));
        assert!(should_transcode_audio(Some("truehd")));
        assert!(should_transcode_audio(Some("flac")));
        assert!(should_transcode_audio(Some("mp3")));
    }

    #[tokio::test]
    async fn endlist_is_appended_once() {
        let dir = tempfile::tempdir().unwrap();
        let playlist = dir.path().join("media.m3u8");
        tokio::fs::write(&playlist, "#EXTM3U\n#EXTINF:6.0,\nseg_00000.m4s")
            .await
            .unwrap();
        ensure_endlist(&playlist).await.unwrap();
        ensure_endlist(&playlist).await.unwrap();
        let text = tokio::fs::read_to_string(&playlist).await.unwrap();
        assert_eq!(text.matches("#EXT-X-ENDLIST").count(), 1);
        assert!(text.ends_with("#EXT-X-ENDLIST\n"));
    }
}
