//! Wired tests for the discovery endpoints: /trending, /movies/popular,
//! /movies/top_rated, /tv/popular, /tv/top_rated against a wiremock TMDB.

use axum_test::TestServer;
use serde_json::{json, Value};
use usenet_streaming_server::{api, config::AppConfig, state::AppState};
use wiremock::{
    matchers::{method, path, query_param},
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

fn movie_item(id: i64, title: &str, with_media_type: bool) -> Value {
    let mut item = json!({
        "id": id,
        "title": title,
        "release_date": "2010-07-15",
        "overview": "A movie.",
        "poster_path": "/poster.jpg",
        "backdrop_path": "/backdrop.jpg",
        "vote_average": 8.1
    });
    if with_media_type {
        item["media_type"] = json!("movie");
    }
    item
}

fn tv_item(id: i64, name: &str, with_media_type: bool) -> Value {
    let mut item = json!({
        "id": id,
        "name": name,
        "first_air_date": "2008-01-20",
        "overview": "A show.",
        "poster_path": "/tvposter.jpg",
        "vote_average": 8.9
    });
    if with_media_type {
        item["media_type"] = json!("tv");
    }
    item
}

#[tokio::test]
async fn trending_defaults_to_all_week_and_drops_people() {
    let tmdb = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/trending/all/week"))
        .and(query_param("api_key", "tmdb-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "page": 1,
            "results": [
                movie_item(27205, "Inception", true),
                { "id": 6193, "name": "Leonardo DiCaprio", "media_type": "person" },
                tv_item(1396, "Breaking Bad", true),
            ],
            "total_pages": 42,
            "total_results": 840
        })))
        .expect(1)
        .mount(&tmdb)
        .await;
    let server = server_with_tmdb(&tmdb).await;

    let response = server
        .get("/api/v1/trending")
        .add_header("x-api-key", API_KEY)
        .await;
    assert_eq!(response.status_code(), 200);
    let body: Value = response.json();
    assert_eq!(body["page"], json!(1));
    assert_eq!(body["total_pages"], json!(42));

    let results = body["results"].as_array().expect("results array");
    assert_eq!(results.len(), 2, "person results must be dropped");
    assert_eq!(results[0]["tmdb_id"], json!(27205));
    assert_eq!(results[0]["media_type"], json!("movie"));
    assert_eq!(results[0]["title"], json!("Inception"));
    assert_eq!(results[0]["year"], json!(2010));
    assert_eq!(
        results[0]["poster_url"],
        json!("https://image.tmdb.org/t/p/w500/poster.jpg")
    );
    assert_eq!(results[1]["media_type"], json!("tv"));
    assert_eq!(results[1]["title"], json!("Breaking Bad"));
}

#[tokio::test]
async fn trending_passes_scope_window_and_page_through() {
    let tmdb = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/trending/movie/day"))
        .and(query_param("page", "3"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "page": 3,
            "results": [movie_item(603, "The Matrix", false)],
            "total_pages": 10,
            "total_results": 200
        })))
        .expect(1)
        .mount(&tmdb)
        .await;
    let server = server_with_tmdb(&tmdb).await;

    let response = server
        .get("/api/v1/trending?media_type=movie&window=day&page=3")
        .add_header("x-api-key", API_KEY)
        .await;
    assert_eq!(response.status_code(), 200);
    let body: Value = response.json();
    assert_eq!(body["page"], json!(3));
    assert_eq!(body["total_pages"], json!(10));
    // media_type is stamped even when TMDB omits it.
    assert_eq!(body["results"][0]["media_type"], json!("movie"));
}

#[tokio::test]
async fn popular_and_top_rated_movies_are_mapped() {
    let tmdb = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/movie/popular"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "page": 1,
            "results": [movie_item(27205, "Inception", false)],
            "total_pages": 500,
            "total_results": 10000
        })))
        .expect(1)
        .mount(&tmdb)
        .await;
    Mock::given(method("GET"))
        .and(path("/movie/top_rated"))
        .and(query_param("page", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "page": 2,
            "results": [movie_item(238, "The Godfather", false)],
            "total_pages": 480,
            "total_results": 9600
        })))
        .expect(1)
        .mount(&tmdb)
        .await;
    let server = server_with_tmdb(&tmdb).await;

    // Static /movies/popular must not be captured by /movies/{tmdb_id}.
    let response = server
        .get("/api/v1/movies/popular")
        .add_header("x-api-key", API_KEY)
        .await;
    assert_eq!(response.status_code(), 200);
    let body: Value = response.json();
    assert_eq!(body["page"], json!(1));
    assert_eq!(body["total_pages"], json!(500));
    assert_eq!(body["results"][0]["media_type"], json!("movie"));
    assert_eq!(body["results"][0]["title"], json!("Inception"));

    let response = server
        .get("/api/v1/movies/top_rated?page=2")
        .add_header("x-api-key", API_KEY)
        .await;
    assert_eq!(response.status_code(), 200);
    let body: Value = response.json();
    assert_eq!(body["page"], json!(2));
    assert_eq!(body["results"][0]["title"], json!("The Godfather"));
}

#[tokio::test]
async fn popular_and_top_rated_tv_are_mapped() {
    let tmdb = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/tv/popular"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "page": 1,
            "results": [tv_item(1396, "Breaking Bad", false)],
            "total_pages": 300,
            "total_results": 6000
        })))
        .expect(1)
        .mount(&tmdb)
        .await;
    Mock::given(method("GET"))
        .and(path("/tv/top_rated"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "page": 1,
            "results": [tv_item(87108, "Chernobyl", false)],
            "total_pages": 120,
            "total_results": 2400
        })))
        .expect(1)
        .mount(&tmdb)
        .await;
    let server = server_with_tmdb(&tmdb).await;

    let response = server
        .get("/api/v1/tv/popular")
        .add_header("x-api-key", API_KEY)
        .await;
    assert_eq!(response.status_code(), 200);
    let body: Value = response.json();
    assert_eq!(body["results"][0]["media_type"], json!("tv"));
    assert_eq!(body["results"][0]["title"], json!("Breaking Bad"));
    assert_eq!(body["results"][0]["year"], json!(2008));

    let response = server
        .get("/api/v1/tv/top_rated")
        .add_header("x-api-key", API_KEY)
        .await;
    assert_eq!(response.status_code(), 200);
    let body: Value = response.json();
    assert_eq!(body["results"][0]["title"], json!("Chernobyl"));
    assert_eq!(body["total_pages"], json!(120));
}

#[tokio::test]
async fn page_zero_is_a_bad_request() {
    let tmdb = MockServer::start().await;
    let server = server_with_tmdb(&tmdb).await;

    for url in [
        "/api/v1/trending?page=0",
        "/api/v1/movies/popular?page=0",
        "/api/v1/tv/top_rated?page=0",
    ] {
        let response = server.get(url).add_header("x-api-key", API_KEY).await;
        assert_eq!(response.status_code(), 400, "url: {url}");
        assert!(response.text().contains("page"), "url: {url}");
    }
}
