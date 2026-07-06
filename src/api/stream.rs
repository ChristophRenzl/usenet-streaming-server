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
    /// Whether the device/display can render HDR (PQ/HLG). When `false`,
    /// HDR sources are tone-mapped to 1080p SDR H.264 instead of
    /// stream-copied. Absent means HDR-capable.
    pub supports_hdr: Option<bool>,
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
    let supports_hdr = request.supports_hdr.unwrap_or(true);

    // Direct playback of one specific finished download.
    if let Some(download_id) = request.download_id {
        let download = db::downloads::get(&state.db, &download_id.to_string())
            .await?
            .ok_or_else(|| AppError::NotFound(format!("download {download_id}")))?;
        return start_disk_session(&state, &download, supports_hdr)
            .await
            .map(Json);
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
                return start_disk_session(&state, &download, supports_hdr)
                    .await
                    .map(Json);
            }
        }
    }

    let candidates = resolve_candidates(&state, &target, request.max_resolution).await?;
    let to_try = pick_candidates(&candidates, request.release_guid.as_deref(), MAX_ATTEMPTS)?;

    let mut failures: Vec<String> = Vec::new();
    for candidate in to_try {
        match start_session(&state, &target, &candidate, supports_hdr).await {
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
    supports_hdr: bool,
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
    match probe_and_spawn(state, &session, supports_hdr).await {
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
    supports_hdr: bool,
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

    match probe_and_spawn(state, &session, supports_hdr).await {
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

async fn probe_and_spawn(
    state: &AppState,
    session: &Arc<Session>,
    supports_hdr: bool,
) -> AppResult<()> {
    let url = loopback_url(state, session);
    let probe = ffprobe::probe_url(&state.config.streaming.ffprobe_path, &url).await?;
    let audio_transcoded = ffmpeg::should_transcode_audio(probe.audio_codec.as_deref());
    // AVPlayer refuses PQ/HLG variants outright on SDR-only outputs, so HDR
    // sources are tone-mapped for clients that declare no HDR support.
    let video_transcoded = !supports_hdr && probe.video_range != "SDR";
    session.set_info(MediaInfo {
        duration_secs: probe.duration_secs,
        video_codec: probe.video_codec.clone(),
        audio_codec: probe.audio_codec,
        audio_transcoded,
        video_range: if video_transcoded {
            "SDR".to_string()
        } else {
            probe.video_range
        },
        video_transcoded,
    });
    ffmpeg::spawn_hls(
        session,
        SpawnOptions {
            ffmpeg_path: &state.config.streaming.ffmpeg_path,
            input_url: &url,
            start_secs: 0.0,
            transcode_audio: audio_transcoded,
            video_codec: probe.video_codec.as_deref(),
            tonemap_to_sdr: video_transcoded,
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
    /// True when the video is tone-mapped to SDR for this session.
    pub video_transcoded: bool,
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
        video_transcoded: info.video_transcoded,
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

/// `?apikey=` on playlist requests. AVPlayer resolves the relative URIs
/// inside a playlist without the query string, so the key must be written
/// back into every URI for the follow-up requests to authenticate.
#[derive(Debug, Deserialize)]
pub struct PlaylistQuery {
    apikey: Option<String>,
}

/// `?apikey=...` (URL-encoded) when a key was presented, `""` otherwise.
fn apikey_suffix(apikey: Option<&str>) -> String {
    match apikey {
        Some(key) => format!(
            "?apikey={}",
            form_urlencoded::byte_serialize(key.as_bytes()).collect::<String>()
        ),
        None => String::new(),
    }
}

/// Complete VOD playlist claiming every segment of the file up front, so
/// players show the full duration and free scrubbing instead of a "live"
/// stream that only spans what ffmpeg has produced. Segments that do not
/// exist yet are made on demand by `hls_segment`.
fn vod_playlist(duration_secs: f64, suffix: &str) -> String {
    let seg = ffmpeg::SEGMENT_SECONDS;
    let count = (duration_secs / seg).ceil().max(1.0) as usize;
    let mut out = format!(
        "#EXTM3U\n\
         #EXT-X-VERSION:7\n\
         #EXT-X-TARGETDURATION:{}\n\
         #EXT-X-MEDIA-SEQUENCE:0\n\
         #EXT-X-PLAYLIST-TYPE:VOD\n\
         #EXT-X-MAP:URI=\"init.mp4{suffix}\"\n",
        seg.ceil() as u64 + 1
    );
    for i in 0..count {
        let len = if i + 1 == count {
            (duration_secs - seg * i as f64).max(0.001)
        } else {
            seg
        };
        out.push_str(&format!("#EXTINF:{len:.6},\nseg_{i:05}.m4s{suffix}\n"));
    }
    out.push_str("#EXT-X-ENDLIST\n");
    out
}

/// Append `suffix` to every URI in an HLS playlist: plain segment lines and
/// the `URI="..."` attribute of `#EXT-X-MAP` tags.
fn propagate_apikey(playlist: &str, suffix: &str) -> String {
    if suffix.is_empty() {
        return playlist.to_string();
    }
    let mut out = String::with_capacity(playlist.len());
    for line in playlist.lines() {
        if line.starts_with("#EXT-X-MAP:") {
            if let Some(start) = line.find("URI=\"") {
                let uri_start = start + "URI=\"".len();
                if let Some(end) = line[uri_start..].find('"') {
                    out.push_str(&line[..uri_start + end]);
                    out.push_str(suffix);
                    out.push_str(&line[uri_start + end..]);
                    out.push('\n');
                    continue;
                }
            }
            out.push_str(line);
        } else if !line.starts_with('#') && !line.trim().is_empty() {
            out.push_str(line);
            out.push_str(suffix);
        } else {
            out.push_str(line);
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
    Query(query): Query<PlaylistQuery>,
) -> AppResult<Response> {
    let session = get_session(&state, &session_id)?;
    session.touch();
    // AVPlayer assumes SDR when VIDEO-RANGE is absent and rejects the stream
    // ("video range specified by playlist is less than actual format
    // description") once the format description says PQ/HLG.
    let info = session.info();
    let video_range = if info.video_range.is_empty() {
        "SDR"
    } else {
        &info.video_range
    };
    let master = format!(
        "#EXTM3U\n\
         #EXT-X-VERSION:7\n\
         #EXT-X-STREAM-INF:BANDWIDTH=20000000,VIDEO-RANGE={video_range}\n\
         media.m3u8{}\n",
        apikey_suffix(query.apikey.as_deref())
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
    Query(query): Query<PlaylistQuery>,
) -> AppResult<Response> {
    let session = get_session(&state, &session_id)?;
    session.touch();
    let suffix = apikey_suffix(query.apikey.as_deref());
    // With a known duration the playlist is synthesized as full VOD; the
    // ffmpeg-written playlist only backs sources ffprobe could not time.
    if let Some(duration) = session.info().duration_secs.filter(|d| *d > 0.0) {
        let body = vod_playlist(duration, &suffix);
        return Ok(([(header::CONTENT_TYPE, PLAYLIST_CONTENT_TYPE)], body).into_response());
    }
    match tokio::fs::read_to_string(session.playlist_path()).await {
        Ok(text) => {
            let body = propagate_apikey(&text, &suffix);
            Ok(([(header::CONTENT_TYPE, PLAYLIST_CONTENT_TYPE)], body).into_response())
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

    // Fast path: the segment is already on disk and complete.
    if segment_complete(&session, &segment).await {
        if let Ok(bytes) = tokio::fs::read(session.temp_dir.join(&segment)).await {
            return Ok(([(header::CONTENT_TYPE, "video/mp4")], bytes).into_response());
        }
    }

    // The VOD playlist promises every segment; the missing ones are made on
    // demand. AVPlayer drops the variant when a segment request has not
    // delivered DATA within ~6s, so the body tails the file while ffmpeg is
    // still writing it, and ffmpeg is restarted right at the requested
    // segment when it is not about to be produced anyway.
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<bytes::Bytes, std::io::Error>>(4);
    // Detached so an aborted HTTP request cannot cancel a restart halfway
    // through; the pump notices the closed channel on its next send.
    tokio::spawn(pump_segment(state, session, segment, tx));
    let body = axum::body::Body::from_stream(futures::stream::unfold(rx, |mut rx| async move {
        rx.recv().await.map(|item| (item, rx))
    }));
    Ok(([(header::CONTENT_TYPE, "video/mp4")], body).into_response())
}

/// Complete once ffmpeg lists it in its own playlist (updated by atomic
/// rename after each finished segment). init.mp4 is written whole at
/// startup.
async fn segment_complete(session: &Arc<Session>, segment: &str) -> bool {
    if segment == "init.mp4" {
        return tokio::fs::try_exists(session.temp_dir.join(segment))
            .await
            .unwrap_or(false);
    }
    tokio::fs::read_to_string(session.playlist_path())
        .await
        .unwrap_or_default()
        .contains(segment)
}

/// Feed `segment` to the player: wait for ffmpeg to create the file
/// (restarting it at the segment's own timestamp when it is not close to
/// being produced), then stream the file's bytes as they are written. The
/// stream ends when the segment is listed complete in ffmpeg's playlist.
async fn pump_segment(
    state: AppState,
    session: Arc<Session>,
    segment: String,
    tx: tokio::sync::mpsc::Sender<Result<bytes::Bytes, std::io::Error>>,
) {
    use tokio::io::AsyncReadExt;

    /// Deregisters the in-flight request however the pump exits.
    struct RequestGuard {
        session: Arc<Session>,
        index: u64,
    }
    impl Drop for RequestGuard {
        fn drop(&mut self) {
            self.session.end_segment_request(self.index);
        }
    }

    let seg = ffmpeg::SEGMENT_SECONDS;
    let index: Option<u64> = segment
        .strip_prefix("seg_")
        .and_then(|rest| rest.strip_suffix(".m4s"))
        .and_then(|digits| digits.parse().ok());
    let _guard = index.map(|index| {
        session.begin_segment_request(index);
        RequestGuard {
            session: session.clone(),
            index,
        }
    });
    // Top-level `free` box; ISO BMFF parsers skip it. Sent as keepalive
    // while the segment is still being produced — AVPlayer drops the
    // variant when a segment request delivers no bytes for ~6s.
    const FREE_BOX: [u8; 8] = [0, 0, 0, 8, b'f', b'r', b'e', b'e'];

    let path = session.temp_dir.join(&segment);
    let deadline = tokio::time::Instant::now() + std::time::Duration::from_secs(90);
    let mut file: Option<tokio::fs::File> = None;
    let mut last_bytes = tokio::time::Instant::now();
    let mut buf = vec![0u8; 256 * 1024];
    loop {
        // The player cancels stale loads on scrubs; a closed channel must
        // free the min-outstanding slot instead of steering restarts.
        if tx.is_closed() {
            return;
        }
        // Long waits are legitimate here; don't let the reaper kill the
        // session under a player that is merely buffering.
        session.touch();
        if let SessionState::Failed(message) = session.state() {
            let _ = tx.send(Err(std::io::Error::other(message))).await;
            return;
        }
        if file.is_none() {
            if let Some(index) = index {
                let window_start = session.start_offset();
                let window_end = window_start + playlist_seconds(&session.playlist_path()).await;
                let target = index as f64 * seg;
                // A segment at most two ahead of the live edge arrives on
                // its own within the player's patience; anything else —
                // behind the window (files wiped) or further ahead than
                // ffmpeg can reach in a few seconds — needs a restart at
                // this segment so its bytes start flowing immediately. The
                // player fetches an ascending burst in parallel, so ONLY
                // the lowest outstanding request may restart; everyone
                // above waits for the sweep to reach them — otherwise the
                // burst degenerates into restarts wiping each other.
                let outside = target + seg <= window_start || target > window_end + 2.0 * seg;
                if outside
                    && session.min_requested() == Some(index)
                    && session.since_spawn() >= std::time::Duration::from_secs(3)
                {
                    if let Err(error) = restart_ffmpeg(&state, &session, target).await {
                        let _ = tx.send(Err(std::io::Error::other(error.to_string()))).await;
                        return;
                    }
                }
            }
            file = tokio::fs::File::open(&path).await.ok();
        }
        if let Some(f) = file.as_mut() {
            match f.read(&mut buf).await {
                Ok(n) if n > 0 => {
                    if tx
                        .send(Ok(bytes::Bytes::copy_from_slice(&buf[..n])))
                        .await
                        .is_err()
                    {
                        return; // client went away
                    }
                    last_bytes = tokio::time::Instant::now();
                    continue; // drain without sleeping
                }
                Ok(_) => {
                    // At the current end of file: done when ffmpeg closed
                    // the segment (listed in its playlist), otherwise more
                    // bytes are coming.
                    if segment_complete(&session, &segment).await {
                        return;
                    }
                    // If a concurrent restart wiped the file from under us,
                    // our handle points at a dead inode. The response is
                    // unsalvageable once real bytes went out (mixed
                    // generations) — abort and let the player retry.
                    if !tokio::fs::try_exists(&path).await.unwrap_or(false) {
                        let _ = tx
                            .send(Err(std::io::Error::other("segment replaced mid-read")))
                            .await;
                        return;
                    }
                }
                Err(error) => {
                    let _ = tx.send(Err(error)).await;
                    return;
                }
            }
        }
        // Keep bytes trickling while ffmpeg works (file not created yet or
        // mid-write pause) so the player's no-data watchdog stays quiet.
        if last_bytes.elapsed() >= std::time::Duration::from_secs(2) {
            if tx
                .send(Ok(bytes::Bytes::from_static(&FREE_BOX)))
                .await
                .is_err()
            {
                return;
            }
            last_bytes = tokio::time::Instant::now();
        }
        if tokio::time::Instant::now() >= deadline {
            let _ = tx
                .send(Err(std::io::Error::other(format!(
                    "segment {segment} was not produced in time"
                ))))
                .await;
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
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

    let start = session.start_offset();
    let produced = playlist_seconds(&session.playlist_path()).await;
    if target >= start && target <= start + produced {
        return Ok(Json(SeekResponse { restarted: false }));
    }

    restart_ffmpeg(&state, &session, target).await?;
    Ok(Json(SeekResponse { restarted: true }))
}

/// Kill ffmpeg, wipe the produced segments and respawn at `target_secs`
/// (snapped down to a segment boundary so numbering and timestamps stay on
/// the global VOD timeline).
async fn restart_ffmpeg(
    state: &AppState,
    session: &Arc<Session>,
    target_secs: f64,
) -> AppResult<()> {
    // Serialize with other seeks/teardown so kill+wipe+respawn is atomic.
    let _control = session.control.lock().await;

    let target = (target_secs.max(0.0) / ffmpeg::SEGMENT_SECONDS).floor() * ffmpeg::SEGMENT_SECONDS;
    // A clustered scrub fires several out-of-window requests; whoever got
    // the lock first has already restarted for this area — don't wipe its
    // output again.
    if session.since_spawn() < std::time::Duration::from_secs(3) {
        let window_start = session.start_offset();
        let window_end = window_start + playlist_seconds(&session.playlist_path()).await;
        if target >= window_start && target <= window_end + 2.0 * ffmpeg::SEGMENT_SECONDS {
            return Ok(());
        }
    }
    tracing::info!(session = %session.id, target, "restarting ffmpeg outside produced window");
    session.kill_ffmpeg().await;
    session.bump_generation();
    wipe_dir(&session.temp_dir).await?;
    session.set_start_offset(target);
    session.set_state(SessionState::Starting);
    session.clear_stderr();

    let url = loopback_url(state, session);
    let info = session.info();
    ffmpeg::spawn_hls(
        session,
        SpawnOptions {
            ffmpeg_path: &state.config.streaming.ffmpeg_path,
            input_url: &url,
            start_secs: target,
            transcode_audio: info.audio_transcoded,
            video_codec: info.video_codec.as_deref(),
            tonemap_to_sdr: info.video_transcoded,
        },
    )
    .await?;
    session.mark_spawned();
    Ok(())
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
    fn vod_playlist_covers_the_whole_duration() {
        let playlist = vod_playlist(13.5, "?apikey=k");
        assert_eq!(
            playlist,
            "#EXTM3U\n\
             #EXT-X-VERSION:7\n\
             #EXT-X-TARGETDURATION:7\n\
             #EXT-X-MEDIA-SEQUENCE:0\n\
             #EXT-X-PLAYLIST-TYPE:VOD\n\
             #EXT-X-MAP:URI=\"init.mp4?apikey=k\"\n\
             #EXTINF:6.000000,\nseg_00000.m4s?apikey=k\n\
             #EXTINF:6.000000,\nseg_00001.m4s?apikey=k\n\
             #EXTINF:1.500000,\nseg_00002.m4s?apikey=k\n\
             #EXT-X-ENDLIST\n"
        );
        // Sub-segment durations still yield one segment.
        assert!(vod_playlist(0.5, "").contains("seg_00000.m4s\n"));
    }

    #[test]
    fn apikey_is_propagated_into_playlist_uris() {
        let playlist = "#EXTM3U\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:6.000000,\nseg_00000.m4s\n#EXTINF:4.5,\nseg_00001.m4s\n";
        let suffix = apikey_suffix(Some("s3cret&x=1"));
        assert_eq!(suffix, "?apikey=s3cret%26x%3D1");
        let rewritten = propagate_apikey(playlist, &suffix);
        assert_eq!(
            rewritten,
            "#EXTM3U\n\
             #EXT-X-MAP:URI=\"init.mp4?apikey=s3cret%26x%3D1\"\n\
             #EXTINF:6.000000,\n\
             seg_00000.m4s?apikey=s3cret%26x%3D1\n\
             #EXTINF:4.5,\n\
             seg_00001.m4s?apikey=s3cret%26x%3D1\n"
        );
        // No key presented: playlist passes through untouched.
        assert_eq!(propagate_apikey(playlist, ""), playlist);
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
