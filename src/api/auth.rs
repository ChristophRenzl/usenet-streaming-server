use axum::{
    extract::{Query, Request, State},
    middleware::Next,
    response::Response,
    Json,
};
use rand::Rng;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

use crate::{
    db,
    error::{AppError, AppResult},
    state::AppState,
};

/// The authenticated identity of a request, inserted as a request extension
/// by [`require_api_key`]. API-key requests act as the built-in owner/admin
/// (user 1) — single-user setups and the Apple clients never see a login.
#[derive(Debug, Clone)]
pub struct CurrentUser {
    pub id: i64,
    pub name: String,
    pub is_admin: bool,
}

impl CurrentUser {
    /// The server owner: what an API-key request acts as.
    fn owner() -> Self {
        Self {
            id: 1,
            name: "owner".into(),
            is_admin: true,
        }
    }
}

#[derive(Deserialize)]
struct ApiKeyQuery {
    apikey: Option<String>,
    /// User bearer token as a query parameter — media players (hls.js,
    /// AVPlayer) cannot always set headers on segment requests.
    token: Option<String>,
}

/// Require a valid API key in `X-Api-Key` or `?apikey=`.
///
/// Two keys can be valid at once: the bootstrap key from the config file /
/// environment (always accepted, so a rotated key can never lock the operator
/// out of a Docker deployment) and the optional rotated key stored in the
/// database via `PUT /settings/app`.
///
/// Query-param support exists because HLS/AVPlayer clients cannot always set
/// custom headers on media requests.
pub async fn require_api_key(
    State(state): State<AppState>,
    mut request: Request,
    next: Next,
) -> Result<Response, AppError> {
    let header_key = request
        .headers()
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let bearer = request
        .headers()
        .get("authorization")
        .and_then(|v| v.to_str().ok())
        .and_then(|v| v.strip_prefix("Bearer "))
        .map(str::to_owned);
    let query: ApiKeyQuery = Query::try_from_uri(request.uri())
        .map(|Query(q)| q)
        .unwrap_or(ApiKeyQuery {
            apikey: None,
            token: None,
        });

    // API key (header or query) grants the owner/admin identity.
    if let Some(key) = header_key.or(query.apikey) {
        if is_valid_key(&state, &key).await? {
            request.extensions_mut().insert(CurrentUser::owner());
            return Ok(next.run(request).await);
        }
        return Err(AppError::Unauthorized);
    }

    // User bearer token (header or ?token= for media requests).
    if let Some(token) = bearer.or(query.token) {
        if let Some(user) = db::users::token_user(&state.db, &token).await? {
            request.extensions_mut().insert(CurrentUser {
                id: user.id,
                name: user.name,
                is_admin: user.is_admin,
            });
            return Ok(next.run(request).await);
        }
    }
    Err(AppError::Unauthorized)
}

// ---- Login ------------------------------------------------------------------

#[derive(Debug, Deserialize, ToSchema)]
pub struct LoginRequest {
    pub username: String,
    pub password: String,
    /// Optional device label shown in future token listings.
    pub device: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct LoginResponse {
    /// Bearer token for `Authorization: Bearer …` (or `?token=` on media
    /// URLs). Valid until logout or user deletion.
    pub token: String,
    pub user: db::users::User,
}

/// Exchange username + password for a bearer token. Mounted *outside* the
/// authenticated API surface (at `/auth/login`) so a login needs no key.
pub async fn login(
    State(state): State<AppState>,
    Json(request): Json<LoginRequest>,
) -> AppResult<Json<LoginResponse>> {
    use argon2::password_hash::PasswordHash;
    use argon2::{Argon2, PasswordVerifier};

    let credentials = db::users::credentials(&state.db, &request.username).await?;
    let Some((user_id, Some(hash))) = credentials else {
        // Unknown user and password-less user answer identically.
        return Err(AppError::Unauthorized);
    };
    let parsed =
        PasswordHash::new(&hash).map_err(|_| AppError::Internal(anyhow::anyhow!("bad hash")))?;
    if Argon2::default()
        .verify_password(request.password.as_bytes(), &parsed)
        .is_err()
    {
        return Err(AppError::Unauthorized);
    }
    let user = db::users::get(&state.db, user_id)
        .await?
        .ok_or(AppError::Unauthorized)?;
    let token = format!("{:032x}", rand::rng().random::<u128>());
    db::users::insert_token(&state.db, &token, user.id, request.device.as_deref()).await?;
    Ok(Json(LoginResponse { token, user }))
}

/// Hash a password for storage.
pub fn hash_password(password: &str) -> AppResult<String> {
    use argon2::password_hash::SaltString;
    use argon2::{Argon2, PasswordHasher};

    let salt_bytes: [u8; 16] = rand::rng().random();
    let salt = SaltString::encode_b64(&salt_bytes)
        .map_err(|e| AppError::Internal(anyhow::anyhow!("salt: {e}")))?;
    Argon2::default()
        .hash_password(password.as_bytes(), &salt)
        .map(|h| h.to_string())
        .map_err(|e| AppError::Internal(anyhow::anyhow!("hashing password: {e}")))
}

/// Check a presented key against the bootstrap config key and, when set, the
/// database override. Both comparisons always run (constant-time each) so the
/// outcome does not leak which key matched.
async fn is_valid_key(state: &AppState, presented: &str) -> AppResult<bool> {
    let config_ok = constant_time_eq(presented, state.config.auth.api_key.as_str());
    let override_ok = db::settings::get(&state.db, db::settings::API_KEY_OVERRIDE)
        .await?
        .filter(|k| !k.is_empty())
        .is_some_and(|k| constant_time_eq(presented, &k));
    Ok(config_ok || override_ok)
}

pub(crate) fn constant_time_eq(a: &str, b: &str) -> bool {
    let (a, b) = (a.as_bytes(), b.as_bytes());
    if a.len() != b.len() {
        return false;
    }
    a.iter().zip(b).fold(0u8, |acc, (x, y)| acc | (x ^ y)) == 0
}

#[cfg(test)]
mod tests {
    use super::constant_time_eq;

    #[test]
    fn eq_semantics() {
        assert!(constant_time_eq("secret", "secret"));
        assert!(!constant_time_eq("secret", "secreT"));
        assert!(!constant_time_eq("secret", "secret2"));
        assert!(!constant_time_eq("", "x"));
        assert!(constant_time_eq("", ""));
    }
}
