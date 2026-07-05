//! Wiremock tests for the TMDB client: search mapping, details with
//! external ids, and error translation.

use serde_json::json;
use usenet_streaming_server::{
    error::AppError,
    tmdb::{client::SearchType, models::MediaType, TmdbClient},
};
use wiremock::{
    matchers::{method, path, query_param},
    Mock, MockServer, ResponseTemplate,
};

fn client(server: &MockServer) -> TmdbClient {
    TmdbClient::new(reqwest::Client::new(), server.uri(), "test-key")
}

#[tokio::test]
async fn multi_search_maps_results_and_drops_people() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/search/multi"))
        .and(query_param("api_key", "test-key"))
        .and(query_param("query", "inception"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "page": 1,
            "results": [
                {
                    "adult": false,
                    "backdrop_path": "/xJHokMbljvjADYdit5fK5VQsXEG.jpg",
                    "id": 27205,
                    "title": "Inception",
                    "original_language": "en",
                    "original_title": "Inception",
                    "overview": "Cobb, a skilled thief who commits corporate espionage...",
                    "poster_path": "/oYuLEt3zVCKq57qu2F8dT7NIa6f.jpg",
                    "media_type": "movie",
                    "genre_ids": [28, 878, 12],
                    "release_date": "2010-07-15",
                    "vote_average": 8.369,
                    "vote_count": 36855
                },
                {
                    "id": 6193,
                    "name": "Leonardo DiCaprio",
                    "media_type": "person",
                    "known_for_department": "Acting"
                },
                {
                    "backdrop_path": "/tsRy63Mu5cu8etL1X7ZLyf7UP1M.jpg",
                    "id": 1396,
                    "name": "Breaking Bad",
                    "overview": "Walter White, a New Mexico chemistry teacher...",
                    "poster_path": "/ztkUQFLlC19CCMYHW9o1zWhJRNq.jpg",
                    "media_type": "tv",
                    "first_air_date": "2008-01-20",
                    "vote_average": 8.9
                }
            ],
            "total_pages": 1,
            "total_results": 3
        })))
        .expect(1)
        .mount(&server)
        .await;

    let results = client(&server)
        .search("inception", SearchType::Multi, None)
        .await
        .expect("search must succeed");

    assert_eq!(results.len(), 2, "person results must be dropped");

    let movie = &results[0];
    assert_eq!(movie.tmdb_id, 27205);
    assert_eq!(movie.media_type, MediaType::Movie);
    assert_eq!(movie.title, "Inception");
    assert_eq!(movie.year, Some(2010));
    assert_eq!(
        movie.poster_url.as_deref(),
        Some("https://image.tmdb.org/t/p/w500/oYuLEt3zVCKq57qu2F8dT7NIa6f.jpg")
    );
    assert_eq!(
        movie.backdrop_url.as_deref(),
        Some("https://image.tmdb.org/t/p/w780/xJHokMbljvjADYdit5fK5VQsXEG.jpg")
    );
    assert_eq!(movie.vote_average, Some(8.369));

    let show = &results[1];
    assert_eq!(show.media_type, MediaType::Tv);
    assert_eq!(show.title, "Breaking Bad");
    assert_eq!(show.year, Some(2008));
}

#[tokio::test]
async fn movie_search_passes_year_filter() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/search/movie"))
        .and(query_param("query", "dune"))
        .and(query_param("year", "2021"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "page": 1,
            "results": [{
                "id": 438631,
                "title": "Dune",
                "release_date": "2021-09-15",
                "overview": "Paul Atreides...",
                "poster_path": "/d5NXSklXo0qyIYkgV94XAgMIckC.jpg",
                "vote_average": 7.8
            }],
            "total_pages": 1,
            "total_results": 1
        })))
        .expect(1)
        .mount(&server)
        .await;

    let results = client(&server)
        .search("dune", SearchType::Movie, Some(2021))
        .await
        .expect("search must succeed");
    assert_eq!(results.len(), 1);
    // /search/movie omits media_type in its payload; the client must force it.
    assert_eq!(results[0].media_type, MediaType::Movie);
    assert_eq!(results[0].year, Some(2021));
}

#[tokio::test]
async fn movie_details_extracts_imdb_id_from_external_ids() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/movie/27205"))
        .and(query_param("append_to_response", "external_ids"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "adult": false,
            "backdrop_path": "/xJHokMbljvjADYdit5fK5VQsXEG.jpg",
            "budget": 160000000,
            "id": 27205,
            "imdb_id": "tt1375666",
            "title": "Inception",
            "overview": "Cobb, a skilled thief...",
            "poster_path": "/oYuLEt3zVCKq57qu2F8dT7NIa6f.jpg",
            "release_date": "2010-07-15",
            "runtime": 148,
            "vote_average": 8.369,
            "external_ids": {
                "imdb_id": "tt1375666",
                "wikidata_id": "Q25188",
                "facebook_id": "inception",
                "instagram_id": null,
                "twitter_id": null
            }
        })))
        .expect(1)
        .mount(&server)
        .await;

    let movie = client(&server)
        .movie_details(27205)
        .await
        .expect("details must succeed");
    assert_eq!(movie.imdb_id.as_deref(), Some("tt1375666"));
    assert_eq!(movie.title, "Inception");
    assert_eq!(movie.year, Some(2010));
    assert_eq!(movie.runtime_minutes, Some(148));
}

