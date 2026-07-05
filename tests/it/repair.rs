//! par2 download-and-repair fallback: health verdict classification over mock
//! NNTP, the repair download job reconstructing a damaged payload byte-for-
//! byte, and the session-start behaviour matrix (Streamable -> 200,
//! Repairable-only -> 202 repairing, Unrecoverable -> 422).
//!
//! Tests that actually run par2 are gated on a `par2` binary being available
//! and skip (with an eprintln) otherwise; CI installs par2 so they run there.

use std::path::Path;
use std::time::Duration;

use serde_json::{json, Value};
use tokio::time::Instant;
use usenet_streaming_server::config::AppConfig;
use usenet_streaming_server::nntp::{NntpPool, NntpTimeouts, PoolOptions};
use usenet_streaming_server::nzb::{assess_release, parse_nzb, select_main, HealthVerdict};
use usenet_streaming_server::state::AppState;
use wiremock::matchers::{method, path, query_param};
use wiremock::{Mock, MockServer, ResponseTemplate};

use crate::support::{
    add_repairable_release, add_yenc_file, add_yenc_file_missing, build_nzb_xml, ffmpeg_available,
    generate_media, par2_available, spawn_app, xorshift_bytes, MockNntp,
};

const API_KEY: &str = "test-api-key";

fn fast_options() -> PoolOptions {
    PoolOptions {
        timeouts: NntpTimeouts {
            connect: Duration::from_secs(2),
            read: Duration::from_secs(5),
            write: Duration::from_secs(5),
        },
        ..PoolOptions::default()
    }
}

// ---- Health verdict classification over mock NNTP -------------------------------

