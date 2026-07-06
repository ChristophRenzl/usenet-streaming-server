//! Download-job end-to-end tests over mock NNTP + mock indexer/TMDB:
//! completion (bytes identical, partial gone), mid-download failure,
//! cancellation, disk playback of finished downloads and startup recovery.

use std::path::Path;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::time::Instant;
use usenet_streaming_server::config::AppConfig;
use usenet_streaming_server::db;
use usenet_streaming_server::state::AppState;
use uuid::Uuid;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::support::{
    add_yenc_file, build_nzb_xml, ffmpeg_available, generate_media, spawn_app, xorshift_bytes,
    MockNntp,
};

const API_KEY: &str = "test-api-key";
const RELEASE_TITLE: &str = "Inception.2010.1080p.BluRay.x264-TEST";

fn base_config(download_dir: &Path, session_dir: &Path) -> AppConfig {
    let mut config = AppConfig::default();
    config.auth.api_key = API_KEY.into();
    config.storage.download_dir = download_dir.to_str().expect("utf-8 dir").to_string();
    config.storage.session_dir = Some(session_dir.to_str().expect("utf-8 dir").to_string());
    config
}

fn rss_with_release(indexer_base: &str) -> String {
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0" xmlns:newznab="http://www.newznab.com/DTD/2010/feeds/attributes/">
  <channel>
    <title>mock</title>
    <item>
      <title>{RELEASE_TITLE}</title>
      <guid isPermaLink="true">https://indexer.example/details/test-guid</guid>
      <link>{indexer_base}/getnzb/test.nzb</link>
      <pubDate>Wed, 03 Jun 2026 12:30:00 +0000</pubDate>
      <newznab:attr name="size" value="734003200"/>
    </item>
  </channel>
</rss>"#
    )
}

/// The distribution chain for download tests: media yEnc'd onto a mock NNTP
/// server, an NZB behind a mock indexer, TMDB mocked, real app on a socket.
struct DownloadStack {
    base: String,
    client: reqwest::Client,
    nntp: MockNntp,
    download_dir: tempfile::TempDir,
    _session_dir: tempfile::TempDir,
    _tmdb: MockServer,
    _indexer: MockServer,
    _server: tokio::task::JoinHandle<()>,
}

async fn download_stack(media: &[u8], part_size: usize) -> DownloadStack {
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

    let download_dir = tempfile::tempdir().expect("download dir");
    let session_dir = tempfile::tempdir().expect("session dir");
    let config = base_config(download_dir.path(), session_dir.path());
    let state = AppState::for_tests(config)
        .await
        .expect("state")
        .with_tmdb_base_url(&tmdb.uri());
    let (base, _state, server) = spawn_app(state).await;
    let client = reqwest::Client::new();

    for (path, body) in [
        ("settings/app", json!({ "tmdb_api_key": "tmdb-key" })),
        (
            "settings/indexers",
            json!({ "name": "mock", "base_url": indexer.uri(), "api_key": "indexer-key" }),
        ),
        (
            "settings/providers",
            json!({
                "name": "mock-nntp",
                "host": "127.0.0.1",
                "port": nntp.addr().port(),
                "use_tls": false,
                "max_connections": 8,
                "priority": 0
            }),
        ),
    ] {
        let request = if path == "settings/app" {
            client.put(format!("{base}/api/v1/{path}"))
        } else {
            client.post(format!("{base}/api/v1/{path}"))
        };
        let response = request
            .header("x-api-key", API_KEY)
            .json(&body)
            .send()
            .await
            .expect("configure");
        assert_eq!(response.status(), 200, "configuring {path} failed");
    }

    DownloadStack {
        base,
        client,
        nntp,
        download_dir,
        _session_dir: session_dir,
        _tmdb: tmdb,
        _indexer: indexer,
        _server: server,
    }
}

impl DownloadStack {
    async fn get(&self, path: &str) -> reqwest::Response {
        self.client
            .get(format!("{}{path}", self.base))
            .header("x-api-key", API_KEY)
            .send()
            .await
            .expect("GET")
    }

    /// POST /downloads for the mocked movie; returns the 202 body.
    async fn create_download(&self) -> Value {
        let response = self
            .client
            .post(format!("{}/api/v1/downloads", self.base))
            .header("x-api-key", API_KEY)
            .json(&json!({ "tmdb_id": 27205, "media_type": "movie" }))
            .send()
            .await
            .expect("POST download");
        let status = response.status();
        let body: Value = response.json().await.expect("download json");
        assert_eq!(status, 202, "download creation failed: {body}");
        body
    }

