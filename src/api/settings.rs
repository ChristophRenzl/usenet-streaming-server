//! Settings CRUD: preferences, indexers, NNTP providers and app settings.
//!
//! Secrets (indexer API keys, provider passwords, TMDB key) are never logged;
//! the TMDB key is additionally masked on read.

use axum::{
    extract::{Path, State},
    Json,
};
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::{
    db::{
        self,
        indexers::{Indexer, IndexerInput},
        preferences::Preferences,
        providers::{Provider, ProviderInput},
    },
    error::{AppError, AppResult},
    indexer::client::NewznabClient,
    state::AppState,
};

// ---- Preferences -----------------------------------------------------------

/// Get the quality preferences (single default user).
#[utoipa::path(get, path = "/settings/preferences", tag = "settings",
    responses((status = 200, body = Preferences)))]
pub async fn get_preferences(State(state): State<AppState>) -> AppResult<Json<Preferences>> {
    Ok(Json(db::preferences::get(&state.db).await?))
}

/// Replace the quality preferences.
#[utoipa::path(put, path = "/settings/preferences", tag = "settings",
    request_body = Preferences,
    responses((status = 200, body = Preferences)))]
pub async fn put_preferences(
    State(state): State<AppState>,
    Json(prefs): Json<Preferences>,
) -> AppResult<Json<Preferences>> {
    if prefs.preferred_resolution > prefs.max_resolution {
        return Err(AppError::BadRequest(
            "preferred_resolution must not exceed max_resolution".into(),
        ));
    }
    if let (Some(preferred), Some(max)) = (prefs.preferred_resolution_tv, prefs.max_resolution_tv) {
        if preferred > max {
            return Err(AppError::BadRequest(
                "preferred_resolution_tv must not exceed max_resolution_tv".into(),
            ));
        }
    }
    db::preferences::set(&state.db, &prefs).await?;
    Ok(Json(db::preferences::get(&state.db).await?))
}

// ---- Indexers ---------------------------------------------------------------

/// List all configured indexers.
#[utoipa::path(get, path = "/settings/indexers", tag = "settings",
    responses((status = 200, body = [Indexer])))]
pub async fn list_indexers(State(state): State<AppState>) -> AppResult<Json<Vec<Indexer>>> {
    Ok(Json(db::indexers::list(&state.db).await?))
}

/// Add an indexer.
#[utoipa::path(post, path = "/settings/indexers", tag = "settings",
    request_body = IndexerInput,
    responses((status = 200, body = Indexer)))]
pub async fn create_indexer(
    State(state): State<AppState>,
    Json(input): Json<IndexerInput>,
) -> AppResult<Json<Indexer>> {
    Ok(Json(db::indexers::create(&state.db, &input).await?))
}

/// Update an indexer.
#[utoipa::path(put, path = "/settings/indexers/{id}", tag = "settings",
    params(("id" = i64, Path, description = "Indexer id")),
    request_body = IndexerInput,
    responses((status = 200, body = Indexer), (status = 404)))]
pub async fn update_indexer(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(input): Json<IndexerInput>,
) -> AppResult<Json<Indexer>> {
    db::indexers::update(&state.db, id, &input)
        .await?
        .map(Json)
        .ok_or_else(|| AppError::NotFound(format!("indexer {id}")))
}

/// Remove an indexer.
#[utoipa::path(delete, path = "/settings/indexers/{id}", tag = "settings",
    params(("id" = i64, Path, description = "Indexer id")),
    responses((status = 204), (status = 404)))]
pub async fn delete_indexer(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> AppResult<axum::http::StatusCode> {
    if db::indexers::delete(&state.db, id).await? {
        Ok(axum::http::StatusCode::NO_CONTENT)
    } else {
        Err(AppError::NotFound(format!("indexer {id}")))
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct IndexerTestResult {
    pub ok: bool,
    pub error: Option<String>,
}

/// Test an indexer's connectivity and credentials with a trivial search.
#[utoipa::path(post, path = "/settings/indexers/{id}/test", tag = "settings",
    params(("id" = i64, Path, description = "Indexer id")),
    responses((status = 200, body = IndexerTestResult), (status = 404)))]
pub async fn test_indexer(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> AppResult<Json<IndexerTestResult>> {
    let indexer = db::indexers::get(&state.db, id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("indexer {id}")))?;
    let client = NewznabClient::new(state.http.clone(), indexer);
    let result = match client.test().await {
        Ok(()) => IndexerTestResult {
            ok: true,
            error: None,
        },
        Err(e) => IndexerTestResult {
            ok: false,
            error: Some(e.to_string()),
        },
    };
    Ok(Json(result))
}

// ---- NNTP providers ---------------------------------------------------------

/// Rebuild the NNTP pool from the current provider table so configuration
/// changes take effect without a restart.
async fn reload_pool(state: &AppState) -> AppResult<()> {
    state
        .nntp_pool
        .reload(db::providers::list(&state.db).await?);
    Ok(())
}

/// List all NNTP providers.
#[utoipa::path(get, path = "/settings/providers", tag = "settings",
    responses((status = 200, body = [Provider])))]
