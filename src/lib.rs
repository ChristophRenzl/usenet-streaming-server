pub mod api;
pub mod config;
pub mod db;
pub mod discovery;
pub mod download;
pub mod error;
pub mod indexer;
pub mod nntp;
pub mod nzb;
pub mod rar;
pub mod release;
pub mod state;
pub mod stream;
pub mod subtitles;
pub mod tmdb;
pub mod trakt;
pub mod vfs;

use std::net::SocketAddr;

use anyhow::Context;
use config::AppConfig;
use state::AppState;

pub async fn run(config: AppConfig) -> anyhow::Result<()> {
    let addr = format!("{}:{}", config.server.host, config.server.port);
    // Public listener on the configured host/port.
    let public = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("failed to bind {addr}"))?;
    let public_addr = public.local_addr().context("reading bound address")?;
    // Advertise on the LAN so clients can auto-discover this server.
    discovery::spawn(public_addr.port());

    // Dedicated internal listener on 127.0.0.1 (ephemeral port). ffmpeg/ffprobe
    // read the virtual files through this loopback URL. Binding it separately
    // means the internal route is reachable regardless of the public `host` —
    // a specific host such as `192.168.1.10` would otherwise leave 127.0.0.1
    // unbound and the probe would get "connection refused".
    let loopback = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .context("failed to bind internal loopback listener")?;
    let loopback_addr = loopback.local_addr().context("reading loopback address")?;

    let state = AppState::new(config)
        .await?
        .with_loopback_base(&format!("http://127.0.0.1:{}", loopback_addr.port()));

    // Periodic Trakt watched-history import (Jellyfin-plugin behavior):
    // shortly after boot, then every 6 hours. No-op unless Trakt is linked;
    // failures only log.
    {
        let state = state.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_secs(120)).await;
            loop {
                if let Err(error) = api::trakt::sync_watched_from_trakt(&state).await {
                    tracing::debug!(%error, "periodic trakt sync failed");
                }
                tokio::time::sleep(std::time::Duration::from_secs(6 * 60 * 60)).await;
            }
        });
    }

    let app = api::router(state);
    // ConnectInfo is required by the loopback guard on /internal/vfs.
    let make = app.into_make_service_with_connect_info::<SocketAddr>();
    let make_loopback = make.clone();

    tracing::info!(
        "listening on http://{public_addr} (docs at /docs); internal loopback on {loopback_addr}"
    );
    tokio::try_join!(
        async move { axum::serve(public, make).await.map_err(anyhow::Error::from) },
        async move {
            axum::serve(loopback, make_loopback)
                .await
                .map_err(anyhow::Error::from)
        },
    )?;
    Ok(())
}
