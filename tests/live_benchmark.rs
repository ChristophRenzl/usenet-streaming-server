//! Live streaming benchmark against a REAL indexer, TMDB and Usenet provider.
//!
//! Measures everything that happens between "the user presses play" and "the
//! first frame can render", stage by stage, plus seek/scrub latency and
//! stability — the two complaints this suite exists to quantify:
//!
//!   1. Pressing play takes long: the session response now carries a
//!      server-side stage breakdown (`timings`), and this test adds the
//!      client-observable stages on top (ready poll, playlists, init.mp4,
//!      first media segments) the way AVPlayer fetches them.
//!   2. Skipping back/forth sometimes takes very long or gets stuck: the
//!      seek script fires out-of-window segment requests exactly like
//!      AVPlayer does on a scrub (ascending burst, previous loads cancelled)
//!      and measures time-to-first-media-byte and time-to-complete per seek,
//!      flagging stalls.
//!
//! The movie under test is "Obsession" (2026, TMDB 1339713) in three release
//! variants: 2160p HDR10 + Atmos (stream-copied for an HDR client), the same
//! release tone-mapped to SDR (HDR-incapable client), and a plain 1080p SDR
//! non-Atmos release.
//!
//! Opt-in and configured like tests/live_e2e.rs: fill in the gitignored
//! `tests/live_settings.toml` (see live_settings.example.toml) including the
//! `[provider]` section, then:
//!
//!   cargo test --test live_benchmark -- --ignored --nocapture --test-threads=1
//!
//! Each test prints a timing table and writes a JSON report next to it in
//! `target/live-benchmark/`. Assertions are deliberately generous ceilings —
//! the printed numbers are the benchmark; the assertions catch "stuck".

use std::time::{Duration, Instant};

use figment::{
    providers::{Env, Format, Toml},
    Figment,
};
use futures::StreamExt;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use usenet_streaming_server::{api, config::AppConfig, state::AppState};

const API_KEY: &str = "live-bench-key";
/// TMDB id of "Obsession" (2026-05-13).
const DEFAULT_MOVIE_TMDB_ID: i64 = 1_339_713;
/// Must match `ffmpeg::SEGMENT_SECONDS` (segment index ↔ time mapping).
const SEGMENT_SECONDS: f64 = 6.0;
/// The 8-byte keepalive box `pump_segment` trickles while ffmpeg works; it is
/// not media data and must not count as the first byte.
const FREE_BOX: [u8; 8] = [0, 0, 0, 8, b'f', b'r', b'e', b'e'];
/// A seek whose first media byte takes longer than this is reported STALLED.
const STALL_AFTER: Duration = Duration::from_secs(30);
/// Hard per-request cap (the server's own segment deadline is 90s).
const FETCH_TIMEOUT: Duration = Duration::from_secs(100);

// ---- Live settings ------------------------------------------------------------

#[derive(Debug, Deserialize)]
struct LiveSettings {
    tmdb: TmdbSettings,
    indexer: IndexerSettings,
    provider: Option<ProviderSettings>,
    #[serde(default)]
    benchmark: BenchmarkSettings,
}

#[derive(Debug, Deserialize)]
struct TmdbSettings {
    api_key: String,
}

#[derive(Debug, Deserialize)]
struct IndexerSettings {
    #[serde(default = "default_indexer_name")]
    name: String,
    base_url: String,
    api_key: String,
}

fn default_indexer_name() -> String {
    "live-indexer".into()
}

#[derive(Debug, Deserialize)]
struct ProviderSettings {
    host: String,
    #[serde(default = "default_nntp_port")]
    port: u16,
    #[serde(default = "default_true")]
    use_tls: bool,
    username: Option<String>,
    password: Option<String>,
    #[serde(default = "default_max_connections")]
    max_connections: i64,
}

fn default_nntp_port() -> u16 {
    563
}

fn default_true() -> bool {
    true
}

fn default_max_connections() -> i64 {
    10
}

#[derive(Debug, Deserialize)]
struct BenchmarkSettings {
    /// Movie under test; defaults to "Obsession" (2026).
    movie_tmdb_id: i64,
}

