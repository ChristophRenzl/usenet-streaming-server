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
        models::{
            Collection, Episode, Genre, GenreList, MediaType, Movie, Person, SearchResult, Season,
            TvShow,
        },
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

// ---- Genres + genre-filtered discovery --------------------------------------

#[derive(Debug, Deserialize, IntoParams)]
pub struct GenresParams {
    /// Which genre catalog to return: `movie` or `tv`.
    pub media_type: MediaType,
}

/// Enriched genre lists are cached this long: the backdrops come from one
/// discover call per genre, which would otherwise fan out ~19 TMDB requests
/// on every app launch for artwork that shifts slowly.
const GENRE_CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(6 * 60 * 60);

type GenreCache =
    std::sync::Mutex<std::collections::HashMap<MediaType, (std::time::Instant, Vec<Genre>)>>;
static GENRE_CACHE: std::sync::LazyLock<GenreCache> = std::sync::LazyLock::new(Default::default);

/// List the TMDB genres for movies or TV shows, each carrying the backdrop of
/// its current top discover hit for the genre-browse tiles. Backdrops are
/// best-effort — a discovery failure yields `backdrop_url: null`, never an
/// error — and the enriched list is cached for a few hours.
#[utoipa::path(get, path = "/genres", tag = "metadata",
    params(GenresParams),
    responses(
        (status = 200, body = GenreList),
        (status = 400, description = "Missing/invalid media_type or TMDB API key not configured"),
        (status = 502, description = "TMDB upstream error"),
    ))]
pub async fn genres(
    State(state): State<AppState>,
    Query(params): Query<GenresParams>,
) -> AppResult<Json<GenreList>> {
    if let Some((at, cached)) = GENRE_CACHE
        .lock()
        .expect("genre cache lock")
        .get(&params.media_type)
        .cloned()
    {
        if at.elapsed() < GENRE_CACHE_TTL {
            return Ok(Json(GenreList { genres: cached }));
        }
    }

    let client = tmdb_client(&state).await?;
    let mut genres = client.genres(params.media_type).await?;
    let backdrops = futures::future::join_all(genres.iter().map(|genre| {
        let client = &client;
        async move {
            client
                .discover(params.media_type, Some(genre.id), None, None)
                .await
                .ok()
                .and_then(|page| {
                    page.results
                        .into_iter()
                        .find_map(|result| result.backdrop_url)
                })
        }
    }))
    .await;
    for (genre, backdrop) in genres.iter_mut().zip(backdrops) {
        genre.backdrop_url = backdrop;
    }

    GENRE_CACHE.lock().expect("genre cache lock").insert(
        params.media_type,
        (std::time::Instant::now(), genres.clone()),
    );
    Ok(Json(GenreList { genres }))
}

/// Movies similar to one movie (TMDB recommendations), for the "More Like
/// This" row on the detail screen.
#[utoipa::path(get, path = "/movies/{tmdb_id}/similar", tag = "metadata",
    params(("tmdb_id" = i64, Path, description = "TMDB movie id")),
    responses(
        (status = 200, body = DiscoverResponse),
        (status = 400, description = "TMDB API key not configured"),
        (status = 502, description = "TMDB upstream error"),
    ))]
pub async fn similar_movies(
    State(state): State<AppState>,
    Path(tmdb_id): Path<i64>,
) -> AppResult<Json<DiscoverResponse>> {
    let client = tmdb_client(&state).await?;
    Ok(Json(
        client
            .recommendations(MediaType::Movie, tmdb_id)
            .await?
            .into(),
    ))
}

/// Shows similar to one show (TMDB recommendations), for the "More Like
/// This" row on the detail screen.
#[utoipa::path(get, path = "/tv/{tmdb_id}/similar", tag = "metadata",
    params(("tmdb_id" = i64, Path, description = "TMDB show id")),
    responses(
        (status = 200, body = DiscoverResponse),
        (status = 400, description = "TMDB API key not configured"),
        (status = 502, description = "TMDB upstream error"),
    ))]
pub async fn similar_tv(
    State(state): State<AppState>,
    Path(tmdb_id): Path<i64>,
) -> AppResult<Json<DiscoverResponse>> {
    let client = tmdb_client(&state).await?;
    Ok(Json(
        client.recommendations(MediaType::Tv, tmdb_id).await?.into(),
    ))
}

/// A movie collection ("saga"): its member movies in release order, for the
/// Collection button on movies that belong to one.
#[utoipa::path(get, path = "/collections/{id}", tag = "metadata",
    params(("id" = i64, Path, description = "TMDB collection id")),
    responses(
        (status = 200, body = Collection),
        (status = 400, description = "TMDB API key not configured"),
        (status = 404, description = "Unknown collection"),
        (status = 502, description = "TMDB upstream error"),
    ))]
pub async fn collection_details(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> AppResult<Json<Collection>> {
    let client = tmdb_client(&state).await?;
    Ok(Json(client.collection(id).await?))
}

/// A person with their movie/TV appearances (most popular first), for the
/// cast browsing screen.
#[utoipa::path(get, path = "/person/{id}", tag = "metadata",
    params(("id" = i64, Path, description = "TMDB person id")),
    responses(
        (status = 200, body = Person),
        (status = 400, description = "TMDB API key not configured"),
        (status = 404, description = "Unknown person"),
        (status = 502, description = "TMDB upstream error"),
    ))]
pub async fn person_details(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> AppResult<Json<Person>> {
    let client = tmdb_client(&state).await?;
    Ok(Json(client.person(id).await?))
}

#[derive(Debug, Deserialize, IntoParams)]
pub struct DiscoverParams {
    /// Which catalog to browse: `movie` or `tv`.
    pub media_type: MediaType,
    /// Optional TMDB genre id to filter by (see `GET /genres`). Omit to
    /// discover without a genre filter.
    pub genre_id: Option<i64>,
    /// 1-based page (defaults to 1).
    pub page: Option<u32>,
    /// TMDB sort order (defaults to `popularity.desc`); passed through as-is.
    pub sort_by: Option<String>,
}

/// Discover movies or TV shows, optionally filtered by genre and sorted.
/// Returns the same paged envelope as `/trending`.
#[utoipa::path(get, path = "/discover", tag = "metadata",
    params(DiscoverParams),
    responses(
        (status = 200, body = DiscoverResponse),
        (status = 400, description = "Missing/invalid media_type, bad page or TMDB API key not configured"),
        (status = 502, description = "TMDB upstream error"),
    ))]
pub async fn discover(
    State(state): State<AppState>,
    Query(params): Query<DiscoverParams>,
) -> AppResult<Json<DiscoverResponse>> {
    let page = validated_page(params.page)?;
    let client = tmdb_client(&state).await?;
    let paged = client
        .discover(
            params.media_type,
            params.genre_id,
            page,
            params.sort_by.as_deref(),
        )
        .await?;
    Ok(Json(paged.into()))
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
        .routes(routes!(genres))
        .routes(routes!(discover))
        .routes(routes!(trending))
        .routes(routes!(popular_movies))
        .routes(routes!(top_rated_movies))
        .routes(routes!(popular_tv))
        .routes(routes!(top_rated_tv))
        .routes(routes!(movie_details))
        .routes(routes!(tv_details))
        .routes(routes!(season_details))
        .routes(routes!(episode_details))
        .routes(routes!(collection_details))
        .routes(routes!(person_details))
        .routes(routes!(similar_movies))
        .routes(routes!(similar_tv))
}
