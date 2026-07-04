pub mod api;
pub mod config;
pub mod db;
pub mod download;
pub mod error;
pub mod indexer;
pub mod nntp;
pub mod nzb;
pub mod rar;
pub mod release;
pub mod state;
pub mod stream;
pub mod tmdb;
pub mod vfs;

use std::net::SocketAddr;

use anyhow::Context;
use config::AppConfig;
use state::AppState;

pub async fn run(config: AppConfig) -> anyhow::Result<()> {
    let addr = format!("{}:{}", config.server.host, config.server.port);
    // Bind before building the state: ffmpeg/ffprobe reach the virtual files
    // through a loopback URL that needs the actually-bound port (which may
    // be ephemeral, e.g. port 0 in tests).
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("failed to bind {addr}"))?;
    let local_addr = listener.local_addr().context("reading bound address")?;

    let state = AppState::new(config)
        .await?
        .with_loopback_base(&format!("http://127.0.0.1:{}", local_addr.port()));
    let app = api::router(state);

    tracing::info!("listening on http://{local_addr} (docs at /docs)");
    // ConnectInfo is required by the loopback guard on /internal/vfs.
    axum::serve(
        listener,
        app.into_make_service_with_connect_info::<SocketAddr>(),
    )
    .await?;
    Ok(())
}
