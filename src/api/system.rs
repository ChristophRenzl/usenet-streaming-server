use axum::Json;
use serde::Serialize;
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::state::AppState;

#[derive(Serialize, ToSchema)]
pub struct ServerInfo {
    pub name: &'static str,
    pub version: &'static str,
}

/// Liveness probe. Unauthenticated, also mounted at `/health`.
#[utoipa::path(get, path = "/system/info", tag = "system",
    responses((status = 200, body = ServerInfo)))]
pub async fn info() -> Json<ServerInfo> {
    Json(ServerInfo {
        name: env!("CARGO_PKG_NAME"),
        version: env!("CARGO_PKG_VERSION"),
    })
}

pub async fn health() -> &'static str {
    "ok"
}

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(info))
}
