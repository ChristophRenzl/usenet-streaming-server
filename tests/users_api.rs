//! Wired tests for multi-user auth: admin user management, the login flow,
//! bearer-token authentication and per-user data isolation.

use axum_test::TestServer;
use serde_json::{json, Value};
use usenet_streaming_server::{api, config::AppConfig, state::AppState};

const API_KEY: &str = "test-api-key";

async fn server() -> TestServer {
    let mut config = AppConfig::default();
    config.auth.api_key = API_KEY.into();
    let state = AppState::for_tests(config).await.expect("test state");
    TestServer::new(api::router(state))
}

#[tokio::test]
async fn login_flow_and_token_auth() {
    let server = server().await;

    // The API key acts as the owner/admin.
    let me: Value = server
        .get("/api/v1/auth/me")
        .add_header("x-api-key", API_KEY)
        .await
        .json();
    assert_eq!(me["id"], json!(1));
    assert_eq!(me["is_admin"], json!(true));

    // Admin creates a user.
    let created: Value = server
        .post("/api/v1/users")
        .add_header("x-api-key", API_KEY)
        .json(&json!({"username": "chris", "password": "secret42"}))
        .await
        .json();
    assert_eq!(created["name"], json!("chris"));
    assert_eq!(created["is_admin"], json!(false));
    assert_eq!(created["has_password"], json!(true));

    // Wrong password is rejected; the right one yields a token.
    let bad = server
        .post("/auth/login")
        .json(&json!({"username": "chris", "password": "nope"}))
        .await;
    assert_eq!(bad.status_code(), 401);

    let login: Value = server
        .post("/auth/login")
        .json(&json!({"username": "CHRIS", "password": "secret42"}))
        .await
        .json();
    let token = login["token"].as_str().expect("token").to_string();
    assert_eq!(login["user"]["name"], json!("chris"));

    // The token authenticates as that user (header and query forms).
    let me: Value = server
        .get("/api/v1/auth/me")
        .add_header("authorization", format!("Bearer {token}"))
        .await
        .json();
    assert_eq!(me["name"], json!("chris"));
    let by_query = server.get(&format!("/api/v1/auth/me?token={token}")).await;
    assert_eq!(by_query.status_code(), 200);

    // Logout invalidates it.
    let out = server
        .post("/api/v1/auth/logout")
        .add_header("authorization", format!("Bearer {token}"))
        .await;
    assert_eq!(out.status_code(), 204);
    let after = server
        .get("/api/v1/auth/me")
        .add_header("authorization", format!("Bearer {token}"))
        .await;
    assert_eq!(after.status_code(), 401);
}

#[tokio::test]
async fn non_admins_cannot_manage_users_and_data_is_isolated() {
    let server = server().await;

    for (name, password) in [("anna", "password1"), ("ben", "password2")] {
        let response = server
            .post("/api/v1/users")
            .add_header("x-api-key", API_KEY)
            .json(&json!({"username": name, "password": password}))
            .await;
        assert_eq!(response.status_code(), 200);
    }
    let anna: Value = server
        .post("/auth/login")
        .json(&json!({"username": "anna", "password": "password1"}))
        .await
        .json();
    let ben: Value = server
        .post("/auth/login")
        .json(&json!({"username": "ben", "password": "password2"}))
        .await
        .json();
    let anna_token = anna["token"].as_str().unwrap();
    let ben_token = ben["token"].as_str().unwrap();

    // A non-admin cannot create users.
    let denied = server
        .post("/api/v1/users")
        .add_header("authorization", format!("Bearer {anna_token}"))
        .json(&json!({"username": "eve", "password": "password3"}))
        .await;
    assert_eq!(denied.status_code(), 400);

    // History written by anna is invisible to ben (and to the owner).
    let update = server
        .post("/api/v1/history")
        .add_header("authorization", format!("Bearer {anna_token}"))
        .json(&json!({
            "tmdb_id": 42, "media_type": "movie", "position_secs": 120.0,
            "duration_secs": 3600.0
        }))
        .await;
    assert_eq!(update.status_code(), 200);

    let annas: Value = server
        .get("/api/v1/history")
        .add_header("authorization", format!("Bearer {anna_token}"))
        .await
        .json();
    assert_eq!(annas.as_array().unwrap().len(), 1);
    let bens: Value = server
        .get("/api/v1/history")
        .add_header("authorization", format!("Bearer {ben_token}"))
        .await
        .json();
    assert_eq!(bens.as_array().unwrap().len(), 0);
    let owners: Value = server
        .get("/api/v1/history")
        .add_header("x-api-key", API_KEY)
        .await
        .json();
    assert_eq!(owners.as_array().unwrap().len(), 0);

    // Deleting anna removes her account, tokens and data.
    let anna_id = anna["user"]["id"].as_i64().unwrap();
    let deleted = server
        .delete(&format!("/api/v1/users/{anna_id}"))
        .add_header("x-api-key", API_KEY)
        .await;
    assert_eq!(deleted.status_code(), 204);
    let gone = server
        .get("/api/v1/auth/me")
        .add_header("authorization", format!("Bearer {anna_token}"))
        .await;
    assert_eq!(gone.status_code(), 401);

    // The owner cannot be deleted.
    let protected = server
        .delete("/api/v1/users/1")
        .add_header("x-api-key", API_KEY)
        .await;
    assert_eq!(protected.status_code(), 400);
}
