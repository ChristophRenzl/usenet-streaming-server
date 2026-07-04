pub mod auth;
pub mod metadata;
pub mod releases;
pub mod settings;
pub mod system;

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
        (name = "metadata", description = "TMDB search and details"),
        (name = "releases", description = "Indexer release search and ranking"),
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
        .merge(releases::router())
        .merge(settings::router())
        .split_for_parts();

    let api_router = api_router.layer(middleware::from_fn_with_state(
        state.clone(),
        auth::require_api_key,
    ));

    Router::new()
        .merge(SwaggerUi::new("/docs").url("/api-docs/openapi.json", api))
        .route("/health", axum::routing::get(system::health))
        .nest("/api/v1", api_router)
        .layer(TraceLayer::new_for_http())
        .with_state(state)
}