#[tokio::test]
async fn tv_details_include_tvdb_id_and_seasons() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/tv/1396"))
        .and(query_param("append_to_response", "external_ids"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "id": 1396,
            "name": "Breaking Bad",
            "first_air_date": "2008-01-20",
            "overview": "Walter White...",
            "poster_path": "/ztkUQFLlC19CCMYHW9o1zWhJRNq.jpg",
            "backdrop_path": "/tsRy63Mu5cu8etL1X7ZLyf7UP1M.jpg",
            "vote_average": 8.9,
            "external_ids": {
                "imdb_id": "tt0903747",
                "tvdb_id": 81189
            },
            "seasons": [
                {
                    "air_date": "2008-01-20",
                    "episode_count": 7,
                    "id": 3572,
                    "name": "Season 1",
                    "poster_path": "/1BP4xYv9ZG4ZVHkL7ocOziBbSYH.jpg",
                    "season_number": 1
                },
                {
                    "air_date": "2009-03-08",
                    "episode_count": 13,
                    "id": 3573,
                    "name": "Season 2",
                    "poster_path": null,
                    "season_number": 2
                }
            ]
        })))
        .mount(&server)
        .await;

    let show = client(&server).tv_details(1396).await.expect("tv details");
    assert_eq!(show.imdb_id.as_deref(), Some("tt0903747"));
    assert_eq!(show.tvdb_id, Some(81189));
    assert_eq!(show.seasons.len(), 2);
    assert_eq!(show.seasons[0].season_number, 1);
    assert_eq!(show.seasons[0].episode_count, Some(7));
    assert!(show.seasons[1].poster_url.is_none());
}

#[tokio::test]
async fn season_details_map_episodes() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/tv/1396/season/1"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "air_date": "2008-01-20",
            "name": "Season 1",
            "overview": "High school chemistry teacher...",
            "season_number": 1,
            "poster_path": "/1BP4xYv9ZG4ZVHkL7ocOziBbSYH.jpg",
            "episodes": [
                {
                    "air_date": "2008-01-20",
                    "episode_number": 1,
                    "name": "Pilot",
                    "overview": "Walter White, a struggling chemistry teacher...",
                    "season_number": 1,
                    "still_path": "/ydlY3iPfeOAvu8gVqrxPoMvzNCn.jpg",
                    "vote_average": 8.2
                },
                {
                    "air_date": "2008-01-27",
                    "episode_number": 2,
                    "name": "Cat's in the Bag...",
                    "overview": "Walt and Jesse attempt to tie up loose ends.",
                    "season_number": 1,
                    "still_path": null,
                    "vote_average": 8.1
                }
            ]
        })))
        .mount(&server)
        .await;

    let season = client(&server)
        .season_details(1396, 1)
        .await
        .expect("season details");
    assert_eq!(season.season_number, 1);
    assert_eq!(season.episodes.len(), 2);
    assert_eq!(season.episodes[0].title.as_deref(), Some("Pilot"));
    assert_eq!(season.episodes[0].episode_number, 1);
    assert_eq!(
        season.episodes[0].still_url.as_deref(),
        Some("https://image.tmdb.org/t/p/w300/ydlY3iPfeOAvu8gVqrxPoMvzNCn.jpg")
    );
    assert!(season.episodes[1].still_url.is_none());
}

#[tokio::test]
async fn trending_defaults_page_fields_when_missing() {
    use usenet_streaming_server::tmdb::client::{TrendingType, TrendingWindow};

    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/trending/tv/day"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({
            "results": [{
                "id": 1396,
                "name": "Breaking Bad",
                "media_type": "tv",
                "first_air_date": "2008-01-20"
            }]
        })))
        .expect(1)
        .mount(&server)
        .await;

    let paged = client(&server)
        .trending(TrendingType::Tv, TrendingWindow::Day, None)
        .await
        .expect("trending must succeed");
    assert_eq!(paged.page, 1, "missing page defaults to 1");
    assert_eq!(paged.total_pages, 1, "missing total_pages defaults to 1");
    assert_eq!(paged.results.len(), 1);
    assert_eq!(paged.results[0].media_type, MediaType::Tv);
}

#[tokio::test]
async fn tmdb_404_maps_to_not_found() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/movie/999999999"))
        .respond_with(ResponseTemplate::new(404).set_body_json(json!({
            "success": false,
            "status_code": 34,
            "status_message": "The resource you requested could not be found."
        })))
        .mount(&server)
        .await;

    let err = client(&server)
        .movie_details(999_999_999)
        .await
        .unwrap_err();
    assert!(matches!(err, AppError::NotFound(_)), "got: {err:?}");
}

#[tokio::test]
async fn tmdb_5xx_maps_to_upstream() {
    let server = MockServer::start().await;
    Mock::given(method("GET"))
        .and(path("/movie/27205"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;

    let err = client(&server).movie_details(27205).await.unwrap_err();
    match err {
        AppError::Upstream(msg) => assert!(msg.contains("503"), "msg: {msg}"),
        other => panic!("expected Upstream, got {other:?}"),
    }
}
