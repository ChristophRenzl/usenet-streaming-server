//! API-level tests for the settings endpoints, driven through the full
//! router (including the auth middleware) via axum-test.

use axum_test::TestServer;
use serde_json::{json, Value};
use usenet_streaming_server::{
    api, config::AppConfig, db::preferences::Preferences, release::parse::Resolution,
    state::AppState,
};

const API_KEY: &str = "test-api-key";

async fn server() -> TestServer {
    let mut config = AppConfig::default();
    config.auth.api_key = API_KEY.into();
    let state = AppState::for_tests(config).await.expect("test state");
    TestServer::new(api::router(state))
}

#[tokio::test]
async fn preferences_require_auth() {
    let server = server().await;
    let response = server.get("/api/v1/settings/preferences").await;
    assert_eq!(response.status_code(), 401);
}

#[tokio::test]
async fn preferences_round_trip() {
    let server = server().await;

    // Defaults seeded by the migration.
    let response = server
        .get("/api/v1/settings/preferences")
        .add_header("x-api-key", API_KEY)
        .await;
    assert_eq!(response.status_code(), 200);
    let defaults: Preferences = response.json();
    assert_eq!(defaults.preferred_resolution, Resolution::R1080p);
    assert_eq!(defaults.max_resolution, Resolution::R2160p);
    assert!(defaults.blocked_terms.contains(&"CAM".to_string()));

    // Update everything and read it back.
    let mut updated = defaults.clone();
    updated.preferred_resolution = Resolution::R720p;
    updated.max_resolution = Resolution::R1080p;
    updated.preferred_video_codecs = vec!["hevc".into()];
    updated.preferred_audio_codecs = vec!["opus".into()];
    updated.max_size_bytes = Some(8_000_000_000);
    updated.language = "de".into();
    updated.allowed_terms = vec!["REMUX".into()];
    updated.blocked_terms = vec!["CAM".into(), "SCREENER".into()];

    let response = server
        .put("/api/v1/settings/preferences")
        .add_header("x-api-key", API_KEY)
        .json(&updated)
        .await;
    assert_eq!(response.status_code(), 200);
    let saved: Preferences = response.json();
    assert_eq!(saved, updated);

    let response = server
        .get("/api/v1/settings/preferences")
        .add_header("x-api-key", API_KEY)
        .await;
    let reread: Preferences = response.json();
    assert_eq!(reread, updated);
}

#[tokio::test]
async fn preferences_reject_impossible_resolution_combo() {
    let server = server().await;
    let response = server
        .put("/api/v1/settings/preferences")
        .add_header("x-api-key", API_KEY)
        .json(&json!({
            "preferred_resolution": "2160p",
            "max_resolution": "720p",
            "preferred_video_codecs": [],
            "preferred_audio_codecs": [],
            "max_size_bytes": null,
            "language": "en",
            "allowed_terms": [],
            "blocked_terms": []
        }))
        .await;
    assert_eq!(response.status_code(), 400);
}

#[tokio::test]
async fn indexers_crud_and_missing_id_is_404() {
    let server = server().await;

    let response = server
        .post("/api/v1/settings/indexers")
        .add_header("x-api-key", API_KEY)
        .json(&json!({
            "name": "nzbgeek",
            "base_url": "https://api.nzbgeek.info",
            "api_key": "abc123"
        }))
        .await;
    assert_eq!(response.status_code(), 200);
    let created: Value = response.json();
    let id = created["id"].as_i64().expect("id");
    assert_eq!(created["enabled"], json!(true), "enabled defaults to true");

    let response = server
        .get("/api/v1/settings/indexers")
        .add_header("x-api-key", API_KEY)
        .await;
    let list: Value = response.json();
    assert_eq!(list.as_array().map(Vec::len), Some(1));

    let response = server
        .put(&format!("/api/v1/settings/indexers/{id}"))
        .add_header("x-api-key", API_KEY)
        .json(&json!({
            "name": "nzbgeek",
            "base_url": "https://api.nzbgeek.info",
            "api_key": "abc123",
            "enabled": false,
            "priority": 5
        }))
        .await;
    assert_eq!(response.status_code(), 200);
    let updated: Value = response.json();
    assert_eq!(updated["enabled"], json!(false));
    assert_eq!(updated["priority"], json!(5));

    let response = server
        .delete(&format!("/api/v1/settings/indexers/{id}"))
        .add_header("x-api-key", API_KEY)
        .await;
    assert_eq!(response.status_code(), 204);

    let response = server
        .delete(&format!("/api/v1/settings/indexers/{id}"))
        .add_header("x-api-key", API_KEY)
        .await;
    assert_eq!(response.status_code(), 404);
}

#[tokio::test]
async fn providers_crud() {
    let server = server().await;

    let response = server
        .post("/api/v1/settings/providers")
        .add_header("x-api-key", API_KEY)
        .json(&json!({
            "name": "eweka",
            "host": "news.eweka.nl",
            "username": "user",
            "password": "pass"
        }))
        .await;
    assert_eq!(response.status_code(), 200);
    let created: Value = response.json();
    let id = created["id"].as_i64().expect("id");
    assert_eq!(created["port"], json!(563), "TLS port default");
    assert_eq!(created["use_tls"], json!(true));
    assert_eq!(created["max_connections"], json!(10));

    let response = server
        .put(&format!("/api/v1/settings/providers/{id}"))
        .add_header("x-api-key", API_KEY)
        .json(&json!({
            "name": "eweka",
            "host": "news.eweka.nl",
            "port": 119,
            "use_tls": false,
            "username": "user",
            "password": "pass2",
            "max_connections": 20,
            "priority": 1,
            "enabled": false
        }))
        .await;
    assert_eq!(response.status_code(), 200);
    let updated: Value = response.json();
    assert_eq!(updated["port"], json!(119));
    assert_eq!(updated["enabled"], json!(false));

    let response = server
        .delete(&format!("/api/v1/settings/providers/{id}"))
        .add_header("x-api-key", API_KEY)
        .await;
    assert_eq!(response.status_code(), 204);
}

#[tokio::test]
async fn app_settings_mask_the_tmdb_key() {
    let server = server().await;

    // Unset by default.
    let response = server
        .get("/api/v1/settings/app")
        .add_header("x-api-key", API_KEY)
        .await;
    let settings: Value = response.json();
    assert_eq!(settings["tmdb_api_key"], Value::Null);

    let response = server
        .put("/api/v1/settings/app")
        .add_header("x-api-key", API_KEY)
        .json(&json!({ "tmdb_api_key": "abcdef7890wxyz" }))
        .await;
    assert_eq!(response.status_code(), 200);
    let settings: Value = response.json();
    assert_eq!(settings["tmdb_api_key"], json!("****wxyz"));

    // The full key must never come back.
    let response = server
        .get("/api/v1/settings/app")
        .add_header("x-api-key", API_KEY)
        .await;
    assert!(!response.text().contains("abcdef7890wxyz"));
}
