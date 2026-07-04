pub mod api;
pub mod config;
pub mod db;
pub mod error;
pub mod indexer;
pub mod release;
pub mod state;
pub mod tmdb;

use anyhow::Context;
use config::AppConfig;
use state::AppState;

pub async fn run(config: AppConfig) -> anyhow::Result<()> {
    let addr = format!("{}:{}", config.server.host, config.server.port);
    let state = AppState::new(config).await?;
    let app = api::router(state);

    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("failed to bind {addr}"))?;
    tracing::info!("listening on http://{addr} (docs at /docs)");
    axum::serve(listener, app).await?;
    Ok(())
}
