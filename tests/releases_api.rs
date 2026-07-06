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
        .and(query_param("append_to_response", "external_ids,videos,credits"))
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

const UHD_RSS_FIXTURE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0" xmlns:newznab="http://www.newznab.com/DTD/2010/feeds/attributes/">
  <channel>
    <title>example</title>
    <item>
      <title>Inception.2010.2160p.WEB-DL.HEVC-UHD</title>
      <guid isPermaLink="true">https://indexer.example/details/uhd</guid>
      <link>https://indexer.example/getnzb/uhd.nzb</link>
      <pubDate>Wed, 03 Jun 2020 12:30:00 +0000</pubDate>
      <newznab:attr name="size" value="20831838208"/>
    </item>
    <item>
      <title>Inception.2010.1080p.BluRay.x264-GROUP</title>
      <guid isPermaLink="true">https://indexer.example/details/good</guid>
      <link>https://indexer.example/getnzb/good.nzb</link>
      <pubDate>Wed, 03 Jun 2020 12:30:00 +0000</pubDate>
      <newznab:attr name="size" value="4831838208"/>
    </item>
  </channel>
</rss>"#;

#[tokio::test]
async fn releases_max_resolution_applies_device_cap() {
    let tmdb = MockServer::start().await;
    let indexer = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/movie/27205"))
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
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(UHD_RSS_FIXTURE, "application/rss+xml; charset=UTF-8"),
        )
        .mount(&indexer)
        .await;

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

    // Without a cap the 2160p release is accepted (user max is 2160p) and
    // ranks below the preferred 1080p one.
    let response = server
        .get("/api/v1/releases?tmdb_id=27205&type=movie")
        .add_header("x-api-key", API_KEY)
        .await;
    assert_eq!(response.status_code(), 200);
    let body: Value = response.json();
    let candidates = body["candidates"].as_array().expect("candidates");
    assert!(candidates.iter().all(|c| c["rejected"].is_null()));

    // With a 1080p device cap the 2160p release is hard-excluded with a
    // device-specific reason and the 1080p release ranks first.
    let response = server
        .get("/api/v1/releases?tmdb_id=27205&type=movie&max_resolution=1080p")
        .add_header("x-api-key", API_KEY)
        .await;
    assert_eq!(response.status_code(), 200);
    let body: Value = response.json();
    let candidates = body["candidates"].as_array().expect("candidates");
    assert_eq!(candidates.len(), 2);

    let best = &candidates[0];
    assert_eq!(
        best["raw"]["title"],
        json!("Inception.2010.1080p.BluRay.x264-GROUP")
    );
    assert_eq!(best["rejected"], Value::Null);

    let capped = &candidates[1];
    assert_eq!(capped["parsed"]["resolution"], json!("2160p"));
    let reason = capped["rejected"].as_str().expect("rejected reason");
    assert!(reason.contains("device max 1080p"), "reason was: {reason}");
}

/// Absolute-numbered anime releases, as returned for `q={title} {absolute}`.
const ANIME_ABSOLUTE_RSS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0" xmlns:newznab="http://www.newznab.com/DTD/2010/feeds/attributes/">
  <channel>
    <title>example</title>
    <item>
      <title>[Erai-raws] One Piece - 1100 [1080p][HEVC]</title>
      <guid isPermaLink="true">https://indexer.example/details/op1100</guid>
      <link>https://indexer.example/getnzb/op1100.nzb</link>
      <pubDate>Wed, 03 Jul 2024 12:30:00 +0000</pubDate>
      <newznab:attr name="size" value="1400000000"/>
    </item>
  </channel>
</rss>"#;

const EMPTY_RSS: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0" xmlns:newznab="http://www.newznab.com/DTD/2010/feeds/attributes/">
  <channel><title>example</title></channel>
</rss>"#;