impl Default for BenchmarkSettings {
    fn default() -> Self {
        Self {
            movie_tmdb_id: DEFAULT_MOVIE_TMDB_ID,
        }
    }
}

const SETTINGS_FILE: &str = concat!(env!("CARGO_MANIFEST_DIR"), "/tests/live_settings.toml");

impl LiveSettings {
    fn load() -> Option<Self> {
        let file_exists = std::path::Path::new(SETTINGS_FILE).exists();
        let env_set = std::env::vars().any(|(k, _)| k.starts_with("LIVE_E2E__"));
        if !file_exists && !env_set {
            eprintln!(
                "SKIP: live settings not configured. Copy tests/live_settings.example.toml \
                 to tests/live_settings.toml (or set LIVE_E2E__* env vars)."
            );
            return None;
        }
        let settings: LiveSettings = Figment::new()
            .merge(Toml::file(SETTINGS_FILE))
            .merge(Env::prefixed("LIVE_E2E__").split("__"))
            .extract()
            .expect("invalid live settings (tests/live_settings.toml / LIVE_E2E__* env)");
        Some(settings)
    }
}

fn ffmpeg_available() -> bool {
    let works = |bin: &str| {
        std::process::Command::new(bin)
            .arg("-version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    };
    works("ffmpeg") && works("ffprobe")
}

// ---- Harness ------------------------------------------------------------------

struct Bench {
    http: reqwest::Client,
    base: String,
    movie_tmdb_id: i64,
}

/// Boot the app on a real socket (ffprobe/ffmpeg read the media back through
/// the loopback VFS route) and configure TMDB + indexer + provider through
/// the public API, exactly as a user would.
async fn bench_app() -> Option<Bench> {
    let settings = LiveSettings::load()?;
    let Some(provider) = &settings.provider else {
        eprintln!("SKIP: the benchmark streams real bytes; add a [provider] section.");
        return None;
    };
    if !ffmpeg_available() {
        eprintln!("SKIP: ffmpeg/ffprobe not on PATH.");
        return None;
    }

    // Surface server-side warnings (indexer failures, ffmpeg restarts, ...)
    // when the runner sets RUST_LOG.
    let _ = tracing_subscriber::fmt()
        .with_env_filter(tracing_subscriber::EnvFilter::from_default_env())
        .try_init();

    let mut config = AppConfig::default();
    config.auth.api_key = API_KEY.into();
    let state = AppState::for_tests(config).await.expect("test state");
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind");
    let base = format!("http://{}", listener.local_addr().expect("addr"));
    let state = state.with_loopback_base(&base);
    let router = api::router(state);
    tokio::spawn(async move {
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<std::net::SocketAddr>(),
        )
        .await
        .expect("serve");
    });

    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(600))
        .build()
        .expect("http client");
    let bench = Bench {
        http,
        base,
        movie_tmdb_id: settings.benchmark.movie_tmdb_id,
    };

    let r = bench
        .http
        .put(bench.url("/settings/app"))
        .header("x-api-key", API_KEY)
        .json(&json!({ "tmdb_api_key": settings.tmdb.api_key }))
        .send()
        .await
        .expect("configure tmdb");
    assert!(r.status().is_success(), "configuring TMDB key failed");
    let r = bench
        .http
        .post(bench.url("/settings/indexers"))
        .header("x-api-key", API_KEY)
        .json(&json!({
            "name": settings.indexer.name,
            "base_url": settings.indexer.base_url,
            "api_key": settings.indexer.api_key,
        }))
        .send()
        .await
        .expect("configure indexer");
    assert!(r.status().is_success(), "configuring indexer failed");
    let r = bench
        .http
        .post(bench.url("/settings/providers"))
        .header("x-api-key", API_KEY)
        .json(&json!({
            "name": "live-provider",
            "host": provider.host,
            "port": provider.port,
            "use_tls": provider.use_tls,
            "username": provider.username,
            "password": provider.password,
            "max_connections": provider.max_connections,
        }))
        .send()
        .await
        .expect("configure provider");
    assert!(r.status().is_success(), "configuring provider failed");

    Some(bench)
}

