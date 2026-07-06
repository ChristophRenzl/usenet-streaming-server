//! API-level tests for the subtitle search endpoint, driven through the full
//! router (auth middleware included) with a wiremock OpenSubtitles backend.

use axum_test::TestServer;
use serde_json::{json, Value};
use usenet_streaming_server::{api, config::AppConfig, state::AppState};
use wiremock::{
    matchers::{header, method, path, query_param},
    Mock, MockServer, ResponseTemplate,
};

const API_KEY: &str = "test-api-key";

async fn server_with_opensubtitles(base_url: &str) -> TestServer {
    server_with_opensubtitles_default(base_url, None).await
}

/// Like [`server_with_opensubtitles`] but with an optional operator config
/// default OpenSubtitles API key (`subtitles.opensubtitles_default_api_key`).
async fn server_with_opensubtitles_default(
    base_url: &str,
    default_key: Option<&str>,
) -> TestServer {
    let mut config = AppConfig::default();
    config.auth.api_key = API_KEY.into();
    config.subtitles.opensubtitles_default_api_key = default_key.map(str::to_string);
    let state = AppState::for_tests(config)
        .await
        .expect("test state")
        .with_opensubtitles_base_url(base_url);
    TestServer::new(api::router(state))
}

async fn set_opensubtitles_key(server: &TestServer) {
    let response = server
        .put("/api/v1/settings/app")
        .add_header("x-api-key", API_KEY)
        .json(&json!({ "opensubtitles_api_key": "os-key-1234" }))
        .await;
    assert_eq!(response.status_code(), 200);
}

#[tokio::test]
async fn subtitle_search_requires_auth() {
    let server = server_with_opensubtitles("http://127.0.0.1:1").await;
    let response = server.get("/api/v1/subtitles/search?tmdb_id=27205").await;
    assert_eq!(response.status_code(), 401);
}

#[tokio::test]
async fn subtitle_search_without_key_returns_400_with_guidance() {
    let server = server_with_opensubtitles("http://127.0.0.1:1").await;
    let response = server
        .get("/api/v1/subtitles/search?tmdb_id=27205")
        .add_header("x-api-key", API_KEY)
        .await;
    assert_eq!(response.status_code(), 400);
    let body: Value = response.json();
    let message = body["error"].as_str().expect("error message");
    assert!(
        message.contains("OpenSubtitles API key not configured"),
        "message: {message}"
    );
}

#[tokio::test]
async fn subtitle_search_happy_path_ranks_results() {
    let os = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/subtitles"))
        .and(query_param("tmdb_id", "27205"))
        .and(query_param("languages", "en"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{
                "id": "1",
                "attributes": {
                    "language": "en",
                    "release": "1080p.BluRay",
                    "download_count": 4242,
                    "hearing_impaired": false,
                    "ai_translated": false,
                    "files": [{ "file_id": 555, "file_name": "sub.srt" }]
                }
            }]
        })))
        .expect(1)
        .mount(&os)
        .await;

    let server = server_with_opensubtitles(&os.uri()).await;
    set_opensubtitles_key(&server).await;

    let response = server
        .get("/api/v1/subtitles/search?tmdb_id=27205&languages=en")
        .add_header("x-api-key", API_KEY)
        .await;
    assert_eq!(response.status_code(), 200);
    let body: Value = response.json();
    assert_eq!(body["languages"], json!(["en"]));
    let results = body["results"].as_array().expect("results");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["file_id"], json!(555));
    assert_eq!(results[0]["language"], json!("en"));
    assert_eq!(results[0]["download_count"], json!(4242));
}

#[tokio::test]
async fn tv_subtitle_search_requires_season_and_episode() {
    let server = server_with_opensubtitles("http://127.0.0.1:1").await;
    set_opensubtitles_key(&server).await;

    let response = server
        .get("/api/v1/subtitles/search?tmdb_id=1396&media_type=tv")
        .add_header("x-api-key", API_KEY)
        .await;
    assert_eq!(response.status_code(), 400);
    let body: Value = response.json();
    assert!(body["error"]
        .as_str()
        .unwrap()
        .contains("season is required"));
}

