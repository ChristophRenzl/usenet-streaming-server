//! Playback sessions and HLS delivery: session creation (release resolution
//! → NZB → virtual file → ffprobe → ffmpeg), playlists, fMP4 segments, raw
//! byte-range access, seeking and teardown.

use std::path::Path as FsPath;
use std::sync::{Arc, LazyLock};

use axum::extract::{Path, Query, State};
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use regex::Regex;
use serde::{Deserialize, Serialize};
use utoipa::ToSchema;
use utoipa_axum::{router::OpenApiRouter, routes};
use uuid::Uuid;

use crate::{
    db,
    error::{AppError, AppResult},
    indexer::{client::NewznabClient, RawRelease},
    nzb::{health_check, main_content_segments, parse_nzb, select_main, MainContent, Nzb},
    release::{
        parse::{parse_release_name, Resolution},
        rank::RankedRelease,
    },
    state::AppState,
    stream::{
        ffmpeg::{self, SpawnOptions},
        ffprobe, open_media_source, range,
        session::{NewSession, Session, SessionState},
        MediaInfo,
    },
    tmdb::models::MediaType,
    vfs::DiskFile,
};

use super::releases::{pick_candidates, resolve_candidates, ReleaseTarget};

/// Maximum release candidates tried before giving up.
pub(crate) const MAX_ATTEMPTS: usize = 5;
/// Segments STATed per candidate during the pre-flight health check.
const HEALTH_SAMPLE: usize = 10;

const PLAYLIST_CONTENT_TYPE: &str = "application/vnd.apple.mpegurl";

static SEGMENT_NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(init\.mp4|seg_\d+\.m4s)$").expect("segment regex"));

// ---- Session creation -------------------------------------------------------

#[derive(Debug, Deserialize, ToSchema)]
pub struct CreateSessionRequest {
    /// TMDB id of the movie or show (required unless `download_id` is set).
    pub tmdb_id: Option<i64>,
    /// `movie` or `tv` (required unless `download_id` is set).
    pub media_type: Option<MediaType>,
    /// Season number (required for `tv`).
    pub season: Option<u32>,
    /// Episode number (required for `tv`).
    pub episode: Option<u32>,
    /// Pin a specific release by its indexer guid instead of automatic
    /// candidate selection.
    pub release_guid: Option<String>,
    /// Play a specific finished download from disk.
    pub download_id: Option<Uuid>,
    /// Skip the completed-download shortcut and always stream from Usenet.
    #[serde(default)]
    pub force_nntp: bool,
    /// Device capability cap (`480p`, `720p`, `1080p`, `2160p`): releases
    /// above the lower of this and the stored preference max are rejected,
    /// and the best supported resolution ranks first.
    pub max_resolution: Option<Resolution>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct CreateSessionResponse {
    pub session_id: Uuid,
    /// HLS entry point (append `?apikey=` for header-less players).
    pub hls_master_url: String,
    /// Byte-range access to the untouched media file.
    pub raw_url: String,
    pub duration_secs: Option<f64>,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    /// True when the audio is transcoded to AAC for HLS.
    pub audio_transcoded: bool,
    /// Container extension of the media file (`mkv`, `mp4`, ...).
    pub container: String,
    pub chosen_release: RankedRelease,
    /// The full ranked candidate list the choice was made from (empty for
    /// disk playback of a finished download).
    pub candidates: Vec<RankedRelease>,
    /// Stored watch position to offer "resume from here", when any.
    pub resume_position_secs: Option<f64>,
    /// Where the media bytes come from: `disk` (finished download) or `nntp`.
    pub source: String,
}

/// Start a playback session. Completed downloads are played straight from
/// disk (unless `force_nntp`); otherwise releases are resolved and the
/// first healthy streamable candidate is probed and remuxed.
#[utoipa::path(post, path = "/stream/sessions", tag = "streaming",
    request_body = CreateSessionRequest,
    responses(
        (status = 200, body = CreateSessionResponse),
        (status = 400, description = "Bad parameters, missing indexers or TMDB key"),
        (status = 404, description = "Unknown TMDB id, release_guid or download_id"),
        (status = 422, description = "No streamable release found; details list per-candidate reasons"),
    ))]
