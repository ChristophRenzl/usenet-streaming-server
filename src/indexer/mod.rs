//! Newznab indexer integration: per-indexer client plus concurrent fan-out.

pub mod client;

use chrono::{DateTime, Utc};
use futures::future::join_all;
use serde::Serialize;
use utoipa::ToSchema;

use crate::db::indexers::Indexer;
use client::NewznabClient;

/// A release as returned by an indexer, before parsing/ranking.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RawRelease {
    pub title: String,
    pub guid: String,
    pub nzb_url: String,
    pub size_bytes: Option<i64>,
    pub posted_at: Option<DateTime<Utc>>,
    pub indexer_id: i64,
    pub indexer_name: String,
    /// TVDB id the indexer reports for this release (`newznab:attr tvdbid`),
    /// used to reject releases that belong to a different show.
    pub tvdb_id: Option<i64>,
    /// IMDb id the indexer reports (`newznab:attr imdbid`), digits only
    /// without the `tt` prefix or leading zeros.
    pub imdb_id: Option<String>,
}

/// What to ask the indexers for.
#[derive(Debug, Clone)]
pub enum SearchQuery {
    /// `t=movie&imdbid=` — IMDb id digits without the `tt` prefix.
    MovieByImdb { imdb_id: String },
    /// `t=tvsearch&tvdbid=&season=&ep=`.
    TvByTvdb {
        tvdb_id: i64,
        season: u32,
        episode: u32,
    },
    /// `t=search&q=` free-text fallback.
    Raw { query: String },
}

/// Search all given indexers concurrently, merging results. Individual
/// indexer failures are logged and skipped so one bad apple cannot break
/// the whole search.
pub async fn search_all(
    http: &reqwest::Client,
    indexers: Vec<Indexer>,
    query: &SearchQuery,
) -> Vec<RawRelease> {
    let searches = indexers.into_iter().map(|indexer| {
        let http = http.clone();
        let query = query.clone();
        async move {
            let name = indexer.name.clone();
            let client = NewznabClient::new(http, indexer);
            match client.search(&query).await {
                Ok(releases) => releases,
                Err(error) => {
                    tracing::warn!(indexer = %name, %error, "indexer search failed, skipping");
                    Vec::new()
                }
            }
        }
    });
    join_all(searches).await.into_iter().flatten().collect()
}
