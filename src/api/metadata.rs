//! TMDB-backed metadata endpoints: search, discovery lists (trending,
//! popular, top rated) and movie/TV/season/episode details.

use axum::{
    extract::{Path, Query, State},
    Json,
};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::{
    db,
    error::{AppError, AppResult},
    state::AppState,
    tmdb::{
        client::{ListKind, PagedSearchResults, SearchType, TrendingType, TrendingWindow},
        models::{Episode, Movie, SearchResult, Season, TvShow},
        TmdbClient,
    },
};

/// Build a TMDB client with the API key from `app_settings`, failing with a
/// helpful 400 when the key is not configured yet.
pub async fn tmdb_client(state: &AppState) -> AppResult<TmdbClient> {
    let key = db::settings::get(&state.db, db::settings::TMDB_API_KEY)
        .await?
        .filter(|k| !k.is_empty())
        .ok_or_else(|| {
            AppError::BadRequest(
                "TMDB API key not configured; set it via PUT /api/v1/settings/app".into(),
            )
        })?;
    Ok(TmdbClient::new(
        state.http.clone(),
        state.tmdb_base_url.as_ref(),
        key,
    ))
}

#[derive(Debug, Deserialize, IntoParams)]
pub struct SearchParams {
    /// Free-text title query.
    pub query: String,
    /// Search scope; defaults to `multi` (movies and TV).
    #[serde(rename = "type", default)]
    pub search_type: SearchType,
    /// Release year (movie) / first air year (tv). Ignored for `multi`.
    pub year: Option<i32>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SearchResponse {
    pub results: Vec<SearchResult>,
}

/// Search TMDB for movies and TV shows.
#[utoipa::path(get, path = "/search", tag = "metadata",
    params(SearchParams),
    responses(
        (status = 200, body = SearchResponse),
        (status = 400, description = "Missing query or TMDB API key not configured"),
        (status = 502, description = "TMDB upstream error"),
    ))]
pub async fn search(
    State(state): State<AppState>,
    Query(params): Query<SearchParams>,
) -> AppResult<Json<SearchResponse>> {
    if params.query.trim().is_empty() {
        return Err(AppError::BadRequest("query must not be empty".into()));
    }
    let client = tmdb_client(&state).await?;
    let results = client
        .search(params.query.trim(), params.search_type, params.year)
        .await?;
    Ok(Json(SearchResponse { results }))
}

// ---- Discovery lists --------------------------------------------------------

/// One page of a discovery list (trending / popular / top rated).
#[derive(Debug, Serialize, ToSchema)]
pub struct DiscoverResponse {
    pub results: Vec<SearchResult>,
    /// 1-based page this response covers.
    pub page: i64,
    pub total_pages: i64,
}

impl From<PagedSearchResults> for DiscoverResponse {
    fn from(paged: PagedSearchResults) -> Self {
        Self {
            results: paged.results,
            page: paged.page,
            total_pages: paged.total_pages,
        }
    }
}

/// Reject `page=0` early; TMDB pages are 1-based.
fn validated_page(page: Option<u32>) -> AppResult<Option<u32>> {
    match page {
        Some(0) => Err(AppError::BadRequest("page must be >= 1".into())),
        other => Ok(other),
    }
}

#[derive(Debug, Deserialize, IntoParams)]
pub struct TrendingParams {
    /// Scope: `all` (default), `movie` or `tv`.
    #[serde(default)]
    pub media_type: TrendingType,
    /// Time window: `day` or `week` (default).
    #[serde(default)]
    pub window: TrendingWindow,
    /// 1-based page (defaults to 1).
    pub page: Option<u32>,
}

/// Trending movies and TV shows (person results are dropped).
#[utoipa::path(get, path = "/trending", tag = "metadata",
    params(TrendingParams),
    responses(
        (status = 200, body = DiscoverResponse),
        (status = 400, description = "Bad page or TMDB API key not configured"),
        (status = 502, description = "TMDB upstream error"),
    ))]
pub async fn trending(
    State(state): State<AppState>,
    Query(params): Query<TrendingParams>,
) -> AppResult<Json<DiscoverResponse>> {
    let page = validated_page(params.page)?;
    let client = tmdb_client(&state).await?;
    let paged = client
        .trending(params.media_type, params.window, page)
        .await?;
    Ok(Json(paged.into()))
}

#[derive(Debug, Deserialize, IntoParams)]
pub struct PageParams {
    /// 1-based page (defaults to 1).
    pub page: Option<u32>,
}

/// Popular movies.
#[utoipa::path(get, path = "/movies/popular", tag = "metadata",
    params(PageParams),
    responses(
        (status = 200, body = DiscoverResponse),
        (status = 400, description = "Bad page or TMDB API key not configured"),
        (status = 502, description = "TMDB upstream error"),
    ))]
pub async fn popular_movies(
    State(state): State<AppState>,
    Query(params): Query<PageParams>,
) -> AppResult<Json<DiscoverResponse>> {
    let page = validated_page(params.page)?;
    let client = tmdb_client(&state).await?;
    Ok(Json(
        client.movie_list(ListKind::Popular, page).await?.into(),
    ))
}