pub async fn create_session(
    State(state): State<AppState>,
    Json(request): Json<CreateSessionRequest>,
) -> AppResult<Json<CreateSessionResponse>> {
    // Direct playback of one specific finished download.
    if let Some(download_id) = request.download_id {
        let download = db::downloads::get(&state.db, &download_id.to_string())
            .await?
            .ok_or_else(|| AppError::NotFound(format!("download {download_id}")))?;
        return start_disk_session(&state, &download).await.map(Json);
    }

    let (tmdb_id, media_type) = request.tmdb_id.zip(request.media_type).ok_or_else(|| {
        AppError::BadRequest("tmdb_id and media_type are required (or pass download_id)".into())
    })?;
    let target = ReleaseTarget {
        tmdb_id,
        media_type,
        season: request.season,
        episode: request.episode,
    }
    .validated()?;

    // A completed download of this exact item plays from disk — no indexer
    // or NNTP provider needed.
    if !request.force_nntp {
        for download in db::downloads::completed_for_item(
            &state.db,
            target.tmdb_id,
            target.media_type.as_str(),
            target.season,
            target.episode,
        )
        .await?
        {
            let Some(path) = download.file_path.as_deref() else {
                continue;
            };
            if tokio::fs::try_exists(path).await.unwrap_or(false) {
                return start_disk_session(&state, &download).await.map(Json);
            }
        }
    }

    let candidates = resolve_candidates(&state, &target, request.max_resolution).await?;
    let to_try = pick_candidates(&candidates, request.release_guid.as_deref(), MAX_ATTEMPTS)?;

    let mut failures: Vec<String> = Vec::new();
    for candidate in to_try {
        match start_session(&state, &target, &candidate).await {
            Ok(session) => {
                return Ok(Json(session_response(
                    &session, candidate, candidates, "nntp",
                )));
            }
            Err(error) => {
                tracing::warn!(release = %candidate.raw.title, %error, "candidate failed, trying next");
                failures.push(format!("{}: {error}", candidate.raw.title));
            }
        }
    }
    Err(AppError::NoRelease(failures.join("; ")))
}

fn session_response(
    session: &Arc<Session>,
    chosen_release: RankedRelease,
    candidates: Vec<RankedRelease>,
    source: &str,
) -> CreateSessionResponse {
    let info = session.info();
    CreateSessionResponse {
        session_id: session.id,
        hls_master_url: format!("/api/v1/stream/{}/master.m3u8", session.id),
        raw_url: format!("/api/v1/stream/{}/raw", session.id),
        duration_secs: info.duration_secs,
        video_codec: info.video_codec,
        audio_codec: info.audio_codec,
        audio_transcoded: info.audio_transcoded,
        container: session.container.clone(),
        chosen_release,
        candidates,
        resume_position_secs: (session.resume_position_secs > 0.0)
            .then_some(session.resume_position_secs),
        source: source.to_string(),
    }
}

/// Grab a candidate's NZB, parse it, pick the main content and run the
/// pre-flight health check. Shared by session creation and download jobs.
pub(crate) async fn fetch_healthy_release(
    state: &AppState,
    candidate: &RankedRelease,
) -> AppResult<(Nzb, MainContent)> {
    let indexer = db::indexers::get(&state.db, candidate.raw.indexer_id)
        .await?
        .ok_or_else(|| {
            AppError::Upstream(format!(
                "indexer {} no longer configured",
                candidate.raw.indexer_id
            ))
        })?;
    let nzb_bytes = NewznabClient::new(state.http.clone(), indexer)
        .grab(&candidate.raw.nzb_url)
        .await?;
    let nzb = parse_nzb(&String::from_utf8_lossy(&nzb_bytes))?;
    let main = select_main(&nzb)?;

    let segments = main_content_segments(&nzb, &main);
    let health = health_check(&segments, &state.nntp_pool, HEALTH_SAMPLE).await?;
    if !health.ok {
        return Err(AppError::NoRelease(format!(
            "health check failed ({}/{} sampled segments missing)",
            health.missing, health.checked
        )));
    }
    Ok((nzb, main))
}