impl Bench {
    fn url(&self, path: &str) -> String {
        format!("{}/api/v1{path}", self.base)
    }

    async fn get_json(&self, path: &str) -> Value {
        let response = self
            .http
            .get(self.url(path))
            .header("x-api-key", API_KEY)
            .send()
            .await
            .expect("GET");
        assert!(
            response.status().is_success(),
            "GET {path} failed: {}",
            response.status()
        );
        response.json().await.expect("json body")
    }

    /// Ranked candidates for the benchmark movie.
    async fn movie_candidates(&self) -> Vec<Value> {
        let body = self
            .get_json(&format!(
                "/releases?tmdb_id={}&type=movie",
                self.movie_tmdb_id
            ))
            .await;
        body["candidates"].as_array().cloned().unwrap_or_default()
    }
}

// ---- Release variant selection ---------------------------------------------------

fn title(candidate: &Value) -> &str {
    candidate["raw"]["title"].as_str().unwrap_or("")
}

fn is_atmos(candidate: &Value) -> bool {
    title(candidate).to_lowercase().contains("atmos")
}

fn resolution(candidate: &Value) -> &str {
    candidate["parsed"]["resolution"].as_str().unwrap_or("")
}

fn is_hdr(candidate: &Value) -> bool {
    candidate["parsed"]["hdr"] == json!(true)
}

fn is_dv(candidate: &Value) -> bool {
    candidate["parsed"]["dolby_vision"] == json!(true)
}

/// Best-ranked candidate matching `want`; accepted ones first, rejected as a
/// fallback (a guid pin overrides rejection server-side).
fn pick<'a>(candidates: &'a [Value], want: &dyn Fn(&Value) -> bool) -> Option<&'a Value> {
    candidates
        .iter()
        .filter(|c| c["rejected"].is_null())
        .find(|c| want(c))
        .or_else(|| candidates.iter().find(|c| want(c)))
}

fn pick_2160p_hdr_atmos(candidates: &[Value]) -> Option<&Value> {
    pick(candidates, &|c| {
        resolution(c) == "2160p" && is_hdr(c) && is_atmos(c)
    })
}

fn pick_1080p_sdr_plain(candidates: &[Value]) -> Option<&Value> {
    pick(candidates, &|c| {
        resolution(c) == "1080p" && !is_hdr(c) && !is_dv(c) && !is_atmos(c)
    })
}

// ---- Measurement primitives -------------------------------------------------------

#[derive(Debug, Serialize)]
struct SegmentFetch {
    /// Time until the first NON-keepalive byte arrived.
    first_media_byte_ms: Option<u64>,
    /// Time until the segment stream ended (segment complete).
    complete_ms: u64,
    media_bytes: u64,
    error: Option<String>,
}

/// GET one HLS file and stream the body, separating `free`-box keepalives
/// from real media bytes.
async fn fetch_segment(http: &reqwest::Client, url: &str) -> SegmentFetch {
    let started = Instant::now();
    let mut fetch = SegmentFetch {
        first_media_byte_ms: None,
        complete_ms: 0,
        media_bytes: 0,
        error: None,
    };
    let done = |mut fetch: SegmentFetch, error: Option<String>| {
        fetch.complete_ms = started.elapsed().as_millis() as u64;
        fetch.error = error;
        fetch
    };

    let response = match http.get(url).header("x-api-key", API_KEY).send().await {
        Ok(response) if response.status().is_success() => response,
        Ok(response) => return done(fetch, Some(format!("HTTP {}", response.status()))),
        Err(error) => return done(fetch, Some(error.to_string())),
    };
    let mut stream = response.bytes_stream();
    // Leading keepalives: skip whole `free` boxes until real bytes appear. A
    // carry holds a keepalive split across chunk boundaries (< 8 bytes).
    let mut carry: Vec<u8> = Vec::new();
    let mut seen_media = false;
    loop {
        let chunk = match tokio::time::timeout(FETCH_TIMEOUT, stream.next()).await {
            Ok(Some(Ok(chunk))) => chunk,
            Ok(Some(Err(error))) => return done(fetch, Some(error.to_string())),
            Ok(None) => break,
            Err(_) => return done(fetch, Some("fetch timeout".into())),
        };
        if seen_media {
            fetch.media_bytes += chunk.len() as u64;
            continue;
        }
        carry.extend_from_slice(&chunk);
        let mut head = 0;
        while carry.len() - head >= 8 && carry[head..head + 8] == FREE_BOX {
            head += 8;
        }
        let rest = &carry[head..];
        // A short tail that prefixes another free box may just be a split
        // keepalive — wait for the next chunk to disambiguate.
        let partial_free = rest.len() < 8 && rest == &FREE_BOX[..rest.len()];
        if !rest.is_empty() && !partial_free {
            // Real media bytes start here.
            seen_media = true;
            fetch.first_media_byte_ms = Some(started.elapsed().as_millis() as u64);
            fetch.media_bytes += rest.len() as u64;
            carry.clear();
        } else {
            carry.drain(..head);
        }
    }
    done(fetch, None)
}

