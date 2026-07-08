//! HTTP client for a single Newznab-compatible indexer.
//!
//! JSON output (`o=json`) is not reliably implemented across indexers, so we
//! always parse the default RSS XML. Never log URLs or errors verbatim that
//! could contain the API key — reqwest errors are stripped via
//! [`reqwest::Error::without_url`].

use bytes::Bytes;
use serde::Deserialize;

use crate::{
    db::indexers::Indexer,
    error::{AppError, AppResult},
};

use super::{RawRelease, SearchQuery};

pub struct NewznabClient {
    http: reqwest::Client,
    indexer: Indexer,
}

impl NewznabClient {
    pub fn new(http: reqwest::Client, indexer: Indexer) -> Self {
        Self { http, indexer }
    }

    fn api_url(&self) -> String {
        let base = self.indexer.base_url.trim_end_matches('/');
        if base.ends_with("/api") {
            base.to_string()
        } else {
            format!("{base}/api")
        }
    }

    /// Run a search and parse the RSS response into raw releases.
    pub async fn search(&self, query: &SearchQuery) -> AppResult<Vec<RawRelease>> {
        let mut params: Vec<(&str, String)> = vec![("apikey", self.indexer.api_key.clone())];
        match query {
            SearchQuery::MovieByImdb { imdb_id } => {
                params.push(("t", "movie".into()));
                params.push(("imdbid", imdb_id.trim_start_matches("tt").to_string()));
            }
            SearchQuery::TvByTvdb {
                tvdb_id,
                season,
                episode,
            } => {
                params.push(("t", "tvsearch".into()));
                params.push(("tvdbid", tvdb_id.to_string()));
                params.push(("season", season.to_string()));
                params.push(("ep", episode.to_string()));
            }
            SearchQuery::Raw { query } => {
                params.push(("t", "search".into()));
                params.push(("q", query.clone()));
            }
        }

        let response = self
            .http
            .get(self.api_url())
            .query(&params)
            .send()
            .await
            .map_err(|e| upstream(&self.indexer.name, &e.without_url()))?;

        let status = response.status();
        if !status.is_success() {
            return Err(AppError::Upstream(format!(
                "indexer '{}' returned HTTP {status}",
                self.indexer.name
            )));
        }

        let body = response
            .text()
            .await
            .map_err(|e| upstream(&self.indexer.name, &e.without_url()))?;

        parse_rss(&body, &self.indexer)
    }

    /// Cheap connectivity/credentials check: a plain `t=search&q=test`.
    pub async fn test(&self) -> AppResult<()> {
        self.search(&SearchQuery::Raw {
            query: "test".into(),
        })
        .await
        .map(|_| ())
    }

    /// Fetch the NZB file behind a release link.
    pub async fn grab(&self, nzb_url: &str) -> AppResult<Bytes> {
        let response = self
            .http
            .get(nzb_url)
            .send()
            .await
            .map_err(|e| upstream(&self.indexer.name, &e.without_url()))?;

        let status = response.status();
        if !status.is_success() {
            return Err(AppError::Upstream(format!(
                "indexer '{}' NZB download returned HTTP {status}",
                self.indexer.name
            )));
        }

        response
            .bytes()
            .await
            .map_err(|e| upstream(&self.indexer.name, &e.without_url()))
    }
}

fn upstream(indexer: &str, error: &dyn std::fmt::Display) -> AppError {
    AppError::Upstream(format!("indexer '{indexer}' request failed: {error}"))
}

// ---- RSS parsing ----------------------------------------------------------

#[derive(Debug, Deserialize)]
struct RssDoc {
    channel: RssChannel,
}

#[derive(Debug, Deserialize)]
struct RssChannel {
    #[serde(default, rename = "item")]
    items: Vec<RssItem>,
}

#[derive(Debug, Deserialize)]
struct RssItem {
    title: Option<String>,
    guid: Option<RssGuid>,
    link: Option<String>,
    #[serde(rename = "pubDate")]
    pub_date: Option<String>,
    enclosure: Option<RssEnclosure>,
    // quick-xml resolves keys to local names, so `newznab:attr` arrives as `attr`.
    #[serde(default, rename = "attr")]
    attrs: Vec<NewznabAttr>,
}

#[derive(Debug, Deserialize)]
struct RssGuid {
    #[serde(rename = "$text")]
    value: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RssEnclosure {
    #[serde(rename = "@url")]
    url: Option<String>,
    #[serde(rename = "@length")]
    length: Option<String>,
}

#[derive(Debug, Deserialize)]
struct NewznabAttr {
    #[serde(rename = "@name")]
    name: String,
    #[serde(rename = "@value")]
    value: String,
}

/// Newznab error document: `<error code="100" description="..."/>`.
#[derive(Debug, Deserialize)]
struct ErrorDoc {
    #[serde(rename = "@code")]
    code: Option<String>,
    #[serde(rename = "@description")]
    description: Option<String>,
}

fn parse_rss(body: &str, indexer: &Indexer) -> AppResult<Vec<RawRelease>> {
    let doc: RssDoc = match quick_xml::de::from_str(body) {
        Ok(doc) => doc,
        Err(rss_err) => {
            // Newznab reports auth/param problems as an <error/> document.
            if let Ok(err) = quick_xml::de::from_str::<ErrorDoc>(body) {
                if err.code.is_some() || err.description.is_some() {
                    return Err(AppError::Upstream(format!(
                        "indexer '{}' error {}: {}",
                        indexer.name,
                        err.code.as_deref().unwrap_or("?"),
                        err.description.as_deref().unwrap_or("unknown"),
                    )));
                }
            }
            return Err(AppError::Upstream(format!(
                "indexer '{}' returned unparseable XML: {rss_err}",
                indexer.name
            )));
        }
    };

    let releases = doc
        .channel
        .items
        .into_iter()
        .filter_map(|item| {
            let title = item.title?;
            let nzb_url = item
                .link
                .or_else(|| item.enclosure.as_ref().and_then(|e| e.url.clone()))?;
            let size_bytes = item
                .attrs
                .iter()
                .find(|a| a.name == "size")
                .and_then(|a| a.value.parse().ok())
                .or_else(|| {
                    item.enclosure
                        .as_ref()
                        .and_then(|e| e.length.as_ref())
                        .and_then(|l| l.parse().ok())
                });
            let posted_at = item.pub_date.as_deref().and_then(|d| {
                chrono::DateTime::parse_from_rfc2822(d)
                    .ok()
                    .map(|dt| dt.to_utc())
            });
            let guid = item
                .guid
                .and_then(|g| g.value)
                .unwrap_or_else(|| nzb_url.clone());
            let attr = |name: &str| {
                item.attrs
                    .iter()
                    .find(|a| a.name == name)
                    .map(|a| a.value.as_str())
            };
            let tvdb_id = attr("tvdbid")
                .and_then(|v| v.parse().ok())
                .filter(|id| *id > 0);
            let imdb_id = attr("imdbid")
                .map(|v| {
                    v.trim_start_matches("tt")
                        .trim_start_matches('0')
                        .to_string()
                })
                .filter(|v| !v.is_empty() && v.chars().all(|c| c.is_ascii_digit()));
            let file_count = attr("files")
                .and_then(|v| v.parse().ok())
                .filter(|count| *count > 0);
            Some(RawRelease {
                title,
                guid,
                nzb_url,
                size_bytes,
                posted_at,
                indexer_id: indexer.id,
                indexer_name: indexer.name.clone(),
                tvdb_id,
                imdb_id,
                file_count,
            })
        })
        .collect();

    Ok(releases)
}
