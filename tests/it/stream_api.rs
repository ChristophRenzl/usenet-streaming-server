//! Streaming API end-to-end tests: raw range semantics, the loopback VFS
//! guard, segment-name validation, the full HLS pipeline over mock NNTP
//! (with real ffmpeg/ffprobe), audio transcoding, seek restarts and the
//! idle-session reaper.

use std::path::Path;
use std::sync::Arc;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::time::Instant;
use usenet_streaming_server::config::AppConfig;
use usenet_streaming_server::state::AppState;
use usenet_streaming_server::stream::{NewSession, Session};
use usenet_streaming_server::tmdb::models::MediaType;
use usenet_streaming_server::vfs::DiskFile;
use uuid::Uuid;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::support::{
    add_yenc_file, build_nzb_xml, ffmpeg_available, generate_media, spawn_app, xorshift_bytes,
    MockNntp,
};

const API_KEY: &str = "test-api-key";

fn test_config(session_dir: &Path) -> AppConfig {
    let mut config = AppConfig::default();
    config.auth.api_key = API_KEY.into();
    config.storage.session_dir = Some(session_dir.to_str().expect("utf-8 dir").to_string());
    config
}

// ---- Disk-backed harness (no ffmpeg, no NNTP) --------------------------------

struct DiskStack {
    base: String,
    state: AppState,
    client: reqwest::Client,
    dir: tempfile::TempDir,
    _server: tokio::task::JoinHandle<()>,
}

async fn disk_stack(tweak: impl FnOnce(&mut AppConfig)) -> DiskStack {
    let dir = tempfile::tempdir().expect("tempdir");
    let mut config = test_config(dir.path());
    tweak(&mut config);
    let state = AppState::for_tests(config).await.expect("state");
    let (base, state, server) = spawn_app(state).await;
    DiskStack {
        base,
        state,
        client: reqwest::Client::new(),
        dir,
        _server: server,
    }
}

impl DiskStack {
    /// Write `payload` to a file and register a session serving it.
    async fn insert_session(&self, payload: &[u8], inner_name: &str) -> Arc<Session> {
        let path = self.dir.path().join("payload.bin");
        std::fs::write(&path, payload).expect("write payload");
        let media = DiskFile::open(&path).await.expect("disk file");
        let session = Session::create(
            NewSession {
                media: Arc::new(media),
                tmdb_id: 1,
                media_type: MediaType::Movie,
                season: None,
                episode: None,
                release_title: "Disk.Test.Release".into(),
                inner_file_name: inner_name.into(),
                resume_position_secs: 0.0,
            },
            self.state.config.storage.session_dir.as_deref(),
        )
        .await
        .expect("session");
        self.state.sessions.insert(session.clone());
        session
    }

    async fn get(&self, path: &str) -> reqwest::Response {
        self.client
            .get(format!("{}{path}", self.base))
            .header("x-api-key", API_KEY)
            .send()
            .await
            .expect("GET")
    }

    async fn get_range(&self, path: &str, range: &str) -> reqwest::Response {
        self.client
            .get(format!("{}{path}", self.base))
            .header("x-api-key", API_KEY)
            .header("range", range)
            .send()
            .await
            .expect("GET with range")
    }
}

// ---- Raw range semantics ------------------------------------------------------