pub async fn list_providers(State(state): State<AppState>) -> AppResult<Json<Vec<Provider>>> {
    Ok(Json(db::providers::list(&state.db).await?))
}

/// Add an NNTP provider.
#[utoipa::path(post, path = "/settings/providers", tag = "settings",
    request_body = ProviderInput,
    responses((status = 200, body = Provider)))]
pub async fn create_provider(
    State(state): State<AppState>,
    Json(input): Json<ProviderInput>,
) -> AppResult<Json<Provider>> {
    let provider = db::providers::create(&state.db, &input).await?;
    reload_pool(&state).await?;
    Ok(Json(provider))
}

/// Update an NNTP provider.
#[utoipa::path(put, path = "/settings/providers/{id}", tag = "settings",
    params(("id" = i64, Path, description = "Provider id")),
    request_body = ProviderInput,
    responses((status = 200, body = Provider), (status = 404)))]
pub async fn update_provider(
    State(state): State<AppState>,
    Path(id): Path<i64>,
    Json(input): Json<ProviderInput>,
) -> AppResult<Json<Provider>> {
    let provider = db::providers::update(&state.db, id, &input)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("provider {id}")))?;
    reload_pool(&state).await?;
    Ok(Json(provider))
}

/// Remove an NNTP provider.
#[utoipa::path(delete, path = "/settings/providers/{id}", tag = "settings",
    params(("id" = i64, Path, description = "Provider id")),
    responses((status = 204), (status = 404)))]
