use std::sync::Arc;

use crate::{config::AppConfig, db};

#[derive(Clone)]
pub struct AppState {
    pub config: Arc<AppConfig>,
    pub db: sqlx::SqlitePool,
}

impl AppState {
    pub async fn new(config: AppConfig) -> anyhow::Result<Self> {
        let db = db::connect(&config.database.path).await?;
        Ok(Self {
            config: Arc::new(config),
            db,
        })
    }

    /// State backed by an in-memory database, for tests.
    pub async fn for_tests(config: AppConfig) -> anyhow::Result<Self> {
        let db = db::connect(":memory:").await?;
        Ok(Self {
            config: Arc::new(config),
            db,
        })
    }
}
