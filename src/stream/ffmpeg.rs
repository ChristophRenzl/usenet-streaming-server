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

/// Dolby codecs that need a licensed decoder: real Apple devices have one,
/// but the tvOS/iOS *simulator* (and some web players) do not. Clients
/// declare `supports_dolby_audio: false` to have these transcoded to AAC.
const DOLBY_AUDIO: &[&str] = &["ac3", "eac3"];

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

/// Degraded HDR→SDR for ffmpeg builds without libzimg (no `zscale`, so no
/// proper linear-light tonemap): swscale converts the BT.2020 matrix to
/// BT.709 but cannot undo the PQ/HLG transfer, so the image is playable but
/// flat/washed out. Still strictly better than failing the session — AVPlayer
/// rejects PQ/HLG outright on SDR-only outputs.
const TONEMAP_TO_SDR_FILTER_FALLBACK: &str = "scale=min(iw\\,1920):-2:flags=bilinear\
    :in_color_matrix=bt2020:out_color_matrix=bt709,format=yuv420p";

/// Whether this ffmpeg binary has a filter (cached per path — the check
/// spawns `ffmpeg -filters` once).
async fn has_filter(ffmpeg_path: &str, name: &str) -> bool {
    use std::collections::HashMap;
    use std::sync::{LazyLock, Mutex};
    static CACHE: LazyLock<Mutex<HashMap<String, bool>>> = LazyLock::new(Mutex::default);

    let key = format!("{ffmpeg_path}\x00{name}");
    if let Some(&known) = CACHE.lock().expect("filter cache lock").get(&key) {
        return known;
    }
    let listed = tokio::process::Command::new(ffmpeg_path)
        .args(["-hide_banner", "-filters"])
        .output()
        .await
        .map(|output| {
            String::from_utf8_lossy(&output.stdout)
                // Filter lines look like ` .S. zscale  V->V  ...`: flags, name.
                .lines()
                .any(|line| line.split_whitespace().nth(1) == Some(name))
        })
        .unwrap_or(false);
    CACHE.lock().expect("filter cache lock").insert(key, listed);
    listed
}

