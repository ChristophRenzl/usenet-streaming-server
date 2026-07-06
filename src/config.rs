use figment::{
    providers::{Env, Format, Serialized, Toml},
    Figment,
};
use serde::{Deserialize, Serialize};

/// Bootstrap configuration loaded from a TOML file with `APP_*` environment
/// variable overrides (e.g. `APP_SERVER__PORT=9090`). Everything mutable via
/// the API (providers, indexers, TMDB key, ...) lives in SQLite instead.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AppConfig {
    pub server: ServerConfig,
    pub auth: AuthConfig,
    pub database: DatabaseConfig,
    pub storage: StorageConfig,
    pub cache: CacheConfig,
    pub streaming: StreamingConfig,
    #[serde(default)]
    pub subtitles: SubtitlesConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub host: String,
    pub port: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct AuthConfig {
    /// Single API key required in the `X-Api-Key` header (or `?apikey=`).
    pub api_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DatabaseConfig {
    pub path: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StorageConfig {
    /// Directory for server-side downloads.
    pub download_dir: String,
    /// Directory for per-session HLS output (defaults to the OS temp dir).
    pub session_dir: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct CacheConfig {
    /// Upper bound for the in-memory decoded segment cache.
    pub memory_bytes: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct StreamingConfig {
    /// Seconds a playback session may be idle before teardown.
    pub session_idle_timeout_secs: u64,
    /// Segments to prefetch ahead of a sequential reader.
    pub readahead_segments: usize,
    pub ffmpeg_path: String,
    pub ffprobe_path: String,
    /// `par2` binary used by the download-and-repair fallback (par2cmdline).
    pub par2_path: String,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SubtitlesConfig {
    /// Operator-supplied default OpenSubtitles consumer API key, applied when no
    /// per-user key is stored in `app_settings`. Lets an operator configure the
    /// key once at deploy time (e.g. `APP_SUBTITLES__OPENSUBTITLES_DEFAULT_API_KEY`)
    /// so users only ever manage their OpenSubtitles username/password. Default
    /// `None`; never bundled — get a key at https://www.opensubtitles.com/consumers.
    #[serde(default)]
    pub opensubtitles_default_api_key: Option<String>,
}

impl Default for AppConfig {
    fn default() -> Self {
        Self {
            server: ServerConfig {
                host: "0.0.0.0".into(),
                port: 8080,
            },
            auth: AuthConfig {
                api_key: String::new(),
            },
            database: DatabaseConfig {
                path: "data/usenet-streamer.db".into(),
            },
            storage: StorageConfig {
                download_dir: "data/downloads".into(),
                session_dir: None,
            },
            cache: CacheConfig {
                memory_bytes: 512 * 1024 * 1024,
            },
            streaming: StreamingConfig {
                session_idle_timeout_secs: 120,
                readahead_segments: 16,
                ffmpeg_path: "ffmpeg".into(),
                ffprobe_path: "ffprobe".into(),
                par2_path: "par2".into(),
            },
            subtitles: SubtitlesConfig::default(),
        }
    }
}

impl AppConfig {
    /// Layering: defaults < TOML file < `APP_*` env vars.
    pub fn load(path: &str) -> anyhow::Result<Self> {
        let config: Self = Figment::from(Serialized::defaults(Self::default()))
            .merge(Toml::file(path))
            .merge(Env::prefixed("APP_").split("__"))
            .extract()?;
        if config.auth.api_key.is_empty() {
            anyhow::bail!(
                "auth.api_key must be set (in {path} or via APP_AUTH__API_KEY); \
                 generate one with e.g. `openssl rand -hex 24`"
            );
        }
        Ok(config)
    }
}

#[cfg(test)]
// figment::Jail's closure returns figment's own large error type.
#[allow(clippy::result_large_err)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_sane() {
        let c = AppConfig::default();
        assert_eq!(c.server.port, 8080);
        assert_eq!(c.streaming.session_idle_timeout_secs, 120);
    }

    #[test]
    fn env_overrides_file() {
        figment::Jail::expect_with(|jail| {
            jail.create_file(
                "config.toml",
                r#"
                [server]
                port = 9000
                [auth]
                api_key = "from-file"
                "#,
            )?;
            jail.set_env("APP_SERVER__PORT", "9001");
            let c = AppConfig::load("config.toml").expect("load");
            assert_eq!(c.server.port, 9001);
            assert_eq!(c.auth.api_key, "from-file");
            Ok(())
        });
    }

    #[test]
    fn opensubtitles_default_key_defaults_to_none() {
        let c = AppConfig::default();
        assert!(c.subtitles.opensubtitles_default_api_key.is_none());
    }

    #[test]
    fn opensubtitles_default_key_from_env() {
        figment::Jail::expect_with(|jail| {
            jail.create_file("config.toml", "[auth]\napi_key = \"from-file\"\n")?;
            jail.set_env("APP_SUBTITLES__OPENSUBTITLES_DEFAULT_API_KEY", "deploy-key");
            let c = AppConfig::load("config.toml").expect("load");
            assert_eq!(
                c.subtitles.opensubtitles_default_api_key.as_deref(),
                Some("deploy-key")
            );
            Ok(())
        });
    }

    #[test]
    fn missing_api_key_is_rejected() {
        figment::Jail::expect_with(|jail| {
            jail.create_file("config.toml", "[server]\nport = 9000\n")?;
            assert!(AppConfig::load("config.toml").is_err());
            Ok(())
        });
    }
}
