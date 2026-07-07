//! Async TMDB API client.
//!
//! The base URL is constructor-injected so tests can point at a wiremock
//! server. The API key travels as the `api_key` query parameter (v3 auth) and
//! must never appear in logs — error messages are built from status codes,
//! not URLs.

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::error::{AppError, AppResult};

use super::models::{
    Collection, Episode, Genre, MediaType, Movie, Person, RawCollection, RawEpisode, RawGenreList,
    RawMovieDetails, RawPerson, RawSearchResponse, RawSeasonDetails, RawTvDetails, SearchResult,
    Season, TvShow,
};

/// Search scope for [`TmdbClient::search`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum SearchType {
    Movie,
    Tv,
    #[default]
    Multi,
}

/// Media-type scope for [`TmdbClient::trending`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum TrendingType {
    #[default]
    All,
    Movie,
    Tv,
}

/// Time window for [`TmdbClient::trending`].
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum TrendingWindow {
    Day,
    #[default]
    Week,
}

impl TrendingWindow {
    fn as_path(self) -> &'static str {
        match self {
            Self::Day => "day",
            Self::Week => "week",
        }
    }
}

/// Curated TMDB list flavor for [`TmdbClient::movie_list`] / [`TmdbClient::tv_list`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListKind {
    Popular,
    TopRated,
}

impl ListKind {
    fn as_path(self) -> &'static str {
        match self {
            Self::Popular => "popular",
            Self::TopRated => "top_rated",
        }
    }
}

/// One page of a TMDB result list (trending / popular / top rated).
#[derive(Debug, Clone)]
pub struct PagedSearchResults {
    pub results: Vec<SearchResult>,
    /// 1-based page this response covers.
    pub page: i64,
    pub total_pages: i64,
}

/// Shared TTL cache for TMDB *detail* lookups (movie/tv/season/episode).
/// Detail payloads change rarely, and session start re-fetches exactly what
/// the browsing UI (or the ranking step moments earlier) already pulled — a
/// short TTL removes those repeat upstream round trips from every play
/// without staleness a user could notice. Cheap to clone; lives on
/// [`AppState`](crate::state::AppState) so the per-request clients share it.
#[derive(Clone, Default)]
pub struct DetailsCache {
    entries: Arc<Mutex<HashMap<String, (Instant, serde_json::Value)>>>,
}

/// How long a cached detail payload is served before re-fetching.
const DETAILS_TTL: Duration = Duration::from_secs(600);
/// Hard cap on cached payloads; expired entries are dropped first, then the
/// whole map (simple and rare — 512 titles of browsing within the TTL).
const DETAILS_CAP: usize = 512;

impl DetailsCache {
    fn get(&self, key: &str) -> Option<serde_json::Value> {
        let entries = self.entries.lock().expect("tmdb cache lock");
        entries
            .get(key)
            .filter(|(at, _)| at.elapsed() < DETAILS_TTL)
            .map(|(_, value)| value.clone())
    }

    fn put(&self, key: String, value: serde_json::Value) {
        let mut entries = self.entries.lock().expect("tmdb cache lock");
        if entries.len() >= DETAILS_CAP {
            entries.retain(|_, (at, _)| at.elapsed() < DETAILS_TTL);
            if entries.len() >= DETAILS_CAP {
                entries.clear();
            }
        }
        entries.insert(key, (Instant::now(), value));
    }
}

pub struct TmdbClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
    details_cache: Option<DetailsCache>,
}

impl TmdbClient {
    pub fn new(
        http: reqwest::Client,
        base_url: impl Into<String>,
        api_key: impl Into<String>,
    ) -> Self {
        Self {
            http,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            api_key: api_key.into(),
            details_cache: None,
        }
    }

    /// Serve detail lookups (movie/tv/season/episode) through `cache`.
    pub fn with_details_cache(mut self, cache: DetailsCache) -> Self {
        self.details_cache = Some(cache);
        self
    }

    async fn get_json<T: DeserializeOwned>(
        &self,
        path: &str,
        params: &[(&str, String)],
    ) -> AppResult<T> {
        let url = format!("{}{path}", self.base_url);
        let response = self
            .http
            .get(&url)
            .query(&[("api_key", self.api_key.as_str())])
            .query(params)
            .send()
            .await
            .map_err(|e| AppError::Upstream(format!("TMDB request failed: {}", e.without_url())))?;

        let status = response.status();
        if status == reqwest::StatusCode::NOT_FOUND {
            return Err(AppError::NotFound(format!("TMDB resource {path}")));
        }
        if !status.is_success() {
            return Err(AppError::Upstream(format!("TMDB returned HTTP {status}")));
        }
        response.json().await.map_err(|e| {
            AppError::Upstream(format!("TMDB response decode failed: {}", e.without_url()))
        })
    }