pub async fn delete_provider(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> AppResult<axum::http::StatusCode> {
    if db::providers::delete(&state.db, id).await? {
        reload_pool(&state).await?;
        Ok(axum::http::StatusCode::NO_CONTENT)
    } else {
        Err(AppError::NotFound(format!("provider {id}")))
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct ProviderTestResult {
    pub ok: bool,
    /// Round-trip time for connect + auth + DATE, when successful.
    pub latency_ms: Option<u64>,
    pub error: Option<String>,
}

/// Test an NNTP provider: dial it, authenticate and issue `DATE`.
#[utoipa::path(post, path = "/settings/providers/{id}/test", tag = "settings",
    params(("id" = i64, Path, description = "Provider id")),
    responses((status = 200, body = ProviderTestResult), (status = 404)))]
pub async fn test_provider(
    State(state): State<AppState>,
    Path(id): Path<i64>,
) -> AppResult<Json<ProviderTestResult>> {
    let provider = db::providers::get(&state.db, id)
        .await?
        .ok_or_else(|| AppError::NotFound(format!("provider {id}")))?;
    let result = match crate::nntp::test_provider(&provider).await {
        Ok(latency) => ProviderTestResult {
            ok: true,
            latency_ms: Some(latency.as_millis() as u64),
            error: None,
        },
        Err(e) => ProviderTestResult {
            ok: false,
            latency_ms: None,
            error: Some(e),
        },
    };
    Ok(Json(result))
}

// ---- App settings -----------------------------------------------------------

#[derive(Debug, Serialize, ToSchema)]
pub struct AppSettings {
    /// Masked TMDB API key (only the last 4 characters are shown), or `null`
    /// when not configured.
    pub tmdb_api_key: Option<String>,
    /// Masked SubDL API key (last 4 characters), or `null` when not set.
    /// SubDL is the fallback subtitle provider when OpenSubtitles cannot
    /// deliver (quota, no result); a free key comes from subdl.com/panel/api.
    pub subdl_api_key: Option<String>,
    /// Ordered subtitle provider preference (e.g. ["opensubtitles","subdl"]),
    /// providers tried in this order. Any known provider not listed is
    /// appended in default order.
    pub subtitle_provider_order: Vec<String>,
    /// Masked per-user OpenSubtitles API key (last 4 characters), or `null`
    /// when no per-user key is stored. Subtitles are optional; playback works
    /// without it. A `null` here does not mean subtitles are unavailable — an
    /// operator config default may still be in effect (see
    /// `opensubtitles_api_key_source`).
    pub opensubtitles_api_key: Option<String>,
    /// Whether an OpenSubtitles API key is effectively configured — true when a
    /// per-user key is set OR an operator config default exists. Convenience
    /// flag for the dashboard checklist, which treats subtitles as optional.
    pub opensubtitles_configured: bool,
    /// Where the effective OpenSubtitles API key comes from: `"user"` (per-user
    /// key stored via the API), `"default"` (operator-supplied config/env
    /// default, so users only need username/password), or `"none"`. Lets the UI
    /// explain the key's origin and make the per-user key optional.
    pub opensubtitles_api_key_source: String,
    /// Whether an operator config default OpenSubtitles API key is present
    /// (`subtitles.opensubtitles_default_api_key`). When true, the per-user key
    /// is optional and only acts as an override.
    pub opensubtitles_default_key_active: bool,
    /// OpenSubtitles account username, when configured. Logging in lifts the
    /// anonymous download quota.
    pub opensubtitles_username: Option<String>,
    /// Whether an OpenSubtitles account password is stored (the password
    /// itself is write-only and never reported back).
    pub opensubtitles_password_set: bool,
    /// Masked server API key currently in effect: the database override when
    /// one is set, otherwise the bootstrap key from the config file.
    pub api_key: String,
    /// Whether a rotated API key (database override) is active. The bootstrap
    /// config key stays valid alongside it as a recovery path.
    pub api_key_override_active: bool,
}

#[derive(Debug, Deserialize, ToSchema)]
pub struct AppSettingsInput {
    /// New TMDB API key. Omit to leave unchanged; send `""` to clear.
    pub tmdb_api_key: Option<String>,
    /// New OpenSubtitles API key. Omit to leave unchanged; send `""` to clear.
    pub opensubtitles_api_key: Option<String>,
    /// New SubDL API key (fallback provider). Omit to leave unchanged; send
    /// `""` to clear.
    pub subdl_api_key: Option<String>,
    /// New subtitle provider order (e.g. ["subdl","opensubtitles"]). Omit to
    /// leave unchanged; send `[]` to reset to the default order.
    pub subtitle_provider_order: Option<Vec<String>>,
    /// OpenSubtitles account username (login lifts the download quota). Omit
    /// to leave unchanged; send `""` to clear.
    pub opensubtitles_username: Option<String>,
    /// OpenSubtitles account password. Omit to leave unchanged; send `""` to
    /// clear. Write-only: it is never reported back.
    pub opensubtitles_password: Option<String>,
    /// New server API key (at least 16 characters). Stored as an override;
    /// the bootstrap key from the config file / environment remains valid.
    pub api_key: Option<String>,
}

fn mask_secret(secret: &str) -> String {
    let chars: Vec<char> = secret.chars().collect();
    if chars.len() <= 4 {
        "****".to_string()
    } else {
        let tail: String = chars[chars.len() - 4..].iter().collect();
        format!("****{tail}")
    }
}

async fn current_app_settings(state: &AppState) -> AppResult<AppSettings> {
    let tmdb_key = db::settings::get(&state.db, db::settings::TMDB_API_KEY)
        .await?
        .filter(|k| !k.is_empty());
    // A per-user key is masked and reported directly; whether subtitles are
    // effectively usable additionally accounts for the operator config default.
    let opensubtitles_key = db::settings::get(&state.db, db::settings::OPENSUBTITLES_API_KEY)
        .await?
        .filter(|k| !k.is_empty());
    let (_, opensubtitles_key_source) =
        crate::api::subtitles::effective_opensubtitles_key(state).await?;
    let opensubtitles_default_key_active = matches!(
        opensubtitles_key_source,
        crate::api::subtitles::ApiKeySource::Default
    ) || state
        .config
        .subtitles
        .opensubtitles_default_api_key
        .as_deref()
        .is_some_and(|k| !k.is_empty());
    let opensubtitles_api_key_source = match opensubtitles_key_source {
        crate::api::subtitles::ApiKeySource::User => "user",
        crate::api::subtitles::ApiKeySource::Default => "default",
        crate::api::subtitles::ApiKeySource::None => "none",
    }
    .to_string();
    let subdl_key = db::settings::get(&state.db, db::settings::SUBDL_API_KEY)
        .await?
        .filter(|k| !k.is_empty());
    let subtitle_provider_order = crate::api::subtitles::subtitle_provider_order(&state).await;
    let opensubtitles_username = db::settings::get(&state.db, db::settings::OPENSUBTITLES_USERNAME)
        .await?
        .filter(|u| !u.is_empty());
    let opensubtitles_password_set =
        db::settings::get(&state.db, db::settings::OPENSUBTITLES_PASSWORD)
            .await?
            .filter(|p| !p.is_empty())
            .is_some();
    let override_key = db::settings::get(&state.db, db::settings::API_KEY_OVERRIDE)
        .await?
        .filter(|k| !k.is_empty());
    let api_key_override_active = override_key.is_some();
    let active_key = override_key.unwrap_or_else(|| state.config.auth.api_key.clone());
    Ok(AppSettings {
        tmdb_api_key: tmdb_key.map(|k| mask_secret(&k)),
        opensubtitles_configured: !matches!(
            opensubtitles_key_source,
            crate::api::subtitles::ApiKeySource::None
        ),
        opensubtitles_api_key: opensubtitles_key.map(|k| mask_secret(&k)),
        subdl_api_key: subdl_key.map(|k| mask_secret(&k)),
        subtitle_provider_order,
        opensubtitles_api_key_source,
        opensubtitles_default_key_active,
        opensubtitles_username,
        opensubtitles_password_set,
        api_key: mask_secret(&active_key),
        api_key_override_active,
    })
}

/// Get app-level settings. Secrets are masked.
#[utoipa::path(get, path = "/settings/app", tag = "settings",
    responses((status = 200, body = AppSettings)))]