    /// Poll until the job reaches one of `accept`; panic on any other
    /// terminal status or timeout.
    async fn wait_for_download(&self, id: &str, accept: &[&str], timeout: Duration) -> Value {
        let deadline = Instant::now() + timeout;
        loop {
            let response = self.get(&format!("/api/v1/downloads/{id}")).await;
            assert_eq!(response.status(), 200);
            let body: Value = response.json().await.expect("download json");
            let status = body["status"].as_str().expect("status").to_string();
            if accept.contains(&status.as_str()) {
                return body;
            }
            assert!(
                !["complete", "failed", "cancelled"].contains(&status.as_str()),
                "download reached unexpected terminal status {status}: {body}"
            );
            assert!(
                Instant::now() < deadline,
                "download did not reach {accept:?} in {timeout:?} (currently {status})"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }

    fn partial_files(&self) -> Vec<std::path::PathBuf> {
        std::fs::read_dir(self.download_dir.path())
            .expect("read download dir")
            .filter_map(|entry| entry.ok())
            .map(|entry| entry.path())
            .filter(|path| path.extension().is_some_and(|ext| ext == "partial"))
            .collect()
    }
}

// ---- Happy path -----------------------------------------------------------------

#[tokio::test]
async fn download_completes_with_identical_bytes_and_delete_removes_file() {
    // > one 4 MiB chunk so the copy loop iterates and reports progress.
    let payload = xorshift_bytes(5 * 1024 * 1024 + 37);
    let stack = download_stack(&payload, 64 * 1024).await;

    let created = stack.create_download().await;
    let id = created["id"].as_str().expect("id").to_string();
    assert_eq!(created["status"], "pending");
    assert_eq!(created["release_title"], RELEASE_TITLE);
    assert_eq!(created["media_type"], "movie");
    assert_eq!(created["tmdb_id"], 27205);
    assert_eq!(created["progress_bytes"], 0);

    let done = stack
        .wait_for_download(&id, &["complete"], Duration::from_secs(30))
        .await;
    assert_eq!(done["total_bytes"], payload.len() as i64);
    assert_eq!(done["progress_bytes"], payload.len() as i64);
    assert_eq!(done["percent"], 100.0);
    assert!(done["error"].is_null());

    // The file carries the inner (yEnc) name, lives in the download dir and
    // is byte-identical; the .partial is gone.
    let file_path = done["file_path"].as_str().expect("file_path");
    assert_eq!(
        Path::new(file_path).file_name().unwrap().to_str(),
        Some("movie.mkv")
    );
    assert!(Path::new(file_path).starts_with(stack.download_dir.path()));
    assert_eq!(std::fs::read(file_path).expect("read download"), payload);
    assert!(stack.partial_files().is_empty(), "no .partial may remain");

    // Listed newest-first with the same data.
    let response = stack.get("/api/v1/downloads").await;
    assert_eq!(response.status(), 200);
    let list: Value = response.json().await.expect("list json");
    assert_eq!(list.as_array().expect("array").len(), 1);
    assert_eq!(list[0]["id"], id.as_str());
    assert_eq!(list[0]["status"], "complete");

    // Deleting a finished download removes the row and (on request) the file.
    let response = stack
        .client
        .delete(format!(
            "{}/api/v1/downloads/{id}?delete_file=true",
            stack.base
        ))
        .header("x-api-key", API_KEY)
        .send()
        .await
        .expect("DELETE");
    assert_eq!(response.status(), 204);
    assert!(!Path::new(file_path).exists(), "file must be deleted");
    let response = stack.get(&format!("/api/v1/downloads/{id}")).await;
    assert_eq!(response.status(), 404);
}

// ---- Failure ---------------------------------------------------------------------

#[tokio::test]
async fn download_fails_when_a_segment_goes_missing_mid_copy() {
    let payload = xorshift_bytes(5 * 1024 * 1024);
    let stack = download_stack(&payload, 64 * 1024).await;

    // Slow the mock down so the job cannot outrun the article removal below
    // (5 MiB / 64 KiB = 80 segments; segment 70 is far beyond any readahead).
    stack.nntp.set_delay(Some(Duration::from_millis(100)));
    let created = stack.create_download().await;
    let id = created["id"].as_str().expect("id").to_string();
    stack.nntp.remove_article("media-70@mock");
    stack.nntp.set_delay(None);

    let failed = stack
        .wait_for_download(&id, &["failed"], Duration::from_secs(30))
        .await;
    let error = failed["error"].as_str().expect("error text");
    assert!(
        error.contains("missing"),
        "error should name the missing article: {error}"
    );
    assert!(stack.partial_files().is_empty(), ".partial must be removed");
    assert!(
        !stack.download_dir.path().join("movie.mkv").exists(),
        "no final file may appear"
    );
}

// ---- Cancellation ------------------------------------------------------------------

#[tokio::test]
async fn cancelling_a_running_download_keeps_the_row_and_removes_the_partial() {
    let payload = xorshift_bytes(3 * 1024 * 1024);
    let stack = download_stack(&payload, 64 * 1024).await;

    // Keep the job busy long enough to cancel it mid-copy.
    stack.nntp.set_delay(Some(Duration::from_millis(100)));
    let created = stack.create_download().await;
    let id = created["id"].as_str().expect("id").to_string();
    stack
        .wait_for_download(&id, &["downloading"], Duration::from_secs(15))
        .await;

    let response = stack
        .client
        .delete(format!("{}/api/v1/downloads/{id}", stack.base))
        .header("x-api-key", API_KEY)
        .send()
        .await
        .expect("DELETE");
    assert_eq!(response.status(), 204);
    stack.nntp.set_delay(None);

    // The row is kept as `cancelled`, nothing remains on disk.
    let response = stack.get(&format!("/api/v1/downloads/{id}")).await;
    assert_eq!(response.status(), 200);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["status"], "cancelled");
    assert!(stack.partial_files().is_empty(), ".partial must be removed");
    assert!(!stack.download_dir.path().join("movie.mkv").exists());
}

// ---- Disk playback -------------------------------------------------------------------

/// Register a finished download row pointing at `file_path`.
async fn seed_completed_download(
    state: &AppState,
    id: &Uuid,
    tmdb_id: i64,
    file_path: &Path,
) -> anyhow::Result<()> {
    let size = std::fs::metadata(file_path)?.len() as i64;
    sqlx::query(
        "INSERT INTO downloads
             (id, user_id, tmdb_id, media_type, season, episode, release_title, nzb_url,
              status, progress_bytes, total_bytes, file_path)
         VALUES (?, 1, ?, 'movie', NULL, NULL, ?, 'https://indexer.example/x.nzb',
                 'complete', ?, ?, ?)",
    )
    .bind(id.to_string())
    .bind(tmdb_id)
    .bind(RELEASE_TITLE)
    .bind(size)
    .bind(size)
    .bind(file_path.to_str().expect("utf-8 path"))
    .execute(&state.db)
    .await?;
    Ok(())
}

#[tokio::test]
async fn completed_downloads_play_from_disk_without_nntp() {
    if !ffmpeg_available() {
        eprintln!("skipping completed_downloads_play_from_disk_without_nntp: ffmpeg not found");
        return;
    }
    let media_dir = tempfile::tempdir().expect("media dir");
    let media_path = generate_media(media_dir.path(), 8, 24, &["-c:a", "ac3", "-b:a", "96k"])
        .expect("generate test media");
    let media = std::fs::read(&media_path).expect("read media");

    // App with NO indexers, NO NNTP providers, NO TMDB key configured.
    let download_dir = tempfile::tempdir().expect("download dir");
    let session_dir = tempfile::tempdir().expect("session dir");
    let stored = download_dir.path().join("Inception.2010.mkv");
    std::fs::copy(&media_path, &stored).expect("copy media into download dir");

    let config = base_config(download_dir.path(), session_dir.path());
    let state = AppState::for_tests(config).await.expect("state");
    let (base, state, _server) = spawn_app(state).await;
    let client = reqwest::Client::new();

    let download_id = Uuid::new_v4();
    seed_completed_download(&state, &download_id, 27205, &stored)
        .await
        .expect("seed download");

    // A stored watch position must surface as resume_position_secs.
    let response = client
        .post(format!("{base}/api/v1/history"))
        .header("x-api-key", API_KEY)
        .json(&json!({ "tmdb_id": 27205, "media_type": "movie", "position_secs": 4.25 }))
        .send()
        .await
        .expect("POST history");
    assert_eq!(response.status(), 200);

    // Session creation by TMDB identity finds the finished download.
    let response = client
        .post(format!("{base}/api/v1/stream/sessions"))
        .header("x-api-key", API_KEY)
        .json(&json!({ "tmdb_id": 27205, "media_type": "movie" }))
        .send()
        .await
        .expect("POST session");
    assert_eq!(response.status(), 200);
    let created: Value = response.json().await.expect("session json");
    assert_eq!(created["source"], "disk");
    assert_eq!(created["container"], "mkv");
    assert_eq!(created["chosen_release"]["raw"]["title"], RELEASE_TITLE);
    assert_eq!(
        created["candidates"].as_array().expect("candidates").len(),
        0
    );
    assert_eq!(created["resume_position_secs"], 4.25);
    let duration = created["duration_secs"].as_f64().expect("duration");
    assert!((6.0..10.0).contains(&duration), "duration {duration}");
    let session_id = created["session_id"].as_str().expect("id").to_string();

    // Raw byte-range access serves the on-disk bytes untouched.
    let response = client
        .get(format!("{base}/api/v1/stream/{session_id}/raw"))
        .header("x-api-key", API_KEY)
        .send()
        .await
        .expect("GET raw");
    assert_eq!(response.status(), 200);
    assert_eq!(&response.bytes().await.expect("raw body")[..], &media[..]);

    // Position reports through the session update the history.
    let response = client
        .put(format!("{base}/api/v1/stream/{session_id}/position"))
        .header("x-api-key", API_KEY)
        .json(&json!({ "position_secs": 6.5 }))
        .send()
        .await
        .expect("PUT position");
    assert_eq!(response.status(), 200);
    let updated: Value = response.json().await.expect("position json");
    assert_eq!(updated["position_secs"], 6.5);
    assert!(
        updated["duration_secs"].as_f64().is_some(),
        "duration comes from the session probe"
    );

    let response = client
        .get(format!("{base}/api/v1/history"))
        .header("x-api-key", API_KEY)
        .send()
        .await
        .expect("GET history");
    let history: Value = response.json().await.expect("history json");
    assert_eq!(history.as_array().expect("array").len(), 1);
    assert_eq!(history[0]["position_secs"], 6.5);

    // force_nntp bypasses the disk shortcut — and fails here because no
    // indexer is configured.
    let response = client
        .post(format!("{base}/api/v1/stream/sessions"))
        .header("x-api-key", API_KEY)
        .json(&json!({ "tmdb_id": 27205, "media_type": "movie", "force_nntp": true }))
        .send()
        .await
        .expect("POST session force_nntp");
    assert_eq!(response.status(), 400, "force_nntp must skip disk playback");

    // Direct playback of a specific download id.
    let response = client
        .post(format!("{base}/api/v1/stream/sessions"))
        .header("x-api-key", API_KEY)
        .json(&json!({ "download_id": download_id }))
        .send()
        .await
        .expect("POST session by download_id");
    assert_eq!(response.status(), 200);
    let by_id: Value = response.json().await.expect("session json");
    assert_eq!(by_id["source"], "disk");

    // Unknown download id is a 404.
    let response = client
        .post(format!("{base}/api/v1/stream/sessions"))
        .header("x-api-key", API_KEY)
        .json(&json!({ "download_id": Uuid::new_v4() }))
        .send()
        .await
        .expect("POST session unknown download");
    assert_eq!(response.status(), 404);

    // Clean up the ffmpeg sessions.
    for id in [
        session_id,
        by_id["session_id"].as_str().expect("id").to_string(),
    ] {
        let response = client
            .delete(format!("{base}/api/v1/stream/{id}"))
            .header("x-api-key", API_KEY)
            .send()
            .await
            .expect("DELETE session");
        assert_eq!(response.status(), 204);
    }
}

// ---- Startup recovery -------------------------------------------------------------

#[tokio::test]
async fn interrupted_downloads_are_failed_on_startup() {
    let dir = tempfile::tempdir().expect("dir");
    let db_path = dir.path().join("recovery.db");
    let db_path = db_path.to_str().expect("utf-8 path");

    // Seed a job that was mid-download when the "previous server" stopped.
    let pool = db::connect(db_path).await.expect("connect");
    sqlx::query(
        "INSERT INTO downloads
             (id, user_id, tmdb_id, media_type, release_title, nzb_url, status,
              progress_bytes, total_bytes)
         VALUES ('stuck', 1, 1, 'movie', 'Movie', 'https://x/a.nzb', 'downloading', 10, 100)",
    )
    .execute(&pool)
    .await
    .expect("seed");
    pool.close().await;

    // Booting the real state over that database must fail the job.
    let mut config = AppConfig::default();
    config.auth.api_key = API_KEY.into();
    config.database.path = db_path.to_string();
    let state = AppState::new(config).await.expect("state");
    let row = db::downloads::get(&state.db, "stuck")
        .await
        .expect("get")
        .expect("row kept");
    assert_eq!(row.status, "failed");
    assert_eq!(row.error.as_deref(), Some("interrupted by server restart"));
}
