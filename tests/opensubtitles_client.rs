//! Wiremock tests for the OpenSubtitles client: search mapping + ranking,
//! the two-step download (link then CDN bytes), and quota/error translation.

use serde_json::json;
use usenet_streaming_server::{
    error::AppError,
    subtitles::{OpenSubtitlesClient, SubtitleQuery},
};
use wiremock::{
    matchers::{body_partial_json, header, method, path, query_param},
    Mock, MockServer, ResponseTemplate,
};

fn client(server: &MockServer) -> OpenSubtitlesClient {
    OpenSubtitlesClient::new(reqwest::Client::new(), server.uri(), "test-key")
}

const SAMPLE_SRT: &str =
    "1\r\n00:00:01,000 --> 00:00:04,000\r\nHello world\r\n\r\n2\r\n00:00:05,000 --> 00:00:06,000\r\nBye\r\n";

#[tokio::test]
async fn movie_search_maps_and_ranks_results() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/subtitles"))
        .and(query_param("tmdb_id", "27205"))
        .and(query_param("languages", "en,de"))
        .and(header("Api-Key", "test-key"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [
                {
                    "id": "ai-en",
                    "type": "subtitle",
                    "attributes": {
                        "language": "en",
                        "release": "AI.Translated.1080p",
                        "download_count": 9000,
                        "hearing_impaired": false,
                        "ai_translated": true,
                        "files": [{ "file_id": 101, "file_name": "ai.srt" }]
                    }
                },
                {
                    "id": "human-en",
                    "type": "subtitle",
                    "attributes": {
                        "language": "EN",
                        "release": "Human.1080p.BluRay",
                        "download_count": 500,
                        "hearing_impaired": false,
                        "ai_translated": false,
                        "files": [{ "file_id": 202, "file_name": "human.srt" }]
                    }
                },
                {
                    "id": "german",
                    "type": "subtitle",
                    "attributes": {
                        "language": "de",
                        "release": "German",
                        "download_count": 100000,
                        "hearing_impaired": true,
                        "ai_translated": false,
                        "files": [{ "file_id": 303, "file_name": "de.srt" }]
                    }
                },
                {
                    "id": "no-file",
                    "type": "subtitle",
                    "attributes": {
                        "language": "en",
                        "download_count": 1,
                        "files": []
                    }
                }
            ]
        })))
        .expect(1)
        .mount(&server)
        .await;

    let results = client(&server)
        .search(&SubtitleQuery {
            tmdb_id: 27205,
            season: None,
            episode: None,
            languages: vec!["en".into(), "de".into()],
            moviehash: None,
        })
        .await
        .expect("search must succeed");

    // The file-less entry is dropped; three remain, ranked: English human,
    // English AI, then German (despite German's huge download count, because
    // English is the preferred language).
    assert_eq!(results.len(), 3, "entries without a file are dropped");
    assert_eq!(results[0].file_id, 202, "human English ranks first");
    assert_eq!(results[0].language, "en", "language is lower-cased");
    assert!(!results[0].ai_translated);
    assert_eq!(results[1].file_id, 101, "AI English second");
    assert!(results[1].ai_translated);
    assert_eq!(results[2].file_id, 303, "German last");
    assert!(results[2].hearing_impaired);
}

#[tokio::test]
async fn episode_search_uses_parent_id_and_season_episode() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/subtitles"))
        .and(query_param("parent_tmdb_id", "1396"))
        .and(query_param("season_number", "2"))
        .and(query_param("episode_number", "5"))
        .and(query_param("languages", "en"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "data": [{
                "id": "ep",
                "attributes": {
                    "language": "en",
                    "download_count": 10,
                    "files": [{ "file_id": 999 }]
                }
            }]
        })))
        .expect(1)
        .mount(&server)
        .await;

    let results = client(&server)
        .search(&SubtitleQuery {
            tmdb_id: 1396,
            season: Some(2),
            episode: Some(5),
            languages: vec!["en".into()],
            moviehash: None,
        })
        .await
        .expect("episode search");
    assert_eq!(results.len(), 1);
    assert_eq!(results[0].file_id, 999);
}

