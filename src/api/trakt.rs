//! Trakt linking + scrobbling endpoints.
//!
//! The user supplies their own Trakt API app credentials once (create one at
//! trakt.tv/oauth/applications), then links the account via the device-code
//! flow: the app shows a short code, the user enters it at
//! trakt.tv/activate, the client polls until Trakt confirms. Scrobbles fire
//! from the streaming session lifecycle and are always best-effort.

use std::sync::Arc;

use axum::{extract::State, Json};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::{
    db,
    error::{AppError, AppResult},
    state::AppState,
    stream::session::Session,
    tmdb::models::MediaType,
    trakt::{DevicePoll, ScrobbleAction, ScrobbleItem, TraktClient, DEFAULT_BASE_URL},
};

/// A configured client from the stored app credentials, or None when the
/// user hasn't set any yet.
async fn configured_client(state: &AppState) -> AppResult<Option<TraktClient>> {
    let id = db::settings::get(&state.db, db::settings::TRAKT_CLIENT_ID)
        .await?
        .filter(|v| !v.is_empty());
    let secret = db::settings::get(&state.db, db::settings::TRAKT_CLIENT_SECRET)
        .await?
        .filter(|v| !v.is_empty());
    Ok(match (id, secret) {
        (Some(id), Some(secret)) => Some(TraktClient::new(
            state.http.clone(),
            DEFAULT_BASE_URL,
            id,
            secret,
        )),
        _ => None,
    })
}

/// The stored access token, refreshed (and re-stored) when it expires within
/// ten minutes. None when the account isn't linked.
async fn valid_access_token(state: &AppState, client: &TraktClient) -> AppResult<Option<String>> {
    let Some(access) = db::settings::get(&state.db, db::settings::TRAKT_ACCESS_TOKEN)
        .await?
        .filter(|v| !v.is_empty())
    else {
        return Ok(None);
    };
    let expires_at = db::settings::get(&state.db, db::settings::TRAKT_EXPIRES_AT)
        .await?
        .and_then(|v| v.parse::<i64>().ok())
        .unwrap_or(0);
    let now = chrono::Utc::now().timestamp();
    if expires_at > now + 600 {
        return Ok(Some(access));
    }
    // Expiring/expired: try a refresh; a failure just means "not linked right
    // now" rather than an error surfaced to playback code.
    let Some(refresh) = db::settings::get(&state.db, db::settings::TRAKT_REFRESH_TOKEN)
        .await?
        .filter(|v| !v.is_empty())
    else {
        return Ok(None);
    };
    match client.refresh(&refresh).await {
        Ok(tokens) => {
            store_tokens(state, &tokens).await?;
            Ok(Some(tokens.access_token))
        }
        Err(error) => {
            tracing::debug!(%error, "trakt token refresh failed");
            Ok(None)
        }
    }
}

async fn store_tokens(state: &AppState, tokens: &crate::trakt::Tokens) -> AppResult<()> {
    db::settings::set(
        &state.db,
        db::settings::TRAKT_ACCESS_TOKEN,
        &tokens.access_token,
    )
    .await?;
    db::settings::set(
        &state.db,
        db::settings::TRAKT_REFRESH_TOKEN,
        &tokens.refresh_token,
    )
    .await?;
    db::settings::set(
        &state.db,
        db::settings::TRAKT_EXPIRES_AT,
        &tokens.expires_at.to_string(),
    )
    .await
}

// ---- Scrobble hooks -------------------------------------------------------

/// Fire-and-forget scrobble for a session lifecycle event. Detached and
/// best-effort: Trakt being down or unlinked never affects playback.
pub fn spawn_scrobble(state: &AppState, session: &Arc<Session>, action: ScrobbleAction) {
    let state = state.clone();
    let session = session.clone();
    tokio::spawn(async move {
        if let Err(error) = scrobble(&state, &session, action).await {
            tracing::debug!(%error, "trakt scrobble failed");
        }
    });
}

async fn scrobble(
    state: &AppState,
    session: &Arc<Session>,
    action: ScrobbleAction,
) -> AppResult<()> {
    let Some(client) = configured_client(state).await? else {
        return Ok(());
    };
    let Some(token) = valid_access_token(state, &client).await? else {
        return Ok(());
    };
    let item = match session.media_type {
        MediaType::Movie => ScrobbleItem::Movie {
            tmdb_id: session.tmdb_id,
        },
        MediaType::Tv => {
            let (Some(season), Some(episode)) = (session.season, session.episode) else {
                return Ok(());
            };
            ScrobbleItem::Episode {
                show_tmdb_id: session.tmdb_id,
                season,
                episode,
            }
        }
    };
    let duration = session.info().duration_secs;
    // The most recent reported position (the client reports every ~10s and
    // once more right before ending the session).
    let position = db::watch_history::position_secs(
        &state.db,
        session.tmdb_id,
        session.media_type.as_str(),
        session.season,
        session.episode,
    )
    .await?
    .unwrap_or(session.resume_position_secs);
    let progress = crate::trakt::progress_percent(position, duration);
    client.scrobble(action, item, progress, &token).await
}

