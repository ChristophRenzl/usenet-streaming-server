//! OpenSubtitles-backed subtitle search.
//!
//! Standalone search endpoint plus the shared client builder reused by the
//! streaming layer to attach subtitles into an HLS session. Subtitles are an
//! optional feature: without a configured OpenSubtitles API key these
//! endpoints return a helpful 400 (mirroring the TMDB "key not configured"
//! message), and the session pipeline degrades gracefully.

use axum::{
    extract::{Query, State},
    Json,
};
use serde::{Deserialize, Serialize};
use utoipa::{IntoParams, ToSchema};
use utoipa_axum::{router::OpenApiRouter, routes};

use crate::{
    db,
    error::{AppError, AppResult},
    state::AppState,
    subtitles::{
        is_token_rejected, OpenSubtitlesClient, SubtitleDownload, SubtitleQuery, SubtitleResult,
    },
    tmdb::models::MediaType,
};

/// Where the effective OpenSubtitles API key comes from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApiKeySource {
    /// A per-user key stored in `app_settings` (takes precedence).
    User,
    /// The operator-supplied config/env default (`subtitles.opensubtitles_default_api_key`).
    Default,
    /// Neither is configured.
    None,
}

/// Resolve the effective OpenSubtitles API key and its source: the per-user
/// `opensubtitles_api_key` from `app_settings` when set, else the operator
/// config default, else `None`. Centralized so search, session auto-attach and
/// the offset/download paths all agree on which key is in effect.
pub async fn effective_opensubtitles_key(
    state: &AppState,
) -> AppResult<(Option<String>, ApiKeySource)> {
    if let Some(key) = db::settings::get(&state.db, db::settings::OPENSUBTITLES_API_KEY)
        .await?
        .filter(|k| !k.is_empty())
    {
        return Ok((Some(key), ApiKeySource::User));
    }
    if let Some(key) = state
        .config
        .subtitles
        .opensubtitles_default_api_key
        .clone()
        .filter(|k| !k.is_empty())
    {
        return Ok((Some(key), ApiKeySource::Default));
    }
    Ok((None, ApiKeySource::None))
}

/// Build an OpenSubtitles client from the effective API key (per-user override,
/// else operator config default), failing with a helpful 400 when neither is
/// configured.
pub async fn opensubtitles_client(state: &AppState) -> AppResult<OpenSubtitlesClient> {
    let key = effective_opensubtitles_key(state).await?.0.ok_or_else(|| {
        AppError::BadRequest(
            "OpenSubtitles API key not configured; set it via PUT /api/v1/settings/app \
             or an operator default (APP_SUBTITLES__OPENSUBTITLES_DEFAULT_API_KEY) \
             (get a free key at https://www.opensubtitles.com/consumers)"
                .into(),
        )
    })?;
    Ok(OpenSubtitlesClient::new(
        state.http.clone(),
        state.opensubtitles_base_url.as_ref(),
        key,
    ))
}

/// The stored OpenSubtitles account credentials, when both are configured.
async fn opensubtitles_credentials(state: &AppState) -> AppResult<Option<(String, String)>> {
    let username = db::settings::get(&state.db, db::settings::OPENSUBTITLES_USERNAME)
        .await?
        .filter(|u| !u.is_empty());
    let password = db::settings::get(&state.db, db::settings::OPENSUBTITLES_PASSWORD)
        .await?
        .filter(|p| !p.is_empty());
    Ok(username.zip(password))
}

/// Acquire a cached OpenSubtitles user token, logging in with the stored
/// credentials when the cache is empty. Returns `None` when no credentials are
/// configured (anonymous download quota then applies — still fine for MVP).
async fn opensubtitles_token(state: &AppState, client: &OpenSubtitlesClient) -> Option<String> {
    if let Some(token) = state.opensubtitles_token.get().await {
        return Some(token);
    }
    let (username, password) = match opensubtitles_credentials(state).await {
        Ok(Some(creds)) => creds,
        Ok(None) => return None,
        Err(error) => {
            tracing::warn!(%error, "reading OpenSubtitles credentials");
            return None;
        }
    };
    match client.login(&username, &password).await {
        Ok(token) => {
            state.opensubtitles_token.set(Some(token.clone())).await;
            Some(token)
        }
        Err(error) => {
            tracing::warn!(%error, "OpenSubtitles login failed; downloading anonymously");
            None
        }
    }
}

/// Download + decode a subtitle by `file_id`, using the cached account token
/// (logging in on demand) for a higher quota. If a cached token is rejected,
/// it is cleared, a fresh login is attempted once, and the download retried.
pub async fn download_subtitle(
    state: &AppState,
    client: &OpenSubtitlesClient,
    file_id: i64,
) -> AppResult<SubtitleDownload> {
    let token = opensubtitles_token(state, client).await;
    match client.download_subtitle(file_id, token.as_deref()).await {
        Ok(download) => Ok(download),
        Err(error) if is_token_rejected(&error) => {
            // Stale cached token: drop it, re-login and retry once.
            state.opensubtitles_token.set(None).await;
            let token = opensubtitles_token(state, client).await;
            client.download_subtitle(file_id, token.as_deref()).await
        }
        Err(error) => Err(error),
    }
}

