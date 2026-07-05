//! Tests for the embedded web admin UI and the OpenAPI security scheme.

use axum_test::TestServer;
use serde_json::Value;
use usenet_streaming_server::{api, config::AppConfig, state::AppState};

async fn server() -> TestServer {
    let mut config = AppConfig::default();
    config.auth.api_key = "test-api-key".into();
    let state = AppState::for_tests(config).await.expect("test state");
    TestServer::new(api::router(state))
}

#[tokio::test]
async fn root_serves_the_admin_ui_without_auth() {
    let server = server().await;
    let response = server.get("/").await;
    assert_eq!(response.status_code(), 200);
    assert!(response
        .header("content-type")
        .to_str()
        .unwrap()
        .starts_with("text/html"));
    let body = response.text();
    assert!(body.contains("<html"), "root serves the SPA shell");
    assert!(body.contains("/assets/app.js"));
    assert!(body.contains("/assets/style.css"));
}

#[tokio::test]
async fn webui_assets_have_correct_content_types_and_no_auth() {
    let server = server().await;

    let response = server.get("/assets/app.js").await;
    assert_eq!(response.status_code(), 200);
    assert!(response
        .header("content-type")
        .to_str()
        .unwrap()
        .starts_with("application/javascript"));
    assert!(response.text().contains("X-Api-Key"));

    let response = server.get("/assets/style.css").await;
    assert_eq!(response.status_code(), 200);
    assert!(response
        .header("content-type")
        .to_str()
        .unwrap()
        .starts_with("text/css"));
    assert!(response.text().contains("--accent"));
}

#[tokio::test]
async fn openapi_declares_the_api_key_security_scheme() {
    let server = server().await;
    let response = server.get("/api-docs/openapi.json").await;
    assert_eq!(response.status_code(), 200);
    let doc: Value = response.json();

    let scheme = &doc["components"]["securitySchemes"]["api_key"];
    assert_eq!(scheme["type"], "apiKey");
    assert_eq!(scheme["in"], "header");
    assert_eq!(scheme["name"], "X-Api-Key");

    // Global requirement so Swagger's Authorize unlocks "Try it out".
    let security = doc["security"].as_array().expect("global security array");
    assert!(security
        .iter()
        .any(|requirement| requirement.get("api_key").is_some()));
}
