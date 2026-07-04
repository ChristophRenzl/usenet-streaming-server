use std::{sync::Arc, time::Duration};

use anyhow::Context;

use crate::{config::AppConfig, db, tmdb};

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<AppConfig>,
    pub db: sqlx::SqlitePool,
    /// Shared HTTP client for all outbound requests (TMDB, indexers).
    pub http: reqwest::Client,
    /// TMDB API base URL; overridable so tests can point at a mock server.
    pub tmdb_base_url: Arc<str>,
}

impl AppState {
    pub async fn new(config: AppConfig) -> anyhow::Result<Self> {
        let db = db::connect(&config.database.path).await?;
        Ok(Self {
            config: Arc::new(config),
            db,
            http: build_http_client()?,
            tmdb_base_url: tmdb::DEFAULT_BASE_URL.into(),
        })
    }

    /// State backed by an in-memory database, for tests.
    pub async fn for_tests(config: AppConfig) -> anyhow::Result<Self> {
        let db = db::connect(":memory:").await?;
        Ok(Self {
            config: Arc::new(config),
            db,
            http: build_http_client()?,
            tmdb_base_url: tmdb::DEFAULT_BASE_URL.into(),
        })
    }

    /// Point the TMDB client at a different base URL (tests).
    pub fn with_tmdb_base_url(mut self, base_url: &str) -> Self {
        self.tmdb_base_url = base_url.into();
        self
    }
}

fn build_http_client() -> anyhow::Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .context("building HTTP client")
}