/// Try to start a session from one candidate: NZB grab → parse → health
/// check → virtual file → ffprobe → ffmpeg. On failure the partially
/// registered session is torn down.
async fn start_session(
    state: &AppState,
    target: &ReleaseTarget,
    candidate: &RankedRelease,
) -> AppResult<Arc<Session>> {
    let (nzb, main) = fetch_healthy_release(state, candidate).await?;

    let source = open_media_source(
        &nzb,
        &main,
        &state.nntp_pool,
        &state.segment_cache,
        state.config.streaming.readahead_segments,
    )
    .await?;

    let resume_position_secs = db::watch_history::position_secs(
        &state.db,
        target.tmdb_id,
        target.media_type.as_str(),
        target.season,
        target.episode,
    )
    .await?
    .unwrap_or(0.0);

    let session = Session::create(
        NewSession {
            media: source.file,
            tmdb_id: target.tmdb_id,
            media_type: target.media_type,
            season: target.season,
            episode: target.episode,
            release_title: candidate.raw.title.clone(),
            inner_file_name: source.inner_file_name,
            resume_position_secs,
        },
        state.config.storage.session_dir.as_deref(),
    )
    .await?;
    state.sessions.insert(session.clone());

    // From here on, clean up the registered session on failure.
    match probe_and_spawn(state, &session).await {
        Ok(()) => {}
        Err(error) => {
            state.sessions.teardown(&session.id).await;
            return Err(error);
        }
    }

    db::watch_history::record_session_start(
        &state.db,
        &db::watch_history::SessionStart {
            tmdb_id: target.tmdb_id,
            media_type: target.media_type.as_str(),
            season: target.season,
            episode: target.episode,
            release_title: &candidate.raw.title,
            indexer_id: Some(candidate.raw.indexer_id),
            nzb_url: &candidate.raw.nzb_url,
            duration_secs: session.info().duration_secs,
        },
    )
    .await?;

    Ok(session)
}

/// Play a finished download from disk: no indexers, no NNTP — a
/// [`DiskFile`] over the stored path feeds the usual probe/HLS pipeline.
async fn start_disk_session(
    state: &AppState,
    download: &db::downloads::Download,
) -> AppResult<CreateSessionResponse> {
    if download.status != "complete" {
        return Err(AppError::BadRequest(format!(
            "download {} is not playable (status: {})",
            download.id, download.status
        )));
    }
    let path = download.file_path.as_deref().ok_or_else(|| {
        AppError::Internal(anyhow::anyhow!(
            "complete download {} has no file path",
            download.id
        ))
    })?;
    if !tokio::fs::try_exists(path).await.unwrap_or(false) {
        return Err(AppError::NotFound(format!(
            "downloaded file for {} (it may have been deleted)",
            download.id
        )));
    }

    let media_type = match download.media_type.as_str() {
        "tv" => MediaType::Tv,
        _ => MediaType::Movie,
    };
    let season = download.season.map(|s| s as u32);
    let episode = download.episode.map(|e| e as u32);

    let media = DiskFile::open(path).await?;
    let inner_file_name = FsPath::new(path)
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| download.release_title.clone());

    let resume_position_secs = db::watch_history::position_secs(
        &state.db,
        download.tmdb_id,
        media_type.as_str(),
        season,
        episode,
    )
    .await?
    .unwrap_or(0.0);

    let session = Session::create(
        NewSession {
            media: Arc::new(media),
            tmdb_id: download.tmdb_id,
            media_type,
            season,
            episode,
            release_title: download.release_title.clone(),
            inner_file_name,
            resume_position_secs,
        },
        state.config.storage.session_dir.as_deref(),
    )
    .await?;
    state.sessions.insert(session.clone());

    match probe_and_spawn(state, &session).await {
        Ok(()) => {}
        Err(error) => {
            state.sessions.teardown(&session.id).await;
            return Err(error);
        }
    }

    db::watch_history::record_session_start(
        &state.db,
        &db::watch_history::SessionStart {
            tmdb_id: download.tmdb_id,
            media_type: media_type.as_str(),
            season,
            episode,
            release_title: &download.release_title,
            indexer_id: None,
            nzb_url: &download.nzb_url,
            duration_secs: session.info().duration_secs,
        },
    )
    .await?;

    Ok(session_response(
        &session,
        synthesized_release(download),
        Vec::new(),
        "disk",
    ))
}

/// A `RankedRelease` stand-in for the session response when playing a
/// finished download (there was no live indexer search).
fn synthesized_release(download: &db::downloads::Download) -> RankedRelease {
    let raw = RawRelease {
        title: download.release_title.clone(),
        guid: format!("download:{}", download.id),
        nzb_url: download.nzb_url.clone(),
        size_bytes: download.total_bytes,
        posted_at: None,
        indexer_id: 0,
        indexer_name: "local download".into(),
    };
    let parsed = parse_release_name(&raw.title);
    RankedRelease {
        raw,
        parsed,
        score: 0,
        rejected: None,
    }
}

