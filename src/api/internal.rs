//! Internal loopback route serving a session's virtual file to ffmpeg /
//! ffprobe. Not part of the public API (excluded from the OpenAPI doc):
//! access requires the per-session token *and* a loopback peer address.

use std::net::SocketAddr;

use axum::extract::{ConnectInfo, FromRequestParts, Path, Query, State};
use axum::http::{request::Parts, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use serde::Deserialize;
use uuid::Uuid;

use crate::state::AppState;

use super::auth::constant_time_eq;
use super::stream::serve_session_file;

/// Peer address extractor that degrades to `None` instead of failing when
/// the server was started without connect-info (e.g. some test setups) —
/// the guard then rejects the request.
pub struct ClientAddr(pub Option<SocketAddr>);

impl<S> FromRequestParts<S> for ClientAddr
where
    S: Send + Sync,
{
    type Rejection = std::convert::Infallible;

    async fn from_request_parts(parts: &mut Parts, _state: &S) -> Result<Self, Self::Rejection> {
        Ok(Self(
            parts
                .extensions
                .get::<ConnectInfo<SocketAddr>>()
                .map(|info| info.0),
        ))
    }
}

#[derive(Debug, Deserialize)]
pub struct VfsQuery {
    token: Option<String>,
}

/// GET /internal/vfs/{session_id}?token= — byte-range access for the local
/// ffmpeg/ffprobe processes only.
pub async fn serve_vfs(
    State(state): State<AppState>,
    Path(session_id): Path<Uuid>,
    Query(query): Query<VfsQuery>,
    ClientAddr(peer): ClientAddr,
    headers: HeaderMap,
) -> Response {
    // Loopback peers only; anything else (or an unknown peer) is forbidden.
    if !peer.is_some_and(|addr| addr.ip().is_loopback()) {
        return (StatusCode::FORBIDDEN, "loopback only").into_response();
    }

    let Some(session) = state.sessions.get(&session_id) else {
        return (StatusCode::NOT_FOUND, "no such session").into_response();
    };

    let token_ok = query
        .token
        .as_deref()
        .is_some_and(|token| constant_time_eq(token, &session.token));
    if !token_ok {
        return (StatusCode::FORBIDDEN, "bad token").into_response();
    }

    serve_session_file(&session, &headers)
}
