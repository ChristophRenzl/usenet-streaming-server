use std::{sync::Arc, time::Duration};

use anyhow::Context;

use crate::{
    config::AppConfig, db, nntp::NntpPool, stream::SessionManager, tmdb, vfs::SegmentCache,
};

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<AppConfig>,
    pub db: sqlx::SqlitePool,
    /// Shared HTTP client for all outbound requests (TMDB, indexers).
    pub http: reqwest::Client,
    /// TMDB API base URL; overridable so tests can point at a mock server.
    pub tmdb_base_url: Arc<str>,
    /// Multi-provider NNTP pool, built from the enabled providers in the DB
    /// and reloaded live when providers change through the API.
    pub nntp_pool: NntpPool,
    /// Shared decoded-segment cache for all virtual files.
    pub segment_cache: SegmentCache,
    /// Live playback sessions.
    pub sessions: SessionManager,
    /// Base URL under which this server reaches itself over loopback
    /// (`http://127.0.0.1:{bound port}`); ffmpeg/ffprobe read the virtual
    /// files through it. Set in `run()` after the listener is bound.
    pub loopback_base: Arc<str>,
}

impl AppState {
    pub async fn new(config: AppConfig) -> anyhow::Result<Self> {
        let db = db::connect(&config.database.path).await?;
        Self::build(config, db).await
    }

    /// State backed by an in-memory database, for tests.
    pub async fn for_tests(config: AppConfig) -> anyhow::Result<Self> {
        let db = db::connect(":memory:").await?;
        Self::build(config, db).await
    }

    async fn build(config: AppConfig, db: sqlx::SqlitePool) -> anyhow::Result<Self> {
        let providers = db::providers::list(&db)
            .await
            .context("loading NNTP providers")?;
        let nntp_pool = NntpPool::new(providers);
        let segment_cache = SegmentCache::new(config.cache.memory_bytes);
        let sessions = SessionManager::new(Duration::from_secs(
            config.streaming.session_idle_timeout_secs,
        ));
        Ok(Self {
            config: Arc::new(config),
            db,
            http: build_http_client()?,
            tmdb_base_url: tmdb::DEFAULT_BASE_URL.into(),
            nntp_pool,
            segment_cache,
            sessions,
            // Placeholder until the listener is bound; `run()` and the test
            // harness overwrite it with the real port.
            loopback_base: "http://127.0.0.1:0".into(),
        })
    }

    /// Point the TMDB client at a different base URL (tests).
    pub fn with_tmdb_base_url(mut self, base_url: &str) -> Self {
        self.tmdb_base_url = base_url.into();
        self
    }

    /// Set the loopback base URL once the listen port is known.
    pub fn with_loopback_base(mut self, base_url: &str) -> Self {
        self.loopback_base = base_url.trim_end_matches('/').into();
        self
    }
}

fn build_http_client() -> anyhow::Result<reqwest::Client> {
    reqwest::Client::builder()
        .timeout(Duration::from_secs(30))
        .build()
        .context("building HTTP client")
}
