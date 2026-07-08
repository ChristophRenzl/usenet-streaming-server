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
    /// Number of files in the post (`newznab:attr files`), when the indexer
    /// reports it. A packaging signal: an unpacked post is the media file
    /// plus a par2 set (≲20 files) while a RAR set is dozens to hundreds —
    /// unpacked releases start streaming seconds faster (no volume walk).
    pub file_count: Option<u32>,
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

/// Search all given indexers concurrently for a single query, merging results.
/// Individual indexer failures are logged and skipped so one bad apple cannot
/// break the whole search.
pub async fn search_all(
    http: &reqwest::Client,
    indexers: Vec<Indexer>,
    query: &SearchQuery,
) -> Vec<RawRelease> {
    search_many(http, indexers, std::slice::from_ref(query)).await
}

/// Run several search strategies (e.g. tvsearch by id, an `SxxExx` text
/// fallback and, for anime, an absolute-episode-number query) across all
/// indexers concurrently and merge the results into one candidate list.
///
/// Every `(indexer, query)` pair is fanned out at once. Results are
/// deduplicated by `raw.guid` first (the same release surfaced by two
/// strategies), then by exact title (the same file with a different guid
/// across strategies). Order is otherwise preserved so ranking sees the union.
pub async fn search_many(
    http: &reqwest::Client,
    indexers: Vec<Indexer>,
    queries: &[SearchQuery],
) -> Vec<RawRelease> {
    let searches = indexers
        .into_iter()
        .flat_map(|indexer| {
            queries.iter().cloned().map(move |query| {
                let http = http.clone();
                let indexer = indexer.clone();
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
            })
        })
        .collect::<Vec<_>>();

    let merged = join_all(searches).await.into_iter().flatten();
    dedupe(merged)
}

/// Drop releases that duplicate an earlier one by guid or by exact title. The
/// first occurrence (search-strategy order) wins.
fn dedupe(releases: impl IntoIterator<Item = RawRelease>) -> Vec<RawRelease> {
    let mut seen_guids = std::collections::HashSet::new();
    let mut seen_titles = std::collections::HashSet::new();
    releases
        .into_iter()
        .filter(|r| seen_guids.insert(r.guid.clone()) && seen_titles.insert(r.title.clone()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(title: &str, guid: &str) -> RawRelease {
        RawRelease {
            title: title.into(),
            guid: guid.into(),
            nzb_url: format!("https://x/{guid}.nzb"),
            size_bytes: None,
            posted_at: None,
            indexer_id: 1,
            indexer_name: "test".into(),
            tvdb_id: None,
            imdb_id: None,
            file_count: None,
        }
    }

    #[test]
    fn dedupe_drops_duplicate_guids_and_titles() {
        let input = vec![
            release("One Piece - 1100 [1080p]", "guid-a"),
            // Same guid surfaced by a second strategy → dropped.
            release("One Piece S23E01", "guid-a"),
            // Same title, different guid (another strategy) → dropped.
            release("One Piece - 1100 [1080p]", "guid-b"),
            // Genuinely distinct → kept.
            release("One Piece - 1100 [720p]", "guid-c"),
        ];
        let out = dedupe(input);
        let titles: Vec<&str> = out.iter().map(|r| r.title.as_str()).collect();
        assert_eq!(
            titles,
            ["One Piece - 1100 [1080p]", "One Piece - 1100 [720p]"]
        );
    }
}