fn loopback_url(state: &AppState, session: &Session) -> String {
    format!(
        "{}/internal/vfs/{}?token={}",
        state.loopback_base, session.id, session.token
    )
}

async fn probe_and_spawn(state: &AppState, session: &Arc<Session>) -> AppResult<()> {
    let url = loopback_url(state, session);
    let probe = ffprobe::probe_url(&state.config.streaming.ffprobe_path, &url).await?;
    let audio_transcoded = ffmpeg::should_transcode_audio(probe.audio_codec.as_deref());
    session.set_info(MediaInfo {
        duration_secs: probe.duration_secs,
        video_codec: probe.video_codec,
        audio_codec: probe.audio_codec,
        audio_transcoded,
    });
    ffmpeg::spawn_hls(
        session,
        SpawnOptions {
            ffmpeg_path: &state.config.streaming.ffmpeg_path,
            input_url: &url,
            start_secs: 0.0,
            transcode_audio: audio_transcoded,
        },
    )
    .await
}

// ---- Session status / teardown ----------------------------------------------

#[derive(Debug, Serialize, ToSchema)]
pub struct SessionStatus {
    pub session_id: Uuid,
    /// `starting`, `ready`, `failed` or `ended`.
    pub state: String,
    /// ffmpeg stderr tail / failure reason when `state == "failed"`.
    pub error: Option<String>,
    pub duration_secs: Option<f64>,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    pub audio_transcoded: bool,
    pub container: String,
    pub release_title: String,
    pub inner_file_name: String,
    /// Number of finished HLS media segments on disk.
    pub segments_ready: usize,
    pub resume_position_secs: Option<f64>,
}

fn get_session(state: &AppState, id: &Uuid) -> AppResult<Arc<Session>> {
    state
        .sessions
        .get(id)
        .ok_or_else(|| AppError::NotFound(format!("session {id}")))
}

/// Current status of a playback session.
#[utoipa::path(get, path = "/stream/{session_id}", tag = "streaming",
    params(("session_id" = Uuid, Path, description = "Session id")),
    responses((status = 200, body = SessionStatus), (status = 404)))]
pub async fn session_status(
    State(state): State<AppState>,
    Path(session_id): Path<Uuid>,
) -> AppResult<Json<SessionStatus>> {
    let session = get_session(&state, &session_id)?;
    let info = session.info();
    let (state_label, error) = match session.state() {
        SessionState::Failed(message) => ("failed".to_string(), Some(message)),
        other => (other.label().to_string(), None),
    };
    Ok(Json(SessionStatus {
        session_id,
        state: state_label,
        error,
        duration_secs: info.duration_secs,
        video_codec: info.video_codec,
        audio_codec: info.audio_codec,
        audio_transcoded: info.audio_transcoded,
        container: session.container.clone(),
        release_title: session.release_title.clone(),
        inner_file_name: session.inner_file_name.clone(),
        segments_ready: count_segments(&session.temp_dir).await,
        resume_position_secs: (session.resume_position_secs > 0.0)
            .then_some(session.resume_position_secs),
    }))
}

async fn count_segments(dir: &FsPath) -> usize {
    let Ok(mut entries) = tokio::fs::read_dir(dir).await else {
        return 0;
    };
    let mut count = 0;
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name();
        let Some(name) = name.to_str() else { continue };
        if name.starts_with("seg_") && name.ends_with(".m4s") {
            count += 1;
        }
    }
    count
}

/// Tear a session down: stop ffmpeg, delete its temp files, free its
/// Usenet connections.
#[utoipa::path(delete, path = "/stream/{session_id}", tag = "streaming",
    params(("session_id" = Uuid, Path, description = "Session id")),
    responses((status = 204), (status = 404)))]
pub async fn delete_session(
    State(state): State<AppState>,
    Path(session_id): Path<Uuid>,
) -> AppResult<StatusCode> {
    if state.sessions.teardown(&session_id).await {
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AppError::NotFound(format!("session {session_id}")))
    }
}

// ---- HLS playlists and segments ----------------------------------------------

/// `?apikey=` propagation. Header-less players (AVPlayer) authenticate via
/// the query parameter, but they resolve child playlist/segment URIs
/// relative to the parent URL — which drops the query string (RFC 3986).
/// Since every /api/v1 route requires the key, the playlists must re-embed
/// the presented key into every URI they reference, or each follow-up
/// request 401s and playback is a black screen.
#[derive(Debug, Deserialize)]
pub struct ApiKeyParam {
    apikey: Option<String>,
}

