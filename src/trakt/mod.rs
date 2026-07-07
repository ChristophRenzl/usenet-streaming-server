//! Trakt.tv integration: device-code OAuth linking and playback scrobbling.
//!
//! Everything here is best-effort side channel — a Trakt hiccup must never
//! affect playback. The user supplies their own Trakt API app credentials
//! (client id + secret) once; tokens are stored in `app_settings` and
//! refreshed automatically before they expire.
//!
//! Scrobbles address media by TMDB id, which Trakt resolves natively:
//! `{"movie":{"ids":{"tmdb":603}}}` or
//! `{"show":{"ids":{"tmdb":1399}},"episode":{"season":1,"number":1}}`.

use serde::Deserialize;
use serde_json::{json, Value};

use crate::error::{AppError, AppResult};

/// Public Trakt API base; injectable for tests.
pub const DEFAULT_BASE_URL: &str = "https://api.trakt.tv";

/// A scrobble lifecycle event. `Start` when playback begins, `Stop` when it
/// ends — Trakt marks the item watched when the stop progress is ≥ 80%.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrobbleAction {
    Start,
    Stop,
}

impl ScrobbleAction {
    fn as_path(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Stop => "stop",
        }
    }
}

/// What is being scrobbled, addressed by TMDB ids.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ScrobbleItem {
    Movie {
        tmdb_id: i64,
    },
    Episode {
        show_tmdb_id: i64,
        season: u32,
        episode: u32,
    },
}

/// The JSON body for a scrobble call: the media reference plus progress %.
pub fn scrobble_body(item: ScrobbleItem, progress: f64) -> Value {
    let progress = progress.clamp(0.0, 100.0);
    match item {
        ScrobbleItem::Movie { tmdb_id } => json!({
            "movie": { "ids": { "tmdb": tmdb_id } },
            "progress": progress,
        }),
        ScrobbleItem::Episode {
            show_tmdb_id,
            season,
            episode,
        } => json!({
            "show": { "ids": { "tmdb": show_tmdb_id } },
            "episode": { "season": season, "number": episode },
            "progress": progress,
        }),
    }
}

/// Playback progress in percent from a position and an optional duration;
/// 0 when the duration is unknown (Trakt then just records activity).
pub fn progress_percent(position_secs: f64, duration_secs: Option<f64>) -> f64 {
    match duration_secs {
        Some(duration) if duration > 0.0 && position_secs.is_finite() => {
            (position_secs / duration * 100.0).clamp(0.0, 100.0)
        }
        _ => 0.0,
    }
}

/// Device-code flow bootstrap: show `user_code` + `verification_url` to the
/// user, then poll with `device_code` every `interval` seconds.
#[derive(Debug, Clone, Deserialize)]
pub struct DeviceCode {
    pub device_code: String,
    pub user_code: String,
    pub verification_url: String,
    pub expires_in: u64,
    pub interval: u64,
}

/// Result of one device-token poll.
#[derive(Debug, Clone)]
pub enum DevicePoll {
    Linked(Tokens),
    /// User hasn't approved yet — keep polling.
    Pending,
    /// Polling too fast — back off.
    SlowDown,
    Denied,
    Expired,
}

/// An issued token pair with its absolute expiry (unix seconds).
#[derive(Debug, Clone)]
pub struct Tokens {
    pub access_token: String,
    pub refresh_token: String,
    pub expires_at: i64,
}

#[derive(Debug, Deserialize)]
struct RawTokens {
    access_token: String,
    refresh_token: String,
    expires_in: i64,
    created_at: i64,
}

impl From<RawTokens> for Tokens {
    fn from(raw: RawTokens) -> Self {
        Tokens {
            access_token: raw.access_token,
            refresh_token: raw.refresh_token,
            expires_at: raw.created_at + raw.expires_in,
        }
    }
}

pub struct TraktClient {
    http: reqwest::Client,
    base_url: String,
    client_id: String,
    client_secret: String,
}

impl TraktClient {
    pub fn new(
        http: reqwest::Client,
        base_url: impl Into<String>,
        client_id: impl Into<String>,
        client_secret: impl Into<String>,
    ) -> Self {
        Self {
            http,
            base_url: base_url.into().trim_end_matches('/').to_string(),
            client_id: client_id.into(),
            client_secret: client_secret.into(),
        }
    }

    /// Begin the device-code flow.
    pub async fn device_code(&self) -> AppResult<DeviceCode> {
        let response = self
            .http
            .post(format!("{}/oauth/device/code", self.base_url))
            .json(&json!({ "client_id": self.client_id }))
            .send()
            .await
            .map_err(|e| {
                AppError::Upstream(format!("Trakt request failed: {}", e.without_url()))
            })?;
        if !response.status().is_success() {
            return Err(AppError::Upstream(format!(
                "Trakt device code failed: HTTP {} (check the client id)",
                response.status()
            )));
        }
        response
            .json()
            .await
            .map_err(|e| AppError::Upstream(format!("Trakt decode failed: {}", e.without_url())))
    }

