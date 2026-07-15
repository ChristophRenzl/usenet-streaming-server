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
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize, ToSchema)]
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

/// A TMDB genre (e.g. `{ "id": 28, "name": "Action" }`).
#[derive(Debug, Clone, Serialize, Deserialize, ToSchema)]
pub struct Genre {
    pub id: i64,
    pub name: String,
    /// Backdrop of the genre's current top discover hit, for genre browse
    /// tiles. Absent in TMDB's own genre list; filled in by the `/genres`
    /// handler (best-effort, `null` when discovery fails).
    #[serde(default)]
    pub backdrop_url: Option<String>,
}

/// The list of genres for a media type, as returned by `GET /genres`.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct GenreList {
    pub genres: Vec<Genre>,
}

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
    pub overview: Option<String>,
    pub runtime_minutes: Option<i64>,
    pub poster_url: Option<String>,
    pub backdrop_url: Option<String>,
    pub vote_average: Option<f64>,
    /// ISO 639-1 code of the title's original language (e.g. "ja").
    pub original_language: Option<String>,
    /// YouTube video key for the best available trailer (build a URL as
    /// `https://youtube.com/watch?v={key}` or `youtube://{key}`), or `null`
    /// when TMDB has no YouTube trailer/teaser.
    pub trailer_youtube_key: Option<String>,
    /// The collection ("saga") this movie belongs to, when TMDB groups it into
    /// one (e.g. "The Lord of the Rings Collection"); fetch its members via
    /// `GET /collections/{id}`.
    pub collection: Option<CollectionRef>,
    /// Top-billed cast (up to 20, TMDB billing order).
    pub cast: Vec<CastMember>,
}

/// Reference to the collection a movie belongs to.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct CollectionRef {
    pub id: i64,
    pub name: String,
}

/// A movie collection (saga) with its member movies in release order.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct Collection {
    pub id: i64,
    pub name: String,
    pub overview: Option<String>,
    pub backdrop_url: Option<String>,
    pub parts: Vec<SearchResult>,
}

/// One cast member of a movie/show, from TMDB credits.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct CastMember {
    pub tmdb_id: i64,
    pub name: String,
    pub character: Option<String>,
    pub profile_url: Option<String>,
}

/// A person with their movie/TV appearances (combined credits, most popular
/// first), for the "other appearances" screen behind a cast photo.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct Person {
    pub tmdb_id: i64,
    pub name: String,
    pub profile_url: Option<String>,
    pub biography: Option<String>,
    pub known_for_department: Option<String>,
    pub appearances: Vec<SearchResult>,
}

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct TvShow {
    pub tmdb_id: i64,
    pub media_type: MediaType,
    pub imdb_id: Option<String>,
    pub tvdb_id: Option<i64>,
    pub title: String,
    pub year: Option<i32>,
    pub overview: Option<String>,
    pub poster_url: Option<String>,
    pub backdrop_url: Option<String>,
    pub vote_average: Option<f64>,
    /// ISO 639-1 code of the show's original language (e.g. "ja").
    pub original_language: Option<String>,
    /// YouTube video key for the best available trailer (build a URL as
    /// `https://youtube.com/watch?v={key}` or `youtube://{key}`), or `null`
    /// when TMDB has no YouTube trailer/teaser.
    pub trailer_youtube_key: Option<String>,
    pub seasons: Vec<SeasonSummary>,
    /// Top-billed cast (up to 20, TMDB billing order).
    pub cast: Vec<CastMember>,
    /// Typical episode length in minutes (TMDB `episode_run_time`, first
    /// entry), used to estimate a release's bitrate for bandwidth gating.
    pub episode_runtime_minutes: Option<i64>,
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

/// TMDB `/genre/{movie,tv}/list` response: `{ "genres": [...] }`.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct RawGenreList {
    #[serde(default)]
    pub genres: Vec<Genre>,
}

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
    /// TMDB popularity, used to order a person's combined credits.
    #[serde(default)]
    pub popularity: Option<f64>,
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

/// The `videos` sub-object appended via `append_to_response=videos`.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct RawVideos {
    #[serde(default)]
    pub results: Vec<RawVideo>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RawVideo {
    #[serde(default)]
    pub site: Option<String>,
    #[serde(rename = "type", default)]
    pub video_type: Option<String>,
    #[serde(default)]
    pub key: Option<String>,
    #[serde(default)]
    pub official: bool,
}