#[tokio::test]
async fn raw_range_semantics_over_disk_file() {
    let stack = disk_stack(|_| {}).await;
    // Odd size, larger than two stream chunks, to exercise multi-chunk reads.
    let payload = xorshift_bytes(3 * 1024 * 1024 + 123);
    let session = stack.insert_session(&payload, "movie.mkv").await;
    let raw = format!("/api/v1/stream/{}/raw", session.id);
    let len = payload.len() as u64;

    // Auth still applies to streaming routes.
    let response = stack
        .client
        .get(format!("{}{raw}", stack.base))
        .send()
        .await
        .expect("GET");
    assert_eq!(response.status(), 401);

    // 200 full body, streamed across multiple 1 MiB chunks.
    let response = stack.get(&raw).await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["accept-ranges"], "bytes");
    assert_eq!(response.headers()["content-type"], "video/x-matroska");
    assert_eq!(
        response.headers()["content-length"],
        len.to_string().as_str()
    );
    assert_eq!(&response.bytes().await.expect("body")[..], &payload[..]);

    // 206 closed mid-range (inclusive bounds).
    let response = stack.get_range(&raw, "bytes=100000-200000").await;
    assert_eq!(response.status(), 206);
    assert_eq!(
        response.headers()["content-range"],
        format!("bytes 100000-200000/{len}").as_str()
    );
    assert_eq!(response.headers()["content-length"], "100001");
    assert_eq!(
        &response.bytes().await.expect("body")[..],
        &payload[100_000..=200_000]
    );

    // 206 open-ended.
    let response = stack.get_range(&raw, "bytes=3000000-").await;
    assert_eq!(response.status(), 206);
    assert_eq!(
        &response.bytes().await.expect("body")[..],
        &payload[3_000_000..]
    );

    // 206 suffix (last 1024 bytes).
    let response = stack.get_range(&raw, "bytes=-1024").await;
    assert_eq!(response.status(), 206);
    assert_eq!(
        response.headers()["content-range"],
        format!("bytes {}-{}/{len}", len - 1024, len - 1).as_str()
    );
    assert_eq!(
        &response.bytes().await.expect("body")[..],
        &payload[payload.len() - 1024..]
    );

    // 206 crossing the 1 MiB chunk boundary.
    let response = stack.get_range(&raw, "bytes=1048570-2097162").await;
    assert_eq!(response.status(), 206);
    assert_eq!(
        &response.bytes().await.expect("body")[..],
        &payload[1_048_570..=2_097_162]
    );

    // 416 for a start at/past EOF, with the required */len Content-Range.
    let response = stack.get_range(&raw, &format!("bytes={len}-")).await;
    assert_eq!(response.status(), 416);
    assert_eq!(
        response.headers()["content-range"],
        format!("bytes */{len}").as_str()
    );

    // 416 for a syntactically invalid range.
    let response = stack.get_range(&raw, "bytes=5-2").await;
    assert_eq!(response.status(), 416);
}

// ---- Loopback VFS guard ---------------------------------------------------------

#[tokio::test]
async fn internal_vfs_requires_loopback_token_and_session() {
    let stack = disk_stack(|_| {}).await;
    let payload = xorshift_bytes(100_000);
    let session = stack.insert_session(&payload, "movie.mkv").await;

    // Missing token.
    let response = stack
        .client
        .get(format!("{}/internal/vfs/{}", stack.base, session.id))
        .send()
        .await
        .expect("GET");
    assert_eq!(response.status(), 403);

    // Wrong token.
    let response = stack
        .client
        .get(format!(
            "{}/internal/vfs/{}?token=deadbeef",
            stack.base, session.id
        ))
        .send()
        .await
        .expect("GET");
    assert_eq!(response.status(), 403);

    // Unknown session (even with a valid-looking token).
    let response = stack
        .client
        .get(format!(
            "{}/internal/vfs/{}?token={}",
            stack.base,
            Uuid::new_v4(),
            session.token
        ))
        .send()
        .await
        .expect("GET");
    assert_eq!(response.status(), 404);

    // Correct token serves the file (no API key needed on this route).
    let response = stack
        .client
        .get(format!(
            "{}/internal/vfs/{}?token={}",
            stack.base, session.id, session.token
        ))
        .send()
        .await
        .expect("GET");
    assert_eq!(response.status(), 200);
    assert_eq!(&response.bytes().await.expect("body")[..], &payload[..]);

    // ... with full range support (what ffmpeg actually uses).
    let response = stack
        .client
        .get(format!(
            "{}/internal/vfs/{}?token={}",
            stack.base, session.id, session.token
        ))
        .header("range", "bytes=10-19")
        .send()
        .await
        .expect("GET");
    assert_eq!(response.status(), 206);
    assert_eq!(
        &response.bytes().await.expect("body")[..],
        &payload[10..=19]
    );
}

// ---- OpenAPI surface ---------------------------------------------------------------

