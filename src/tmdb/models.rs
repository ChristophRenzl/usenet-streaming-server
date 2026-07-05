//! Raw TMDB response shapes and the clean DTOs we expose through our API.

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

const IMAGE_BASE: &str = "https://image.tmdb.org/t/p";

fn image_url(path: Option<&str>, size: &str) -> Option<String> {
    path.map(|p| format!("{IMAGE_BASE}/{size}{p}"))
}

fn year_of(date: Option<&str>) -> Option<i32> {
    date.and_then(|d| d.get(0..4)).and_then(|y| y.parse().ok())
}

/// What kind of media a search hit refers to.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, ToSchema)]
#[serde(rename_all = "lowercase")]
pub enum MediaType {
    Movie,
    Tv,
}

impl MediaType {
    /// Stable lowercase name as stored in the database.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Movie => "movie",
            Self::Tv => "tv",
        }
    }
}

// ---- Clean DTOs ------------------------------------------------------------

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct SearchResult {
    pub tmdb_id: i64,
    pub media_type: MediaType,
    pub title: String,
    pub year: Option<i32>,
    pub overview: Option<String>,
    pub poster_url: Option<String>,
    pub backdrop_url: Option<String>,
    pub vote_average: Option<f64>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct Movie {
    pub tmdb_id: i64,
    pub media_type: MediaType,
    pub imdb_id: Option<String>,
    pub title: String,
    pub year: Option<i32>,
    /// ISO 639-1 code of the original language (e.g. "ja" for anime).
    pub original_language: Option<String>,
    pub overview: Option<String>,
    pub runtime_minutes: Option<i64>,
    pub poster_url: Option<String>,
    pub backdrop_url: Option<String>,
    pub vote_average: Option<f64>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct TvShow {
    pub tmdb_id: i64,
    pub media_type: MediaType,
    pub imdb_id: Option<String>,
    pub tvdb_id: Option<i64>,
    pub title: String,
    pub year: Option<i32>,
    /// ISO 639-1 code of the original language (e.g. "ja" for anime).
    pub original_language: Option<String>,
    pub overview: Option<String>,
    pub poster_url: Option<String>,
    pub backdrop_url: Option<String>,
    pub vote_average: Option<f64>,
    pub seasons: Vec<SeasonSummary>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct SeasonSummary {
    pub season_number: u32,
    pub title: Option<String>,
    pub episode_count: Option<i64>,
    pub air_date: Option<String>,
    pub poster_url: Option<String>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct Season {
    pub season_number: u32,
    pub title: Option<String>,
    pub overview: Option<String>,
    pub air_date: Option<String>,
    pub poster_url: Option<String>,
    pub episodes: Vec<Episode>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct Episode {
    pub season_number: u32,
    pub episode_number: u32,
    pub title: Option<String>,
    pub overview: Option<String>,
    pub air_date: Option<String>,
    pub still_url: Option<String>,
    pub vote_average: Option<f64>,
}

// ---- Raw TMDB shapes -------------------------------------------------------

#[derive(Debug, Deserialize)]
pub(crate) struct RawSearchResponse {
    #[serde(default = "default_page")]
    pub page: i64,
    #[serde(default = "default_page")]
    pub total_pages: i64,
    #[serde(default)]
    pub results: Vec<RawSearchItem>,
}

fn default_page() -> i64 {
    1
}

#[derive(Debug, Deserialize)]
pub(crate) struct RawSearchItem {
    pub id: i64,
    pub media_type: Option<String>,
    pub title: Option<String>,
    pub name: Option<String>,
    pub release_date: Option<String>,
    pub first_air_date: Option<String>,
    pub overview: Option<String>,
    pub poster_path: Option<String>,
    pub backdrop_path: Option<String>,
    pub vote_average: Option<f64>,
}

impl RawSearchItem {
    /// `forced` overrides the media type for /search/movie & /search/tv,
    /// which omit `media_type` in their responses.
    pub(crate) fn into_result(self, forced: Option<MediaType>) -> Option<SearchResult> {
        let media_type = forced.or(match self.media_type.as_deref() {
            Some("movie") => Some(MediaType::Movie),
            Some("tv") => Some(MediaType::Tv),
            _ => None, // people etc. are dropped from multi search
        })?;
        let title = match media_type {
            MediaType::Movie => self.title,
            MediaType::Tv => self.name,
        }?;
        let date = match media_type {
            MediaType::Movie => self.release_date,
            MediaType::Tv => self.first_air_date,
        };
        Some(SearchResult {
            tmdb_id: self.id,
            media_type,
            title,
            year: year_of(date.as_deref()),
            overview: self.overview,
            poster_url: image_url(self.poster_path.as_deref(), "w500"),
            backdrop_url: image_url(self.backdrop_path.as_deref(), "w780"),
            vote_average: self.vote_average,
        })
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct RawExternalIds {
    pub imdb_id: Option<String>,
    pub tvdb_id: Option<i64>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RawMovieDetails {
    pub id: i64,
    pub title: String,
    pub original_language: Option<String>,
    pub release_date: Option<String>,
    pub overview: Option<String>,
    pub runtime: Option<i64>,
    pub poster_path: Option<String>,
    pub backdrop_path: Option<String>,
    pub vote_average: Option<f64>,
    pub imdb_id: Option<String>,
    pub external_ids: Option<RawExternalIds>,
}

impl From<RawMovieDetails> for Movie {
    fn from(raw: RawMovieDetails) -> Self {
        let imdb_id = raw
            .external_ids
            .and_then(|e| e.imdb_id)
            .or(raw.imdb_id)
            .filter(|s| !s.is_empty());
        Movie {
            tmdb_id: raw.id,
            media_type: MediaType::Movie,
            imdb_id,
            title: raw.title,
            year: year_of(raw.release_date.as_deref()),
            original_language: raw.original_language.filter(|s| !s.is_empty()),
            overview: raw.overview,
            runtime_minutes: raw.runtime,
            poster_url: image_url(raw.poster_path.as_deref(), "w500"),
            backdrop_url: image_url(raw.backdrop_path.as_deref(), "w780"),
            vote_average: raw.vote_average,
        }
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct RawTvDetails {
    pub id: i64,
    pub name: String,
    pub original_language: Option<String>,
    pub first_air_date: Option<String>,
    pub overview: Option<String>,
    pub poster_path: Option<String>,
    pub backdrop_path: Option<String>,
    pub vote_average: Option<f64>,
    pub external_ids: Option<RawExternalIds>,
    #[serde(default)]
    pub seasons: Vec<RawSeasonSummary>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RawSeasonSummary {
    pub season_number: u32,
    pub name: Option<String>,
    pub episode_count: Option<i64>,
    pub air_date: Option<String>,
    pub poster_path: Option<String>,
}

impl From<RawTvDetails> for TvShow {
    fn from(raw: RawTvDetails) -> Self {
        let (imdb_id, tvdb_id) = raw
            .external_ids
            .map(|e| (e.imdb_id.filter(|s| !s.is_empty()), e.tvdb_id))
            .unwrap_or((None, None));
        TvShow {
            tmdb_id: raw.id,
            media_type: MediaType::Tv,
            imdb_id,
            tvdb_id,
            title: raw.name,
            year: year_of(raw.first_air_date.as_deref()),
            original_language: raw.original_language.filter(|s| !s.is_empty()),
            overview: raw.overview,
            poster_url: image_url(raw.poster_path.as_deref(), "w500"),
            backdrop_url: image_url(raw.backdrop_path.as_deref(), "w780"),
            vote_average: raw.vote_average,
            seasons: raw
                .seasons
                .into_iter()
                .map(|s| SeasonSummary {
                    season_number: s.season_number,
                    title: s.name,
                    episode_count: s.episode_count,
                    air_date: s.air_date,
                    poster_url: image_url(s.poster_path.as_deref(), "w500"),
                })
                .collect(),
        }
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct RawSeasonDetails {
    pub season_number: u32,
    pub name: Option<String>,
    pub overview: Option<String>,
    pub air_date: Option<String>,
    pub poster_path: Option<String>,
    #[serde(default)]
    pub episodes: Vec<RawEpisode>,
}

impl From<RawSeasonDetails> for Season {
    fn from(raw: RawSeasonDetails) -> Self {
        Season {
            season_number: raw.season_number,
            title: raw.name,
            overview: raw.overview,
            air_date: raw.air_date,
            poster_url: image_url(raw.poster_path.as_deref(), "w500"),
            episodes: raw.episodes.into_iter().map(Episode::from).collect(),
        }
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct RawEpisode {
    pub season_number: u32,
    pub episode_number: u32,
    pub name: Option<String>,
    pub overview: Option<String>,
    pub air_date: Option<String>,
    pub still_path: Option<String>,
    pub vote_average: Option<f64>,
}

impl From<RawEpisode> for Episode {
    fn from(raw: RawEpisode) -> Self {
        Episode {
            season_number: raw.season_number,
            episode_number: raw.episode_number,
            title: raw.name,
            overview: raw.overview,
            air_date: raw.air_date,
            still_url: image_url(raw.still_path.as_deref(), "w300"),
            vote_average: raw.vote_average,
        }
    }
}