impl ApiKeyParam {
    /// `?apikey=<encoded>` when the request authenticated by query, else "".
    /// Header-authenticated clients keep sending the header themselves.
    fn uri_suffix(&self) -> String {
        match &self.apikey {
            Some(key) => format!("?apikey={}", percent_encode_component(key)),
            None => String::new(),
        }
    }
}

/// RFC 3986 percent-encoding of a query component (unreserved kept as-is).
fn percent_encode_component(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                (b as char).to_string()
            }
            _ => format!("%{b:02X}"),
        })
        .collect()
}

static MAP_URI: LazyLock<Regex> = LazyLock::new(|| Regex::new(r#"URI="([^"]*)""#).unwrap());

/// Append `suffix` to every URI in an HLS playlist: plain segment lines and
/// `URI="..."` attributes (EXT-X-MAP).
fn playlist_with_suffix(playlist: &str, suffix: &str) -> String {
    if suffix.is_empty() {
        return playlist.to_string();
    }
    let mut out = String::with_capacity(playlist.len() + 64);
    for line in playlist.lines() {
        if line.contains("URI=\"") {
            out.push_str(&MAP_URI.replace_all(line, |caps: &regex::Captures| {
                format!("URI=\"{}{}\"", &caps[1], suffix)
            }));
        } else if line.starts_with('#') || line.trim().is_empty() {
            out.push_str(line);
        } else {
            out.push_str(line);
            out.push_str(suffix);
        }
        out.push('\n');
    }
    out
}

/// HLS master playlist (single variant, points at `media.m3u8`).
#[utoipa::path(get, path = "/stream/{session_id}/master.m3u8", tag = "streaming",
    params(("session_id" = Uuid, Path, description = "Session id")),
    responses(
        (status = 200, description = "M3U8 master playlist", content_type = "application/vnd.apple.mpegurl"),
        (status = 404),
    ))]
pub async fn master_playlist(
    State(state): State<AppState>,
    Path(session_id): Path<Uuid>,
    Query(auth): Query<ApiKeyParam>,
) -> AppResult<Response> {
    let session = get_session(&state, &session_id)?;
    session.touch();
    let master = format!(
        "#EXTM3U\n\
         #EXT-X-VERSION:7\n\
         #EXT-X-STREAM-INF:BANDWIDTH=20000000\n\
         media.m3u8{}\n",
        auth.uri_suffix()
    );
    Ok(([(header::CONTENT_TYPE, PLAYLIST_CONTENT_TYPE)], master).into_response())
}

/// HLS media playlist written by ffmpeg. While the session is still
/// starting this responds 503 with `Retry-After: 1`.
#[utoipa::path(get, path = "/stream/{session_id}/media.m3u8", tag = "streaming",
    params(("session_id" = Uuid, Path, description = "Session id")),
    responses(
        (status = 200, description = "M3U8 media playlist", content_type = "application/vnd.apple.mpegurl"),
        (status = 404),
        (status = 503, description = "Session still starting; retry shortly"),
    ))]
pub async fn media_playlist(
    State(state): State<AppState>,
    Path(session_id): Path<Uuid>,
    Query(auth): Query<ApiKeyParam>,
) -> AppResult<Response> {
    let session = get_session(&state, &session_id)?;
    session.touch();
    match tokio::fs::read(session.playlist_path()).await {
        Ok(bytes) => {
            let playlist =
                playlist_with_suffix(&String::from_utf8_lossy(&bytes), &auth.uri_suffix());
            Ok(([(header::CONTENT_TYPE, PLAYLIST_CONTENT_TYPE)], playlist).into_response())
        }
        Err(_) => match session.state() {
            SessionState::Starting => Ok((
                StatusCode::SERVICE_UNAVAILABLE,
                [(header::RETRY_AFTER, "1")],
                "playlist not ready yet",
            )
                .into_response()),
            SessionState::Failed(message) => Err(AppError::Upstream(format!(
                "session failed before producing a playlist: {message}"
            ))),
            _ => Err(AppError::NotFound("media playlist".into())),
        },
    }
}

