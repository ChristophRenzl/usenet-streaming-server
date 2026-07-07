//! TMDB (The Movie Database) metadata client and DTOs.

pub mod client;
pub mod models;

pub use client::{DetailsCache, TmdbClient};

/// Production TMDB API base URL. Injectable in [`TmdbClient::new`] for tests.
pub const DEFAULT_BASE_URL: &str = "https://api.themoviedb.org/3";