#[tokio::test]
async fn openapi_documents_streaming_but_not_internal_routes() {
    let stack = disk_stack(|_| {}).await;
    let response = stack
        .client
        .get(format!("{}/api-docs/openapi.json", stack.base))
        .send()
        .await
        .expect("GET openapi");
    assert_eq!(response.status(), 200);
    let doc: Value = response.json().await.expect("openapi json");
    let paths = doc["paths"].as_object().expect("paths");

    for documented in [
        "/stream/sessions",
        "/stream/{session_id}",
        "/stream/{session_id}/master.m3u8",
        "/stream/{session_id}/media.m3u8",
        "/stream/{session_id}/{segment}",
        "/stream/{session_id}/raw",
        "/stream/{session_id}/seek",
    ] {
        assert!(paths.contains_key(documented), "missing {documented}");
    }
    assert!(
        !paths.keys().any(|path| path.contains("internal")),
        "internal loopback route must stay out of the OpenAPI doc"
    );
    let tags: Vec<&str> = doc["tags"]
        .as_array()
        .expect("tags")
        .iter()
        .filter_map(|tag| tag["name"].as_str())
        .collect();
    assert!(tags.contains(&"streaming"), "tags: {tags:?}");
}

// ---- Segment name validation + playlist availability -----------------------------

#[tokio::test]
async fn segment_names_are_validated_and_playlist_waits() {
    let stack = disk_stack(|_| {}).await;
    let session = stack
        .insert_session(&xorshift_bytes(1000), "movie.mkv")
        .await;
    let id = session.id;

    // Plant a file *outside* the session dir that a traversal would reach.
    let secret = stack.dir.path().join("secret.txt");
    std::fs::write(&secret, b"top secret").expect("write secret");

    for attempt in [
        "%2e%2e%2f%2e%2e%2fetc%2fpasswd",
        "%2e%2e%2fsecret.txt",
        "..%2fsecret.txt",
        "seg_00000.m4s.tmp",
    ] {
        let response = stack.get(&format!("/api/v1/stream/{id}/{attempt}")).await;
        assert_eq!(response.status(), 400, "attempt {attempt} must be rejected");
    }

    // A valid name that does not exist is a plain 404.
    let response = stack.get(&format!("/api/v1/stream/{id}/init.mp4")).await;
    assert_eq!(response.status(), 404);

    // The media playlist reports "try again" while the session is starting.
    let response = stack.get(&format!("/api/v1/stream/{id}/media.m3u8")).await;
    assert_eq!(response.status(), 503);
    assert_eq!(response.headers()["retry-after"], "1");

    // The master playlist and the status document are always available.
    let response = stack.get(&format!("/api/v1/stream/{id}/master.m3u8")).await;
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers()["content-type"],
        "application/vnd.apple.mpegurl"
    );
    assert!(response.text().await.expect("body").contains("media.m3u8"));

    let response = stack.get(&format!("/api/v1/stream/{id}")).await;
    assert_eq!(response.status(), 200);
    let status: Value = response.json().await.expect("json");
    assert_eq!(status["state"], "starting");
    assert_eq!(status["container"], "mkv");
    assert_eq!(status["segments_ready"], 0);
}

// ---- Idle reaper -----------------------------------------------------------------

#[tokio::test]
async fn idle_sessions_are_reaped() {
    let stack = disk_stack(|config| {
        config.streaming.session_idle_timeout_secs = 1;
    })
    .await;
    let session = stack
        .insert_session(&xorshift_bytes(1000), "movie.mkv")
        .await;
    let temp_dir = session.temp_dir.clone();
    assert!(stack.state.sessions.get(&session.id).is_some());
    assert!(temp_dir.exists());

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        if stack.state.sessions.get(&session.id).is_none() && !temp_dir.exists() {
            break;
        }
        assert!(
            Instant::now() < deadline,
            "idle session was not reaped within 10s"
        );
        tokio::time::sleep(Duration::from_millis(200)).await;
    }

    // And the HTTP surface agrees.
    let response = stack.get(&format!("/api/v1/stream/{}", session.id)).await;
    assert_eq!(response.status(), 404);
}

// ---- Full-stack HLS harness --------------------------------------------------------

