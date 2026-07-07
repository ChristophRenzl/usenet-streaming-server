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

// ---- Bad-release blacklist ----------------------------------------------------

#[tokio::test]
async fn blacklisting_a_session_release_is_idempotent_and_manageable() {
    let stack = disk_stack(|_| {}).await;
    let session = stack.insert_session(b"payload", "movie.mkv").await;
    let blacklist_url = format!("{}/api/v1/stream/{}/blacklist", stack.base, session.id);

    // First flag records the entry (with the optional reason)...
    let first: Value = stack
        .client
        .post(&blacklist_url)
        .header("x-api-key", API_KEY)
        .json(&json!({ "reason": "audio out of sync" }))
        .send()
        .await
        .expect("POST blacklist")
        .json()
        .await
        .expect("blacklist json");
    assert_eq!(first["release_title"], "Disk.Test.Release");
    assert_eq!(first["created"], true);

    // ...a repeat flag (and a body-less POST) is a no-op.
    let again: Value = stack
        .client
        .post(&blacklist_url)
        .header("x-api-key", API_KEY)
        .send()
        .await
        .expect("POST blacklist again")
        .json()
        .await
        .expect("blacklist json");
    assert_eq!(again["created"], false);

    // The management listing shows it; deleting un-blacklists exactly once.
    let listed: Value = stack
        .get("/api/v1/releases/blacklist")
        .await
        .json()
        .await
        .expect("listing json");
    let entries = listed["entries"].as_array().expect("entries array");
    assert_eq!(entries.len(), 1);
    assert_eq!(entries[0]["title"], "Disk.Test.Release");
    assert_eq!(entries[0]["reason"], "audio out of sync");

    let id = entries[0]["id"].as_i64().expect("entry id");
    let delete_url = format!("{}/api/v1/releases/blacklist/{id}", stack.base);
    let deleted = stack
        .client
        .delete(&delete_url)
        .header("x-api-key", API_KEY)
        .send()
        .await
        .expect("DELETE entry");
    assert_eq!(deleted.status(), 204);
    let missing = stack
        .client
        .delete(&delete_url)
        .header("x-api-key", API_KEY)
        .send()
        .await
        .expect("DELETE entry again");
    assert_eq!(missing.status(), 404);

    // Unknown sessions cannot blacklist anything.
    let unknown = stack
        .client
        .post(format!(
            "{}/api/v1/stream/{}/blacklist",
            stack.base,
            Uuid::new_v4()
        ))
        .header("x-api-key", API_KEY)
        .send()
        .await
        .expect("POST unknown session");
    assert_eq!(unknown.status(), 404);
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
        "/stream/{session_id}/subtitles",
        "/stream/{session_id}/subtitles/{language}/offset",
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

    // A valid name that does not exist yet answers immediately (AVPlayer
    // drops variants without a response within 6s) and keeps the body
    // pending until ffmpeg produces the file.
    let response = tokio::time::timeout(
        Duration::from_secs(3),
        stack.get(&format!("/api/v1/stream/{id}/init.mp4")),
    )
    .await
    .expect("headers must arrive immediately");
    assert_eq!(response.status(), 200);
    let body = tokio::time::timeout(Duration::from_secs(1), response.bytes()).await;
    assert!(body.is_err(), "body must stay pending until produced");

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
      <title>Inception.2010.1080p.BluRay.x264-TEST</title>
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
    opensubtitles: MockServer,
    _tmdb: MockServer,
    _indexer: MockServer,
    _server: tokio::task::JoinHandle<()>,
}