/// Pick the best YouTube trailer key from a `videos.results[]` list.
///
/// Preference order: an official YouTube "Trailer", then any YouTube
/// "Trailer", then any YouTube "Teaser". Returns `None` when no suitable
/// YouTube video exists.
pub(crate) fn best_trailer_youtube_key(videos: &RawVideos) -> Option<String> {
    let is_youtube = |v: &&RawVideo| v.site.as_deref() == Some("YouTube") && v.key.is_some();
    let is_type = |v: &&RawVideo, ty: &str| v.video_type.as_deref() == Some(ty);

    videos
        .results
        .iter()
        // Official YouTube trailer.
        .find(|v| is_youtube(v) && is_type(v, "Trailer") && v.official)
        // Any YouTube trailer.
        .or_else(|| {
            videos
                .results
                .iter()
                .find(|v| is_youtube(v) && is_type(v, "Trailer"))
        })
        // Any YouTube teaser.
        .or_else(|| {
            videos
                .results
                .iter()
                .find(|v| is_youtube(v) && is_type(v, "Teaser"))
        })
        .and_then(|v| v.key.clone())
}

/// The `credits` sub-object appended via `append_to_response=credits`.
#[derive(Debug, Default, Deserialize)]
pub(crate) struct RawCredits {
    #[serde(default)]
    pub cast: Vec<RawCastMember>,
}

#[derive(Debug, Deserialize)]
pub(crate) struct RawCastMember {
    pub id: i64,
    pub name: String,
    #[serde(default)]
    pub character: Option<String>,
    #[serde(default)]
    pub profile_path: Option<String>,
    /// TMDB billing order (0 = top billing).
    #[serde(default)]
    pub order: Option<i64>,
}

/// Top-billed cast from an appended credits block: billing order, capped so a
/// detail response doesn't ship hundreds of one-line extras.
pub(crate) fn top_cast(credits: Option<RawCredits>) -> Vec<CastMember> {
    const MAX_CAST: usize = 20;
    let mut cast = credits.map(|c| c.cast).unwrap_or_default();
    cast.sort_by_key(|member| member.order.unwrap_or(i64::MAX));
    cast.truncate(MAX_CAST);
    cast.into_iter()
        .map(|member| CastMember {
            tmdb_id: member.id,
            name: member.name,
            character: member.character.filter(|c| !c.is_empty()),
            profile_url: image_url(member.profile_path.as_deref(), "w185"),
        })
        .collect()
}

/// `belongs_to_collection` on movie details.
#[derive(Debug, Deserialize)]
pub(crate) struct RawCollectionRef {
    pub id: i64,
    pub name: String,
}

/// TMDB `/collection/{id}`.
#[derive(Debug, Deserialize)]
pub(crate) struct RawCollection {
    pub id: i64,
    pub name: String,
    #[serde(default)]
    pub overview: Option<String>,
    #[serde(default)]
    pub backdrop_path: Option<String>,
    #[serde(default)]
    pub parts: Vec<RawSearchItem>,
}

impl From<RawCollection> for Collection {
    fn from(raw: RawCollection) -> Self {
        let mut parts: Vec<SearchResult> = raw
            .parts
            .into_iter()
            // Collection parts are movies; TMDB omits media_type here.
            .filter_map(|item| item.into_result(Some(MediaType::Movie)))
            .collect();
        // Release order reads naturally for a saga; unreleased (year-less)
        // entries sink to the end.
        parts.sort_by_key(|part| part.year.unwrap_or(i32::MAX));
        Collection {
            id: raw.id,
            name: raw.name,
            overview: raw.overview.filter(|o| !o.is_empty()),
            backdrop_url: image_url(raw.backdrop_path.as_deref(), "w1280"),
            parts,
        }
    }
}

/// TMDB `/person/{id}` with `append_to_response=combined_credits`.
#[derive(Debug, Deserialize)]
pub(crate) struct RawPerson {
    pub id: i64,
    pub name: String,
    #[serde(default)]
    pub biography: Option<String>,
    #[serde(default)]
    pub profile_path: Option<String>,
    #[serde(default)]
    pub known_for_department: Option<String>,
    #[serde(default)]
    pub combined_credits: Option<RawCombinedCredits>,
}

#[derive(Debug, Default, Deserialize)]
pub(crate) struct RawCombinedCredits {
    #[serde(default)]
    pub cast: Vec<RawSearchItem>,
}

