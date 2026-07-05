use axum::{
    extract::{Query, Request, State},
    middleware::Next,
    response::Response,
};
use serde::Deserialize;

use crate::{
    db,
    error::{AppError, AppResult},
    state::AppState,
};

#[derive(Deserialize)]
struct ApiKeyQuery {
    apikey: Option<String>,
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
    request: Request,
    next: Next,
) -> Result<Response, AppError> {
    let header_key = request
        .headers()
        .get("x-api-key")
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);

    let presented = match header_key {
        Some(key) => Some(key),
        None => {
            let Query(q): Query<ApiKeyQuery> =
                Query::try_from_uri(request.uri()).unwrap_or(Query(ApiKeyQuery { apikey: None }));
            q.apikey
        }
    };

    let authorized = match presented {
        Some(key) => is_valid_key(&state, &key).await?,
        None => false,
    };

    if !authorized {
        return Err(AppError::Unauthorized);
    }
    Ok(next.run(request).await)
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