#[tokio::test]
async fn subtitle_search_uses_config_default_key_without_user_key() {
    // Only the operator config default is set — no per-user key. The search
    // must still work and carry the default key in the Api-Key header.
    let os = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/subtitles"))
        .and(query_param("tmdb_id", "27205"))
        .and(header("Api-Key", "deploy-default-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{
                "id": "1",
                "attributes": {
                    "language": "en",
                    "release": "1080p.BluRay",
                    "download_count": 10,
                    "hearing_impaired": false,
                    "ai_translated": false,
                    "files": [{ "file_id": 777, "file_name": "sub.srt" }]
                }
            }]
        })))
        .expect(1)
        .mount(&os)
        .await;

    let server = server_with_opensubtitles_default(&os.uri(), Some("deploy-default-key")).await;
    // Configure only username/password (the "Jellyfin experience"): no api key.
    let response = server
        .put("/api/v1/settings/app")
        .add_header("x-api-key", API_KEY)
        .json(&json!({
            "opensubtitles_username": "alice",
            "opensubtitles_password": "s3cret",
        }))
        .await;
    assert_eq!(response.status_code(), 200);

    let response = server
        .get("/api/v1/subtitles/search?tmdb_id=27205&languages=en")
        .add_header("x-api-key", API_KEY)
        .await;
    assert_eq!(response.status_code(), 200);
    let body: Value = response.json();
    let results = body["results"].as_array().expect("results");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0]["file_id"], json!(777));
}

#[tokio::test]
async fn app_settings_report_config_default_as_active() {
    let server = server_with_opensubtitles_default("http://127.0.0.1:1", Some("deploy-key")).await;
    let response = server
        .get("/api/v1/settings/app")
        .add_header("x-api-key", API_KEY)
        .await;
    let settings: Value = response.json();
    // No per-user key stored, but the default makes subtitles configured.
    assert_eq!(settings["opensubtitles_api_key"], Value::Null);
    assert_eq!(settings["opensubtitles_configured"], json!(true));
    assert_eq!(settings["opensubtitles_default_key_active"], json!(true));
    assert_eq!(settings["opensubtitles_api_key_source"], json!("default"));
    // The default key itself must never be leaked back.
    assert!(!response.text().contains("deploy-key"));
}

#[tokio::test]
async fn app_settings_report_user_key_source_over_default() {
    let server = server_with_opensubtitles_default("http://127.0.0.1:1", Some("deploy-key")).await;
    let response = server
        .put("/api/v1/settings/app")
        .add_header("x-api-key", API_KEY)
        .json(&json!({ "opensubtitles_api_key": "user-key-abcd" }))
        .await;
    assert_eq!(response.status_code(), 200);
    let settings: Value = response.json();
    // A per-user key wins; the default is still reported as present/active.
    assert_eq!(settings["opensubtitles_api_key"], json!("****abcd"));
    assert_eq!(settings["opensubtitles_configured"], json!(true));
    assert_eq!(settings["opensubtitles_api_key_source"], json!("user"));
    assert_eq!(settings["opensubtitles_default_key_active"], json!(true));
}

#[tokio::test]
async fn app_settings_report_no_key_source_when_nothing_configured() {
    let server = server_with_opensubtitles("http://127.0.0.1:1").await;
    let response = server
        .get("/api/v1/settings/app")
        .add_header("x-api-key", API_KEY)
        .await;
    let settings: Value = response.json();
    assert_eq!(settings["opensubtitles_configured"], json!(false));
    assert_eq!(settings["opensubtitles_default_key_active"], json!(false));
    assert_eq!(settings["opensubtitles_api_key_source"], json!("none"));
}

#[tokio::test]
async fn app_settings_mask_the_opensubtitles_key() {
    let server = server_with_opensubtitles("http://127.0.0.1:1").await;

    // Unset by default; the checklist flag is false.
    let response = server
        .get("/api/v1/settings/app")
        .add_header("x-api-key", API_KEY)
        .await;
    let settings: Value = response.json();
    assert_eq!(settings["opensubtitles_api_key"], Value::Null);
    assert_eq!(settings["opensubtitles_configured"], json!(false));

    let response = server
        .put("/api/v1/settings/app")
        .add_header("x-api-key", API_KEY)
        .json(&json!({ "opensubtitles_api_key": "abcdef7890wxyz" }))
        .await;
    assert_eq!(response.status_code(), 200);
    let settings: Value = response.json();
    assert_eq!(settings["opensubtitles_api_key"], json!("****wxyz"));
    assert_eq!(settings["opensubtitles_configured"], json!(true));

    // The full key must never come back.
    let response = server
        .get("/api/v1/settings/app")
        .add_header("x-api-key", API_KEY)
        .await;
    assert!(!response.text().contains("abcdef7890wxyz"));
}