/// Split a comma list of languages into trimmed, lower-cased ISO codes.
pub fn parse_languages(raw: &str) -> Vec<String> {
    raw.split(',')
        .map(|s| s.trim().to_ascii_lowercase())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Resolve the effective language list: the caller's `languages` when given,
/// else the stored preference language (unless it is `original`), else `en`.
pub async fn effective_languages(
    state: &AppState,
    requested: Option<&str>,
) -> AppResult<Vec<String>> {
    if let Some(raw) = requested {
        let langs = parse_languages(raw);
        if !langs.is_empty() {
            return Ok(langs);
        }
    }
    let pref = db::preferences::get(&state.db).await?.language;
    let pref = pref.trim().to_ascii_lowercase();
    if pref.is_empty() || pref == "original" {
        Ok(vec!["en".to_string()])
    } else {
        Ok(vec![pref])
    }
}

/// Default media type for subtitle search (`movie`); `MediaType` has no
/// `Default` impl of its own.
fn default_media_type() -> MediaType {
    MediaType::Movie
}

#[derive(Debug, Deserialize, IntoParams)]
pub struct SubtitleSearchParams {
    /// TMDB id: the movie id for `movie`, the *show* id for `tv`.
    pub tmdb_id: i64,
    /// `movie` (default) or `tv`.
    #[serde(default = "default_media_type")]
    pub media_type: MediaType,
    /// Season number (required for `tv`).
    pub season: Option<u32>,
    /// Episode number (required for `tv`).
    pub episode: Option<u32>,
    /// Comma-separated ISO 639-1 languages, most-preferred first (e.g.
    /// `en,de`). Defaults to the preference language, else `en`.
    pub languages: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SubtitleSearchResponse {
    /// Ranked subtitle candidates (preferred language, then human, then
    /// popularity).
    pub results: Vec<SubtitleResult>,
    /// The effective languages the search was run with.
    pub languages: Vec<String>,
}

/// Search OpenSubtitles for a movie or episode.
#[utoipa::path(get, path = "/subtitles/search", tag = "subtitles",
    params(SubtitleSearchParams),
    responses(
        (status = 200, body = SubtitleSearchResponse),
        (status = 400, description = "Missing tv season/episode, or OpenSubtitles API key not configured"),
        (status = 502, description = "OpenSubtitles upstream error"),
    ))]
pub async fn search(
    State(state): State<AppState>,
    Query(params): Query<SubtitleSearchParams>,
) -> AppResult<Json<SubtitleSearchResponse>> {
    let (season, episode) = match params.media_type {
        MediaType::Tv => {
            let season = params.season.ok_or_else(|| {
                AppError::BadRequest("season is required for tv subtitle search".into())
            })?;
            let episode = params.episode.ok_or_else(|| {
                AppError::BadRequest("episode is required for tv subtitle search".into())
            })?;
            (Some(season), Some(episode))
        }
        MediaType::Movie => (None, None),
    };

    let languages = effective_languages(&state, params.languages.as_deref()).await?;
    let client = opensubtitles_client(&state).await?;
    let results = client
        .search(&SubtitleQuery {
            tmdb_id: params.tmdb_id,
            season,
            episode,
            languages: languages.clone(),
            // Standalone tmdb-based search: no file, so no moviehash. Hash
            // matching only applies in the session auto-attach path.
            moviehash: None,
        })
        .await?;
    Ok(Json(SubtitleSearchResponse { results, languages }))
}

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new().routes(routes!(search))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::AppConfig;

    #[test]
    fn language_parsing_trims_and_lowercases() {
        assert_eq!(parse_languages(" EN , de ,, Fr "), vec!["en", "de", "fr"]);
        assert!(parse_languages("").is_empty());
    }

    async fn test_state(default_key: Option<&str>) -> AppState {
        let mut config = AppConfig::default();
        config.auth.api_key = "unit-test-key".into();
        config.subtitles.opensubtitles_default_api_key = default_key.map(str::to_string);
        AppState::for_tests(config).await.expect("test state")
    }

    #[tokio::test]
    async fn effective_key_user_wins_over_default() {
        let state = test_state(Some("default-key")).await;
        db::settings::set(&state.db, db::settings::OPENSUBTITLES_API_KEY, "user-key")
            .await
            .unwrap();
        let (key, source) = effective_opensubtitles_key(&state).await.unwrap();
        assert_eq!(key.as_deref(), Some("user-key"));
        assert_eq!(source, ApiKeySource::User);
    }

    #[tokio::test]
    async fn effective_key_falls_back_to_default() {
        let state = test_state(Some("default-key")).await;
        let (key, source) = effective_opensubtitles_key(&state).await.unwrap();
        assert_eq!(key.as_deref(), Some("default-key"));
        assert_eq!(source, ApiKeySource::Default);
    }

    #[tokio::test]
    async fn effective_key_none_when_neither_set() {
        let state = test_state(None).await;
        let (key, source) = effective_opensubtitles_key(&state).await.unwrap();
        assert!(key.is_none());
        assert_eq!(source, ApiKeySource::None);
    }

    #[tokio::test]
    async fn empty_user_key_falls_back_to_default() {
        let state = test_state(Some("default-key")).await;
        db::settings::set(&state.db, db::settings::OPENSUBTITLES_API_KEY, "")
            .await
            .unwrap();
        let (key, source) = effective_opensubtitles_key(&state).await.unwrap();
        assert_eq!(key.as_deref(), Some("default-key"));
        assert_eq!(source, ApiKeySource::Default);
    }

    #[tokio::test]
    async fn client_errors_when_not_configured() {
        let state = test_state(None).await;
        match opensubtitles_client(&state).await {
            Ok(_) => panic!("expected not-configured error"),
            Err(err) => assert!(matches!(err, AppError::BadRequest(_))),
        }
    }
}