impl From<RawPerson> for Person {
    fn from(raw: RawPerson) -> Self {
        const MAX_APPEARANCES: usize = 40;
        let mut credits = raw.combined_credits.unwrap_or_default().cast;
        // Most popular first, so the screen leads with the works people
        // actually know the person from.
        credits.sort_by(|a, b| {
            b.popularity
                .unwrap_or(0.0)
                .total_cmp(&a.popularity.unwrap_or(0.0))
        });
        let mut seen = std::collections::HashSet::new();
        let appearances: Vec<SearchResult> = credits
            .into_iter()
            // Combined credits carry media_type; people/others are dropped.
            .filter_map(|item| item.into_result(None))
            // A person can have several credit rows on one title (multiple
            // roles); the grid wants each title once.
            .filter(|result| seen.insert((result.media_type, result.tmdb_id)))
            .take(MAX_APPEARANCES)
            .collect();
        Person {
            tmdb_id: raw.id,
            name: raw.name,
            profile_url: image_url(raw.profile_path.as_deref(), "w185"),
            biography: raw.biography.filter(|b| !b.is_empty()),
            known_for_department: raw.known_for_department,
            appearances,
        }
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct RawMovieDetails {
    pub id: i64,
    pub title: String,
    pub release_date: Option<String>,
    pub overview: Option<String>,
    pub runtime: Option<i64>,
    pub poster_path: Option<String>,
    pub backdrop_path: Option<String>,
    pub vote_average: Option<f64>,
    pub imdb_id: Option<String>,
    pub original_language: Option<String>,
    pub external_ids: Option<RawExternalIds>,
    #[serde(default)]
    pub videos: Option<RawVideos>,
    #[serde(default)]
    pub belongs_to_collection: Option<RawCollectionRef>,
    #[serde(default)]
    pub credits: Option<RawCredits>,
}

impl From<RawMovieDetails> for Movie {
    fn from(raw: RawMovieDetails) -> Self {
        let imdb_id = raw
            .external_ids
            .and_then(|e| e.imdb_id)
            .or(raw.imdb_id)
            .filter(|s| !s.is_empty());
        let trailer_youtube_key = raw.videos.as_ref().and_then(best_trailer_youtube_key);
        Movie {
            tmdb_id: raw.id,
            media_type: MediaType::Movie,
            imdb_id,
            title: raw.title,
            year: year_of(raw.release_date.as_deref()),
            overview: raw.overview,
            runtime_minutes: raw.runtime,
            poster_url: image_url(raw.poster_path.as_deref(), "w500"),
            backdrop_url: image_url(raw.backdrop_path.as_deref(), "w1280"),
            vote_average: raw.vote_average,
            original_language: raw.original_language,
            trailer_youtube_key,
            collection: raw.belongs_to_collection.map(|c| CollectionRef {
                id: c.id,
                name: c.name,
            }),
            cast: top_cast(raw.credits),
        }
    }
}

#[derive(Debug, Deserialize)]
pub(crate) struct RawTvDetails {
    pub id: i64,
    pub name: String,
    pub first_air_date: Option<String>,
    pub overview: Option<String>,
    pub poster_path: Option<String>,
    pub backdrop_path: Option<String>,
    pub vote_average: Option<f64>,
    pub original_language: Option<String>,
    pub external_ids: Option<RawExternalIds>,
    #[serde(default)]
    pub videos: Option<RawVideos>,
    #[serde(default)]
    pub seasons: Vec<RawSeasonSummary>,
    #[serde(default)]
    pub credits: Option<RawCredits>,
    #[serde(default)]
    pub episode_run_time: Vec<i64>,
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
        let trailer_youtube_key = raw.videos.as_ref().and_then(best_trailer_youtube_key);
        TvShow {
            tmdb_id: raw.id,
            media_type: MediaType::Tv,
            imdb_id,
            tvdb_id,
            title: raw.name,
            year: year_of(raw.first_air_date.as_deref()),
            overview: raw.overview,
            poster_url: image_url(raw.poster_path.as_deref(), "w500"),
            backdrop_url: image_url(raw.backdrop_path.as_deref(), "w1280"),
            vote_average: raw.vote_average,
            original_language: raw.original_language,
            trailer_youtube_key,
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
            cast: top_cast(raw.credits),
            episode_runtime_minutes: raw.episode_run_time.first().copied(),
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
