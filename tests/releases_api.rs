//! End-to-end test for GET /releases: TMDB lookup, indexer fan-out,
//! parsing and ranking through the full router.

use axum_test::TestServer;
use serde_json::{json, Value};
use usenet_streaming_server::{api, config::AppConfig, state::AppState};
use wiremock::{
    matchers::{method, path, query_param},
    Mock, MockServer, ResponseTemplate,
};

const API_KEY: &str = "test-api-key";

const RSS_FIXTURE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0" xmlns:newznab="http://www.newznab.com/DTD/2010/feeds/attributes/">
  <channel>
    <title>example</title>
    <item>
      <title>Inception.2010.1080p.BluRay.x264-GROUP</title>
      <guid isPermaLink="true">https://indexer.example/details/good</guid>
      <link>https://indexer.example/getnzb/good.nzb</link>
      <pubDate>Wed, 03 Jun 2020 12:30:00 +0000</pubDate>
      <newznab:attr name="size" value="4831838208"/>
    </item>
    <item>
      <title>Inception.2010.HDCAM.x264-JUNK</title>
      <guid isPermaLink="true">https://indexer.example/details/junk</guid>
      <link>https://indexer.example/getnzb/junk.nzb</link>
      <pubDate>Thu, 04 Jun 2020 08:00:00 +0000</pubDate>
      <newznab:attr name="size" value="1500000000"/>
    </item>
  </channel>
</rss>"#;

#[tokio::test]
async fn releases_endpoint_ranks_indexer_results() {
    let tmdb = MockServer::start().await;
    let indexer = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/movie/27205"))
        .and(query_param("append_to_response", "external_ids"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": 27205,
            "title": "Inception",
            "release_date": "2010-07-15",
            "imdb_id": "tt1375666",
            "external_ids": { "imdb_id": "tt1375666" }
        })))
        .mount(&tmdb)
        .await;

    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("t", "movie"))
        .and(query_param("imdbid", "1375666"))
        .and(query_param("apikey", "indexer-key"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(RSS_FIXTURE, "application/rss+xml; charset=UTF-8"),
        )
        .expect(1)
        .mount(&indexer)
        .await;

    let mut config = AppConfig::default();
    config.auth.api_key = API_KEY.into();
    let state = AppState::for_tests(config)
        .await
        .expect("test state")
        .with_tmdb_base_url(&tmdb.uri());
    let server = TestServer::new(api::router(state));

    // Configure the TMDB key and one indexer through the API itself.
    let response = server
        .put("/api/v1/settings/app")
        .add_header("x-api-key", API_KEY)
        .json(&json!({ "tmdb_api_key": "tmdb-key" }))
        .await;
    assert_eq!(response.status_code(), 200);

    let response = server
        .post("/api/v1/settings/indexers")
        .add_header("x-api-key", API_KEY)
        .json(&json!({
            "name": "mock",
            "base_url": indexer.uri(),
            "api_key": "indexer-key"
        }))
        .await;
    assert_eq!(response.status_code(), 200);

    let response = server
        .get("/api/v1/releases?tmdb_id=27205&type=movie")
        .add_header("x-api-key", API_KEY)
        .await;
    assert_eq!(response.status_code(), 200);
    let body: Value = response.json();
    let candidates = body["candidates"].as_array().expect("candidates array");
    assert_eq!(candidates.len(), 2);

    // The BluRay release is accepted and ranked first.
    let best = &candidates[0];
    assert_eq!(
        best["raw"]["title"],
        json!("Inception.2010.1080p.BluRay.x264-GROUP")
    );
    assert_eq!(best["rejected"], Value::Null);
    assert_eq!(best["parsed"]["resolution"], json!("1080p"));
    assert_eq!(best["parsed"]["source"], json!("bluray"));

    // The HDCAM release is kept but rejected (default blocked terms).
    let junk = &candidates[1];
    assert!(junk["rejected"].as_str().unwrap().contains("blocked term"));
}

#[tokio::test]
async fn releases_for_tv_require_season_and_episode() {
    let mut config = AppConfig::default();
    config.auth.api_key = API_KEY.into();
    let state = AppState::for_tests(config).await.expect("test state");
    let server = TestServer::new(api::router(state));

    // Need at least one enabled indexer to get past the indexer check.
    let response = server
        .post("/api/v1/settings/indexers")
        .add_header("x-api-key", API_KEY)
        .json(&json!({
            "name": "mock",
            "base_url": "http://127.0.0.1:1",
            "api_key": "k"
        }))
        .await;
    assert_eq!(response.status_code(), 200);

    let response = server
        .get("/api/v1/releases?tmdb_id=1396&type=tv")
        .add_header("x-api-key", API_KEY)
        .await;
    assert_eq!(response.status_code(), 400);
    assert!(response.text().contains("season and episode"));
}

#[tokio::test]
async fn releases_without_indexers_is_a_bad_request() {
    let mut config = AppConfig::default();
    config.auth.api_key = API_KEY.into();
    let state = AppState::for_tests(config).await.expect("test state");
    let server = TestServer::new(api::router(state));

    let response = server
        .get("/api/v1/releases?tmdb_id=27205&type=movie")
        .add_header("x-api-key", API_KEY)
        .await;
    assert_eq!(response.status_code(), 400);
    assert!(response.text().contains("no enabled indexers"));
}
