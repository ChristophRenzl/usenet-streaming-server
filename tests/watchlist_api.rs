//! Wired tests for the watchlist endpoints: idempotent add (with TMDB
//! denormalization), newest-first listing, status checks and deletion.

use axum_test::TestServer;
use serde_json::{json, Value};
use usenet_streaming_server::{api, config::AppConfig, state::AppState};
use wiremock::{
    matchers::{method, path},
    Mock, MockServer, ResponseTemplate,
};

const API_KEY: &str = "test-api-key";

/// Router wired to a mock TMDB with the TMDB key configured via the API.
async fn server_with_tmdb(tmdb: &MockServer) -> TestServer {
    let mut config = AppConfig::default();
    config.auth.api_key = API_KEY.into();
    let state = AppState::for_tests(config)
        .await
        .expect("test state")
        .with_tmdb_base_url(&tmdb.uri());
    let server = TestServer::new(api::router(state));

    let response = server
        .put("/api/v1/settings/app")
        .add_header("x-api-key", API_KEY)
        .json(&json!({ "tmdb_api_key": "tmdb-key" }))
        .await;
    assert_eq!(response.status_code(), 200);
    server
}

fn mock_movie_details(id: i64, title: &str) -> Mock {
    Mock::given(method("GET"))
        .and(path(format!("/movie/{id}")))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": id,
            "title": title,
            "release_date": "2010-07-15",
            "overview": "A mind-bending heist.",
            "poster_path": "/poster.jpg",
            "backdrop_path": "/backdrop.jpg",
            "vote_average": 8.4,
            "imdb_id": "tt1375666",
            "external_ids": { "imdb_id": "tt1375666" }
        })))
}

#[tokio::test]
async fn watchlist_round_trip() {
    let tmdb = MockServer::start().await;
    // expect(1): the idempotent re-add must not hit TMDB again.
    mock_movie_details(27205, "Inception")
        .expect(1)
        .mount(&tmdb)
        .await;
    let server = server_with_tmdb(&tmdb).await;

    // Fresh add: 201 with the denormalized TMDB details.
    let response = server
        .post("/api/v1/watchlist")
        .add_header("x-api-key", API_KEY)
        .json(&json!({ "tmdb_id": 27205, "media_type": "movie" }))
        .await;
    assert_eq!(response.status_code(), 201);
    let created: Value = response.json();
    assert_eq!(created["tmdb_id"], json!(27205));
    assert_eq!(created["media_type"], json!("movie"));
    assert_eq!(created["title"], json!("Inception"));
    assert_eq!(created["year"], json!(2010));
    assert_eq!(
        created["poster_url"],
        json!("https://image.tmdb.org/t/p/w500/poster.jpg")
    );
    assert_eq!(
        created["backdrop_url"],
        json!("https://image.tmdb.org/t/p/w780/backdrop.jpg")
    );
    assert_eq!(created["overview"], json!("A mind-bending heist."));
    assert_eq!(created["vote_average"], json!(8.4));
    assert!(created["added_at"].as_str().is_some());

    // Re-add: 200 with the same row.
    let response = server
        .post("/api/v1/watchlist")
        .add_header("x-api-key", API_KEY)
        .json(&json!({ "tmdb_id": 27205, "media_type": "movie" }))
        .await;
    assert_eq!(response.status_code(), 200);
    let again: Value = response.json();
    assert_eq!(again["id"], created["id"]);

    // List contains exactly the one item.
    let response = server
        .get("/api/v1/watchlist")
        .add_header("x-api-key", API_KEY)
        .await;
    assert_eq!(response.status_code(), 200);
    let list: Value = response.json();
    assert_eq!(list.as_array().expect("array").len(), 1);
    assert_eq!(list[0]["title"], json!("Inception"));

    // Status: present.
    let response = server
        .get("/api/v1/watchlist/status?tmdb_id=27205&media_type=movie")
        .add_header("x-api-key", API_KEY)
        .await;
    assert_eq!(response.status_code(), 200);
    assert_eq!(response.json::<Value>()["in_watchlist"], json!(true));

    // Delete: 204, then absent everywhere, then 404 on repeat.
    let response = server
        .delete("/api/v1/watchlist/27205?media_type=movie")
        .add_header("x-api-key", API_KEY)
        .await;
    assert_eq!(response.status_code(), 204);

    let response = server
        .get("/api/v1/watchlist/status?tmdb_id=27205&media_type=movie")
        .add_header("x-api-key", API_KEY)
        .await;
    assert_eq!(response.json::<Value>()["in_watchlist"], json!(false));

    let response = server
        .delete("/api/v1/watchlist/27205?media_type=movie")
        .add_header("x-api-key", API_KEY)
        .await;
    assert_eq!(response.status_code(), 404);
}

#[tokio::test]
async fn watchlist_lists_newest_first_and_supports_tv() {
    let tmdb = MockServer::start().await;
    mock_movie_details(27205, "Inception").mount(&tmdb).await;
    Mock::given(method("GET"))
        .and(path("/tv/1396"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": 1396,
            "name": "Breaking Bad",
            "first_air_date": "2008-01-20",
            "overview": "A chemistry teacher turns to crime.",
            "poster_path": "/tvposter.jpg",
            "backdrop_path": "/tvbackdrop.jpg",
            "vote_average": 8.9,
            "external_ids": { "imdb_id": "tt0903747", "tvdb_id": 81189 },
            "seasons": []
        })))
        .expect(1)
        .mount(&tmdb)
        .await;
    let server = server_with_tmdb(&tmdb).await;

    for body in [
        json!({ "tmdb_id": 27205, "media_type": "movie" }),
        json!({ "tmdb_id": 1396, "media_type": "tv" }),
    ] {
        let response = server
            .post("/api/v1/watchlist")
            .add_header("x-api-key", API_KEY)
            .json(&body)
            .await;
        assert_eq!(response.status_code(), 201);
    }

    let response = server
        .get("/api/v1/watchlist")
        .add_header("x-api-key", API_KEY)
        .await;
    let list: Value = response.json();
    let items = list.as_array().expect("array");
    assert_eq!(items.len(), 2);
    // Newest first: the TV show was added last.
    assert_eq!(items[0]["media_type"], json!("tv"));
    assert_eq!(items[0]["title"], json!("Breaking Bad"));
    assert_eq!(items[0]["year"], json!(2008));
    assert_eq!(items[1]["title"], json!("Inception"));

    // The same tmdb_id under the other media type is a separate item.
    let response = server
        .get("/api/v1/watchlist/status?tmdb_id=27205&media_type=tv")
        .add_header("x-api-key", API_KEY)
        .await;
    assert_eq!(response.json::<Value>()["in_watchlist"], json!(false));
}

#[tokio::test]
async fn adding_an_unknown_tmdb_id_is_not_found() {
    let tmdb = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/movie/999999999"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({
            "success": false,
            "status_code": 34,
            "status_message": "The resource you requested could not be found."
        })))
        .mount(&tmdb)
        .await;
    let server = server_with_tmdb(&tmdb).await;

    let response = server
        .post("/api/v1/watchlist")
        .add_header("x-api-key", API_KEY)
        .json(&json!({ "tmdb_id": 999999999, "media_type": "movie" }))
        .await;
    assert_eq!(response.status_code(), 404);

    // Nothing was stored.
    let response = server
        .get("/api/v1/watchlist")
        .add_header("x-api-key", API_KEY)
        .await;
    assert_eq!(response.json::<Value>().as_array().unwrap().len(), 0);
}
