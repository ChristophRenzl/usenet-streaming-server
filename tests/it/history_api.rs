//! Watch-history API tests: upsert semantics, percent math, validation,
//! deletion and the OpenAPI surface of the new modules.

use serde_json::{json, Value};
use usenet_streaming_server::config::AppConfig;
use usenet_streaming_server::state::AppState;

use crate::support::spawn_app;

const API_KEY: &str = "test-api-key";

struct HistoryStack {
    base: String,
    client: reqwest::Client,
    _server: tokio::task::JoinHandle<()>,
}

async fn history_stack() -> HistoryStack {
    let mut config = AppConfig::default();
    config.auth.api_key = API_KEY.into();
    let state = AppState::for_tests(config).await.expect("state");
    let (base, _state, server) = spawn_app(state).await;
    HistoryStack {
        base,
        client: reqwest::Client::new(),
        _server: server,
    }
}

impl HistoryStack {
    async fn get(&self, path: &str) -> reqwest::Response {
        self.client
            .get(format!("{}{path}", self.base))
            .header("x-api-key", API_KEY)
            .send()
            .await
            .expect("GET")
    }

    async fn post_history(&self, body: Value) -> reqwest::Response {
        self.client
            .post(format!("{}/api/v1/history", self.base))
            .header("x-api-key", API_KEY)
            .json(&body)
            .send()
            .await
            .expect("POST history")
    }
}

#[tokio::test]
async fn history_upsert_percent_and_delete() {
    let stack = history_stack().await;

    // Empty to begin with, and auth applies.
    let response = stack
        .client
        .get(format!("{}/api/v1/history", stack.base))
        .send()
        .await
        .expect("GET");
    assert_eq!(response.status(), 401);
    let response = stack.get("/api/v1/history").await;
    assert_eq!(response.status(), 200);
    assert_eq!(response.json::<Value>().await.expect("json"), json!([]));

    // First position write creates the row.
    let response = stack
        .post_history(json!({
            "tmdb_id": 550, "media_type": "movie",
            "position_secs": 900.0, "duration_secs": 3600.0
        }))
        .await;
    assert_eq!(response.status(), 200);
    let movie: Value = response.json().await.expect("json");
    assert_eq!(movie["position_secs"], 900.0);
    assert_eq!(movie["percent_watched"], 25.0);
    let movie_id = movie["id"].as_i64().expect("id");

    // Second write updates in place; the stored duration survives.
    let response = stack
        .post_history(json!({
            "tmdb_id": 550, "media_type": "movie", "position_secs": 1800.0
        }))
        .await;
    assert_eq!(response.status(), 200);
    let movie: Value = response.json().await.expect("json");
    assert_eq!(movie["id"], movie_id, "upsert must not create a second row");
    assert_eq!(movie["duration_secs"], 3600.0);
    assert_eq!(movie["percent_watched"], 50.0);

    // TV episodes need season+episode and are tracked separately.
    let response = stack
        .post_history(json!({ "tmdb_id": 1399, "media_type": "tv", "position_secs": 10.0 }))
        .await;
    assert_eq!(response.status(), 400);
    let response = stack
        .post_history(json!({
            "tmdb_id": 1399, "media_type": "tv", "season": 1, "episode": 3,
            "position_secs": 10.0
        }))
        .await;
    assert_eq!(response.status(), 200);
    let episode: Value = response.json().await.expect("json");
    assert_eq!(episode["season"], 1);
    assert_eq!(episode["episode"], 3);
    assert!(
        episode["percent_watched"].is_null(),
        "no percent without a duration"
    );

    // Invalid positions are rejected.
    for bad in [
        json!({ "tmdb_id": 550, "media_type": "movie", "position_secs": -1.0 }),
        json!({ "tmdb_id": 550, "media_type": "movie", "position_secs": 1.0, "duration_secs": -2.0 }),
    ] {
        assert_eq!(stack.post_history(bad).await.status(), 400);
    }

    // Newest-first listing (the episode was written last).
    let response = stack.get("/api/v1/history").await;
    let list: Value = response.json().await.expect("json");
    let list = list.as_array().expect("array");
    assert_eq!(list.len(), 2);
    assert_eq!(list[0]["media_type"], "tv");
    assert_eq!(list[1]["media_type"], "movie");

    // Deletion.
    let response = stack
        .client
        .delete(format!("{}/api/v1/history/{movie_id}", stack.base))
        .header("x-api-key", API_KEY)
        .send()
        .await
        .expect("DELETE");
    assert_eq!(response.status(), 204);
    let response = stack
        .client
        .delete(format!("{}/api/v1/history/{movie_id}", stack.base))
        .header("x-api-key", API_KEY)
        .send()
        .await
        .expect("DELETE again");
    assert_eq!(response.status(), 404);
    let response = stack.get("/api/v1/history").await;
    let list: Value = response.json().await.expect("json");
    assert_eq!(list.as_array().expect("array").len(), 1);
}

#[tokio::test]
async fn position_updates_for_unknown_sessions_are_404() {
    let stack = history_stack().await;
    let response = stack
        .client
        .put(format!(
            "{}/api/v1/stream/{}/position",
            stack.base,
            uuid::Uuid::new_v4()
        ))
        .header("x-api-key", API_KEY)
        .json(&json!({ "position_secs": 1.0 }))
        .send()
        .await
        .expect("PUT position");
    assert_eq!(response.status(), 404);
}

#[tokio::test]
async fn openapi_documents_downloads_and_history() {
    let stack = history_stack().await;
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
        "/downloads",
        "/downloads/{id}",
        "/history",
        "/history/{id}",
        "/stream/{session_id}/position",
    ] {
        assert!(paths.contains_key(documented), "missing {documented}");
    }
    let tags: Vec<&str> = doc["tags"]
        .as_array()
        .expect("tags")
        .iter()
        .filter_map(|tag| tag["name"].as_str())
        .collect();
    for tag in ["downloads", "history"] {
        assert!(tags.contains(&tag), "missing tag {tag} in {tags:?}");
    }
}