    /// [`get_json`](Self::get_json) with the details cache in front (when one
    /// is attached). The raw JSON payload is cached so one entry serves any
    /// deserialization target.
    async fn get_json_cached<T: DeserializeOwned>(
        &self,
        path: &str,
        params: &[(&str, String)],
    ) -> AppResult<T> {
        let Some(cache) = &self.details_cache else {
            return self.get_json(path, params).await;
        };
        let key = format!("{path}?{params:?}");
        let value = match cache.get(&key) {
            Some(value) => value,
            None => {
                let value: serde_json::Value = self.get_json(path, params).await?;
                cache.put(key, value.clone());
                value
            }
        };
        serde_json::from_value(value)
            .map_err(|e| AppError::Upstream(format!("TMDB response decode failed: {e}")))
    }

    /// Search movies/TV. `year` narrows movie (release year) and TV (first air
    /// year) searches; TMDB's multi search does not support a year filter.
    pub async fn search(
        &self,
        query: &str,
        search_type: SearchType,
        year: Option<i32>,
    ) -> AppResult<Vec<SearchResult>> {
        let mut params = vec![("query", query.to_string())];
        let (path, forced) = match search_type {
            SearchType::Movie => {
                if let Some(y) = year {
                    params.push(("year", y.to_string()));
                }
                ("/search/movie", Some(MediaType::Movie))
            }
            SearchType::Tv => {
                if let Some(y) = year {
                    params.push(("first_air_date_year", y.to_string()));
                }
                ("/search/tv", Some(MediaType::Tv))
            }
            SearchType::Multi => ("/search/multi", None),
        };
        let raw: RawSearchResponse = self.get_json(path, &params).await?;
        Ok(raw
            .results
            .into_iter()
            .filter_map(|item| item.into_result(forced))
            .collect())
    }

    /// One page of a TMDB list endpoint mapped to [`SearchResult`]s. `forced`
    /// stamps the media type onto endpoints that omit `media_type` in their
    /// payload; items that are neither movie nor TV (people) are dropped.
    async fn paged_list(
        &self,
        path: &str,
        forced: Option<MediaType>,
        page: Option<u32>,
    ) -> AppResult<PagedSearchResults> {
        let mut params = Vec::new();
        if let Some(page) = page {
            params.push(("page", page.to_string()));
        }
        let raw: RawSearchResponse = self.get_json(path, &params).await?;
        Ok(PagedSearchResults {
            page: raw.page,
            total_pages: raw.total_pages,
            results: raw
                .results
                .into_iter()
                .filter_map(|item| item.into_result(forced))
                .collect(),
        })
    }

    /// Trending movies/TV: `/trending/{all|movie|tv}/{day|week}`.
    pub async fn trending(
        &self,
        scope: TrendingType,
        window: TrendingWindow,
        page: Option<u32>,
    ) -> AppResult<PagedSearchResults> {
        let (segment, forced) = match scope {
            TrendingType::All => ("all", None),
            TrendingType::Movie => ("movie", Some(MediaType::Movie)),
            TrendingType::Tv => ("tv", Some(MediaType::Tv)),
        };
        self.paged_list(
            &format!("/trending/{segment}/{}", window.as_path()),
            forced,
            page,
        )
        .await
    }

    /// Curated movie list: `/movie/popular` or `/movie/top_rated`.
    pub async fn movie_list(
        &self,
        kind: ListKind,
        page: Option<u32>,
    ) -> AppResult<PagedSearchResults> {
        self.paged_list(
            &format!("/movie/{}", kind.as_path()),
            Some(MediaType::Movie),
            page,
        )
        .await
    }

    /// Curated TV list: `/tv/popular` or `/tv/top_rated`.
    pub async fn tv_list(
        &self,
        kind: ListKind,
        page: Option<u32>,
    ) -> AppResult<PagedSearchResults> {
        self.paged_list(
            &format!("/tv/{}", kind.as_path()),
            Some(MediaType::Tv),
            page,
        )
        .await
    }

