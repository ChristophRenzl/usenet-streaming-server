//! Wiremock tests for the Newznab client: RSS parsing, NZB grab, error
//! documents and the multi-indexer fan-out.

use chrono::{TimeZone, Utc};
use usenet_streaming_server::{
    db::indexers::Indexer,
    error::AppError,
    indexer::{self, client::NewznabClient, SearchQuery},
};
use wiremock::{
    matchers::{method, path, query_param},
    Mock, MockServer, ResponseTemplate,
};

const RSS_FIXTURE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<rss version="2.0" xmlns:atom="http://www.w3.org/2005/Atom" xmlns:newznab="http://www.newznab.com/DTD/2010/feeds/attributes/">
  <channel>
    <atom:link href="https://indexer.example/api" rel="self" type="application/rss+xml"/>
    <title>example</title>
    <description>example API results</description>
    <item>
      <title>Movie.Name.2020.1080p.BluRay.x264-GROUP</title>
      <guid isPermaLink="true">https://indexer.example/details/abc123</guid>
      <link>https://indexer.example/getnzb/abc123.nzb&amp;i=1&amp;r=secret</link>
      <comments>https://indexer.example/details/abc123#comments</comments>
      <pubDate>Wed, 03 Jun 2020 12:30:00 +0000</pubDate>
      <category>Movies &gt; HD</category>
      <enclosure url="https://indexer.example/getnzb/abc123.nzb&amp;i=1&amp;r=secret" length="4294967296" type="application/x-nzb"/>
      <newznab:attr name="category" value="2040"/>
      <newznab:attr name="size" value="4831838208"/>
      <newznab:attr name="imdb" value="1375666"/>
      <newznab:attr name="grabs" value="120"/>
      <newznab:attr name="files" value="74"/>
    </item>
    <item>
      <title>Movie.Name.2020.720p.WEB-DL.H.264-OTHER</title>
      <guid isPermaLink="false">def456</guid>
      <link>https://indexer.example/getnzb/def456.nzb</link>
      <pubDate>Thu, 04 Jun 2020 08:00:00 +0000</pubDate>
      <enclosure url="https://indexer.example/getnzb/def456.nzb" length="2147483648" type="application/x-nzb"/>
    </item>
  </channel>
</rss>"#;

fn indexer_for(server: &MockServer) -> Indexer {
    Indexer {
        id: 7,
        name: "mock-indexer".into(),
        base_url: server.uri(),
        api_key: "s3cret".into(),
        enabled: true,
        priority: 0,
    }
}

#[tokio::test]
async fn movie_search_parses_rss_items() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("t", "movie"))
        .and(query_param("apikey", "s3cret"))
        // The client must strip the "tt" prefix per newznab convention.
        .and(query_param("imdbid", "1375666"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(RSS_FIXTURE, "application/rss+xml; charset=UTF-8"),
        )
        .expect(1)
        .mount(&server)
        .await;

    let client = NewznabClient::new(reqwest::Client::new(), indexer_for(&server));
    let releases = client
        .search(&SearchQuery::MovieByImdb {
            imdb_id: "tt1375666".into(),
        })
        .await
        .expect("search must succeed");

    assert_eq!(releases.len(), 2);

    let first = &releases[0];
    assert_eq!(first.title, "Movie.Name.2020.1080p.BluRay.x264-GROUP");
    assert_eq!(first.guid, "https://indexer.example/details/abc123");
    assert_eq!(
        first.nzb_url,
        "https://indexer.example/getnzb/abc123.nzb&i=1&r=secret"
    );
    // newznab:attr size wins over the enclosure length.
    assert_eq!(first.size_bytes, Some(4_831_838_208));
    assert_eq!(
        first.posted_at,
        Some(Utc.with_ymd_and_hms(2020, 6, 3, 12, 30, 0).unwrap())
    );
    assert_eq!(first.indexer_id, 7);
    assert_eq!(first.indexer_name, "mock-indexer");
    // Packaging signal for ranking: the `files` attr when reported.
    assert_eq!(first.file_count, Some(74));

    let second = &releases[1];
    assert_eq!(second.guid, "def456");
    // No newznab size attr — fall back to the enclosure length.
    assert_eq!(second.size_bytes, Some(2_147_483_648));
    assert_eq!(second.file_count, None);
}

#[tokio::test]
async fn tv_search_sends_tvdb_params() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .and(query_param("t", "tvsearch"))
        .and(query_param("tvdbid", "81189"))
        .and(query_param("season", "1"))
        .and(query_param("ep", "2"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"<?xml version="1.0"?><rss version="2.0"><channel><title>x</title></channel></rss>"#,
            "application/rss+xml",
        ))
        .expect(1)
        .mount(&server)
        .await;

    let client = NewznabClient::new(reqwest::Client::new(), indexer_for(&server));
    let releases = client
        .search(&SearchQuery::TvByTvdb {
            tvdb_id: 81189,
            season: 1,
            episode: 2,
        })
        .await
        .expect("search must succeed");
    assert!(releases.is_empty());
}

#[tokio::test]
async fn grab_fetches_nzb_bytes() {
    let server = MockServer::start().await;
    let nzb = br#"<?xml version="1.0" encoding="utf-8"?><nzb><file subject="test"/></nzb>"#;
    Mock::given(method("GET"))
        .and(path("/getnzb/abc123.nzb"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(nzb.to_vec(), "application/x-nzb"))
        .expect(1)
        .mount(&server)
        .await;

    let client = NewznabClient::new(reqwest::Client::new(), indexer_for(&server));
    let bytes = client
        .grab(&format!("{}/getnzb/abc123.nzb", server.uri()))
        .await
        .expect("grab must succeed");
    assert_eq!(bytes.as_ref(), nzb.as_slice());
}

#[tokio::test]
async fn newznab_error_document_maps_to_upstream() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .respond_with(ResponseTemplate::new(200).set_body_raw(
            r#"<?xml version="1.0" encoding="UTF-8"?><error code="100" description="Incorrect user credentials"/>"#,
            "application/xml",
        ))
        .mount(&server)
        .await;

    let client = NewznabClient::new(reqwest::Client::new(), indexer_for(&server));
    let err = client
        .search(&SearchQuery::Raw {
            query: "test".into(),
        })
        .await
        .unwrap_err();
    match err {
        AppError::Upstream(msg) => {
            assert!(msg.contains("100"), "msg: {msg}");
            assert!(msg.contains("Incorrect user credentials"), "msg: {msg}");
        }
        other => panic!("expected Upstream, got {other:?}"),
    }
}

#[tokio::test]
async fn fan_out_tolerates_a_broken_indexer() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/api"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_raw(RSS_FIXTURE, "application/rss+xml; charset=UTF-8"),
        )
        .mount(&server)
        .await;

    let good = indexer_for(&server);
    let broken = Indexer {
        id: 8,
        name: "unreachable".into(),
        // Nothing listens here — the fan-out must skip it, not fail.
        base_url: "http://127.0.0.1:1".into(),
        api_key: "k".into(),
        enabled: true,
        priority: 0,
    };

    let releases = indexer::search_all(
        &reqwest::Client::new(),
        vec![good, broken],
        &SearchQuery::MovieByImdb {
            imdb_id: "1375666".into(),
        },
    )
    .await;

    assert_eq!(releases.len(), 2, "good indexer results must survive");
    assert!(releases.iter().all(|r| r.indexer_name == "mock-indexer"));
}