pub async fn get_app_settings(State(state): State<AppState>) -> AppResult<Json<AppSettings>> {
    Ok(Json(current_app_settings(&state).await?))
}

/// Store `value` for `key`, or DELETE the row when `value` is empty so it
/// reads back as "not set". An explicit empty string in the PUT body is the
/// clear signal (a field omitted from the body is left untouched by the
/// caller); a non-empty value is stored as-is (already trimmed by the caller
/// where trimming is wanted).
async fn upsert_or_clear(state: &AppState, key: &str, value: &str) -> AppResult<()> {
    if value.is_empty() {
        db::settings::delete(&state.db, key).await
    } else {
        db::settings::set(&state.db, key, value).await
    }
}

/// Update app-level settings. A new `api_key` (min. 16 characters) is stored
/// as an override; the bootstrap config key stays valid for recovery.
///
/// TMDB / OpenSubtitles fields follow "omit = unchanged, `\"\"` = clear": an
/// explicit empty string deletes the stored value so it reverts to not-set
/// (and any cached OpenSubtitles login token is dropped).
#[utoipa::path(put, path = "/settings/app", tag = "settings",
    request_body = AppSettingsInput,
    responses(
        (status = 200, body = AppSettings),
        (status = 400, description = "api_key shorter than 16 characters"),
    ))]
pub async fn put_app_settings(
    State(state): State<AppState>,
    Json(input): Json<AppSettingsInput>,
) -> AppResult<Json<AppSettings>> {
    if let Some(key) = &input.api_key {
        let key = key.trim();
        if key.chars().count() < 16 {
            return Err(AppError::BadRequest(
                "api_key must be at least 16 characters".into(),
            ));
        }
        db::settings::set(&state.db, db::settings::API_KEY_OVERRIDE, key).await?;
    }
    if let Some(key) = input.tmdb_api_key {
        upsert_or_clear(&state, db::settings::TMDB_API_KEY, key.trim()).await?;
    }
    let opensubtitles_changed = input.opensubtitles_api_key.is_some()
        || input.opensubtitles_username.is_some()
        || input.opensubtitles_password.is_some();
    if let Some(key) = input.opensubtitles_api_key {
        upsert_or_clear(&state, db::settings::OPENSUBTITLES_API_KEY, key.trim()).await?;
    }
    if let Some(key) = input.subdl_api_key {
        upsert_or_clear(&state, db::settings::SUBDL_API_KEY, key.trim()).await?;
    }
    if let Some(order) = input.subtitle_provider_order {
        let joined = order
            .iter()
            .map(|p| p.trim().to_ascii_lowercase())
            .filter(|p| !p.is_empty())
            .collect::<Vec<_>>()
            .join(",");
        upsert_or_clear(&state, db::settings::SUBTITLE_PROVIDER_ORDER, &joined).await?;
    }
    if let Some(username) = input.opensubtitles_username {
        upsert_or_clear(
            &state,
            db::settings::OPENSUBTITLES_USERNAME,
            username.trim(),
        )
        .await?;
    }
    if let Some(password) = input.opensubtitles_password {
        // Deliberately not trimmed: passwords may legitimately contain spaces.
        upsert_or_clear(&state, db::settings::OPENSUBTITLES_PASSWORD, &password).await?;
    }
    if opensubtitles_changed {
        // A cached login token may belong to the old key/credentials.
        state.opensubtitles_token.set(None).await;
    }
    Ok(Json(current_app_settings(&state).await?))
}

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(get_preferences, put_preferences))
        .routes(routes!(list_indexers, create_indexer))
        .routes(routes!(update_indexer, delete_indexer))
        .routes(routes!(test_indexer))
        .routes(routes!(list_providers, create_provider))
        .routes(routes!(update_provider, delete_provider))
        .routes(routes!(test_provider))
        .routes(routes!(get_app_settings, put_app_settings))
}

#[cfg(test)]
mod tests {
    use super::mask_secret;

    #[test]
    fn masking_shows_only_last_four() {
        assert_eq!(mask_secret("abcdefgh1234"), "****1234");
        assert_eq!(mask_secret("abcd"), "****");
        assert_eq!(mask_secret("ab"), "****");
    }
}