/// One fMP4 file from the session dir: `init.mp4` or `seg_NNNNN.m4s`.
#[utoipa::path(get, path = "/stream/{session_id}/{segment}", tag = "streaming",
    params(
        ("session_id" = Uuid, Path, description = "Session id"),
        ("segment" = String, Path, description = "`init.mp4` or `seg_NNNNN.m4s`"),
    ),
    responses(
        (status = 200, description = "fMP4 init/media segment", content_type = "video/mp4"),
        (status = 400, description = "Invalid segment name"),
        (status = 404),
    ))]
pub async fn hls_segment(
    State(state): State<AppState>,
    Path((session_id, segment)): Path<(Uuid, String)>,
) -> AppResult<Response> {
    let session = get_session(&state, &session_id)?;
    // Strict allowlist: the path parameter must never escape the temp dir.
    if !SEGMENT_NAME.is_match(&segment) {
        return Err(AppError::BadRequest(format!(
            "invalid segment name '{segment}'"
        )));
    }
    session.touch();
    match tokio::fs::read(session.temp_dir.join(&segment)).await {
        Ok(bytes) => Ok(([(header::CONTENT_TYPE, "video/mp4")], bytes).into_response()),
        Err(_) => Err(AppError::NotFound(format!("segment {segment}"))),
    }
}

// ---- Raw byte-range access ----------------------------------------------------

/// The source media file with RFC 7233 single-range support, for players
/// that handle the container directly.
#[utoipa::path(get, path = "/stream/{session_id}/raw", tag = "streaming",
    params(("session_id" = Uuid, Path, description = "Session id")),
    responses(
        (status = 200, description = "Whole file"),
        (status = 206, description = "Requested byte range"),
        (status = 404),
        (status = 416, description = "Unsatisfiable range"),
    ))]
pub async fn raw_media(
    State(state): State<AppState>,
    Path(session_id): Path<Uuid>,
    headers: HeaderMap,
) -> AppResult<Response> {
    let session = get_session(&state, &session_id)?;
    session.touch();
    Ok(serve_session_file(&session, &headers))
}

/// Shared by `/stream/{id}/raw` and `/internal/vfs/{id}`: build the range
/// response and wire mid-stream failures back into the session state.
pub(crate) fn serve_session_file(session: &Arc<Session>, headers: &HeaderMap) -> Response {
    let range_header = headers
        .get(header::RANGE)
        .and_then(|value| value.to_str().ok());
    let on_error = {
        let session = session.clone();
        move |error: &AppError| {
            if matches!(error, AppError::MissingSegment(_)) {
                session.mark_stream_failure(error.to_string());
            }
            tracing::warn!(session = %session.id, %error, "aborting media stream");
        }
    };
    range::range_response(
        session.media.clone(),
        &session.inner_file_name,
        range_header,
        on_error,
    )
}

// ---- Seeking -------------------------------------------------------------------

#[derive(Debug, Deserialize, ToSchema)]
pub struct SeekRequest {
    /// Absolute target position in seconds.
    pub time_secs: f64,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct SeekResponse {
    /// True when ffmpeg was restarted at the new offset and the playlist was
    /// wiped (players must reload it); false when the target was already
    /// covered by the produced playlist.
    pub restarted: bool,
}

/// Seek. Targets inside the already-produced playlist are a no-op; anything
/// else restarts ffmpeg at the target time.
#[utoipa::path(post, path = "/stream/{session_id}/seek", tag = "streaming",
    params(("session_id" = Uuid, Path, description = "Session id")),
    request_body = SeekRequest,
    responses((status = 200, body = SeekResponse), (status = 404)))]
pub async fn seek_session(
    State(state): State<AppState>,
    Path(session_id): Path<Uuid>,
    Json(request): Json<SeekRequest>,
) -> AppResult<Json<SeekResponse>> {
    let session = get_session(&state, &session_id)?;
    session.touch();
    let target = if request.time_secs.is_finite() {
        request.time_secs.max(0.0)
    } else {
        return Err(AppError::BadRequest("time_secs must be finite".into()));
    };

    // Serialize with other seeks/teardown so kill+wipe+respawn is atomic.
    let _control = session.control.lock().await;

    let start = session.start_offset();
    let produced = playlist_seconds(&session.playlist_path()).await;
    if target >= start && target <= start + produced {
        return Ok(Json(SeekResponse { restarted: false }));
    }

    tracing::info!(session = %session.id, target, "seek outside produced window; restarting ffmpeg");
    session.kill_ffmpeg().await;
    session.bump_generation();
    wipe_dir(&session.temp_dir).await?;
    session.set_start_offset(target);
    session.set_state(SessionState::Starting);
    session.clear_stderr();

    let url = loopback_url(&state, &session);
    ffmpeg::spawn_hls(
        &session,
        SpawnOptions {
            ffmpeg_path: &state.config.streaming.ffmpeg_path,
            input_url: &url,
            start_secs: target,
            transcode_audio: session.info().audio_transcoded,
        },
    )
    .await?;

    Ok(Json(SeekResponse { restarted: true }))
}