fn rss_with_release(indexer_base: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0" xmlns:newznab="http://www.newznab.com/DTD/2010/feeds/attributes/">
  <channel>
    <title>mock</title>
    <item>
      <title>Test.Movie.2026.1080p.BluRay.x264-TEST</title>
      <guid isPermaLink="true">https://indexer.example/details/test-guid</guid>
      <link>{indexer_base}/getnzb/test.nzb</link>
      <pubDate>Wed, 03 Jun 2026 12:30:00 +0000</pubDate>
      <newznab:attr name="size" value="734003200"/>
    </item>
  </channel>
</rss>"#
    )
}

/// The whole distribution chain in-process: media bytes yEnc'd onto a mock
/// NNTP server, an NZB behind a mock indexer, TMDB mocked, the real app
/// served on a loopback socket, providers/indexers configured via the API.
struct HlsStack {
    base: String,
    state: AppState,
    client: reqwest::Client,
    nntp: MockNntp,
    session_root: tempfile::TempDir,
    _tmdb: MockServer,
    _indexer: MockServer,
    _server: tokio::task::JoinHandle<()>,
}

async fn hls_stack(media: &[u8], part_size: usize, tweak: impl FnOnce(&mut AppConfig)) -> HlsStack {
    let nntp = MockNntp::start(None).await;
    let segments = add_yenc_file(&nntp, "media", media, part_size, "movie.mkv");
    let nzb_xml = build_nzb_xml(&[(r#"Movie [1/1] - "movie.mkv" yEnc"#.to_string(), segments)]);

    let tmdb = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/movie/27205"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": 27205,
            "title": "Inception",
            "release_date": "2010-07-15",
            "imdb_id": "tt1375666",
            "external_ids": { "imdb_id": "tt1375666" }
        })))
        .mount(&tmdb)
        .await;

    let indexer = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("t", "movie"))
        .and(query_param("imdbid", "1375666"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(rss_with_release(&indexer.uri()), "application/rss+xml"),
        )
        .mount(&indexer)
        .await;
    Mock::given(method("GET"))
        .and(path("/getnzb/test.nzb"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(nzb_xml, "application/x-nzb"))
        .mount(&indexer)
        .await;

    let session_root = tempfile::tempdir().expect("session root");
    let mut config = test_config(session_root.path());
    tweak(&mut config);
    let state = AppState::for_tests(config)
        .await
        .expect("state")
        .with_tmdb_base_url(&tmdb.uri());
    let (base, state, server) = spawn_app(state).await;
    let client = reqwest::Client::new();

    // Configure TMDB key, indexer and NNTP provider through the API; the
    // provider POST must reload the pool live (it started empty).
    let response = client
        .put(format!("{base}/api/v1/settings/app"))
        .header("x-api-key", API_KEY)
        .json(&json!({ "tmdb_api_key": "tmdb-key" }))
        .send()
        .await
        .expect("PUT app settings");
    assert_eq!(response.status(), 200);

    let response = client
        .post(format!("{base}/api/v1/settings/indexers"))
        .header("x-api-key", API_KEY)
        .json(&json!({
            "name": "mock",
            "base_url": indexer.uri(),
            "api_key": "indexer-key"
        }))
        .send()
        .await
        .expect("POST indexer");
    assert_eq!(response.status(), 200);

    let response = client
        .post(format!("{base}/api/v1/settings/providers"))
        .header("x-api-key", API_KEY)
        .json(&json!({
            "name": "mock-nntp",
            "host": "127.0.0.1",
            "port": nntp.addr().port(),
            "use_tls": false,
            "max_connections": 8,
            "priority": 0
        }))
        .send()
        .await
        .expect("POST provider");
    assert_eq!(response.status(), 200);

    HlsStack {
        base,
        state,
        client,
        nntp,
        session_root,
        _tmdb: tmdb,
        _indexer: indexer,
        _server: server,
    }
}

impl HlsStack {
    async fn get(&self, path: &str) -> reqwest::Response {
        self.client
            .get(format!("{}{path}", self.base))
            .header("x-api-key", API_KEY)
            .send()
            .await
            .expect("GET")
    }

    async fn create_session(&self) -> Value {
        let response = self
            .client
            .post(format!("{}/api/v1/stream/sessions", self.base))
            .header("x-api-key", API_KEY)
            .json(&json!({ "tmdb_id": 27205, "media_type": "movie" }))
            .send()
            .await
            .expect("POST session");
        let status = response.status();
        let body: Value = response.json().await.expect("session json");
        assert_eq!(status, 200, "session creation failed: {body}");
        body
    }

    /// Poll the status endpoint until the state is one of `accept`.
    /// Panics on `failed` or when `timeout` elapses.
    async fn wait_for_state(&self, id: &str, accept: &[&str], timeout: Duration) -> Value {
        let deadline = Instant::now() + timeout;
        loop {
            let response = self.get(&format!("/api/v1/stream/{id}")).await;
            assert_eq!(response.status(), 200);
            let status: Value = response.json().await.expect("status json");
            let state = status["state"].as_str().expect("state").to_string();
            if accept.contains(&state.as_str()) {
                return status;
            }
            assert_ne!(
                state,
                "failed",
                "session failed: {}",
                status["error"].as_str().unwrap_or("?")
            );
            assert!(
                Instant::now() < deadline,
                "session did not reach {accept:?} in {timeout:?} (currently {state})"
            );
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }

    /// Fetch the media playlist, tolerating 503 while ffmpeg warms up.
    async fn media_playlist(&self, id: &str, timeout: Duration) -> String {
        let deadline = Instant::now() + timeout;
        loop {
            let response = self.get(&format!("/api/v1/stream/{id}/media.m3u8")).await;
            if response.status() == 200 {
                return response.text().await.expect("playlist");
            }
            assert_eq!(response.status(), 503, "unexpected playlist status");
            assert!(
                Instant::now() < deadline,
                "media playlist never appeared within {timeout:?}"
            );
            tokio::time::sleep(Duration::from_millis(250)).await;
        }
    }
}

fn extinf_sum(playlist: &str) -> f64 {
    playlist
        .lines()
        .filter_map(|line| line.strip_prefix("#EXTINF:"))
        .filter_map(|rest| rest.split(',').next())
        .filter_map(|value| value.trim().parse::<f64>().ok())
        .sum()
}

// ---- Full-stack HLS -------------------------------------------------------------

#[tokio::test]
async fn hls_full_stack_remuxes_mock_usenet_release() {
    if !ffmpeg_available() {
        eprintln!("skipping hls_full_stack_remuxes_mock_usenet_release: ffmpeg/ffprobe not found");
        return;
    }
    let media_dir = tempfile::tempdir().expect("media dir");
    let media_path = generate_media(media_dir.path(), 10, 24, &["-c:a", "ac3", "-b:a", "96k"])
        .expect("generate ac3 test media");
    let media = std::fs::read(&media_path).expect("read media");

    let stack = hls_stack(&media, 64 * 1024, |_| {}).await;
    let created = stack.create_session().await;
    let id = created["session_id"]
        .as_str()
        .expect("session id")
        .to_string();

    // Probe-derived metadata.
    assert_eq!(created["container"], "mkv");
    assert_eq!(created["video_codec"], "h264");
    assert_eq!(created["audio_codec"], "ac3");
    assert_eq!(created["audio_transcoded"], false);
    let duration = created["duration_secs"].as_f64().expect("duration");
    assert!((8.0..12.0).contains(&duration), "duration {duration}");
    assert_eq!(
        created["chosen_release"]["raw"]["title"],
        "Test.Movie.2026.1080p.BluRay.x264-TEST"
    );
    assert!(!created["candidates"]
        .as_array()
        .expect("candidates")
        .is_empty());
    assert_eq!(
        created["hls_master_url"].as_str().expect("master url"),
        format!("/api/v1/stream/{id}/master.m3u8")
    );
    assert!(created["resume_position_secs"].is_null());

    stack
        .wait_for_state(&id, &["ready", "ended"], Duration::from_secs(30))
        .await;

    // Master playlist, fetched with ?apikey= like a real player would.
    let response = stack
        .client
        .get(format!(
            "{}/api/v1/stream/{id}/master.m3u8?apikey={API_KEY}",
            stack.base
        ))
        .send()
        .await
        .expect("GET master");
    assert_eq!(response.status(), 200);
    assert_eq!(
        response.headers()["content-type"],
        "application/vnd.apple.mpegurl"
    );
    let master = response.text().await.expect("master");
    assert!(master.contains("#EXT-X-STREAM-INF"));
    assert!(master.contains("media.m3u8"));

    // Media playlist references the init segment and at least one segment.
    let playlist = stack.media_playlist(&id, Duration::from_secs(10)).await;
    assert!(playlist.contains("init.mp4"), "playlist:\n{playlist}");
    let first_segment = playlist
        .lines()
        .find(|line| line.starts_with("seg_"))
        .expect("segment in playlist")
        .to_string();

    let response = stack.get(&format!("/api/v1/stream/{id}/init.mp4")).await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.headers()["content-type"], "video/mp4");
    assert!(!response.bytes().await.expect("init").is_empty());

    let response = stack
        .get(&format!("/api/v1/stream/{id}/{first_segment}"))
        .await;
    assert_eq!(response.status(), 200);
    assert!(!response.bytes().await.expect("segment").is_empty());

    // Raw range access returns the source bytes.
    let response = stack
        .client
        .get(format!("{}/api/v1/stream/{id}/raw", stack.base))
        .header("x-api-key", API_KEY)
        .header("range", "bytes=0-15")
        .send()
        .await
        .expect("GET raw");
    assert_eq!(response.status(), 206);
    assert_eq!(&response.bytes().await.expect("raw")[..], &media[..16]);

    // ffmpeg finishes: playlist is finalized as VOD and roughly covers the
    // clip (segment count x 6s target duration within +-50%).
    let status = stack
        .wait_for_state(&id, &["ended"], Duration::from_secs(30))
        .await;
    assert!(status["segments_ready"].as_u64().expect("segments") >= 1);
    let playlist = stack.media_playlist(&id, Duration::from_secs(5)).await;
    assert!(playlist.contains("#EXT-X-ENDLIST"), "playlist:\n{playlist}");
    let segment_count = playlist
        .lines()
        .filter(|line| line.starts_with("seg_"))
        .count();
    let covered = segment_count as f64 * 6.0;
    assert!(
        (5.0..=15.0).contains(&covered),
        "{segment_count} segments x 6s = {covered}s does not roughly match 10s"
    );
    let total = extinf_sum(&playlist);
    assert!((8.0..12.0).contains(&total), "EXTINF sum {total}");

    // Teardown: 204, then gone (HTTP and filesystem).
    let session_dir = stack.session_root.path().join(&id);
    assert!(session_dir.exists());
    let response = stack
        .client
        .delete(format!("{}/api/v1/stream/{id}", stack.base))
        .header("x-api-key", API_KEY)
        .send()
        .await
        .expect("DELETE");
    assert_eq!(response.status(), 204);
    let response = stack.get(&format!("/api/v1/stream/{id}")).await;
    assert_eq!(response.status(), 404);
    assert!(!session_dir.exists(), "session dir must be removed");
    assert!(stack.state.sessions.is_empty());
}

// ---- Audio transcoding -------------------------------------------------------------

#[tokio::test]
async fn hls_transcodes_non_copyable_audio_to_aac() {
    if !ffmpeg_available() {
        eprintln!("skipping hls_transcodes_non_copyable_audio_to_aac: ffmpeg/ffprobe not found");
        return;
    }
    let media_dir = tempfile::tempdir().expect("media dir");
    // Prefer DTS (experimental 'dca' encoder); fall back to FLAC when the
    // build lacks it. Both must be transcoded for HLS.
    let (media_path, expected_codec) = match generate_media(
        media_dir.path(),
        8,
        24,
        &[
            "-c:a",
            "dca",
            "-strict",
            "experimental",
            "-ar",
            "48000",
            "-ac",
            "2",
        ],
    ) {
        Some(path) => (path, "dts"),
        None => {
            eprintln!("dca encoder unavailable; using flac as the non-copyable codec");
            let path = generate_media(media_dir.path(), 8, 24, &["-c:a", "flac"])
                .expect("generate flac test media");
            (path, "flac")
        }
    };
    let media = std::fs::read(&media_path).expect("read media");

    let stack = hls_stack(&media, 64 * 1024, |_| {}).await;
    let created = stack.create_session().await;
    let id = created["session_id"]
        .as_str()
        .expect("session id")
        .to_string();
    assert_eq!(created["audio_transcoded"], true);
    assert_eq!(created["audio_codec"], expected_codec);

    stack
        .wait_for_state(&id, &["ended"], Duration::from_secs(30))
        .await;
    let playlist = stack.media_playlist(&id, Duration::from_secs(5)).await;

    // Concatenate init + all media segments into one fMP4 and probe it: the
    // audio track must have been transcoded to AAC.
    let mut concatenated = Vec::new();
    let response = stack.get(&format!("/api/v1/stream/{id}/init.mp4")).await;
    assert_eq!(response.status(), 200);
    concatenated.extend_from_slice(&response.bytes().await.expect("init"));
    for segment in playlist.lines().filter(|line| line.starts_with("seg_")) {
        let response = stack.get(&format!("/api/v1/stream/{id}/{segment}")).await;
        assert_eq!(response.status(), 200);
        concatenated.extend_from_slice(&response.bytes().await.expect("segment"));
    }
    let probe_path = media_dir.path().join("hls-output.mp4");
    std::fs::write(&probe_path, &concatenated).expect("write concat");

    let output = std::process::Command::new("ffprobe")
        .args(["-v", "quiet", "-print_format", "json", "-show_streams"])
        .arg(&probe_path)
        .output()
        .expect("run ffprobe");
    assert!(output.status.success(), "ffprobe failed on HLS output");
    let doc: Value = serde_json::from_slice(&output.stdout).expect("probe json");
    let audio = doc["streams"]
        .as_array()
        .expect("streams")
        .iter()
        .find(|stream| stream["codec_type"] == "audio")
        .expect("audio stream in HLS output");
    assert_eq!(audio["codec_name"], "aac");
}

// ---- Seek --------------------------------------------------------------------------

#[tokio::test]
async fn seek_beyond_frontier_restarts_ffmpeg() {
    if !ffmpeg_available() {
        eprintln!("skipping seek_beyond_frontier_restarts_ffmpeg: ffmpeg/ffprobe not found");
        return;
    }
    let media_dir = tempfile::tempdir().expect("media dir");
    let media_path = generate_media(media_dir.path(), 30, 12, &["-c:a", "ac3", "-b:a", "96k"])
        .expect("generate 30s test media");
    let media = std::fs::read(&media_path).expect("read media");

    // No caching, no readahead: every read hits the (slowed) mock server, so
    // ffmpeg cannot outrun the seek below.
    let stack = hls_stack(&media, 32 * 1024, |config| {
        config.cache.memory_bytes = 0;
        config.streaming.readahead_segments = 0;
    })
    .await;
    stack.nntp.set_delay(Some(Duration::from_millis(100)));

    let created = stack.create_session().await;
    let id = created["session_id"]
        .as_str()
        .expect("session id")
        .to_string();

    // Seek far beyond anything ffmpeg can have produced yet.
    let response = stack
        .client
        .post(format!("{}/api/v1/stream/{id}/seek", stack.base))
        .header("x-api-key", API_KEY)
        .json(&json!({ "time_secs": 25.0 }))
        .send()
        .await
        .expect("POST seek");
    assert_eq!(response.status(), 200);
    let seek: Value = response.json().await.expect("seek json");
    assert_eq!(seek["restarted"], true, "seek to 25s must restart ffmpeg");

    // Let the restarted ffmpeg run at full speed and finish the tail.
    stack.nntp.set_delay(None);
    stack
        .wait_for_state(&id, &["ended"], Duration::from_secs(45))
        .await;
    let playlist = stack.media_playlist(&id, Duration::from_secs(5)).await;
    assert!(playlist.contains("#EXT-X-ENDLIST"));
    let produced = extinf_sum(&playlist);
    assert!(
        (2.0..=9.0).contains(&produced),
        "playlist should cover ~5s (25..30), got {produced}s"
    );

    // A target inside the freshly produced window is a no-op.
    let response = stack
        .client
        .post(format!("{}/api/v1/stream/{id}/seek", stack.base))
        .header("x-api-key", API_KEY)
        .json(&json!({ "time_secs": 26.0 }))
        .send()
        .await
        .expect("POST seek");
    assert_eq!(response.status(), 200);
    let seek: Value = response.json().await.expect("seek json");
    assert_eq!(seek["restarted"], false);
}