    /// Genre list for a media type: `/genre/movie/list` or `/genre/tv/list`.
    pub async fn genres(&self, media_type: MediaType) -> AppResult<Vec<Genre>> {
        let raw: RawGenreList = self
            .get_json(&format!("/genre/{}/list", media_type.as_str()), &[])
            .await?;
        Ok(raw.genres)
    }

    /// Discover movies or TV shows, optionally filtered by genre and sorted.
    /// Maps `/discover/movie` or `/discover/tv` with `with_genres`, `page` and
    /// `sort_by` (TMDB default `popularity.desc` when `sort_by` is `None`).
    pub async fn discover(
        &self,
        media_type: MediaType,
        genre_id: Option<i64>,
        page: Option<u32>,
        sort_by: Option<&str>,
    ) -> AppResult<PagedSearchResults> {
        let mut params = Vec::new();
        if let Some(genre_id) = genre_id {
            params.push(("with_genres", genre_id.to_string()));
        }
        if let Some(page) = page {
            params.push(("page", page.to_string()));
        }
        if let Some(sort_by) = sort_by {
            params.push(("sort_by", sort_by.to_string()));
        }
        let path = format!("/discover/{}", media_type.as_str());
        let raw: RawSearchResponse = self.get_json(&path, &params).await?;
        Ok(PagedSearchResults {
            page: raw.page,
            total_pages: raw.total_pages,
            results: raw
                .results
                .into_iter()
                .filter_map(|item| item.into_result(Some(media_type)))
                .collect(),
        })
    }

    /// Movie details including the IMDb id, best YouTube trailer key, cast and
    /// collection membership (via `append_to_response=external_ids,videos,credits`).
    pub async fn movie_details(&self, tmdb_id: i64) -> AppResult<Movie> {
        let raw: RawMovieDetails = self
            .get_json_cached(
                &format!("/movie/{tmdb_id}"),
                &[(
                    "append_to_response",
                    "external_ids,videos,credits".to_string(),
                )],
            )
            .await?;
        Ok(raw.into())
    }

    /// TV show details including external ids (IMDb/TVDB), the best YouTube
    /// trailer key, the season list and cast (via
    /// `append_to_response=external_ids,videos,credits`).
    pub async fn tv_details(&self, tmdb_id: i64) -> AppResult<TvShow> {
        let raw: RawTvDetails = self
            .get_json_cached(
                &format!("/tv/{tmdb_id}"),
                &[(
                    "append_to_response",
                    "external_ids,videos,credits".to_string(),
                )],
            )
            .await?;
        Ok(raw.into())
    }

    /// Titles similar to one movie/show (TMDB recommendations, page 1) — the
    /// "More Like This" row on detail screens.
    pub async fn recommendations(
        &self,
        media_type: MediaType,
        tmdb_id: i64,
    ) -> AppResult<PagedSearchResults> {
        let segment = match media_type {
            MediaType::Movie => "movie",
            MediaType::Tv => "tv",
        };
        self.paged_list(
            &format!("/{segment}/{tmdb_id}/recommendations"),
            Some(media_type),
            None,
        )
        .await
    }

    /// A movie collection ("saga") with its member movies in release order.
    pub async fn collection(&self, id: i64) -> AppResult<Collection> {
        let raw: RawCollection = self.get_json(&format!("/collection/{id}"), &[]).await?;
        Ok(raw.into())
    }

    /// A person with their combined movie/TV credits, most popular first.
    pub async fn person(&self, id: i64) -> AppResult<Person> {
        let raw: RawPerson = self
            .get_json(
                &format!("/person/{id}"),
                &[("append_to_response", "combined_credits".to_string())],
            )
            .await?;
        Ok(raw.into())
    }

    pub async fn season_details(&self, tmdb_id: i64, season: u32) -> AppResult<Season> {
        let raw: RawSeasonDetails = self
            .get_json_cached(&format!("/tv/{tmdb_id}/season/{season}"), &[])
            .await?;
        Ok(raw.into())
    }

    pub async fn episode_details(
        &self,
        tmdb_id: i64,
        season: u32,
        episode: u32,
    ) -> AppResult<Episode> {
        let raw: RawEpisode = self
            .get_json_cached(
                &format!("/tv/{tmdb_id}/season/{season}/episode/{episode}"),
                &[],
            )
            .await?;
        Ok(raw.into())
    }
}