// ---- Reports ------------------------------------------------------------------------

#[derive(Debug, Serialize)]
struct SeekReport {
    label: String,
    target_secs: f64,
    segment_index: u64,
    first_media_byte_ms: Option<u64>,
    complete_ms: u64,
    stalled: bool,
    error: Option<String>,
}

#[derive(Debug, Default, Serialize)]
struct ScenarioReport {
    scenario: String,
    release: String,
    session: Value,
    stages: Vec<(String, u64)>,
    estimated_first_frame_ms: u64,
    seeks: Vec<SeekReport>,
}

impl ScenarioReport {
    fn print(&self) {
        eprintln!("\n================================================================");
        eprintln!("SCENARIO {}", self.scenario);
        eprintln!("release   {}", self.release);
        eprintln!(
            "media     container={} video={} (transcoded={}) audio={} (transcoded={}) duration={}s source={}",
            self.session["container"].as_str().unwrap_or("?"),
            self.session["video_codec"].as_str().unwrap_or("?"),
            self.session["video_transcoded"],
            self.session["audio_codec"].as_str().unwrap_or("?"),
            self.session["audio_transcoded"],
            self.session["duration_secs"].as_f64().unwrap_or(0.0).round(),
            self.session["source"].as_str().unwrap_or("?"),
        );
        eprintln!("----------------------------------------------------------------");
        eprintln!("{:<38} {:>8}", "stage", "ms");
        for (stage, ms) in &self.stages {
            eprintln!("{stage:<38} {ms:>8}");
        }
        eprintln!(
            "{:<38} {:>8}  ({:.1}s)",
            "ESTIMATED PRESS-PLAY → FIRST FRAME",
            self.estimated_first_frame_ms,
            self.estimated_first_frame_ms as f64 / 1000.0
        );
        if !self.seeks.is_empty() {
            eprintln!("----------------------------------------------------------------");
            eprintln!(
                "{:<24} {:>8} {:>6} {:>12} {:>10}  verdict",
                "seek", "target", "seg", "first_byte", "complete"
            );
            for seek in &self.seeks {
                eprintln!(
                    "{:<24} {:>7.0}s {:>6} {:>10}ms {:>8}ms  {}",
                    seek.label,
                    seek.target_secs,
                    seek.segment_index,
                    seek.first_media_byte_ms
                        .map(|v| v.to_string())
                        .unwrap_or_else(|| "-".into()),
                    seek.complete_ms,
                    match (&seek.error, seek.stalled) {
                        (Some(error), _) => format!("ERROR: {error}"),
                        (None, true) => "STALLED".into(),
                        (None, false) => "ok".into(),
                    }
                );
            }
        }
        eprintln!("================================================================\n");
    }

    fn write_json(&self) {
        let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/live-benchmark");
        let _ = std::fs::create_dir_all(&dir);
        let path = dir.join(format!("{}.json", self.scenario));
        if let Ok(text) = serde_json::to_string_pretty(self) {
            let _ = std::fs::write(&path, text);
            eprintln!("report written to {}", path.display());
        }
    }
}

// ---- Scenario runner ------------------------------------------------------------------

struct LiveSession {
    id: String,
    duration_secs: f64,
}

