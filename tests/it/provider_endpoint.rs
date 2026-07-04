//! API-level test for POST /api/v1/settings/providers/{id}/test.

use axum_test::TestServer;
use serde_json::{json, Value};
use usenet_streaming_server::{api, config::AppConfig, state::AppState};

use crate::support::MockNntp;

const API_KEY: &str = "test-api-key";

async fn server() -> TestServer {
    let mut config = AppConfig::default();
    config.auth.api_key = API_KEY.into();
    let state = AppState::for_tests(config).await.expect("test state");
    TestServer::new(api::router(state))
}

async fn create_provider(server: &TestServer, host: &str, port: u16) -> i64 {
    let response = server
        .post("/api/v1/settings/providers")
        .add_header("x-api-key", API_KEY)
        .json(&json!({
            "name": "mock",
            "host": host,
            "port": port,
            "use_tls": false,
            "username": "user",
            "password": "pass",
            "max_connections": 2,
            "priority": 0,
        }))
        .await;
    assert_eq!(response.status_code(), 200);
    response.json::<Value>()["id"].as_i64().expect("id")
}

#[tokio::test]
async fn provider_test_endpoint_reports_ok_and_latency() {
    let mock = MockNntp::start(Some(("user", "pass"))).await;
    let server = server().await;
    let id = create_provider(&server, "127.0.0.1", mock.addr().port()).await;

    let response = server
        .post(&format!("/api/v1/settings/providers/{id}/test"))
        .add_header("x-api-key", API_KEY)
        .await;
    assert_eq!(response.status_code(), 200);
    let body: Value = response.json();
    assert_eq!(body["ok"], true);
    assert!(body["latency_ms"].is_u64());
    assert_eq!(body["error"], Value::Null);
}

#[tokio::test]
async fn provider_test_endpoint_reports_failure() {
    // Reserve a port and drop the listener so the dial is refused.
    let dead_port = {
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        l.local_addr().unwrap().port()
    };
    let server = server().await;
    let id = create_provider(&server, "127.0.0.1", dead_port).await;

    let response = server
        .post(&format!("/api/v1/settings/providers/{id}/test"))
        .add_header("x-api-key", API_KEY)
        .await;
    assert_eq!(response.status_code(), 200);
    let body: Value = response.json();
    assert_eq!(body["ok"], false);
    assert_eq!(body["latency_ms"], Value::Null);
    assert!(body["error"].as_str().is_some_and(|e| !e.is_empty()));
}

#[tokio::test]
async fn provider_test_endpoint_requires_auth_and_existing_provider() {
    let server = server().await;

    let response = server.post("/api/v1/settings/providers/1/test").await;
    assert_eq!(response.status_code(), 401);

    let response = server
        .post("/api/v1/settings/providers/9999/test")
        .add_header("x-api-key", API_KEY)
        .await;
    assert_eq!(response.status_code(), 404);
}
