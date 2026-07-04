//! Async TMDB API client.
//!
//! The base URL is constructor-injected so tests can point at a wiremock
//! server. The API key travels as the `api_key` query parameter (v3 auth) and
//! must never appear in logs — error messages are built from status codes,
//! not URLs.

use serde::de::DeserializeOwned;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::error::{AppError, AppResult};

use super::models::{
    Episode, MediaType, Movie, RawEpisode, RawMovieDetails, RawSearchResponse, RawSeasonDetails,
    RawTvDetails, SearchResult, Season, TvShow,
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

pub struct TmdbClient {
    http: reqwest::Client,
    base_url: String,
    api_key: String,
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
        }
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

    /// Movie details including the IMDb id (via `append_to_response=external_ids`).
    pub async fn movie_details(&self, tmdb_id: i64) -> AppResult<Movie> {
        let raw: RawMovieDetails = self
            .get_json(
                &format!("/movie/{tmdb_id}"),
                &[("append_to_response", "external_ids".to_string())],
            )
            .await?;
        Ok(raw.into())
    }

    /// TV show details including external ids (IMDb/TVDB) and the season list.
    pub async fn tv_details(&self, tmdb_id: i64) -> AppResult<TvShow> {
        let raw: RawTvDetails = self
            .get_json(
                &format!("/tv/{tmdb_id}"),
                &[("append_to_response", "external_ids".to_string())],
            )
            .await?;
        Ok(raw.into())
    }

    pub async fn season_details(&self, tmdb_id: i64, season: u32) -> AppResult<Season> {
        let raw: RawSeasonDetails = self
            .get_json(&format!("/tv/{tmdb_id}/season/{season}"), &[])
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
            .get_json(
                &format!("/tv/{tmdb_id}/season/{season}/episode/{episode}"),
                &[],
            )
            .await?;
        Ok(raw.into())
    }
}