async fn hls_stack(media: &[u8], part_size: usize, tweak: impl FnOnce(&mut AppConfig)) -> HlsStack {
    let nntp = MockNntp::start(None).await;
    let segments = add_yenc_file(&nntp, "media", media, part_size, "movie.mkv");
    let nzb_xml = build_nzb_xml(&[(r#"Movie [1/1] - "movie.mkv" yEnc"#.to_string(), segments)]);

    let opensubtitles = MockServer::start().await;
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
        .with_tmdb_base_url(&tmdb.uri())
        .with_opensubtitles_base_url(&opensubtitles.uri());
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
        opensubtitles,
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
        self.create_session_body(json!({ "tmdb_id": 27205, "media_type": "movie" }))
            .await
    }

    /// Create a session from an arbitrary request body (200 asserted).
    async fn create_session_body(&self, body: Value) -> Value {
        let response = self
            .client
            .post(format!("{}/api/v1/stream/sessions", self.base))
            .header("x-api-key", API_KEY)
            .json(&body)
            .send()
            .await
            .expect("POST session");
        let status = response.status();
        let body: Value = response.json().await.expect("session json");
        assert_eq!(status, 200, "session creation failed: {body}");
        body
    }

    /// Configure the OpenSubtitles API key and mount a mock search (returning
    /// one hash-matched English subtitle) plus the two-step download (link +
    /// CDN bytes) on the `opensubtitles` mock server, so the session
    /// auto-attach path finds and downloads a subtitle.
    async fn setup_opensubtitles(&self, srt: &str) {
        let response = self
            .client
            .put(format!("{}/api/v1/settings/app", self.base))
            .header("x-api-key", API_KEY)
            .json(&json!({ "opensubtitles_api_key": "os-key-1234" }))
            .send()
            .await
            .expect("PUT os key");
        assert_eq!(response.status(), 200);

        // Search: one English, moviehash-matched subtitle (so no fps rescale).
        Mock::given(method("GET"))
            .and(path("/subtitles"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "data": [{
                    "id": "sub-en",
                    "attributes": {
                        "language": "en",
                        "release": "Inception.2010.1080p.BluRay",
                        "download_count": 1234,
                        "hearing_impaired": false,
                        "ai_translated": false,
                        "moviehash_match": true,
                        "files": [{ "file_id": 555, "file_name": "sub.srt" }]
                    }
                }]
            })))
            .mount(&self.opensubtitles)
            .await;

        // Download link → CDN bytes (the SRT itself).
        let cdn_link = format!("{}/cdn/sub.srt", self.opensubtitles.uri());
        Mock::given(method("POST"))
            .and(path("/download"))
            .respond_with(ResponseTemplate::new(200).set_body_json(json!({
                "link": cdn_link,
                "remaining": 99
            })))
            .mount(&self.opensubtitles)
            .await;
        Mock::given(method("GET"))
            .and(path("/cdn/sub.srt"))
            .respond_with(ResponseTemplate::new(200).set_body_string(srt.to_string()))
            .mount(&self.opensubtitles)
            .await;
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
        "Inception.2010.1080p.BluRay.x264-TEST"
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
    // The served playlist always claims the whole file as VOD…
    let playlist = stack.media_playlist(&id, Duration::from_secs(5)).await;
    assert!(playlist.contains("#EXT-X-ENDLIST"));
    let claimed = extinf_sum(&playlist);
    assert!(
        (28.0..=32.0).contains(&claimed),
        "VOD playlist should claim the full ~30s, got {claimed}s"
    );
    // …and the restarted ffmpeg produced the tail on the global numbering:
    // 25s snaps down to segment 4 (4 x 6s = 24s).
    let response = stack
        .get(&format!("/api/v1/stream/{id}/seg_00004.m4s"))
        .await;
    assert_eq!(response.status(), 200, "tail segment after restart");
    assert!(!response.bytes().await.expect("segment").is_empty());

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

// ---- Subtitles: auto-attach, HLS rendition and manual offset -----------------------

/// Parse the first cue's start timestamp (seconds) from a WebVTT document.
fn first_cue_start_secs(vtt: &str) -> f64 {
    let timing = vtt
        .lines()
        .find(|l| l.contains("-->"))
        .expect("a cue timing line");
    let start = timing.split("-->").next().unwrap().trim();
    // HH:MM:SS.mmm
    let (hms, ms) = start.split_once('.').expect("fractional seconds");
    let mut parts = hms.split(':').map(|p| p.parse::<f64>().unwrap());
    let h = parts.next().unwrap();
    let m = parts.next().unwrap();
    let s = parts.next().unwrap();
    h * 3600.0 + m * 60.0 + s + ms.parse::<f64>().unwrap() / 1000.0
}

#[tokio::test]
async fn hls_auto_attaches_subtitle_and_manual_offset_shifts_cues() {
    if !ffmpeg_available() {
        eprintln!("skipping hls_auto_attaches_subtitle_and_manual_offset_shifts_cues: ffmpeg/ffprobe not found");
        return;
    }
    let media_dir = tempfile::tempdir().expect("media dir");
    let media_path = generate_media(media_dir.path(), 10, 24, &["-c:a", "ac3", "-b:a", "96k"])
        .expect("generate test media");
    let media = std::fs::read(&media_path).expect("read media");

    let stack = hls_stack(&media, 64 * 1024, |_| {}).await;
    // A subtitle whose first cue starts at 5.000s.
    let srt = "1\r\n00:00:05,000 --> 00:00:07,000\r\nHello subtitle\r\n";
    stack.setup_opensubtitles(srt).await;

    // Start a session asking for English subtitles: the server searches
    // OpenSubtitles (with the media's moviehash), downloads and attaches it.
    let created = stack
        .create_session_body(json!({
            "tmdb_id": 27205,
            "media_type": "movie",
            "subtitle_languages": ["en"]
        }))
        .await;
    let id = created["session_id"]
        .as_str()
        .expect("session id")
        .to_string();

    // The session response advertises the attached subtitle track.
    let tracks = created["subtitle_tracks"]
        .as_array()
        .expect("subtitle_tracks");
    assert_eq!(tracks.len(), 1, "one English track attached: {created}");
    assert_eq!(tracks[0]["language"], "en");
    assert_eq!(tracks[0]["default"], true, "first track is default");
    let playlist_url = tracks[0]["playlist_url"].as_str().expect("playlist_url");
    assert_eq!(playlist_url, format!("/api/v1/stream/{id}/sub_en_1.m3u8"));

    // The master playlist advertises the subtitle rendition for AVPlayer.
    let master = stack
        .get(&format!("/api/v1/stream/{id}/master.m3u8"))
        .await
        .text()
        .await
        .expect("master");
    assert!(
        master.contains("#EXT-X-MEDIA:TYPE=SUBTITLES"),
        "master must advertise subtitles:\n{master}"
    );
    assert!(master.contains("LANGUAGE=\"en\""), "master:\n{master}");
    assert!(master.contains("SUBTITLES=\"subs\""), "variant:\n{master}");

    // The served WebVTT has the cue at its original 5.000s (hash-matched, so
    // no fps rescale).
    let vtt = stack
        .get(&format!("/api/v1/stream/{id}/sub_en_1.vtt"))
        .await
        .text()
        .await
        .expect("vtt");
    assert!(vtt.starts_with("WEBVTT"), "vtt:\n{vtt}");
    assert!(vtt.contains("Hello subtitle"));
    assert!(
        (first_cue_start_secs(&vtt) - 5.0).abs() < 0.01,
        "cue at 5s:\n{vtt}"
    );

    // Nudge the subtitle +2000ms via the manual offset endpoint (addressed by
    // language). The cue moves from 5.000s to 7.000s.
    let response = stack
        .client
        .post(format!(
            "{}/api/v1/stream/{id}/subtitles/en/offset",
            stack.base
        ))
        .header("x-api-key", API_KEY)
        .json(&json!({ "ms": 2000 }))
        .send()
        .await
        .expect("POST offset");
    assert_eq!(response.status(), 200);
    let updated: Value = response.json().await.expect("offset json");
    assert_eq!(updated["language"], "en");

    let vtt = stack
        .get(&format!("/api/v1/stream/{id}/sub_en_1.vtt"))
        .await
        .text()
        .await
        .expect("vtt after offset");
    assert!(vtt.starts_with("WEBVTT"), "still valid vtt:\n{vtt}");
    assert!(
        (first_cue_start_secs(&vtt) - 7.0).abs() < 0.01,
        "cue moved to 7s:\n{vtt}"
    );

    // Offset is absolute, not cumulative: re-sending +2000 keeps it at 7s.
    let response = stack
        .client
        .post(format!(
            "{}/api/v1/stream/{id}/subtitles/en/offset",
            stack.base
        ))
        .header("x-api-key", API_KEY)
        .json(&json!({ "ms": 2000 }))
        .send()
        .await
        .expect("POST offset again");
    assert_eq!(response.status(), 200);
    let vtt = stack
        .get(&format!("/api/v1/stream/{id}/sub_en_1.vtt"))
        .await
        .text()
        .await
        .expect("vtt after repeat offset");
    assert!(
        (first_cue_start_secs(&vtt) - 7.0).abs() < 0.01,
        "absolute offset does not compound:\n{vtt}"
    );

    // Unknown language → 404.
    let response = stack
        .client
        .post(format!(
            "{}/api/v1/stream/{id}/subtitles/zz/offset",
            stack.base
        ))
        .header("x-api-key", API_KEY)
        .json(&json!({ "ms": 100 }))
        .send()
        .await
        .expect("POST offset unknown lang");
    assert_eq!(response.status(), 404);
}