    /// One poll of the device-token endpoint. Trakt signals the flow state
    /// through status codes; only transport errors are hard failures.
    pub async fn poll_device_token(&self, device_code: &str) -> AppResult<DevicePoll> {
        let response = self
            .http
            .post(format!("{}/oauth/device/token", self.base_url))
            .json(&json!({
                "code": device_code,
                "client_id": self.client_id,
                "client_secret": self.client_secret,
            }))
            .send()
            .await
            .map_err(|e| {
                AppError::Upstream(format!("Trakt request failed: {}", e.without_url()))
            })?;
        match response.status().as_u16() {
            200 => {
                let raw: RawTokens = response.json().await.map_err(|e| {
                    AppError::Upstream(format!("Trakt decode failed: {}", e.without_url()))
                })?;
                Ok(DevicePoll::Linked(raw.into()))
            }
            400 | 409 => Ok(DevicePoll::Pending),
            429 => Ok(DevicePoll::SlowDown),
            404 | 410 => Ok(DevicePoll::Expired),
            418 => Ok(DevicePoll::Denied),
            status => Err(AppError::Upstream(format!(
                "Trakt device token poll failed: HTTP {status}"
            ))),
        }
    }

    /// Exchange a refresh token for a fresh pair.
    pub async fn refresh(&self, refresh_token: &str) -> AppResult<Tokens> {
        let response = self
            .http
            .post(format!("{}/oauth/token", self.base_url))
            .json(&json!({
                "refresh_token": refresh_token,
                "client_id": self.client_id,
                "client_secret": self.client_secret,
                "redirect_uri": "urn:ietf:wg:oauth:2.0:oob",
                "grant_type": "refresh_token",
            }))
            .send()
            .await
            .map_err(|e| {
                AppError::Upstream(format!("Trakt request failed: {}", e.without_url()))
            })?;
        if !response.status().is_success() {
            return Err(AppError::Upstream(format!(
                "Trakt token refresh failed: HTTP {}",
                response.status()
            )));
        }
        let raw: RawTokens = response
            .json()
            .await
            .map_err(|e| AppError::Upstream(format!("Trakt decode failed: {}", e.without_url())))?;
        Ok(raw.into())
    }

    /// Fire one scrobble event.
    pub async fn scrobble(
        &self,
        action: ScrobbleAction,
        item: ScrobbleItem,
        progress: f64,
        access_token: &str,
    ) -> AppResult<()> {
        let response = self
            .http
            .post(format!("{}/scrobble/{}", self.base_url, action.as_path()))
            .bearer_auth(access_token)
            .header("trakt-api-version", "2")
            .header("trakt-api-key", self.client_id.clone())
            .json(&scrobble_body(item, progress))
            .send()
            .await
            .map_err(|e| {
                AppError::Upstream(format!("Trakt request failed: {}", e.without_url()))
            })?;
        if !response.status().is_success() {
            return Err(AppError::Upstream(format!(
                "Trakt scrobble {} failed: HTTP {}",
                action.as_path(),
                response.status()
            )));
        }
        Ok(())
    }

    /// Add or remove one item from the Trakt watch history — how manual
    /// watched/unwatched marks propagate (the Jellyfin-plugin behavior).
    pub async fn history_write(
        &self,
        item: ScrobbleItem,
        add: bool,
        access_token: &str,
    ) -> AppResult<()> {
        let path = if add {
            "/sync/history"
        } else {
            "/sync/history/remove"
        };
        let response = self
            .http
            .post(format!("{}{path}", self.base_url))
            .bearer_auth(access_token)
            .header("trakt-api-version", "2")
            .header("trakt-api-key", self.client_id.clone())
            .json(&history_body(item))
            .send()
            .await
            .map_err(|e| {
                AppError::Upstream(format!("Trakt request failed: {}", e.without_url()))
            })?;
        if !response.status().is_success() {
            return Err(AppError::Upstream(format!(
                "Trakt history write failed: HTTP {}",
                response.status()
            )));
        }
        Ok(())
    }

    /// Everything the account has watched: movies with their last-watched
    /// timestamps.
    pub async fn watched_movies(&self, access_token: &str) -> AppResult<Vec<WatchedMovie>> {
        let raw: Vec<RawWatchedMovie> = self
            .get_synced("/sync/watched/movies", access_token)
            .await?;
        Ok(raw
            .into_iter()
            .filter_map(|m| {
                Some(WatchedMovie {
                    tmdb_id: m.movie.ids.tmdb?,
                    last_watched_at: m.last_watched_at,
                })
            })
            .collect())
    }

    /// Everything the account has watched: shows with per-episode
    /// last-watched timestamps.
    pub async fn watched_shows(&self, access_token: &str) -> AppResult<Vec<WatchedShow>> {
        let raw: Vec<RawWatchedShow> = self.get_synced("/sync/watched/shows", access_token).await?;
        Ok(raw
            .into_iter()
            .filter_map(|s| {
                Some(WatchedShow {
                    tmdb_id: s.show.ids.tmdb?,
                    seasons: s
                        .seasons
                        .into_iter()
                        .map(|season| WatchedSeason {
                            number: season.number,
                            episodes: season
                                .episodes
                                .into_iter()
                                .map(|e| WatchedEpisode {
                                    number: e.number,
                                    last_watched_at: e.last_watched_at,
                                })
                                .collect(),
                        })
                        .collect(),
                })
            })
            .collect())
    }