/// Create a session for one pinned release variant and measure the full
/// AVPlayer-shaped startup path. Returns the live session for seek testing.
async fn run_startup(
    bench: &Bench,
    report: &mut ScenarioReport,
    release_guid: &str,
    max_resolution: &str,
    supports_hdr: bool,
) -> LiveSession {
    // Press play.
    let t = Instant::now();
    let response = bench
        .http
        .post(bench.url("/stream/sessions"))
        .header("x-api-key", API_KEY)
        .json(&json!({
            "tmdb_id": bench.movie_tmdb_id,
            "media_type": "movie",
            "release_guid": release_guid,
            "max_resolution": max_resolution,
            "supports_hdr": supports_hdr,
            "subtitle_languages": ["en"],
        }))
        .send()
        .await
        .expect("create session");
    let status = response.status();
    let body: Value = response.json().await.expect("session json");
    let create_ms = t.elapsed().as_millis() as u64;
    assert_eq!(
        status, 200,
        "session creation failed (repair fallback or error): {body}"
    );

    // Server-side stage breakdown from the response.
    for stage in body["timings"].as_array().into_iter().flatten() {
        report.stages.push((
            format!("server:{}", stage["stage"].as_str().unwrap_or("?")),
            stage["ms"].as_u64().unwrap_or(0),
        ));
    }
    report
        .stages
        .push(("create_session_http_total".into(), create_ms));
    report.session = body.clone();
    let session_id = body["session_id"].as_str().expect("session id").to_string();
    let duration_secs = body["duration_secs"].as_f64().unwrap_or(0.0);

    // Poll status until ready, like the client's waitUntilReady.
    let t = Instant::now();
    loop {
        let status = bench.get_json(&format!("/stream/{session_id}")).await;
        match status["state"].as_str() {
            Some("ready") => break,
            Some("failed") => panic!("session failed while starting: {}", status["error"]),
            _ => {}
        }
        assert!(
            t.elapsed() < Duration::from_secs(60),
            "session not ready within 60s"
        );
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    report
        .stages
        .push(("ready_poll".into(), t.elapsed().as_millis() as u64));

    // The AVPlayer fetch sequence: master → media → init → first segments.
    for (label, path) in [
        (
            "master_playlist",
            format!("/stream/{session_id}/master.m3u8"),
        ),
        ("media_playlist", format!("/stream/{session_id}/media.m3u8")),
    ] {
        let t = Instant::now();
        let response = bench
            .http
            .get(bench.url(&path))
            .header("x-api-key", API_KEY)
            .send()
            .await
            .expect(label);
        assert!(response.status().is_success(), "{label} failed");
        let _ = response.bytes().await;
        report
            .stages
            .push((label.into(), t.elapsed().as_millis() as u64));
    }
    let init = fetch_segment(
        &bench.http,
        &bench.url(&format!("/stream/{session_id}/init.mp4")),
    )
    .await;
    assert!(init.error.is_none(), "init.mp4 failed: {:?}", init.error);
    report.stages.push(("init_mp4".into(), init.complete_ms));

    // AVPlayer buffers roughly two segments before rendering the first frame.
    for index in 0..3u64 {
        let fetch = fetch_segment(
            &bench.http,
            &bench.url(&format!("/stream/{session_id}/seg_{index:05}.m4s")),
        )
        .await;
        assert!(
            fetch.error.is_none(),
            "seg_{index:05} failed: {:?}",
            fetch.error
        );
        if index == 0 {
            report.stages.push((
                "seg0_first_media_byte".into(),
                fetch.first_media_byte_ms.unwrap_or(fetch.complete_ms),
            ));
        }
        report
            .stages
            .push((format!("seg{index}_complete"), fetch.complete_ms));
    }

    // Everything on the sequential path up to and including segment 1 —
    // segment 0's completion overlaps segment 1's fetch in reality, so this
    // is the pessimistic (serial) estimate.
    report.estimated_first_frame_ms = report
        .stages
        .iter()
        .filter(|(stage, _)| {
            !stage.starts_with("server:")
                && stage != "seg0_first_media_byte"
                && stage != "seg2_complete"
        })
        .map(|(_, ms)| ms)
        .sum();

    LiveSession {
        id: session_id,
        duration_secs,
    }
}

/// One scrub: cancel any previous loads (drop), then fire the AVPlayer-style
/// ascending burst (target, +1, +2) and measure the target segment.
async fn seek(bench: &Bench, session: &LiveSession, label: &str, target_secs: f64) -> SeekReport {
    let index = (target_secs / SEGMENT_SECONDS).floor() as u64;
    let seg_url = |i: u64| bench.url(&format!("/stream/{}/seg_{i:05}.m4s", session.id));
    let burst1 = tokio::spawn({
        let http = bench.http.clone();
        let url = seg_url(index + 1);
        async move { fetch_segment(&http, &url).await }
    });
    let burst2 = tokio::spawn({
        let http = bench.http.clone();
        let url = seg_url(index + 2);
        async move { fetch_segment(&http, &url).await }
    });
    let fetch = fetch_segment(&bench.http, &seg_url(index)).await;
    burst1.abort();
    burst2.abort();
    let stalled = fetch.error.is_some()
        || Duration::from_millis(fetch.first_media_byte_ms.unwrap_or(fetch.complete_ms))
            >= STALL_AFTER;
    SeekReport {
        label: label.into(),
        target_secs,
        segment_index: index,
        first_media_byte_ms: fetch.first_media_byte_ms,
        complete_ms: fetch.complete_ms,
        stalled,
        error: fetch.error,
    }
}

/// The full scrub script: far jumps forward/back, a near-forward skip that
/// should ride the running ffmpeg, a clustered double-scrub, rapid
/// back-and-forth with cancelled loads, and a settle seek at the end.
async fn run_seek_script(bench: &Bench, session: &LiveSession, report: &mut ScenarioReport) {
    let d = session.duration_secs;
    assert!(d > 600.0, "movie too short for the seek script: {d}s");

    let script: [(&str, f64); 4] = [
        ("fwd_far_40pct", 0.40 * d),
        ("back_far_10pct", 0.10 * d),
        ("fwd_far_70pct", 0.70 * d),
        // Right after landing at 70%: a near-forward skip 2 segments ahead —
        // inside the "let ffmpeg sweep reach it" window, no restart expected.
        ("near_fwd_+12s", 0.70 * d + 12.0),
    ];
    for (label, target) in script {
        let seek_report = seek(bench, session, label, target).await;
        report.seeks.push(seek_report);
        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    // Rapid back-and-forth: fire scrubs 400ms apart, cancelling each previous
    // load — exactly what holding the skip button does. Only the final
    // position is measured; the aborted ones just leave their debris behind.
    let ping = 0.30 * d;
    let pong = 0.65 * d;
    for i in 0..4 {
        let target = if i % 2 == 0 { ping } else { pong };
        let index = (target / SEGMENT_SECONDS).floor() as u64;
        let http = bench.http.clone();
        let url = bench.url(&format!("/stream/{}/seg_{index:05}.m4s", session.id));
        let cancelled = tokio::spawn(async move { fetch_segment(&http, &url).await });
        tokio::time::sleep(Duration::from_millis(400)).await;
        cancelled.abort();
    }
    let settle = seek(bench, session, "settle_after_rapid", pong + SEGMENT_SECONDS).await;
    report.seeks.push(settle);

    // And one plain backward skip from there (the "skip back 10s" button).
    tokio::time::sleep(Duration::from_secs(2)).await;
    let back = seek(bench, session, "back_10s", pong - 10.0).await;
    report.seeks.push(back);
}

async fn end_session(bench: &Bench, session: &LiveSession) {
    let _ = bench
        .http
        .delete(bench.url(&format!("/stream/{}", session.id)))
        .header("x-api-key", API_KEY)
        .send()
        .await;
}

fn assert_no_stalls(report: &ScenarioReport) {
    let stalls: Vec<&SeekReport> = report.seeks.iter().filter(|s| s.stalled).collect();
    assert!(
        stalls.is_empty(),
        "{} seek(s) stalled (> {STALL_AFTER:?} to first media byte or errored): {:?}",
        stalls.len(),
        stalls
            .iter()
            .map(|s| format!("{} ({:?})", s.label, s.error))
            .collect::<Vec<_>>()
    );
}

// ---- Tests -------------------------------------------------------------------------

/// 2160p HDR10 + Dolby Atmos release, played by an HDR-capable client
/// (video stream-copied, TrueHD/DDP Atmos audio transcoded to AAC).
#[tokio::test]
#[ignore = "live benchmark: requires tests/live_settings.toml with [provider] + ffmpeg"]
async fn bench_2160p_hdr10_atmos() {
    let Some(bench) = bench_app().await else {
        return;
    };
    let candidates = bench.movie_candidates().await;
    let Some(candidate) = pick_2160p_hdr_atmos(&candidates) else {
        panic!(
            "no 2160p HDR10 Atmos release for movie {} among {} candidates",
            bench.movie_tmdb_id,
            candidates.len()
        );
    };

    let mut report = ScenarioReport {
        scenario: "2160p_hdr10_atmos_hdr_client".into(),
        release: title(candidate).into(),
        ..Default::default()
    };
    let guid = candidate["raw"]["guid"].as_str().expect("guid").to_string();
    let session = run_startup(&bench, &mut report, &guid, "2160p", true).await;
    run_seek_script(&bench, &session, &mut report).await;
    end_session(&bench, &session).await;

    report.print();
    report.write_json();
    assert_no_stalls(&report);
}

/// The same 2160p HDR10 + Atmos release, played by an SDR-only client:
/// the server tone-maps to 1080p SDR H.264 — the heaviest path.
#[tokio::test]
#[ignore = "live benchmark: requires tests/live_settings.toml with [provider] + ffmpeg"]
async fn bench_2160p_tonemapped_to_sdr() {
    let Some(bench) = bench_app().await else {
        return;
    };
    let candidates = bench.movie_candidates().await;
    let Some(candidate) = pick_2160p_hdr_atmos(&candidates) else {
        panic!(
            "no 2160p HDR10 Atmos release for movie {} among {} candidates",
            bench.movie_tmdb_id,
            candidates.len()
        );
    };

    let mut report = ScenarioReport {
        scenario: "2160p_hdr10_atmos_tonemapped_sdr".into(),
        release: title(candidate).into(),
        ..Default::default()
    };
    let guid = candidate["raw"]["guid"].as_str().expect("guid").to_string();
    let session = run_startup(&bench, &mut report, &guid, "2160p", false).await;
    // Tone-mapping encodes near realtime — keep the seek load light.
    let d = session.duration_secs;
    let fwd = seek(&bench, &session, "fwd_far_40pct", 0.40 * d).await;
    report.seeks.push(fwd);
    tokio::time::sleep(Duration::from_secs(2)).await;
    let back = seek(&bench, &session, "back_far_10pct", 0.10 * d).await;
    report.seeks.push(back);
    end_session(&bench, &session).await;

    report.print();
    report.write_json();
    assert_no_stalls(&report);
}

/// A plain 1080p SDR non-Atmos release — the everyday default path.
#[tokio::test]
#[ignore = "live benchmark: requires tests/live_settings.toml with [provider] + ffmpeg"]
async fn bench_1080p_sdr_no_atmos() {
    let Some(bench) = bench_app().await else {
        return;
    };
    let candidates = bench.movie_candidates().await;
    let Some(candidate) = pick_1080p_sdr_plain(&candidates) else {
        panic!(
            "no plain 1080p SDR (non-Atmos) release for movie {} among {} candidates",
            bench.movie_tmdb_id,
            candidates.len()
        );
    };

    let mut report = ScenarioReport {
        scenario: "1080p_sdr_no_atmos".into(),
        release: title(candidate).into(),
        ..Default::default()
    };
    let guid = candidate["raw"]["guid"].as_str().expect("guid").to_string();
    let session = run_startup(&bench, &mut report, &guid, "1080p", true).await;
    run_seek_script(&bench, &session, &mut report).await;
    end_session(&bench, &session).await;

    report.print();
    report.write_json();
    assert_no_stalls(&report);
}