/// All main-content segments present, no par2 needed -> Streamable.
#[tokio::test]
async fn verdict_streamable_when_all_present() {
    let server = MockNntp::start(None).await;
    let payload = xorshift_bytes(40 * 1024);
    let segments = add_yenc_file(&server, "media", &payload, 4 * 1024, "movie.mkv");
    let nzb_xml = build_nzb_xml(&[(r#"Rel [1/1] - "movie.mkv" yEnc"#.into(), segments)]);
    let nzb = parse_nzb(&nzb_xml).expect("parse");
    let main = select_main(&nzb).expect("main");

    let pool = NntpPool::with_options(vec![server.provider("p", 0, 4)], fast_options());
    let a = assess_release(&nzb, &main, &pool, 10)
        .await
        .expect("assess");
    assert_eq!(a.verdict, HealthVerdict::Streamable);
    assert!(a.health.ok);
}

/// ~10% of the main content missing but ample par2 recovery -> Repairable.
#[tokio::test]
async fn verdict_repairable_with_sufficient_par2() {
    let server = MockNntp::start(None).await;
    // 100 parts of 1 KiB; drop 10 spread across the file (endpoints kept so
    // the "first & last present" streaming rule still fails only on ratio).
    let payload = xorshift_bytes(100 * 1024);
    let missing: Vec<usize> = (1..=10).map(|k| k * 9).collect(); // 9,18,...,90
    let media_segments =
        add_yenc_file_missing(&server, "media", &payload, 1024, "movie.mkv", &missing);
    // A par2 file whose encoded size comfortably exceeds the missing payload
    // (~10 KiB missing; make the par2 "recovery" file ~30 KiB).
    let par2_blob = xorshift_bytes(30 * 1024);
    let par2_segments = add_yenc_file(&server, "par2", &par2_blob, 1024, "movie.mkv.par2");
    let nzb_xml = build_nzb_xml(&[
        (r#"Rel [1/1] - "movie.mkv" yEnc"#.into(), media_segments),
        (r#"Rel [1/1] - "movie.mkv.par2" yEnc"#.into(), par2_segments),
    ]);
    let nzb = parse_nzb(&nzb_xml).expect("parse");
    let main = select_main(&nzb).expect("main");

    let pool = NntpPool::with_options(vec![server.provider("p", 0, 4)], fast_options());
    let a = assess_release(&nzb, &main, &pool, 20)
        .await
        .expect("assess");
    assert!(!a.health.ok, "10% missing must not be streamable");
    assert_eq!(a.verdict, HealthVerdict::Repairable, "assessment: {a:?}");
    assert!(a.par2_recovery_bytes >= a.estimated_missing_bytes);
}

/// ~10% missing but NO par2 -> Unrecoverable.
#[tokio::test]
async fn verdict_unrecoverable_without_par2() {
    let server = MockNntp::start(None).await;
    let payload = xorshift_bytes(100 * 1024);
    let missing: Vec<usize> = (1..=10).map(|k| k * 9).collect();
    let media_segments =
        add_yenc_file_missing(&server, "media", &payload, 1024, "movie.mkv", &missing);
    let nzb_xml = build_nzb_xml(&[(r#"Rel [1/1] - "movie.mkv" yEnc"#.into(), media_segments)]);
    let nzb = parse_nzb(&nzb_xml).expect("parse");
    let main = select_main(&nzb).expect("main");

    let pool = NntpPool::with_options(vec![server.provider("p", 0, 4)], fast_options());
    let a = assess_release(&nzb, &main, &pool, 20)
        .await
        .expect("assess");
    assert_eq!(a.verdict, HealthVerdict::Unrecoverable, "assessment: {a:?}");
    assert_eq!(a.par2_recovery_bytes, 0);
}

// ---- Full-stack repair harness --------------------------------------------------

const RELEASE_TITLE: &str = "Test.Movie.2026.1080p.BluRay.x264-TEST";

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

struct RepairStack {
    base: String,
    client: reqwest::Client,
    download_dir: tempfile::TempDir,
    _session_dir: tempfile::TempDir,
    _nntp: MockNntp,
    _tmdb: MockServer,
    _indexer: MockServer,
    _server: tokio::task::JoinHandle<()>,
}

/// Build the full stack around an NZB (already registered on `nntp`).
async fn repair_stack(nntp: MockNntp, nzb_xml: String) -> RepairStack {
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

    for (p, body) in [
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
        let request = if p == "settings/app" {
            client.put(format!("{base}/api/v1/{p}"))
        } else {
            client.post(format!("{base}/api/v1/{p}"))
        };
        let response = request
            .header("x-api-key", API_KEY)
            .json(&body)
            .send()
            .await
            .expect("configure");
        assert_eq!(response.status(), 200, "configuring {p} failed");
    }

    RepairStack {
        base,
        client,
        download_dir,
        _session_dir: session_dir,
        _nntp: nntp,
        _tmdb: tmdb,
        _indexer: indexer,
        _server: server,
    }
}

impl RepairStack {
    async fn get(&self, path: &str) -> reqwest::Response {
        self.client
            .get(format!("{}{path}", self.base))
            .header("x-api-key", API_KEY)
            .send()
            .await
            .expect("GET")
    }

    async fn post_session(&self, body: Value) -> reqwest::Response {
        self.client
            .post(format!("{}/api/v1/stream/sessions", self.base))
            .header("x-api-key", API_KEY)
            .json(&body)
            .send()
            .await
            .expect("POST session")
    }

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
                !["complete", "failed", "cancelled"].contains(&status.as_str())
                    || accept.contains(&status.as_str()),
                "download reached unexpected terminal status {status}: {body}"
            );
            assert!(
                Instant::now() < deadline,
                "download did not reach {accept:?} in {timeout:?} (currently {status})"
            );
            tokio::time::sleep(Duration::from_millis(100)).await;
        }
    }
}

// ---- Repair download job reconstructs the payload -------------------------------

#[tokio::test]
async fn repair_job_reconstructs_damaged_payload() {
    if !par2_available() {
        eprintln!("skipping repair_job_reconstructs_damaged_payload: par2 not installed");
        return;
    }
    // 500 KiB payload in 4 KiB parts (~125 parts); drop ~10% of them. 30%
    // par2 redundancy comfortably covers a 10% loss.
    let payload = xorshift_bytes(500 * 1024);
    let part_size = 4 * 1024;
    let total_parts = payload.len().div_ceil(part_size);
    let missing: Vec<usize> = (0..total_parts).filter(|i| i % 10 == 3).collect();
    assert!(!missing.is_empty());

    let nntp = MockNntp::start(None).await;
    let files = add_repairable_release(
        &nntp,
        &payload,
        part_size,
        "movie.mkv",
        &missing,
        30, // redundancy %
    );
    let nzb_xml = build_nzb_xml(&files);
    let stack = repair_stack(nntp, nzb_xml).await;

    // The release is repairable-only: session start returns 202 repairing.
    let response = stack
        .post_session(json!({ "tmdb_id": 27205, "media_type": "movie" }))
        .await;
    assert_eq!(response.status(), 202, "repairable release must 202");
    let body: Value = response.json().await.expect("repairing json");
    assert_eq!(body["status"], "repairing");
    let download_id = body["download_id"]
        .as_str()
        .expect("download_id")
        .to_string();
    assert_eq!(body["release_title"], RELEASE_TITLE);
    assert!(!body["candidates"]
        .as_array()
        .expect("candidates")
        .is_empty());

    // The repair job runs to completion and the final file is byte-identical.
    let done = stack
        .wait_for_download(&download_id, &["complete"], Duration::from_secs(60))
        .await;
    assert_eq!(done["phase"], "complete");
    let file_path = done["file_path"].as_str().expect("file_path");
    assert!(Path::new(file_path).starts_with(stack.download_dir.path()));
    assert_eq!(
        std::fs::read(file_path).expect("read repaired media"),
        payload,
        "repaired file must match the original payload byte-for-byte"
    );

    // No .repair-* working dir may remain.
    let leftovers: Vec<_> = std::fs::read_dir(stack.download_dir.path())
        .expect("read dir")
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().starts_with(".repair-"))
        .collect();
    assert!(leftovers.is_empty(), "repair work dir must be cleaned up");
}

// ---- Session-start behaviour matrix ---------------------------------------------

#[tokio::test]
async fn session_start_202_repairing_when_only_repairable() {
    // A synthetic repairable release (no real par2 needed for the 202 verdict:
    // the assessment only sizes par2 recovery vs missing payload). 100 parts,
    // health sampled at 10 evenly-spaced indices [0,11,22,..,99]; drop two of
    // the sampled interior points so the API's size-10 sample sees 20% missing
    // (repairable, <=30%) while endpoints stay present.
    let payload = xorshift_bytes(100 * 1024);
    let missing: Vec<usize> = vec![11, 22];

    let nntp = MockNntp::start(None).await;
    let media_segments =
        add_yenc_file_missing(&nntp, "media", &payload, 1024, "movie.mkv", &missing);
    let par2_blob = xorshift_bytes(30 * 1024);
    let par2_segments = add_yenc_file(&nntp, "par2", &par2_blob, 1024, "movie.mkv.par2");
    let nzb_xml = build_nzb_xml(&[
        (r#"Rel [1/1] - "movie.mkv" yEnc"#.into(), media_segments),
        (r#"Rel [1/1] - "movie.mkv.par2" yEnc"#.into(), par2_segments),
    ]);
    let stack = repair_stack(nntp, nzb_xml).await;

    let response = stack
        .post_session(json!({ "tmdb_id": 27205, "media_type": "movie" }))
        .await;
    assert_eq!(response.status(), 202);
    let body: Value = response.json().await.expect("json");
    assert_eq!(body["status"], "repairing");
    assert!(body["download_id"].as_str().is_some());
}

#[tokio::test]
async fn session_start_422_when_unrecoverable() {
    // Heavily damaged, no par2 -> Unrecoverable -> 422.
    let payload = xorshift_bytes(100 * 1024);
    let missing: Vec<usize> = (0..100).filter(|i| i % 2 == 0).collect(); // 50% gone
    let nntp = MockNntp::start(None).await;
    let media_segments =
        add_yenc_file_missing(&nntp, "media", &payload, 1024, "movie.mkv", &missing);
    let nzb_xml = build_nzb_xml(&[(r#"Rel [1/1] - "movie.mkv" yEnc"#.into(), media_segments)]);
    let stack = repair_stack(nntp, nzb_xml).await;

    let response = stack
        .post_session(json!({ "tmdb_id": 27205, "media_type": "movie" }))
        .await;
    assert_eq!(response.status(), 422, "unrecoverable must 422");
}

#[tokio::test]
async fn session_start_200_when_streamable() {
    if !ffmpeg_available() {
        eprintln!("skipping session_start_200_when_streamable: ffmpeg/ffprobe not found");
        return;
    }
    // A fully-present real media file -> Streamable -> 200 session (no repair).
    let media_dir = tempfile::tempdir().expect("media dir");
    let media_path = generate_media(media_dir.path(), 6, 24, &["-c:a", "ac3", "-b:a", "96k"])
        .expect("generate test media");
    let media = std::fs::read(&media_path).expect("read media");

    let nntp = MockNntp::start(None).await;
    let segments = add_yenc_file(&nntp, "media", &media, 64 * 1024, "movie.mkv");
    let nzb_xml = build_nzb_xml(&[(r#"Rel [1/1] - "movie.mkv" yEnc"#.into(), segments)]);
    let stack = repair_stack(nntp, nzb_xml).await;

    let response = stack
        .post_session(json!({ "tmdb_id": 27205, "media_type": "movie" }))
        .await;
    let status = response.status();
    let body: Value = response.json().await.expect("json");
    assert_eq!(status, 200, "streamable release must 200: {body}");
    assert_eq!(body["source"], "nntp");
    assert!(body["session_id"].as_str().is_some());

    // Clean up the ffmpeg session.
    let id = body["session_id"].as_str().unwrap();
    let _ = stack
        .client
        .delete(format!("{}/api/v1/stream/{id}", stack.base))
        .header("x-api-key", API_KEY)
        .send()
        .await;
}