    async fn get_synced<T: serde::de::DeserializeOwned>(
        &self,
        path: &str,
        access_token: &str,
    ) -> AppResult<T> {
        let response = self
            .http
            .get(format!("{}{path}", self.base_url))
            .bearer_auth(access_token)
            .header("trakt-api-version", "2")
            .header("trakt-api-key", self.client_id.clone())
            .send()
            .await
            .map_err(|e| {
                AppError::Upstream(format!("Trakt request failed: {}", e.without_url()))
            })?;
        if !response.status().is_success() {
            return Err(AppError::Upstream(format!(
                "Trakt {path} failed: HTTP {}",
                response.status()
            )));
        }
        response
            .json()
            .await
            .map_err(|e| AppError::Upstream(format!("Trakt decode failed: {}", e.without_url())))
    }
}

/// The JSON body for a history add/remove of one item.
pub fn history_body(item: ScrobbleItem) -> Value {
    match item {
        ScrobbleItem::Movie { tmdb_id } => json!({
            "movies": [{ "ids": { "tmdb": tmdb_id } }],
        }),
        ScrobbleItem::Episode {
            show_tmdb_id,
            season,
            episode,
        } => json!({
            "shows": [{
                "ids": { "tmdb": show_tmdb_id },
                "seasons": [{
                    "number": season,
                    "episodes": [{ "number": episode }],
                }],
            }],
        }),
    }
}

/// One watched movie from `/sync/watched/movies`.
#[derive(Debug, Clone)]
pub struct WatchedMovie {
    pub tmdb_id: i64,
    pub last_watched_at: Option<String>,
}

/// One watched show from `/sync/watched/shows`.
#[derive(Debug, Clone)]
pub struct WatchedShow {
    pub tmdb_id: i64,
    pub seasons: Vec<WatchedSeason>,
}

#[derive(Debug, Clone)]
pub struct WatchedSeason {
    pub number: u32,
    pub episodes: Vec<WatchedEpisode>,
}

#[derive(Debug, Clone)]
pub struct WatchedEpisode {
    pub number: u32,
    pub last_watched_at: Option<String>,
}

#[derive(Debug, Deserialize)]
struct RawIds {
    tmdb: Option<i64>,
}

#[derive(Debug, Deserialize)]
struct RawWatchedMovie {
    last_watched_at: Option<String>,
    movie: RawMovieRef,
}

#[derive(Debug, Deserialize)]
struct RawMovieRef {
    ids: RawIds,
}

#[derive(Debug, Deserialize)]
struct RawWatchedShow {
    show: RawShowRef,
    #[serde(default)]
    seasons: Vec<RawWatchedSeason>,
}

#[derive(Debug, Deserialize)]
struct RawShowRef {
    ids: RawIds,
}

#[derive(Debug, Deserialize)]
struct RawWatchedSeason {
    number: u32,
    #[serde(default)]
    episodes: Vec<RawWatchedEpisode>,
}

#[derive(Debug, Deserialize)]
struct RawWatchedEpisode {
    number: u32,
    last_watched_at: Option<String>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn movie_scrobble_body_uses_tmdb_id() {
        let body = scrobble_body(ScrobbleItem::Movie { tmdb_id: 603 }, 42.5);
        assert_eq!(body["movie"]["ids"]["tmdb"], 603);
        assert_eq!(body["progress"], 42.5);
        assert!(body.get("show").is_none());
    }

    #[test]
    fn episode_scrobble_body_carries_show_and_numbers() {
        let body = scrobble_body(
            ScrobbleItem::Episode {
                show_tmdb_id: 1399,
                season: 2,
                episode: 5,
            },
            99.0,
        );
        assert_eq!(body["show"]["ids"]["tmdb"], 1399);
        assert_eq!(body["episode"]["season"], 2);
        assert_eq!(body["episode"]["number"], 5);
    }

    #[test]
    fn scrobble_progress_is_clamped() {
        let body = scrobble_body(ScrobbleItem::Movie { tmdb_id: 1 }, 140.0);
        assert_eq!(body["progress"], 100.0);
        let body = scrobble_body(ScrobbleItem::Movie { tmdb_id: 1 }, -3.0);
        assert_eq!(body["progress"], 0.0);
    }

    #[test]
    fn progress_percent_handles_unknown_duration() {
        assert_eq!(progress_percent(600.0, Some(1200.0)), 50.0);
        assert_eq!(progress_percent(600.0, None), 0.0);
        assert_eq!(progress_percent(600.0, Some(0.0)), 0.0);
        assert_eq!(progress_percent(2400.0, Some(1200.0)), 100.0);
    }

    #[test]
    fn token_expiry_is_absolute() {
        let tokens: Tokens = RawTokens {
            access_token: "a".into(),
            refresh_token: "r".into(),
            expires_in: 7200,
            created_at: 1_000_000,
        }
        .into();
        assert_eq!(tokens.expires_at, 1_007_200);
    }
}