/// Sum of `#EXTINF` durations in the playlist; 0 when absent/unreadable.
async fn playlist_seconds(playlist: &FsPath) -> f64 {
    let Ok(text) = tokio::fs::read_to_string(playlist).await else {
        return 0.0;
    };
    text.lines()
        .filter_map(|line| line.strip_prefix("#EXTINF:"))
        .filter_map(|rest| rest.split(',').next())
        .filter_map(|value| value.trim().parse::<f64>().ok())
        .sum()
}

/// Delete all files inside the session dir (playlist + segments), keeping
/// the directory itself.
async fn wipe_dir(dir: &FsPath) -> AppResult<()> {
    let mut entries = tokio::fs::read_dir(dir)
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("reading session dir: {e}")))?;
    while let Ok(Some(entry)) = entries.next_entry().await {
        if let Err(e) = tokio::fs::remove_file(entry.path()).await {
            tracing::warn!(path = %entry.path().display(), error = %e, "failed to remove file");
        }
    }
    Ok(())
}

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(create_session))
        .routes(routes!(session_status, delete_session))
        .routes(routes!(master_playlist))
        .routes(routes!(media_playlist))
        .routes(routes!(raw_media))
        .routes(routes!(seek_session))
        .routes(routes!(hls_segment))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn segment_names_are_strictly_validated() {
        for good in ["init.mp4", "seg_00000.m4s", "seg_12345.m4s", "seg_1.m4s"] {
            assert!(SEGMENT_NAME.is_match(good), "should accept {good}");
        }
        for bad in [
            "../../etc/passwd",
            "..%2F..%2Fetc%2Fpasswd",
            "seg_.m4s",
            "seg_00000.m4s.tmp",
            "media.m3u8",
            "init.mp4/..",
            "seg_00000.mp4",
            "SEG_00000.M4S",
            "",
            ".",
            "..",
        ] {
            assert!(!SEGMENT_NAME.is_match(bad), "should reject {bad}");
        }
    }

    #[test]
    fn playlist_suffix_rewrites_all_uris() {
        let playlist = "#EXTM3U\n\
                        #EXT-X-VERSION:7\n\
                        #EXT-X-MAP:URI=\"init.mp4\"\n\
                        #EXTINF:6.000000,\n\
                        seg_00000.m4s\n\
                        #EXTINF:4.5,\n\
                        seg_00001.m4s\n";
        let out = playlist_with_suffix(playlist, "?apikey=se%2Fcret");
        assert!(out.contains("#EXT-X-MAP:URI=\"init.mp4?apikey=se%2Fcret\""));
        assert!(out.contains("\nseg_00000.m4s?apikey=se%2Fcret\n"));
        assert!(out.contains("\nseg_00001.m4s?apikey=se%2Fcret\n"));
        // comment/tag lines are untouched
        assert!(out.contains("#EXTINF:6.000000,\n"));
        // no suffix -> byte-identical playlist
        assert_eq!(playlist_with_suffix(playlist, ""), playlist);
    }

    #[test]
    fn apikey_suffix_is_percent_encoded() {
        let auth = ApiKeyParam {
            apikey: Some("k/e y+&=?".into()),
        };
        assert_eq!(auth.uri_suffix(), "?apikey=k%2Fe%20y%2B%26%3D%3F");
        assert_eq!(ApiKeyParam { apikey: None }.uri_suffix(), "");
    }

    #[tokio::test]
    async fn playlist_seconds_sums_extinf() {
        let dir = tempfile::tempdir().unwrap();
        let playlist = dir.path().join("media.m3u8");
        tokio::fs::write(
            &playlist,
            "#EXTM3U\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:6.000000,\nseg_00000.m4s\n#EXTINF:4.5,\nseg_00001.m4s\n",
        )
        .await
        .unwrap();
        assert!((playlist_seconds(&playlist).await - 10.5).abs() < 1e-9);
        assert_eq!(playlist_seconds(&dir.path().join("missing")).await, 0.0);
    }
}
