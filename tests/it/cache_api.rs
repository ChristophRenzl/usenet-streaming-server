//! Stream-cache integration tests: cache-hit session creation (plays from
//! disk with `source == "cache"`, no indexer/NNTP needed), LRU stamping,
//! usage stats and clearing.

use std::path::Path;

use serde_json::{json, Value};
use usenet_streaming_server::config::AppConfig;
use usenet_streaming_server::db;
use usenet_streaming_server::state::AppState;
use uuid::Uuid;

use crate::support::{ffmpeg_available, generate_media, spawn_app};

const API_KEY: &str = "test-api-key";
const RELEASE_TITLE: &str = "Inception.2010.1080p.BluRay.x264-CACHED";

/// Register a completed cache-originated entry pointing at `file_path`.
async fn seed_cache_entry(
    state: &AppState,
    id: &Uuid,
    tmdb_id: i64,
    file_path: &Path,
) -> anyhow::Result<()> {
    let size = std::fs::metadata(file_path)?.len() as i64;
    sqlx::query(
        "INSERT INTO downloads
             (id, user_id, tmdb_id, media_type, season, episode, release_title, nzb_url,
              status, progress_bytes, total_bytes, file_path, origin)
         VALUES (?, 1, ?, 'movie', NULL, NULL, ?, 'https://indexer.example/x.nzb',
                 'complete', ?, ?, ?, 'cache')",
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
async fn cache_hit_plays_from_disk_and_clearing_respects_live_sessions() {
    if !ffmpeg_available() {
        eprintln!("skipping cache_hit_plays_from_disk_and_clearing_respects_live_sessions: ffmpeg not found");
        return;
    }
    let media_dir = tempfile::tempdir().expect("media dir");
    let media_path = generate_media(media_dir.path(), 8, 24, &["-c:a", "ac3", "-b:a", "96k"])
        .expect("generate test media");

    // App with NO indexers, NO NNTP providers, NO TMDB key configured — a
    // cache hit must not need any of them.
    let download_dir = tempfile::tempdir().expect("download dir");
    let cache_dir = tempfile::tempdir().expect("cache dir");
    let session_dir = tempfile::tempdir().expect("session dir");
    let mut config = AppConfig::default();
    config.auth.api_key = API_KEY.into();
    config.storage.download_dir = download_dir.path().to_str().expect("utf-8").to_string();
    config.storage.session_dir = Some(session_dir.path().to_str().expect("utf-8").to_string());
    config.storage.cache_dir = Some(cache_dir.path().to_str().expect("utf-8").to_string());

    let cached_file = cache_dir.path().join("Inception.2010.mkv");
    std::fs::copy(&media_path, &cached_file).expect("copy media into cache dir");
    let size = std::fs::metadata(&cached_file).expect("metadata").len();

    let state = AppState::for_tests(config).await.expect("state");
    let (base, state, _server) = spawn_app(state).await;
    let client = reqwest::Client::new();

    let entry_id = Uuid::new_v4();
    seed_cache_entry(&state, &entry_id, 27205, &cached_file)
        .await
        .expect("seed cache entry");

    // Stats see the entry.
    let response = client
        .get(format!("{base}/api/v1/cache"))
        .header("x-api-key", API_KEY)
        .send()
        .await
        .expect("GET cache stats");
    assert_eq!(response.status(), 200);
    let stats: Value = response.json().await.expect("stats json");
    assert_eq!(stats["enabled"], true, "cache defaults to enabled");
    assert_eq!(stats["entry_count"], 1);
    assert_eq!(stats["used_bytes"], size);
    assert_eq!(
        stats["max_bytes"].as_u64(),
        Some(5000 * 1_000_000_000),
        "default cap is 5 TB"
    );

    // Session creation by TMDB identity hits the cache: served from disk,
    // reported as `cache` so clients can show "Preparing cached data".
    let response = client
        .post(format!("{base}/api/v1/stream/sessions"))
        .header("x-api-key", API_KEY)
        .json(&json!({ "tmdb_id": 27205, "media_type": "movie" }))
        .send()
        .await
        .expect("POST session");
    assert_eq!(response.status(), 200);
    let created: Value = response.json().await.expect("session json");
    assert_eq!(created["source"], "cache");
    assert_eq!(created["container"], "mkv");
    assert_eq!(created["chosen_release"]["raw"]["title"], RELEASE_TITLE);
    let session_id = created["session_id"].as_str().expect("id").to_string();

    // The cache hit stamped last_played_at (the LRU clock).
    let row = db::downloads::get(&state.db, &entry_id.to_string())
        .await
        .expect("get entry")
        .expect("entry row");
    assert_eq!(row.origin, "cache");
    assert!(
        row.last_played_at.is_some(),
        "cache hits must stamp last_played_at"
    );

    // A pinned release_guid skips the cache shortcut — the request fails
    // here because a live indexer search would be needed (none configured).
    let response = client
        .post(format!("{base}/api/v1/stream/sessions"))
        .header("x-api-key", API_KEY)
        .json(&json!({
            "tmdb_id": 27205,
            "media_type": "movie",
            "release_guid": "some-other-release"
        }))
        .send()
        .await
        .expect("POST pinned session");
    assert_eq!(
        response.status(),
        400,
        "a guid pin must bypass the cache shortcut"
    );

    // Clearing while the entry is being played keeps it.
    let response = client
        .delete(format!("{base}/api/v1/cache"))
        .header("x-api-key", API_KEY)
        .send()
        .await
        .expect("DELETE cache (playing)");
    assert_eq!(response.status(), 200);
    let cleared: Value = response.json().await.expect("clear json");
    assert_eq!(cleared["removed"], 0, "playing entries are never removed");
    assert!(cached_file.exists());

    // End the session; clearing now removes the entry and its file.
    let response = client
        .delete(format!("{base}/api/v1/stream/{session_id}"))
        .header("x-api-key", API_KEY)
        .send()
        .await
        .expect("DELETE session");
    assert_eq!(response.status(), 204);

    let response = client
        .delete(format!("{base}/api/v1/cache"))
        .header("x-api-key", API_KEY)
        .send()
        .await
        .expect("DELETE cache");
    assert_eq!(response.status(), 200);
    let cleared: Value = response.json().await.expect("clear json");
    assert_eq!(cleared["removed"], 1);
    assert!(!cached_file.exists(), "cached file must be deleted");
    assert!(
        db::downloads::get(&state.db, &entry_id.to_string())
            .await
            .expect("get entry")
            .is_none(),
        "cache row must be deleted"
    );

    let response = client
        .get(format!("{base}/api/v1/cache"))
        .header("x-api-key", API_KEY)
        .send()
        .await
        .expect("GET cache stats");
    let stats: Value = response.json().await.expect("stats json");
    assert_eq!(stats["entry_count"], 0);
    assert_eq!(stats["used_bytes"], 0);
}

#[tokio::test]
async fn cache_settings_round_trip_through_the_app_settings() {
    let mut config = AppConfig::default();
    config.auth.api_key = API_KEY.into();
    let dir = tempfile::tempdir().expect("dir");
    config.storage.cache_dir = Some(dir.path().to_str().expect("utf-8").to_string());
    let state = AppState::for_tests(config).await.expect("state");
    let (base, _state, _server) = spawn_app(state).await;
    let client = reqwest::Client::new();

    // Defaults: enabled, 5000 GB.
    let response = client
        .get(format!("{base}/api/v1/settings/app"))
        .header("x-api-key", API_KEY)
        .send()
        .await
        .expect("GET settings");
    let settings: Value = response.json().await.expect("settings json");
    assert_eq!(settings["stream_cache_enabled"], true);
    assert_eq!(settings["stream_cache_max_gb"], 5000);

    // Update both; they read back and the cap reflects in the stats.
    let response = client
        .put(format!("{base}/api/v1/settings/app"))
        .header("x-api-key", API_KEY)
        .json(&json!({ "stream_cache_enabled": false, "stream_cache_max_gb": 750 }))
        .send()
        .await
        .expect("PUT settings");
    assert_eq!(response.status(), 200);
    let settings: Value = response.json().await.expect("settings json");
    assert_eq!(settings["stream_cache_enabled"], false);
    assert_eq!(settings["stream_cache_max_gb"], 750);

    let response = client
        .get(format!("{base}/api/v1/cache"))
        .header("x-api-key", API_KEY)
        .send()
        .await
        .expect("GET cache stats");
    let stats: Value = response.json().await.expect("stats json");
    assert_eq!(stats["enabled"], false);
    assert_eq!(stats["max_bytes"].as_u64(), Some(750 * 1_000_000_000));

    // A zero cap is rejected.
    let response = client
        .put(format!("{base}/api/v1/settings/app"))
        .header("x-api-key", API_KEY)
        .json(&json!({ "stream_cache_max_gb": 0 }))
        .send()
        .await
        .expect("PUT settings");
    assert_eq!(response.status(), 400);
}