// ---- Linking endpoints ------------------------------------------------------

#[derive(Debug, Serialize, ToSchema)]
pub struct TraktStatus {
    /// Whether Trakt app credentials are stored.
    pub configured: bool,
    /// Whether an account is linked (token present).
    pub linked: bool,
}

/// Current Trakt link state.
#[utoipa::path(get, path = "/trakt/status", tag = "settings",
    responses((status = 200, body = TraktStatus)))]
pub async fn trakt_status(State(state): State<AppState>) -> AppResult<Json<TraktStatus>> {
    let configured = configured_client(&state).await?.is_some();
    let linked = db::settings::get(&state.db, db::settings::TRAKT_ACCESS_TOKEN)
        .await?
        .filter(|v| !v.is_empty())
        .is_some();
    Ok(Json(TraktStatus { configured, linked }))
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct LinkRequest {
    /// Trakt app client id; stored when non-empty (first-time setup).
    pub client_id: Option<String>,
    /// Trakt app client secret; stored when non-empty (first-time setup).
    pub client_secret: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct LinkStartResponse {
    /// Opaque code the client passes back to the poll endpoint.
    pub device_code: String,
    /// Short code the user types at the verification URL.
    pub user_code: String,
    /// Where the user approves the link (trakt.tv/activate).
    pub verification_url: String,
    /// Seconds until the codes expire.
    pub expires_in: u64,
    /// Poll no faster than this many seconds.
    pub interval: u64,
}

/// Start linking a Trakt account (device-code flow). Optionally stores the
/// app credentials first, so the initial setup is a single call.
#[utoipa::path(post, path = "/trakt/link", tag = "settings",
    request_body = LinkRequest,
    responses(
        (status = 200, body = LinkStartResponse),
        (status = 400, description = "No Trakt app credentials configured"),
        (status = 502, description = "Trakt upstream error"),
    ))]
pub async fn trakt_link(
    State(state): State<AppState>,
    Json(request): Json<LinkRequest>,
) -> AppResult<Json<LinkStartResponse>> {
    if let Some(id) = request
        .client_id
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        db::settings::set(&state.db, db::settings::TRAKT_CLIENT_ID, id).await?;
    }
    if let Some(secret) = request
        .client_secret
        .as_deref()
        .map(str::trim)
        .filter(|v| !v.is_empty())
    {
        db::settings::set(&state.db, db::settings::TRAKT_CLIENT_SECRET, secret).await?;
    }
    let Some(client) = configured_client(&state).await? else {
        return Err(AppError::BadRequest(
            "Trakt app credentials not configured; create an app at \
             trakt.tv/oauth/applications and provide client_id + client_secret"
                .into(),
        ));
    };
    let code = client.device_code().await?;
    Ok(Json(LinkStartResponse {
        device_code: code.device_code,
        user_code: code.user_code,
        verification_url: code.verification_url,
        expires_in: code.expires_in,
        interval: code.interval,
    }))
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct PollRequest {
    pub device_code: String,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct PollResponse {
    /// `linked`, `pending`, `slow_down`, `denied` or `expired`.
    pub status: String,
}

/// One poll of the pending device link; stores the tokens once approved.
#[utoipa::path(post, path = "/trakt/link/poll", tag = "settings",
    request_body = PollRequest,
    responses(
        (status = 200, body = PollResponse),
        (status = 400, description = "No Trakt app credentials configured"),
        (status = 502, description = "Trakt upstream error"),
    ))]
pub async fn trakt_link_poll(
    State(state): State<AppState>,
    Json(request): Json<PollRequest>,
) -> AppResult<Json<PollResponse>> {
    let Some(client) = configured_client(&state).await? else {
        return Err(AppError::BadRequest(
            "Trakt app credentials not configured".into(),
        ));
    };
    let status = match client.poll_device_token(&request.device_code).await? {
        DevicePoll::Linked(tokens) => {
            store_tokens(&state, &tokens).await?;
            "linked"
        }
        DevicePoll::Pending => "pending",
        DevicePoll::SlowDown => "slow_down",
        DevicePoll::Denied => "denied",
        DevicePoll::Expired => "expired",
    };
    Ok(Json(PollResponse {
        status: status.into(),
    }))
}

/// Unlink the Trakt account (clears tokens; the app credentials stay).
#[utoipa::path(delete, path = "/trakt/link", tag = "settings",
    responses((status = 204)))]
pub async fn trakt_unlink(State(state): State<AppState>) -> AppResult<axum::http::StatusCode> {
    db::settings::delete(&state.db, db::settings::TRAKT_ACCESS_TOKEN).await?;
    db::settings::delete(&state.db, db::settings::TRAKT_REFRESH_TOKEN).await?;
    db::settings::delete(&state.db, db::settings::TRAKT_EXPIRES_AT).await?;
    Ok(axum::http::StatusCode::NO_CONTENT)
}

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(trakt_status))
        .routes(routes!(trakt_link, trakt_unlink))
        .routes(routes!(trakt_link_poll))
}
