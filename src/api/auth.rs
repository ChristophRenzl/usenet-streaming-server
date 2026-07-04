use axum::{
    extract::{Query, Request, State},
    middleware::Next,
    response::Response,
};
use serde::Deserialize;

use crate::{error::AppError, state::AppState};

#[derive(Deserialize)]
struct ApiKeyQuery {
    apikey: Option<String>,
}

/// Require the configured API key in `X-Api-Key` or `?apikey=`.
///
/// Query-param support exists because HLS/AVPlayer clients cannot always set
/// custom headers on media requests.
pub async fn require_api_key(
    State(state): State<AppState>,
    request: Request,
    next: Next,
) -> Result<Response, AppError> {
    let expected = state.config.auth.api_key.as_str();

    let header_key = request
        .headers()
        .get("x-api-key")
        .and_then(|v| v.to_str().ok());

    let authorized = match header_key {
        Some(key) => constant_time_eq(key, expected),
        None => {
            let Query(q): Query<ApiKeyQuery> =
                Query::try_from_uri(request.uri()).unwrap_or(Query(ApiKeyQuery { apikey: None }));
            q.apikey.is_some_and(|k| constant_time_eq(&k, expected))
        }
    };

    if !authorized {
        return Err(AppError::Unauthorized);
    }
    Ok(next.run(request).await)
}

fn constant_time_eq(a: &str, b: &str) -> bool {
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
