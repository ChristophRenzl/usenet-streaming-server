//! Embedded web admin UI.
//!
//! Three static files (HTML/CSS/JS, no build step, no CDN dependencies) are
//! compiled into the binary and served unauthenticated at `/` — the UI itself
//! asks for the API key and talks to `/api/v1` with it.

use axum::{
    http::header::{CACHE_CONTROL, CONTENT_TYPE},
    response::IntoResponse,
    routing::get,
    Router,
};

use crate::state::AppState;

const INDEX_HTML: &str = include_str!("../webui/index.html");
const APP_JS: &str = include_str!("../webui/app.js");
const STYLE_CSS: &str = include_str!("../webui/style.css");

fn asset(content_type: &'static str, body: &'static str) -> impl IntoResponse {
    (
        [
            (CONTENT_TYPE, content_type),
            // Always revalidate so a server update ships UI changes instantly.
            (CACHE_CONTROL, "no-cache"),
        ],
        body,
    )
}

async fn index() -> impl IntoResponse {
    asset("text/html; charset=utf-8", INDEX_HTML)
}

async fn app_js() -> impl IntoResponse {
    asset("application/javascript; charset=utf-8", APP_JS)
}

async fn style_css() -> impl IntoResponse {
    asset("text/css; charset=utf-8", STYLE_CSS)
}

pub fn router() -> Router<AppState> {
    Router::new()
        .route("/", get(index))
        .route("/assets/app.js", get(app_js))
        .route("/assets/style.css", get(style_css))
}
