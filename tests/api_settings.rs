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
async fn api_key_rotation_keeps_both_keys_valid() {
    let server = server().await;

    // No override yet: the masked config key is reported.
    let response = server
        .get("/api/v1/settings/app")
        .add_header("x-api-key", API_KEY)
        .await;
    let settings: Value = response.json();
    assert_eq!(settings["api_key_override_active"], json!(false));
    assert_eq!(settings["api_key"], json!("****-key"));

    // Rotate the key.
    let new_key = "rotated-key-0123456789";
    let response = server
        .put("/api/v1/settings/app")
        .add_header("x-api-key", API_KEY)
        .json(&json!({ "api_key": new_key }))
        .await;
    assert_eq!(response.status_code(), 200);
    let settings: Value = response.json();
    assert_eq!(settings["api_key_override_active"], json!(true));
    assert_eq!(settings["api_key"], json!("****6789"));
    assert!(
        !response.text().contains(new_key),
        "full rotated key must never be echoed back"
    );

    // Both the bootstrap config key and the rotated key authenticate.
    let response = server
        .get("/api/v1/settings/preferences")
        .add_header("x-api-key", API_KEY)
        .await;
    assert_eq!(response.status_code(), 200, "config key stays valid");
    let response = server
        .get("/api/v1/settings/preferences")
        .add_header("x-api-key", new_key)
        .await;
    assert_eq!(response.status_code(), 200, "rotated key is accepted");

    // The rotated key also works via the ?apikey= query parameter.
    let response = server
        .get(&format!("/api/v1/settings/preferences?apikey={new_key}"))
        .await;
    assert_eq!(response.status_code(), 200);

    // Wrong keys are still rejected.
    let response = server
        .get("/api/v1/settings/preferences")
        .add_header("x-api-key", "wrong-key-0123456789")
        .await;
    assert_eq!(response.status_code(), 401);
}

#[tokio::test]
async fn api_key_rotation_rejects_short_keys() {
    let server = server().await;

    let response = server
        .put("/api/v1/settings/app")
        .add_header("x-api-key", API_KEY)
        .json(&json!({ "api_key": "too-short" }))
        .await;
    assert_eq!(response.status_code(), 400);

    // Nothing was stored: no override active, only the config key works.
    let response = server
        .get("/api/v1/settings/app")
        .add_header("x-api-key", API_KEY)
        .await;
    let settings: Value = response.json();
    assert_eq!(settings["api_key_override_active"], json!(false));
    let response = server
        .get("/api/v1/settings/preferences")
        .add_header("x-api-key", "too-short")
        .await;
    assert_eq!(response.status_code(), 401);
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

#[tokio::test]
async fn app_settings_trim_surrounding_whitespace_on_keys() {
    let server = server().await;

    // A pasted key/username with surrounding whitespace and a trailing newline
    // (e.g. copied from a webpage) must be stored trimmed, otherwise the
    // trailing bytes corrupt the OpenSubtitles Api-Key header (301/403).
    let response = server
        .put("/api/v1/settings/app")
        .add_header("x-api-key", API_KEY)
        .json(&json!({
            "tmdb_api_key": "  abcdef7890wxyz\n",
            "opensubtitles_api_key": "\topensubs1234\n",
            "opensubtitles_username": "  alice \n"
        }))
        .await;
    assert_eq!(response.status_code(), 200);
    let settings: Value = response.json();

    // If the key were stored untrimmed, mask_secret would surface the trailing
    // "\n" in the last four characters instead of the real tail.
    assert_eq!(
        settings["tmdb_api_key"],
        json!("****wxyz"),
        "tmdb key must be trimmed before storage"
    );
    assert_eq!(
        settings["opensubtitles_api_key"],
        json!("****1234"),
        "opensubtitles key must be trimmed before storage"
    );
    // Username is reported verbatim (not masked); it must come back trimmed.
    assert_eq!(
        settings["opensubtitles_username"],
        json!("alice"),
        "opensubtitles username must be trimmed before storage"
    );
}

#[tokio::test]
async fn opensubtitles_key_is_cleared_by_empty_string() {
    let server = server().await;

    // Store a per-user key.
    let response = server
        .put("/api/v1/settings/app")
        .add_header("x-api-key", API_KEY)
        .json(&json!({ "opensubtitles_api_key": "opensubs1234" }))
        .await;
    assert_eq!(response.status_code(), 200);
    let settings: Value = response.json();
    assert_eq!(settings["opensubtitles_api_key"], json!("****1234"));
    assert_eq!(
        settings["opensubtitles_configured"],
        json!(true),
        "a stored per-user key configures subtitles"
    );
    assert_eq!(settings["opensubtitles_api_key_source"], json!("user"));

    // An explicit empty string clears it.
    let response = server
        .put("/api/v1/settings/app")
        .add_header("x-api-key", API_KEY)
        .json(&json!({ "opensubtitles_api_key": "" }))
        .await;
    assert_eq!(response.status_code(), 200);
    let settings: Value = response.json();
    assert_eq!(
        settings["opensubtitles_api_key"],
        Value::Null,
        "cleared key reads back as not-set"
    );
    // No config default in the test config, so subtitles are now unconfigured.
    assert_eq!(settings["opensubtitles_configured"], json!(false));
    assert_eq!(settings["opensubtitles_api_key_source"], json!("none"));
}

#[tokio::test]
async fn opensubtitles_account_is_cleared_by_empty_strings() {
    let server = server().await;

    // Sign in with username + password.
    let response = server
        .put("/api/v1/settings/app")
        .add_header("x-api-key", API_KEY)
        .json(&json!({
            "opensubtitles_username": "alice",
            "opensubtitles_password": "s3cret"
        }))
        .await;
    assert_eq!(response.status_code(), 200);
    let settings: Value = response.json();
    assert_eq!(settings["opensubtitles_username"], json!("alice"));
    assert_eq!(settings["opensubtitles_password_set"], json!(true));

    // Clear the account: empty username + empty password.
    let response = server
        .put("/api/v1/settings/app")
        .add_header("x-api-key", API_KEY)
        .json(&json!({
            "opensubtitles_username": "",
            "opensubtitles_password": ""
        }))
        .await;
    assert_eq!(response.status_code(), 200);
    let settings: Value = response.json();
    assert_eq!(
        settings["opensubtitles_username"],
        Value::Null,
        "cleared username reads back as not-set"
    );
    assert_eq!(
        settings["opensubtitles_password_set"],
        json!(false),
        "cleared password reads back as not-set"
    );
}

#[tokio::test]
async fn tmdb_key_is_cleared_by_empty_string() {
    let server = server().await;

    let response = server
        .put("/api/v1/settings/app")
        .add_header("x-api-key", API_KEY)
        .json(&json!({ "tmdb_api_key": "abcdef7890wxyz" }))
        .await;
    assert_eq!(response.status_code(), 200);
    assert_eq!(response.json::<Value>()["tmdb_api_key"], json!("****wxyz"));

    let response = server
        .put("/api/v1/settings/app")
        .add_header("x-api-key", API_KEY)
        .json(&json!({ "tmdb_api_key": "" }))
        .await;
    assert_eq!(response.status_code(), 200);
    assert_eq!(response.json::<Value>()["tmdb_api_key"], Value::Null);
}