/// Anime (original_language "ja", multi-season): the scene-numbered tvsearch
/// returns nothing, but the absolute-episode-number query (`One Piece 1100`)
/// finds the real release, and it lands in the candidate list.
#[tokio::test]
async fn releases_for_anime_issue_absolute_episode_query() {
    let tmdb = MockServer::start().await;
    let indexer = MockServer::start().await;

    // Season 1 has 1099 episodes; season 2 episode 1 → absolute 1100.
    Mock::given(method("GET"))
        .and(path("/tv/81797"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": 81797,
            "name": "One Piece",
            "first_air_date": "1999-10-20",
            "original_language": "ja",
            "external_ids": { "tvdb_id": 81797 },
            "seasons": [
                { "season_number": 0, "episode_count": 10 },
                { "season_number": 1, "episode_count": 1099 },
                { "season_number": 2, "episode_count": 50 }
            ]
        })))
        .mount(&tmdb)
        .await;

    // tvsearch (scene numbering) returns zero results for newer anime.
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("t", "tvsearch"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(EMPTY_RSS, "application/rss+xml"))
        .mount(&indexer)
        .await;

    // The SxxExx text fallback also finds nothing.
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("t", "search"))
        .and(query_param("q", "One Piece S02E01"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(EMPTY_RSS, "application/rss+xml"))
        .mount(&indexer)
        .await;

    // The absolute-number query finds the real release.
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("t", "search"))
        .and(query_param("q", "One Piece 1100"))
        .respond_with(
            ResponseTemplate::new(200).set_body_raw(ANIME_ABSOLUTE_RSS, "application/rss+xml"),
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

    let response = server
        .put("/api/v1/settings/app")
        .add_header("x-api-key", API_KEY)
        .json(&json!({ "tmdb_api_key": "tmdb-key" }))
        .await;
    assert_eq!(response.status_code(), 200);
    let response = server
        .post("/api/v1/settings/indexers")
        .add_header("x-api-key", API_KEY)
        .json(&json!({ "name": "mock", "base_url": indexer.uri(), "api_key": "indexer-key" }))
        .await;
    assert_eq!(response.status_code(), 200);

    let response = server
        .get("/api/v1/releases?tmdb_id=81797&type=tv&season=2&episode=1")
        .add_header("x-api-key", API_KEY)
        .await;
    assert_eq!(response.status_code(), 200);
    let body: Value = response.json();
    let candidates = body["candidates"].as_array().expect("candidates array");
    assert!(
        candidates
            .iter()
            .any(|c| c["raw"]["title"] == json!("[Erai-raws] One Piece - 1100 [1080p][HEVC]")),
        "absolute-named anime release must appear: {candidates:?}"
    );
}

/// A non-anime (English) show must NOT issue the absolute-number query — only
/// the tvsearch and SxxExx strategies run.
#[tokio::test]
async fn releases_for_non_anime_do_not_issue_absolute_query() {
    let tmdb = MockServer::start().await;
    let indexer = MockServer::start().await;

    Mock::given(method("GET"))
        .and(path("/tv/1396"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": 1396,
            "name": "Breaking Bad",
            "first_air_date": "2008-01-20",
            "original_language": "en",
            "external_ids": { "tvdb_id": 81189 },
            "seasons": [
                { "season_number": 1, "episode_count": 7 },
                { "season_number": 2, "episode_count": 13 }
            ]
        })))
        .mount(&tmdb)
        .await;

    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("t", "tvsearch"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(EMPTY_RSS, "application/rss+xml"))
        .mount(&indexer)
        .await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("t", "search"))
        .and(query_param("q", "Breaking Bad S02E01"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(EMPTY_RSS, "application/rss+xml"))
        .mount(&indexer)
        .await;
    // An absolute-number query would be `Breaking Bad 8`; assert it is never
    // sent by making the matching mock expect exactly zero hits.
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("t", "search"))
        .and(query_param("q", "Breaking Bad 8"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(EMPTY_RSS, "application/rss+xml"))
        .expect(0)
        .mount(&indexer)
        .await;

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
    let response = server
        .post("/api/v1/settings/indexers")
        .add_header("x-api-key", API_KEY)
        .json(&json!({ "name": "mock", "base_url": indexer.uri(), "api_key": "indexer-key" }))
        .await;
    assert_eq!(response.status_code(), 200);

    let response = server
        .get("/api/v1/releases?tmdb_id=1396&type=tv&season=2&episode=1")
        .add_header("x-api-key", API_KEY)
        .await;
    assert_eq!(response.status_code(), 200);
    // The `.expect(0)` mock verifies on drop that the absolute query never fired.
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
