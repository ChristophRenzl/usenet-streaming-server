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
pub mod webui;

use axum::{middleware, Router};
use tower_http::trace::TraceLayer;
use utoipa::{
    openapi::security::{ApiKey, ApiKeyValue, SecurityRequirement, SecurityScheme},
    Modify, OpenApi,
};
use utoipa_axum::router::OpenApiRouter;
use utoipa_swagger_ui::SwaggerUi;

use crate::state::AppState;

/// Registers the `X-Api-Key` header scheme and requires it globally so the
/// Swagger UI "Authorize" button unlocks "Try it out" for every endpoint.
struct SecurityAddon;

impl Modify for SecurityAddon {
    fn modify(&self, openapi: &mut utoipa::openapi::OpenApi) {
        let components = openapi.components.get_or_insert_with(Default::default);
        components.add_security_scheme(
            "api_key",
            SecurityScheme::ApiKey(ApiKey::Header(ApiKeyValue::with_description(
                "X-Api-Key",
                "Server API key: the bootstrap key from config.toml / APP_AUTH__API_KEY, \
                 or the rotated key set via PUT /settings/app.",
            ))),
        );
        openapi.security = Some(vec![SecurityRequirement::new(
            "api_key",
            Vec::<String>::new(),
        )]);
    }
}

#[derive(OpenApi)]
#[openapi(
    info(
        title = "Usenet Streaming Server",
        description = "Search movies/TV via TMDB and stream them on-the-fly from Usenet.",
        license(name = "MIT")
    ),
    modifiers(&SecurityAddon),
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
        // Embedded admin UI at the root; unauthenticated static files (the
        // UI collects the API key itself and sends it on every request).
        .merge(webui::router())
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