/// Top-rated movies.
#[utoipa::path(get, path = "/movies/top_rated", tag = "metadata",
    params(PageParams),
    responses(
        (status = 200, body = DiscoverResponse),
        (status = 400, description = "Bad page or TMDB API key not configured"),
        (status = 502, description = "TMDB upstream error"),
    ))]
pub async fn top_rated_movies(
    State(state): State<AppState>,
    Query(params): Query<PageParams>,
) -> AppResult<Json<DiscoverResponse>> {
    let page = validated_page(params.page)?;
    let client = tmdb_client(&state).await?;
    Ok(Json(
        client.movie_list(ListKind::TopRated, page).await?.into(),
    ))
}

/// Popular TV shows.
#[utoipa::path(get, path = "/tv/popular", tag = "metadata",
    params(PageParams),
    responses(
        (status = 200, body = DiscoverResponse),
        (status = 400, description = "Bad page or TMDB API key not configured"),
        (status = 502, description = "TMDB upstream error"),
    ))]
pub async fn popular_tv(
    State(state): State<AppState>,
    Query(params): Query<PageParams>,
) -> AppResult<Json<DiscoverResponse>> {
    let page = validated_page(params.page)?;
    let client = tmdb_client(&state).await?;
    Ok(Json(client.tv_list(ListKind::Popular, page).await?.into()))
}

/// Top-rated TV shows.
#[utoipa::path(get, path = "/tv/top_rated", tag = "metadata",
    params(PageParams),
    responses(
        (status = 200, body = DiscoverResponse),
        (status = 400, description = "Bad page or TMDB API key not configured"),
        (status = 502, description = "TMDB upstream error"),
    ))]
pub async fn top_rated_tv(
    State(state): State<AppState>,
    Query(params): Query<PageParams>,
) -> AppResult<Json<DiscoverResponse>> {
    let page = validated_page(params.page)?;
    let client = tmdb_client(&state).await?;
    Ok(Json(client.tv_list(ListKind::TopRated, page).await?.into()))
}

// ---- Details ----------------------------------------------------------------

/// Movie details (includes IMDb id).
#[utoipa::path(get, path = "/movies/{tmdb_id}", tag = "metadata",
    params(("tmdb_id" = i64, Path, description = "TMDB movie id")),
    responses(
        (status = 200, body = Movie),
        (status = 404, description = "Unknown TMDB id"),
    ))]
pub async fn movie_details(
    State(state): State<AppState>,
    Path(tmdb_id): Path<i64>,
) -> AppResult<Json<Movie>> {
    let client = tmdb_client(&state).await?;
    Ok(Json(client.movie_details(tmdb_id).await?))
}

/// TV show details (includes external ids and season list).
#[utoipa::path(get, path = "/tv/{tmdb_id}", tag = "metadata",
    params(("tmdb_id" = i64, Path, description = "TMDB TV show id")),
    responses(
        (status = 200, body = TvShow),
        (status = 404, description = "Unknown TMDB id"),
    ))]
pub async fn tv_details(
    State(state): State<AppState>,
    Path(tmdb_id): Path<i64>,
) -> AppResult<Json<TvShow>> {
    let client = tmdb_client(&state).await?;
    Ok(Json(client.tv_details(tmdb_id).await?))
}

/// Season details with the full episode list.
#[utoipa::path(get, path = "/tv/{tmdb_id}/season/{season}", tag = "metadata",
    params(
        ("tmdb_id" = i64, Path, description = "TMDB TV show id"),
        ("season" = u32, Path, description = "Season number"),
    ),
    responses(
        (status = 200, body = Season),
        (status = 404, description = "Unknown show or season"),
    ))]
pub async fn season_details(
    State(state): State<AppState>,
    Path((tmdb_id, season)): Path<(i64, u32)>,
) -> AppResult<Json<Season>> {
    let client = tmdb_client(&state).await?;
    Ok(Json(client.season_details(tmdb_id, season).await?))
}

/// Single episode details.
#[utoipa::path(get, path = "/tv/{tmdb_id}/season/{season}/episode/{episode}", tag = "metadata",
    params(
        ("tmdb_id" = i64, Path, description = "TMDB TV show id"),
        ("season" = u32, Path, description = "Season number"),
        ("episode" = u32, Path, description = "Episode number"),
    ),
    responses(
        (status = 200, body = Episode),
        (status = 404, description = "Unknown show, season or episode"),
    ))]
pub async fn episode_details(
    State(state): State<AppState>,
    Path((tmdb_id, season, episode)): Path<(i64, u32, u32)>,
) -> AppResult<Json<Episode>> {
    let client = tmdb_client(&state).await?;
    Ok(Json(
        client.episode_details(tmdb_id, season, episode).await?,
    ))
}

pub fn router() -> OpenApiRouter<AppState> {
    // Static segments (`/movies/popular`) win over captures
    // (`/movies/{tmdb_id}`) in axum 0.8, so both can coexist.
    OpenApiRouter::new()
        .routes(routes!(search))
        .routes(routes!(trending))
        .routes(routes!(popular_movies))
        .routes(routes!(top_rated_movies))
        .routes(routes!(popular_tv))
        .routes(routes!(top_rated_tv))
        .routes(routes!(movie_details))
        .routes(routes!(tv_details))
        .routes(routes!(season_details))
        .routes(routes!(episode_details))
}
