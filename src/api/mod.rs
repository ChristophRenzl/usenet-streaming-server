pub mod auth;
pub mod downloads;
pub mod history;
pub mod internal;
pub mod metadata;
pub mod releases;
pub mod settings;
pub mod stream;
pub mod system;
pub mod watchlist;

use axum::{middleware, Router};
use tower_http::trace::TraceLayer;
use utoipa::OpenApi;
use utoipa_axum::router::OpenApiRouter;
use utoipa_swagger_ui::SwaggerUi;

use crate::state::AppState;

#[derive(OpenApi)]
#[openapi(
    info(
        title = "Usenet Streaming Server",
        description = "Search movies/TV via TMDB and stream them on-the-fly from Usenet.",
        license(name = "MIT")
    ),
    tags(
        (name = "system", description = "Health and server info"),
        (name = "metadata", description = "TMDB search, discovery lists and details"),
        (name = "watchlist", description = "Saved movies/TV shows for later viewing"),
        (name = "releases", description = "Indexer release search and ranking"),
        (name = "streaming", description = "Playback sessions, HLS delivery and raw byte-range access"),
        (name = "downloads", description = "Server-side download jobs and disk playback"),
        (name = "history", description = "Watch history and resume positions"),
        (name = "settings", description = "Preferences, indexers, providers, app settings"),
    )
)]
struct ApiDoc;

pub fn router(state: AppState) -> Router {
    // Authenticated /api/v1 surface. Each feature module contributes an
    // OpenApiRouter merged here.
    let (api_router, api) = OpenApiRouter::with_openapi(ApiDoc::openapi())
        .merge(system::router())
        .merge(metadata::router())
        .merge(watchlist::router())
        .merge(releases::router())
        .merge(stream::router())
        .merge(downloads::router())
        .merge(history::router())
        .merge(settings::router())
        .split_for_parts();

    let api_router = api_router.layer(middleware::from_fn_with_state(
        state.clone(),
        auth::require_api_key,
    ));

    Router::new()
        .merge(SwaggerUi::new("/docs").url("/api-docs/openapi.json", api))
        .route("/health", axum::routing::get(system::health))
        // Loopback-only virtual-file access for ffmpeg/ffprobe. Deliberately
        // outside /api/v1 (its own token guard) and outside the OpenAPI doc.
        .route(
            "/internal/vfs/{session_id}",
            axum::routing::get(internal::serve_vfs),
        )
        .nest("/api/v1", api_router)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}