/// Whether the source audio must be transcoded to AAC. `None` (no audio
/// stream) needs no transcoding. With `dolby_ok = false`, AC3/E-AC3 are
/// transcoded too (players without a Dolby decoder).
pub fn should_transcode_audio(codec: Option<&str>, dolby_ok: bool) -> bool {
    match codec {
        None => false,
        Some(codec) => {
            let codec = codec.to_ascii_lowercase();
            !COPYABLE_AUDIO.contains(&codec.as_str())
                || (!dolby_ok && DOLBY_AUDIO.contains(&codec.as_str()))
        }
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
    /// Which audio stream (0-based, among audio streams) to serve — picked
    /// by language preference so dual-language releases don't default to
    /// the dub that happens to be muxed first.
    pub audio_stream_index: usize,
    /// Embedded text-subtitle extractions: `(global stream index, output
    /// path)`. Each becomes an extra growing-WebVTT output of the same
    /// process, so extraction costs no additional input bandwidth.
    pub subtitle_extractions: Vec<(i64, std::path::PathBuf)>,
}

/// Spawn ffmpeg writing an fMP4 event playlist into the session's temp dir
/// and start the monitor tasks (stderr capture, readiness poll, exit
/// handling). The child is stored on the session so seek/teardown can kill
/// it.
pub async fn spawn_hls(session: &Arc<Session>, options: SpawnOptions<'_>) -> AppResult<()> {
    let dir = &session.temp_dir;
    let tonemap_filter = if options.tonemap_to_sdr {
        if has_filter(options.ffmpeg_path, "zscale").await {
            TONEMAP_TO_SDR_FILTER
        } else {
            tracing::warn!(
                session = %session.id,
                "ffmpeg lacks zscale (libzimg); using degraded matrix-only HDR→SDR conversion"
            );
            TONEMAP_TO_SDR_FILTER_FALLBACK
        }
    } else {
        TONEMAP_TO_SDR_FILTER
    };
    let mut cmd = build_command(&options, dir, tonemap_filter);
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

/// The full ffmpeg invocation for [`spawn_hls`] (separate so tests can
/// inspect the argument list without spawning anything).
fn build_command(
    options: &SpawnOptions<'_>,
    dir: &Path,
    tonemap_filter: &str,
) -> tokio::process::Command {
    let mut cmd = tokio::process::Command::new(options.ffmpeg_path);
    cmd.args(["-nostdin", "-y", "-seekable", "1"]);
    if options.start_secs > 0.0 {
        cmd.arg("-ss").arg(format!("{:.3}", options.start_secs));
    }
    cmd.arg("-i").arg(options.input_url);
    cmd.args(["-map", "0:v:0"]);
    cmd.arg("-map")
        .arg(format!("0:a:{}?", options.audio_stream_index));
    if options.tonemap_to_sdr {
        cmd.args(["-vf", tonemap_filter]);
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
        // Imperfect sources (jittery timestamps, gaps where the decoder
        // skipped damaged frames) otherwise slide the re-encoded audio
        // earlier over time — progressive lip-sync drift. async=1 locks the
        // audio to its timestamps by padding silence into gaps / trimming
        // overlaps instead of packing frames back-to-back.
        cmd.args(["-af", "aresample=async=1:min_hard_comp=0.100:first_pts=0"]);
    } else {
        cmd.args(["-c:a", "copy"]);
    }
    // One file-wide shift (instead of the muxer-dependent default) so the
    // audio/video relative offset provably survives the negative-timestamp
    // fixup. NOT `make_zero`: that forces the first timestamp to exactly 0
    // even when positive, silently cancelling the `-output_ts_offset` a seek
    // restart adds below — restarted segments then carry zero-based fMP4
    // timestamps while the playlist (and the WebVTT cues, whose outputs
    // never had this flag) stay on the global timeline. AVPlayer rebases the
    // video from the playlist so the picture survives, but it anchors
    // subtitle cues to the segments' internal timestamps — every cue after a
    // resume landed minutes in the future and never displayed.
    // `make_non_negative` is the same file-wide shift but only when
    // timestamps are actually negative, so the restart offset survives.
    cmd.args(["-avoid_negative_ts", "make_non_negative"]);
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
    // Embedded text subtitles ride along as extra WebVTT outputs of the same
    // process. The `?` map suffix keeps a vanished stream from failing the
    // whole spawn; `-output_ts_offset` keeps post-seek cue times on the
    // global VOD timeline like the video segments.
    for (stream_index, path) in &options.subtitle_extractions {
        cmd.arg("-map").arg(format!("0:{stream_index}?"));
        cmd.args(["-c:s", "webvtt", "-f", "webvtt"]);
        // Subtitle text trickles out slowly; without per-packet flushing the
        // cues sit in ffmpeg's ~32K output buffer for most of the runtime and
        // the growing VTT on disk stays empty — windows then serve no cues.
        cmd.args(["-flush_packets", "1"]);
        if options.start_secs > 0.0 {
            cmd.arg("-output_ts_offset")
                .arg(format!("{:.3}", options.start_secs));
        }
        cmd.arg(path);
    }
    cmd
}

/// Flip Starting -> Ready as soon as the session is playable.
///
/// With a known duration the client plays from the synthetic VOD playlist and
/// segments are pumped on demand as ffmpeg writes them — the session is
/// usable as soon as `init.mp4` exists, which is 1-2s before the first full
/// media segment lands in ffmpeg's own playlist. Sources without a probed
/// duration are served ffmpeg's playlist directly and keep waiting for it.
async fn watch_ready(session: Arc<Session>, generation: u64) {
    let playlist = session.playlist_path();
    let init = session.temp_dir.join("init.mp4");
    let synthetic_vod = session
        .info()
        .duration_secs
        .filter(|duration| *duration > 0.0)
        .is_some();
    loop {
        if session.generation() != generation || !matches!(session.state(), SessionState::Starting)
        {
            return;
        }
        let ready = tokio::fs::try_exists(&playlist).await.unwrap_or(false)
            || (synthetic_vod && tokio::fs::try_exists(&init).await.unwrap_or(false));
        if ready {
            session.mark_ready(generation);
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
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
        assert!(!should_transcode_audio(None, true));
        assert!(!should_transcode_audio(Some("aac"), true));
        assert!(!should_transcode_audio(Some("AC3"), true));
        assert!(!should_transcode_audio(Some("eac3"), true));
        assert!(should_transcode_audio(Some("dts"), true));
        assert!(should_transcode_audio(Some("truehd"), true));
        assert!(should_transcode_audio(Some("flac"), true));
        assert!(should_transcode_audio(Some("mp3"), true));
        // No Dolby decoder: AC3/E-AC3 get transcoded, AAC still copies.
        assert!(should_transcode_audio(Some("ac3"), false));
        assert!(should_transcode_audio(Some("EAC3"), false));
        assert!(!should_transcode_audio(Some("aac"), false));
        assert!(!should_transcode_audio(None, false));
    }

    fn args(options: &SpawnOptions<'_>) -> Vec<String> {
        build_command(options, Path::new("/tmp/session"), TONEMAP_TO_SDR_FILTER)
            .as_std()
            .get_args()
            .map(|a| a.to_string_lossy().into_owned())
            .collect()
    }

    fn base_options(transcode_audio: bool) -> SpawnOptions<'static> {
        SpawnOptions {
            ffmpeg_path: "ffmpeg",
            input_url: "http://127.0.0.1/vfs",
            start_secs: 0.0,
            transcode_audio,
            video_codec: Some("h264"),
            tonemap_to_sdr: false,
            audio_stream_index: 0,
            subtitle_extractions: Vec::new(),
        }
    }

    #[test]
    fn subtitle_extractions_become_webvtt_side_outputs() {
        let mut options = base_options(false);
        options.subtitle_extractions = vec![(
            2,
            std::path::PathBuf::from("/tmp/session/sub_emb_en_f000000.vtt"),
        )];
        let args = args(&options);
        assert!(has_pair(&args, "-map", "0:2?"));
        assert!(has_pair(&args, "-c:s", "webvtt"));
        assert!(has_pair(&args, "-flush_packets", "1"));
        assert!(args
            .last()
            .is_some_and(|a| a.ends_with("sub_emb_en_f000000.vtt")));
    }

    /// Two consecutive arguments (a flag and its value).
    fn has_pair(args: &[String], flag: &str, value: &str) -> bool {
        args.windows(2).any(|w| w[0] == flag && w[1] == value)
    }

    #[test]
    fn audio_transcode_locks_audio_to_timestamps() {
        let args = args(&SpawnOptions {
            transcode_audio: true,
            ..base_options(true)
        });
        assert!(has_pair(
            &args,
            "-af",
            "aresample=async=1:min_hard_comp=0.100:first_pts=0"
        ));
    }

    #[test]
    fn audio_copy_has_no_audio_filter() {
        let args = args(&base_options(false));
        assert!(!args.iter().any(|a| a == "-af"));
        assert!(has_pair(&args, "-c:a", "copy"));
    }

    #[test]
    fn negative_timestamp_fixup_is_one_shared_shift() {
        // make_non_negative, NOT make_zero — make_zero would force restarted
        // spawns back to timestamp 0, cancelling -output_ts_offset and
        // desyncing WebVTT cues from the video timeline after a resume.
        for transcode_audio in [false, true] {
            let args = args(&base_options(transcode_audio));
            assert!(has_pair(&args, "-avoid_negative_ts", "make_non_negative"));
        }
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