#[tokio::test]
async fn download_resolves_link_then_fetches_bytes() {
    let server = MockServer::start().await;
    // The CDN link is served by the same mock server (a second path).
    let cdn_link = format!("{}/cdn/sub.srt", server.uri());

    Mock::given(method("POST"))
        .and(path("/download"))
        .and(header("Api-Key", "test-key"))
        .and(body_partial_json(json!({ "file_id": 202 })))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "link": cdn_link,
            "file_name": "sub.srt",
            "requests": 3,
            "remaining": 97
        })))
        .expect(1)
        .mount(&server)
        .await;

    Mock::given(method("GET"))
        .and(path("/cdn/sub.srt"))
        .respond_with(ResponseTemplate::new(200).set_body_string(SAMPLE_SRT))
        .expect(1)
        .mount(&server)
        .await;

    let download = client(&server)
        .download_subtitle(202, None)
        .await
        .expect("download must succeed");
    assert!(download.text.contains("Hello world"));
    assert_eq!(download.remaining_quota, Some(97));
}

#[tokio::test]
async fn download_latin1_bytes_are_decoded() {
    let server = MockServer::start().await;
    let cdn_link = format!("{}/cdn/latin1.srt", server.uri());
    Mock::given(method("POST"))
        .and(path("/download"))
        .respond_with(
            ResponseTemplate::new(200).set_body_json(json!({ "link": cdn_link, "remaining": 5 })),
        )
        .mount(&server)
        .await;
    // "café" as latin-1: c a f 0xE9.
    let body: Vec<u8> = vec![
        b'1', b'\n', b'0', b'0', b':', b'0', b'0', b':', b'0', b'1', b',', b'0', b'0', b'0', b' ',
        b'-', b'-', b'>', b' ', b'0', b'0', b':', b'0', b'0', b':', b'0', b'2', b',', b'0', b'0',
        b'0', b'\n', b'c', b'a', b'f', 0xE9, b'\n',
    ];
    Mock::given(method("GET"))
        .and(path("/cdn/latin1.srt"))
        .respond_with(ResponseTemplate::new(200).set_body_bytes(body))
        .mount(&server)
        .await;

    let download = client(&server)
        .download_subtitle(1, None)
        .await
        .expect("download");
    assert!(download.text.contains("café"), "got: {}", download.text);
}

#[tokio::test]
async fn download_quota_exhausted_maps_to_upstream() {
    let server = MockServer::start().await;
    Mock::given(method("POST"))
        .and(path("/download"))
        .respond_with(ResponseTemplate::new(406).set_body_json(json!({
            "message": "You have downloaded your allowed 20 subtitles for today."
        })))
        .mount(&server)
        .await;

    let err = client(&server)
        .download_subtitle(202, None)
        .await
        .unwrap_err();
    match err {
        AppError::Upstream(msg) => assert!(msg.contains("quota"), "msg: {msg}"),
        other => panic!("expected Upstream, got {other:?}"),
    }
}

#[tokio::test]
async fn search_rate_limited_maps_to_upstream() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/subtitles"))
        .respond_with(ResponseTemplate::new(429))
        .mount(&server)
        .await;

    let err = client(&server)
        .search(&SubtitleQuery {
            tmdb_id: 1,
            season: None,
            episode: None,
            languages: vec!["en".into()],
            moviehash: None,
        })
        .await
        .unwrap_err();
    match err {
        AppError::Upstream(msg) => assert!(msg.contains("rate limit"), "msg: {msg}"),
        other => panic!("expected Upstream, got {other:?}"),
    }
}

#[tokio::test]
async fn bad_api_key_maps_to_upstream() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/subtitles"))
        .respond_with(ResponseTemplate::new(401))
        .mount(&server)
        .await;

    let err = client(&server)
        .search(&SubtitleQuery {
            tmdb_id: 1,
            season: None,
            episode: None,
            languages: vec![],
            moviehash: None,
        })
        .await
        .unwrap_err();
    assert!(matches!(err, AppError::Upstream(_)), "got: {err:?}");
}
