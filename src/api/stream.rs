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
    nzb::{
        assess_release, health_check, main_content_segments, parse_nzb, select_main, HealthVerdict,
        MainContent, Nzb, RepairAssessment,
    },
    release::{
        parse::{parse_release_name, Resolution},
        rank::RankedRelease,
    },
    state::AppState,
    stream::{
        ffmpeg::{self, SpawnOptions},
        ffprobe, fingerprint, intro, open_media_source, range,
        session::{NewSession, Session, SessionState},
        MediaInfo,
    },
    subtitles::{self, SubtitleTrack},
    tmdb::models::MediaType,
    vfs::DiskFile,
};

use super::metadata::tmdb_client;
use super::releases::{pick_candidates, resolve_candidates, ReleaseTarget};
use super::subtitles::{download_subtitle, opensubtitles_client};

/// Maximum release candidates tried before giving up.
pub(crate) const MAX_ATTEMPTS: usize = 5;
/// Candidates health-checked while looking for a directly-streamable release.
/// A directly-streamable release is always preferred over one that needs
/// download-and-repair, so we scan deeper down the ranked list here than the
/// plain download path does — falling back to repair only once this many
/// ranked candidates have all turned out non-streamable.
pub(crate) const MAX_STREAMABLE_SCAN: usize = 15;
/// Segments STATed per candidate during the pre-flight health check.
const HEALTH_SAMPLE: usize = 10;

const PLAYLIST_CONTENT_TYPE: &str = "application/vnd.apple.mpegurl";

// ---- Session-start stage timings ---------------------------------------------

/// One timed stage of session startup, surfaced in the session response so
/// clients (and the live benchmark) can see where the press-play latency
/// actually went. Stages appear in execution order; a stage repeats when a
/// candidate failed and the next one was tried.
#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct StageTiming {
    /// Stage name (`resolve_candidates`, `nzb_grab`, `health_check`,
    /// `open_source`, `tmdb_lookup`, `ffprobe`, `ffmpeg_spawn`,
    /// `subtitle_attach_wait`, ...).
    pub stage: String,
    /// Wall-clock milliseconds spent in this stage.
    pub ms: u64,
}

/// Records wall-clock time between marks. Every `mark` closes the stage that
/// started at the previous mark (or at construction).
struct StageTimer {
    started: std::time::Instant,
    last: std::time::Instant,
    stages: Vec<StageTiming>,
}

impl StageTimer {
    fn new() -> Self {
        let now = std::time::Instant::now();
        Self {
            started: now,
            last: now,
            stages: Vec::new(),
        }
    }

    /// Close the current stage under `name` and start the next one.
    fn mark(&mut self, name: &str) {
        let now = std::time::Instant::now();
        self.stages.push(StageTiming {
            stage: name.to_string(),
            ms: now.duration_since(self.last).as_millis() as u64,
        });
        self.last = now;
    }

    fn total_ms(&self) -> u64 {
        self.started.elapsed().as_millis() as u64
    }
}

static SEGMENT_NAME: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^(init\.mp4|seg_\d+\.m4s)$").expect("segment regex"));

/// Strict allowlist for external-subtitle files served from the session dir:
/// `sub_<lang>_<n>.m3u8` or `.vtt`, where `<lang>` is a lower-case ISO code
/// with an optional regional suffix (`en`, `ger`, `pt-br`). Fully anchored
/// with no path separators, `.` or `..`, so the path-traversal guard is not
/// weakened.
static SUBTITLE_FILE_NAME: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^sub_[a-z]{2,4}(-[a-z]{2,4})?_\d+\.(m3u8|vtt)$").expect("subtitle file regex")
});

/// Embedded-subtitle rendition playlist (`sub_emb_en.m3u8`), written at
/// session start with fixed windows listed upfront.
static EMBEDDED_SUB_PLAYLIST: LazyLock<Regex> =
    LazyLock::new(|| Regex::new(r"^sub_emb_[a-z]{2,3}\.m3u8$").expect("embedded playlist regex"));

/// One embedded-subtitle window (`sub_emb_en_w0004.vtt`), sliced on demand
/// from the growing extraction fragments.
static EMBEDDED_SUB_WINDOW: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^sub_emb_([a-z]{2,3})_w(\d{4})\.vtt$").expect("embedded window regex")
});

const VTT_CONTENT_TYPE: &str = "text/vtt";

// ---- Session creation -------------------------------------------------------

/// Client playback capabilities declared on session creation, defaulting to
/// fully capable (real Apple devices).
#[derive(Debug, Clone, Copy)]
struct ClientCapabilities {
    /// Can render PQ/HLG; `false` tone-maps HDR sources to SDR.
    supports_hdr: bool,
    /// Has a Dolby (AC-3/E-AC3) decoder; `false` transcodes those to AAC.
    supports_dolby_audio: bool,
}

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
    /// Skip the streaming attempt and go straight to a download-and-repair
    /// job for the best repairable candidate (returns 202 `repairing`). Fails
    /// with 422 when no candidate is even repairable.
    #[serde(default)]
    pub force_repair: bool,
    /// Device capability cap (`480p`, `720p`, `1080p`, `2160p`): releases
    /// above the lower of this and the stored preference max are rejected,
    /// and the best supported resolution ranks first.
    pub max_resolution: Option<Resolution>,
    /// Optional ISO 639-1 languages (e.g. `["en","de"]`). When set and an
    /// OpenSubtitles API key is configured, the server best-effort searches
    /// and attaches the top subtitle per language during session start.
    /// Subtitle failures are non-fatal — the session still starts without
    /// subtitles and logs the reason.
    pub subtitle_languages: Option<Vec<String>>,
    /// Whether the device/display can render HDR (PQ/HLG). When `false`,
    /// HDR sources are tone-mapped to 1080p SDR H.264 instead of
    /// stream-copied. Absent means HDR-capable.
    pub supports_hdr: Option<bool>,
    /// Whether the player has a Dolby (AC-3/E-AC3) decoder. Real Apple
    /// devices do; the tvOS/iOS simulator and some web players do not. When
    /// `false`, AC-3/E-AC3 audio is transcoded to AAC instead of copied.
    /// Absent means Dolby-capable.
    pub supports_dolby_audio: Option<bool>,
    /// When `true`, ignore the stored blocked terms so pre-release/low-quality
    /// rips (CAM/TS/…) are candidates — the "allow pre-release" retry for a
    /// brand-new title with no proper release yet.
    #[serde(default)]
    pub ignore_blocked_terms: bool,
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
    /// True when the video is tone-mapped to SDR for this session.
    pub video_transcoded: bool,
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
    /// External subtitle tracks attached at session start (empty when none
    /// were requested, none were found, or no OpenSubtitles key is set).
    pub subtitle_tracks: Vec<SubtitleTrackInfo>,
    /// End (seconds) of the media's intro/opening chapter, when it has one —
    /// the client offers "Skip Intro" up to this timestamp. `null` for the
    /// vast majority of releases (no chapters), where the client falls back to
    /// its own heuristic.
    pub intro_end_secs: Option<f64>,
    /// Embedded chapter markers from the probe, in file order. Empty when the
    /// release has no chapters.
    pub chapters: Vec<ChapterInfo>,
    /// Wall-clock breakdown of session startup, in execution order. A stage
    /// repeats when a failed candidate forced a retry with the next one.
    pub timings: Vec<StageTiming>,
    /// Total wall-clock milliseconds from request receipt to response.
    pub total_startup_ms: u64,
}

/// A media chapter marker surfaced to the client.
#[derive(Debug, Serialize, ToSchema)]
pub struct ChapterInfo {
    pub start_secs: f64,
    pub end_secs: f64,
    pub title: Option<String>,
}

impl ChapterInfo {
    fn from_chapter(chapter: &crate::stream::ffprobe::Chapter) -> Self {
        Self {
            start_secs: chapter.start_secs,
            end_secs: chapter.end_secs,
            title: chapter.title.clone(),
        }
    }
}

/// Returned with HTTP 202 when no candidate is streamable but at least one is
/// repairable: a download-and-repair job was started. Poll
/// `GET /downloads/{download_id}` for progress; once it is `complete`, start a
/// session again (or with `download_id`) to play the repaired file from disk.
#[derive(Debug, Serialize, ToSchema)]
pub struct RepairingResponse {
    /// Always `"repairing"` — lets clients distinguish this from a 200 session.
    pub status: String,
    /// The download job reconstructing the release via par2.
    pub download_id: Uuid,
    /// Title of the release being repaired.
    pub release_title: String,
    /// The full ranked candidate list the choice was made from.
    pub candidates: Vec<RankedRelease>,
}

/// Either a started playback session (200) or a started repair job (202).
pub enum SessionOrRepair {
    Session(Box<CreateSessionResponse>),
    Repairing(RepairingResponse),
}

impl IntoResponse for SessionOrRepair {
    fn into_response(self) -> Response {
        match self {
            Self::Session(session) => (StatusCode::OK, Json(*session)).into_response(),
            Self::Repairing(repair) => (StatusCode::ACCEPTED, Json(repair)).into_response(),
        }
    }
}

/// Start a playback session. Completed downloads are played straight from
/// disk (unless `force_nntp`); otherwise releases are resolved and the first
/// healthy streamable candidate is probed and remuxed.
///
/// When no candidate is streamable but at least one is *repairable* (too
/// damaged to stream, yet recoverable from its par2 recovery files), a
/// download-and-repair job is started and the endpoint returns **202** with a
/// [`RepairingResponse`] instead of 422. Poll the download, then start again
/// once it completes to play the repaired file from disk. Pass `force_repair`
/// to skip streaming and go straight to repair.
#[utoipa::path(post, path = "/stream/sessions", tag = "streaming",
    request_body = CreateSessionRequest,
    responses(
        (status = 200, body = CreateSessionResponse, description = "Playback session started (streaming or disk)"),
        (status = 202, body = RepairingResponse, description = "No streamable candidate; a download-and-repair job was started"),
        (status = 400, description = "Bad parameters, missing indexers or TMDB key"),
        (status = 404, description = "Unknown TMDB id, release_guid or download_id"),
        (status = 422, description = "No streamable or repairable release found; details list per-candidate reasons"),
    ))]
pub async fn create_session(
    State(state): State<AppState>,
    axum::Extension(current): axum::Extension<super::auth::CurrentUser>,
    Json(request): Json<CreateSessionRequest>,
) -> AppResult<SessionOrRepair> {
    let user_id = current.id;
    // Requested subtitle languages (best-effort auto-attach after start).
    let subtitle_languages = request.subtitle_languages.clone().unwrap_or_default();
    let capabilities = ClientCapabilities {
        supports_hdr: request.supports_hdr.unwrap_or(true),
        supports_dolby_audio: request.supports_dolby_audio.unwrap_or(true),
    };
    let mut timer = StageTimer::new();

    // Direct playback of one specific finished download.
    if let Some(download_id) = request.download_id {
        let download = db::downloads::get(&state.db, &download_id.to_string())
            .await?
            .ok_or_else(|| AppError::NotFound(format!("download {download_id}")))?;
        return start_disk_session(
            &state,
            user_id,
            &download,
            &subtitle_languages,
            capabilities,
            timer,
        )
        .await
        .map(|s| SessionOrRepair::Session(Box::new(s)));
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
            // A completed download of a release later marked as bad must not
            // short-circuit back to the same file.
            if db::release_blacklist::contains(&state.db, &download.release_title).await? {
                continue;
            }
            if tokio::fs::try_exists(path).await.unwrap_or(false) {
                return start_disk_session(
                    &state,
                    user_id,
                    &download,
                    &subtitle_languages,
                    capabilities,
                    timer,
                )
                .await
                .map(|s| SessionOrRepair::Session(Box::new(s)));
            }
        }
    }

    let candidates = resolve_candidates(
        &state,
        &target,
        request.max_resolution,
        request.ignore_blocked_terms,
    )
    .await?;
    timer.mark("resolve_candidates");
    // Prefer the release this item was last watched with, so resuming from
    // history continues in the exact same release when it is still available.
    let last_release = db::watch_history::last_release_title(
        &state.db,
        user_id,
        target.tmdb_id,
        target.media_type.as_str(),
        target.season,
        target.episode,
    )
    .await?;
    // Scan deeper for a directly-streamable release before considering repair:
    // never fall back to download-and-repair while a clean release still exists
    // further down the ranked list.
    let mut to_try = pick_candidates(
        &candidates,
        request.release_guid.as_deref(),
        last_release.as_deref(),
        MAX_STREAMABLE_SCAN,
    )?;

    // Packaging preference: pre-grab the top NZBs concurrently (the chosen
    // one is needed anyway) and move a genuinely *unpacked* release ahead of
    // near-tied RAR sets — an unpacked post skips the per-volume header walk
    // and starts streaming seconds faster. The newznab `files` attribute is
    // too unreliable across indexers for this; only the NZB itself tells the
    // truth. A manual guid pin and the last-watched front-load are never
    // reordered.
    let mut pregrabbed = pregrab_candidates(&state, &to_try).await;
    timer.mark("nzb_pregrab");
    if request.release_guid.is_none()
        && last_release.as_deref() != Some(to_try[0].raw.title.as_str())
    {
        reorder_by_packaging(&mut to_try, &pregrabbed);
    }

    let mut failures: Vec<String> = Vec::new();
    // Remember the first repairable candidate (in rank order) as the fallback.
    let mut best_repairable: Option<(RankedRelease, Nzb, MainContent)> = None;

    for candidate in to_try {
        // Grab (reusing the pre-grab when it succeeded) + assess this
        // candidate once.
        let grabbed = pregrabbed.remove(&candidate.raw.guid);
        let (assessment, nzb, main) = match assess_candidate(
            &state, &candidate, grabbed, &mut timer,
        )
        .await
        {
            Ok(triple) => triple,
            Err(error) => {
                tracing::warn!(release = %candidate.raw.title, %error, "candidate assessment failed, trying next");
                failures.push(format!("{}: {error}", candidate.raw.title));
                continue;
            }
        };

        match assessment.verdict {
            HealthVerdict::Streamable if !request.force_repair => {
                match start_streamable_session(
                    &state,
                    user_id,
                    &target,
                    &candidate,
                    nzb,
                    main,
                    capabilities,
                    &subtitle_languages,
                    &mut timer,
                )
                .await
                {
                    Ok(session) => {
                        return Ok(SessionOrRepair::Session(Box::new(session_response(
                            &session, candidate, candidates, "nntp", &timer,
                        ))));
                    }
                    Err(error) => {
                        tracing::warn!(release = %candidate.raw.title, %error, "streamable candidate failed to start, trying next");
                        failures.push(format!("{}: {error}", candidate.raw.title));
                    }
                }
            }
            HealthVerdict::Streamable | HealthVerdict::Repairable => {
                // Either force_repair on a streamable one, or a genuinely
                // repairable-only candidate. Keep the best (first) as fallback.
                if best_repairable.is_none() {
                    best_repairable = Some((candidate.clone(), nzb, main));
                }
                failures.push(format!(
                    "{}: not streamable (repairable, {}/{} sampled missing)",
                    candidate.raw.title, assessment.health.missing, assessment.health.checked
                ));
            }
            HealthVerdict::Unrecoverable => {
                failures.push(format!(
                    "{}: unrecoverable ({}/{} sampled missing, par2 {} bytes)",
                    candidate.raw.title,
                    assessment.health.missing,
                    assessment.health.checked,
                    assessment.par2_recovery_bytes
                ));
            }
        }
    }

    // No streamable candidate started. Fall back to download-and-repair.
    if let Some((candidate, nzb, main)) = best_repairable {
        let download_id = start_repair_job(&state, &target, &candidate, nzb, main).await?;
        return Ok(SessionOrRepair::Repairing(RepairingResponse {
            status: "repairing".into(),
            download_id,
            release_title: candidate.raw.title.clone(),
            candidates,
        }));
    }

    Err(AppError::NoRelease(failures.join("; ")))
}

/// Start a download-and-repair job for a repairable candidate and return its
/// id. The job runs in the background; on completion it marks the row complete
/// with a file path, and the normal disk-playback path serves it.
async fn start_repair_job(
    state: &AppState,
    target: &ReleaseTarget,
    candidate: &RankedRelease,
    nzb: Nzb,
    main: MainContent,
) -> AppResult<Uuid> {
    let id = Uuid::new_v4();
    db::downloads::insert(
        &state.db,
        &db::downloads::NewDownload {
            id: &id.to_string(),
            tmdb_id: target.tmdb_id,
            media_type: target.media_type.as_str(),
            season: target.season,
            episode: target.episode,
            release_title: &candidate.raw.title,
            nzb_url: &candidate.raw.nzb_url,
        },
    )
    .await?;
    state.downloads.spawn(
        state.clone(),
        id,
        crate::download::DownloadJob::repair(nzb, main),
    );
    tracing::info!(download = %id, release = %candidate.raw.title, "repair job queued from session start");
    Ok(id)
}

fn session_response(
    session: &Arc<Session>,
    chosen_release: RankedRelease,
    candidates: Vec<RankedRelease>,
    source: &str,
    timer: &StageTimer,
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
        video_transcoded: info.video_transcoded,
        container: session.container.clone(),
        chosen_release,
        candidates,
        resume_position_secs: (session.resume_position_secs > 0.0)
            .then_some(session.resume_position_secs),
        source: source.to_string(),
        subtitle_tracks: session
            .subtitle_tracks()
            .iter()
            .map(|t| SubtitleTrackInfo::from_track(&session.id, t))
            .collect(),
        intro_end_secs: session.intro_end_secs(),
        chapters: info
            .chapters
            .iter()
            .map(ChapterInfo::from_chapter)
            .collect(),
        timings: timer.stages.clone(),
        total_startup_ms: timer.total_ms(),
    }
}

/// Best-effort audio-fingerprint intro detection at session start, for TV
/// episodes with a known season+episode whose probe found *no* chapter-based
/// intro (chapters always take priority). Cheap and never blocking:
///
/// - If a season intro is already cached, apply it to the session **before
///   this returns** (a quick DB read), so the `CreateSessionResponse` carries
///   `intro_end_secs`.
/// - Otherwise spawn a detached background task that fingerprints this
///   episode's first ~240s of audio over the loopback, stores it, and — if a
///   sibling episode's fingerprint already exists — runs the comparison. A
///   detected intro is cached per season and applied to the (still-live)
///   session, which the client picks up via `GET /stream/{id}` status polling.
///
/// Any failure (fpcalc missing, fetch/compare error) is logged and dropped;
/// `intro_end_secs` simply stays `None`. Needs two episodes of a season played
/// before detection can complete.
async fn apply_intro_detection(state: &AppState, session: &Arc<Session>) {
    // TV only, with a full season+episode identity, and only when chapters did
    // not already answer it.
    if session.media_type != MediaType::Tv || session.info().intro_end_secs.is_some() {
        return;
    }
    let (Some(season), Some(episode)) = (session.season, session.episode) else {
        return;
    };
    let tmdb_id = session.tmdb_id;

    // Cache hit: the season's intro is already known — apply synchronously so
    // the session response carries it, and we're done.
    match db::fingerprints::season_intro(&state.db, tmdb_id, season).await {
        Ok(Some(intro)) => {
            session.set_intro_end_override(intro.intro_end_secs);
            tracing::debug!(session = %session.id, tmdb_id, season, "applied cached season intro");
            return;
        }
        Ok(None) => {}
        Err(error) => {
            tracing::debug!(%error, "season-intro lookup failed; skipping intro detection");
            return;
        }
    }

    // No cached intro: fingerprint this episode and compare siblings in a
    // detached task so the ~240s audio fetch never blocks the response.
    let state = state.clone();
    let session = session.clone();
    tokio::spawn(async move {
        // Fingerprint this episode's audio over the loopback (best-effort).
        let url = loopback_url(&state, &session);
        let points =
            match fingerprint::fingerprint_url(&state.config.analysis.fpcalc_path, &url).await {
                Ok(points) => points,
                Err(error) => {
                    tracing::debug!(session = %session.id, %error, "fingerprinting episode failed");
                    return;
                }
            };
        let bytes = fingerprint::to_bytes(&points);
        if let Err(error) = db::fingerprints::upsert_episode_fingerprint(
            &state.db, tmdb_id, season, episode, &bytes,
        )
        .await
        {
            tracing::debug!(%error, "storing episode fingerprint failed");
            return;
        }

        // Compare against any already-stored sibling from the same season.
        let sibling = match db::fingerprints::sibling_fingerprint(
            &state.db, tmdb_id, season, episode,
        )
        .await
        {
            Ok(Some(bytes)) => fingerprint::from_bytes(&bytes),
            Ok(None) => {
                tracing::debug!(
                    session = %session.id, tmdb_id, season, episode,
                    "no sibling fingerprint yet; intro detection needs a second episode"
                );
                return;
            }
            Err(error) => {
                tracing::debug!(%error, "loading sibling fingerprint failed");
                return;
            }
        };

        let Some((start_secs, end_secs)) = intro::find_intro(&points, &sibling) else {
            tracing::debug!(session = %session.id, tmdb_id, season, "no intro detected from fingerprints");
            return;
        };

        let intro = db::fingerprints::SeasonIntro {
            intro_start_secs: start_secs,
            intro_end_secs: end_secs,
        };
        if let Err(error) =
            db::fingerprints::upsert_season_intro(&state.db, tmdb_id, season, intro).await
        {
            tracing::debug!(%error, "storing season intro failed");
        }
        session.set_intro_end_override(end_secs);
        tracing::info!(
            session = %session.id, tmdb_id, season,
            intro_end_secs = end_secs,
            "detected intro via audio fingerprint"
        );
    });
}

// Best-effort auto-attach of subtitles at session start, split in two so the
// slow half can overlap the probe/ffmpeg spawn. Never fails the session: a
// missing key, no results or an upstream error is logged and skipped.
//
// Two accuracy features ride along:
//
// - the media's OpenSubtitles **moviehash** is computed from its
//   [`VirtualFile`](crate::vfs::VirtualFile) and passed to the search, so
//   hash-matched (release-accurate) subtitles are found and ranked first;
// - for a chosen subtitle that is *not* hash-matched but reports its own
//   `fps`, cue times are rescaled by `media_fps / subtitle_fps` to correct
//   frame-rate drift. Hash-matched subs are assumed correct and left as-is.

/// One subtitle fetched and ready to attach: everything slow (the moviehash
/// reads, OpenSubtitles search + download) already happened.
struct PrefetchedSubtitle {
    language: String,
    srt_text: String,
    subtitle_fps: Option<f64>,
    hash_match: bool,
}

/// The slow half of subtitle auto-attach: moviehash + OpenSubtitles search +
/// download per language. Only reads the virtual file — never the session —
/// so callers run it concurrently with the probe/ffmpeg spawn instead of
/// adding it to the session-start critical path. Best-effort: any failure is
/// logged and that language is skipped.
#[allow(clippy::too_many_arguments)]
async fn prefetch_subtitles(
    state: AppState,
    media: Arc<dyn crate::vfs::VirtualFile>,
    session_id: Uuid,
    tmdb_id: i64,
    media_type: MediaType,
    season: Option<u32>,
    episode: Option<u32>,
    languages: Vec<String>,
) -> Vec<PrefetchedSubtitle> {
    if languages.is_empty() {
        return Vec::new();
    }
    // Provider chain: embedded tracks attach at spawn (checked at attach
    // time); OpenSubtitles is the primary external source; SubDL fills in
    // when OpenSubtitles cannot deliver (quota exhausted, no result,
    // download error).
    let state = &state;
    let os_client = match opensubtitles_client(state).await {
        Ok(client) => Some(client),
        Err(error) => {
            tracing::info!(session = %session_id, %error, "OpenSubtitles unavailable");
            None
        }
    };
    let subdl = super::subtitles::subdl_client(state).await;
    if os_client.is_none() && subdl.is_none() {
        return Vec::new();
    }
    // Providers are tried in the admin-configured order; the first that
    // returns a subtitle for a language wins.
    let provider_order = super::subtitles::subtitle_provider_order(state).await;

    // Release-accurate matching: compute the media's moviehash once. Non-fatal.
    let moviehash = match subtitles::osdb_hash(&media).await {
        Ok(hash) => hash,
        Err(error) => {
            tracing::warn!(session = %session_id, %error, "computing moviehash failed");
            None
        }
    };

    // Episodes key on the show tmdb id + S/E; movies on the tmdb id.
    let (season, episode) = match media_type {
        MediaType::Tv => (season, episode),
        MediaType::Movie => (None, None),
    };

    // Note: embedded-track coverage cannot be checked here — it is only
    // known once the probe ran, and this prefetch deliberately overlaps the
    // probe. The embedded-first skip happens at attach time instead; a
    // prefetch for a covered language is wasted work the download cache
    // absorbs on repeats.
    let mut prefetched = Vec::new();
    for language in &languages {
        let query = subtitles::SubtitleQuery {
            tmdb_id,
            season,
            episode,
            languages: vec![language.clone()],
            moviehash: moviehash.clone(),
        };
        let mut subtitle = None;
        for provider in &provider_order {
            subtitle = match provider.as_str() {
                "opensubtitles" => match &os_client {
                    Some(client) => {
                        prefetch_opensubtitles(state, client, &query, session_id, language).await
                    }
                    None => None,
                },
                "subdl" => match &subdl {
                    Some(client) => {
                        prefetch_subdl(state, client, &query, session_id, language).await
                    }
                    None => None,
                },
                _ => None,
            };
            if subtitle.is_some() {
                break;
            }
        }
        prefetched.extend(subtitle);
    }
    prefetched
}

/// One OpenSubtitles attempt for one language: search → download (cached).
async fn prefetch_opensubtitles(
    state: &AppState,
    client: &crate::subtitles::OpenSubtitlesClient,
    query: &subtitles::SubtitleQuery,
    session_id: Uuid,
    language: &str,
) -> Option<PrefetchedSubtitle> {
    let top = match client.search(query).await {
        Ok(results) => results.into_iter().next(),
        Err(error) => {
            tracing::warn!(session = %session_id, %language, %error, "subtitle search failed");
            return None;
        }
    };
    let Some(result) = top else {
        tracing::info!(session = %session_id, %language, "no OpenSubtitles result");
        return None;
    };
    match download_subtitle(state, client, result.file_id).await {
        Ok(srt) => Some(PrefetchedSubtitle {
            language: language.to_string(),
            srt_text: srt.text,
            subtitle_fps: result.fps,
            hash_match: result.moviehash_match,
        }),
        Err(error) => {
            tracing::warn!(session = %session_id, %language, %error, "subtitle download failed");
            None
        }
    }
}

/// One SubDL fallback attempt for one language: search → download, cached
/// under a synthetic key so repeats are free like OpenSubtitles downloads.
async fn prefetch_subdl(
    state: &AppState,
    client: &crate::subtitles::subdl::SubdlClient,
    query: &subtitles::SubtitleQuery,
    session_id: Uuid,
    language: &str,
) -> Option<PrefetchedSubtitle> {
    let results = match client.search(query, language).await {
        Ok(results) => results,
        Err(error) => {
            tracing::warn!(session = %session_id, %language, %error, "SubDL search failed");
            return None;
        }
    };
    // Prefer a non-SDH hit, like the embedded-stream selection does.
    let Some(result) = results
        .iter()
        .find(|r| !r.hearing_impaired)
        .or_else(|| results.first())
    else {
        tracing::info!(session = %session_id, %language, "no SubDL result");
        return None;
    };
    let cache_key = db::subtitle_cache::synthetic_key(&result.url);
    let srt = match db::subtitle_cache::get(&state.db, cache_key).await {
        Ok(Some(cached)) => cached,
        _ => match client.download(&result.url).await {
            Ok(text) => {
                if let Err(error) = db::subtitle_cache::put(&state.db, cache_key, &text).await {
                    tracing::warn!(%error, "caching SubDL subtitle failed");
                }
                text
            }
            Err(error) => {
                tracing::warn!(session = %session_id, %language, %error, "SubDL download failed");
                return None;
            }
        },
    };
    tracing::info!(
        session = %session_id,
        %language,
        release = result.release_name.as_deref().unwrap_or("?"),
        "prefetched SubDL subtitle"
    );
    Some(PrefetchedSubtitle {
        language: language.to_string(),
        srt_text: srt,
        subtitle_fps: None,
        hash_match: false,
    })
}

/// The fast half of subtitle auto-attach: the fps-rescale decision (needs the
/// probe's media fps, so it must run after the probe finished) and writing
/// the WebVTT rendition into the session dir. Best-effort, like the prefetch.
async fn attach_prefetched_subtitles(session: &Arc<Session>, prefetched: Vec<PrefetchedSubtitle>) {
    let media_fps = session.info().fps;
    for subtitle in prefetched {
        // Embedded-subtitles-first: probe_and_spawn attached release-accurate
        // embedded tracks for the requested languages; an external download
        // for a language that is already covered is dropped here.
        if session
            .subtitle_track_by_language(&subtitle.language)
            .is_some()
        {
            tracing::debug!(
                session = %session.id,
                language = %subtitle.language,
                "embedded track covers language; dropping external subtitle"
            );
            continue;
        }
        let fps_scale = fps_rescale(media_fps, subtitle.subtitle_fps, subtitle.hash_match);
        // The first attached track becomes the default.
        let make_default = session.subtitle_tracks().is_empty();
        match subtitles::attach_subtitle(
            session,
            &subtitle.language,
            &subtitle.srt_text,
            make_default,
            fps_scale,
        )
        .await
        {
            Ok(track) => tracing::info!(
                session = %session.id,
                language = %track.language,
                hash_match = subtitle.hash_match,
                fps_scale = ?fps_scale,
                "attached subtitle"
            ),
            Err(error) => {
                tracing::warn!(
                    session = %session.id,
                    language = %subtitle.language,
                    %error,
                    "attaching subtitle failed"
                )
            }
        }
    }
}

/// The fps rescale factor (`media_fps / subtitle_fps`) to apply to a subtitle,
/// or `None` when no correction should happen: hash-matched subs are assumed
/// release-accurate, and a correction only applies when both frame rates are
/// known and differ meaningfully (> 0.1 fps).
fn fps_rescale(media_fps: Option<f64>, subtitle_fps: Option<f64>, hash_match: bool) -> Option<f64> {
    if hash_match {
        return None;
    }
    let (media, sub) = (media_fps?, subtitle_fps?);
    if !media.is_finite() || !sub.is_finite() || media <= 0.0 || sub <= 0.0 {
        return None;
    }
    ((media - sub).abs() > 0.1).then_some(media / sub)
}

/// Grab a candidate's NZB, parse it and pick the main content (no health
/// check). Shared by streaming, downloads and repair assessment.
pub(crate) async fn grab_and_select(
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
    Ok((nzb, main))
}

/// Grab a candidate's NZB, parse it, pick the main content and run the
/// pre-flight health check. Errors unless the release is streamable. Used by
/// the download-job API (which streams the main content to disk).
pub(crate) async fn fetch_healthy_release(
    state: &AppState,
    candidate: &RankedRelease,
) -> AppResult<(Nzb, MainContent)> {
    let (nzb, main) = grab_and_select(state, candidate).await?;
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

/// How many top candidates get their NZB pre-grabbed concurrently at session
/// start, to learn their real packaging (plain file vs RAR set) before
/// committing to one. NZBs are small; the extra grabs are the only cost.
const PREGRAB_CANDIDATES: usize = 3;
/// Candidates scoring within this band of the front-runner count as
/// near-tied: an unpacked one among them is preferred over RAR sets. The
/// band sits below every meaningful quality signal in ranking (resolution
/// 300+/tier, source 100/step, codec 150-300), so a genuinely better release
/// is never displaced by a merely faster-starting one.
const PACKAGING_SCORE_BAND: i64 = 150;

/// Concurrently grab + parse the NZBs of the first [`PREGRAB_CANDIDATES`]
/// candidates, keyed by guid. Failures are logged and skipped — the main
/// loop re-grabs (and properly records) them when their turn comes.
async fn pregrab_candidates(
    state: &AppState,
    to_try: &[RankedRelease],
) -> std::collections::HashMap<String, (Nzb, MainContent)> {
    let grabs = to_try.iter().take(PREGRAB_CANDIDATES).map(|candidate| {
        let state = state.clone();
        let candidate = candidate.clone();
        async move {
            match grab_and_select(&state, &candidate).await {
                Ok(pair) => Some((candidate.raw.guid.clone(), pair)),
                Err(error) => {
                    tracing::debug!(release = %candidate.raw.title, %error, "NZB pre-grab failed");
                    None
                }
            }
        }
    });
    futures::future::join_all(grabs)
        .await
        .into_iter()
        .flatten()
        .collect()
}

/// Stable-reorder the pre-grabbed head of the candidate list so unpacked
/// releases within [`PACKAGING_SCORE_BAND`] of the front-runner come before
/// RAR sets. Everything else (unknown packaging, out-of-band scores, the
/// tail beyond the pre-grab window) keeps its rank order.
fn reorder_by_packaging(
    to_try: &mut [RankedRelease],
    pregrabbed: &std::collections::HashMap<String, (Nzb, MainContent)>,
) {
    let Some(top_score) = to_try.first().map(|c| c.score) else {
        return;
    };
    let head = to_try.len().min(PREGRAB_CANDIDATES);
    let preferred = |candidate: &RankedRelease| {
        matches!(
            pregrabbed.get(&candidate.raw.guid),
            Some((_, MainContent::Plain(_)))
        ) && top_score - candidate.score <= PACKAGING_SCORE_BAND
    };
    if let Some(first) = to_try[..head].iter().find(|c| preferred(c)) {
        if !preferred(&to_try[0]) {
            tracing::info!(
                release = %first.raw.title,
                "preferring unpacked release over near-tied RAR set"
            );
        }
    }
    to_try[..head].sort_by_key(|candidate| !preferred(candidate));
}

/// Grab a candidate (unless `grabbed` already carries its pre-grabbed NZB)
/// and run the full repairability assessment. Returns the verdict together
/// with the parsed NZB and main content so the caller can reuse them (to
/// stream or to start a repair job) without a second grab.
async fn assess_candidate(
    state: &AppState,
    candidate: &RankedRelease,
    grabbed: Option<(Nzb, MainContent)>,
    timer: &mut StageTimer,
) -> AppResult<(RepairAssessment, Nzb, MainContent)> {
    let (nzb, main) = match grabbed {
        Some(pair) => pair,
        None => grab_and_select(state, candidate).await?,
    };
    timer.mark("nzb_grab");
    let assessment = assess_release(&nzb, &main, &state.nntp_pool, HEALTH_SAMPLE).await?;
    timer.mark("health_check");
    Ok((assessment, nzb, main))
}

/// Best-effort TMDB lookup at session start: watch-history metadata (title,
/// artwork, episode title/still) plus the title's original language for
/// original-audio selection. Any failure — missing TMDB key, upstream error —
/// degrades to empty fields instead of failing the session; the history
/// upsert keeps previously stored values (COALESCE).
struct MediaLookup {
    meta: db::watch_history::MediaMeta,
    original_language: Option<String>,
}

async fn fetch_media_lookup(
    state: &AppState,
    tmdb_id: i64,
    media_type: MediaType,
    season: Option<u32>,
    episode: Option<u32>,
) -> MediaLookup {
    let mut meta = db::watch_history::MediaMeta::default();
    let mut original_language = None;
    let tmdb = match tmdb_client(state).await {
        Ok(tmdb) => tmdb,
        Err(error) => {
            tracing::info!(%error, "skipping media lookup (no TMDB client)");
            return MediaLookup {
                meta,
                original_language,
            };
        }
    };
    match media_type {
        MediaType::Movie => match tmdb.movie_details(tmdb_id).await {
            Ok(movie) => {
                meta.title = Some(movie.title);
                meta.poster_url = movie.poster_url;
                meta.backdrop_url = movie.backdrop_url;
                original_language = movie.original_language;
            }
            Err(error) => tracing::info!(tmdb_id, %error, "media lookup failed"),
        },
        MediaType::Tv => {
            match tmdb.tv_details(tmdb_id).await {
                Ok(show) => {
                    meta.title = Some(show.title);
                    meta.poster_url = show.poster_url;
                    meta.backdrop_url = show.backdrop_url;
                    original_language = show.original_language;
                }
                Err(error) => tracing::info!(tmdb_id, %error, "media lookup failed"),
            }
            if let Some((season, episode)) = season.zip(episode) {
                match tmdb.episode_details(tmdb_id, season, episode).await {
                    Ok(details) => {
                        meta.episode_title = details.title;
                        meta.still_url = details.still_url;
                    }
                    Err(error) => {
                        tracing::info!(tmdb_id, season, episode, %error, "episode metadata lookup failed");
                    }
                }
            }
        }
    }
    MediaLookup {
        meta,
        original_language,
    }
}

/// The ISO 639-1 audio language playback should prefer, from the stored
/// `language` preference: `original` resolves to the title's TMDB original
/// language, a code/name resolves to its code, and anything else falls back
/// to English (the scene default — matching how ranking treats untagged
/// releases).
fn preferred_audio_language(pref: &str, original_language: Option<&str>) -> String {
    let pref = pref.trim().to_lowercase();
    let code = match pref.as_str() {
        "original" => {
            return original_language
                .map(ffprobe::primary_language_code)
                .unwrap_or_else(|| "en".to_string())
        }
        "german" => "de",
        "french" => "fr",
        "italian" => "it",
        "spanish" => "es",
        "dutch" => "nl",
        "korean" => "ko",
        "japanese" => "ja",
        "hindi" => "hi",
        "russian" => "ru",
        "english" => "en",
        other if other.len() == 2 && other.chars().all(|c| c.is_ascii_alphabetic()) => other,
        _ => "en",
    };
    code.to_string()
}

/// Start a session from an already-grabbed, already-assessed streamable
/// candidate: virtual file → ffprobe → ffmpeg, with the subtitle prefetch
/// riding along concurrently. On failure the partially registered session is
/// torn down.
#[allow(clippy::too_many_arguments)]
async fn start_streamable_session(
    state: &AppState,
    user_id: i64,
    target: &ReleaseTarget,
    candidate: &RankedRelease,
    nzb: Nzb,
    main: MainContent,
    capabilities: ClientCapabilities,
    subtitle_languages: &[String],
    timer: &mut StageTimer,
) -> AppResult<Arc<Session>> {
    let source = open_media_source(
        &nzb,
        &main,
        &state.nntp_pool,
        &state.segment_cache,
        state.config.streaming.readahead_segments,
    )
    .await?;
    timer.mark("open_source");

    let resume_position_secs = db::watch_history::position_secs(
        &state.db,
        user_id,
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
            user_id,
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
    timer.mark("session_setup");

    let lookup = fetch_media_lookup(
        state,
        target.tmdb_id,
        target.media_type,
        target.season,
        target.episode,
    )
    .await;
    let audio_language = preferred_audio_language(
        &db::preferences::get(&state.db).await?.language,
        lookup.original_language.as_deref(),
    );
    timer.mark("tmdb_lookup");

    // From here on, clean up the registered session on failure. The subtitle
    // prefetch (moviehash reads + OpenSubtitles) only touches the virtual
    // file, so it runs as its own task overlapping the probe/ffmpeg spawn
    // instead of adding its seconds to the critical path; attaching needs
    // the probed fps (and the embedded-track coverage) and happens right
    // after.
    let prefetch = tokio::spawn(prefetch_subtitles(
        state.clone(),
        session.media.clone(),
        session.id,
        target.tmdb_id,
        target.media_type,
        target.season,
        target.episode,
        subtitle_languages.to_vec(),
    ));
    match probe_and_spawn(
        state,
        &session,
        capabilities,
        &audio_language,
        subtitle_languages,
        Some(timer),
    )
    .await
    {
        Ok(()) => {}
        Err(error) => {
            prefetch.abort();
            state.sessions.teardown(&session.id).await;
            return Err(error);
        }
    }
    attach_prefetched_subtitles(&session, prefetch.await.unwrap_or_default()).await;
    timer.mark("subtitle_attach_wait");

    // Best-effort intro detection (chapters first, then audio fingerprint):
    // applies a cached season intro immediately, else fingerprints in the
    // background. Never blocks or fails the session.
    apply_intro_detection(state, &session).await;

    db::watch_history::record_session_start(
        &state.db,
        user_id,
        &db::watch_history::SessionStart {
            tmdb_id: target.tmdb_id,
            media_type: target.media_type.as_str(),
            season: target.season,
            episode: target.episode,
            release_title: &candidate.raw.title,
            indexer_id: Some(candidate.raw.indexer_id),
            nzb_url: &candidate.raw.nzb_url,
            duration_secs: session.info().duration_secs,
            meta: lookup.meta,
        },
    )
    .await?;
    timer.mark("finalize");

    // Best-effort Trakt scrobble; a Trakt problem never affects playback.
    super::trakt::spawn_scrobble(state, &session, crate::trakt::ScrobbleAction::Start);

    Ok(session)
}

/// Play a finished download from disk: no indexers, no NNTP — a
/// [`DiskFile`] over the stored path feeds the usual probe/HLS pipeline.
async fn start_disk_session(
    state: &AppState,
    user_id: i64,
    download: &db::downloads::Download,
    subtitle_languages: &[String],
    capabilities: ClientCapabilities,
    mut timer: StageTimer,
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
        user_id,
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
            user_id,
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
    timer.mark("session_setup");

    let lookup = fetch_media_lookup(state, download.tmdb_id, media_type, season, episode).await;
    let audio_language = preferred_audio_language(
        &db::preferences::get(&state.db).await?.language,
        lookup.original_language.as_deref(),
    );
    timer.mark("tmdb_lookup");

    // Subtitle prefetch overlaps the probe/ffmpeg spawn, as in the NNTP path.
    let prefetch = tokio::spawn(prefetch_subtitles(
        state.clone(),
        session.media.clone(),
        session.id,
        download.tmdb_id,
        media_type,
        season,
        episode,
        subtitle_languages.to_vec(),
    ));
    match probe_and_spawn(
        state,
        &session,
        capabilities,
        &audio_language,
        subtitle_languages,
        Some(&mut timer),
    )
    .await
    {
        Ok(()) => {}
        Err(error) => {
            prefetch.abort();
            state.sessions.teardown(&session.id).await;
            return Err(error);
        }
    }
    attach_prefetched_subtitles(&session, prefetch.await.unwrap_or_default()).await;
    timer.mark("subtitle_attach_wait");

    // Best-effort intro detection, as for the streaming path (no-op for movies
    // or when a chapter intro was already found).
    apply_intro_detection(state, &session).await;

    db::watch_history::record_session_start(
        &state.db,
        user_id,
        &db::watch_history::SessionStart {
            tmdb_id: download.tmdb_id,
            media_type: media_type.as_str(),
            season,
            episode,
            release_title: &download.release_title,
            indexer_id: None,
            nzb_url: &download.nzb_url,
            duration_secs: session.info().duration_secs,
            meta: lookup.meta,
        },
    )
    .await?;

    timer.mark("finalize");

    // Best-effort Trakt scrobble; a Trakt problem never affects playback.
    super::trakt::spawn_scrobble(state, &session, crate::trakt::ScrobbleAction::Start);

    Ok(session_response(
        &session,
        synthesized_release(download),
        Vec::new(),
        "disk",
        &timer,
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
        tvdb_id: None,
        imdb_id: None,
        file_count: None,
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
    capabilities: ClientCapabilities,
    audio_language: &str,
    subtitle_languages: &[String],
    mut timer: Option<&mut StageTimer>,
) -> AppResult<()> {
    let url = loopback_url(state, session);
    let probe = ffprobe::probe_url(&state.config.streaming.ffprobe_path, &url).await?;
    if let Some(timer) = timer.as_deref_mut() {
        timer.mark("ffprobe");
    }
    // Serve the audio stream matching the preferred language — dual-language
    // releases put the dub first, so stream 0 is often the wrong track.
    let audio_stream_index =
        ffprobe::select_audio_stream(&probe.audio_streams, Some(audio_language));
    let audio = probe.audio_streams.get(audio_stream_index);
    let audio_codec = audio.and_then(|s| s.codec.clone());
    if audio_stream_index != 0 {
        tracing::info!(
            session = %session.id,
            audio_stream_index,
            language = audio.and_then(|s| s.language.as_deref()).unwrap_or("?"),
            "selected non-first audio stream by language preference"
        );
    }
    let audio_transcoded =
        ffmpeg::should_transcode_audio(audio_codec.as_deref(), capabilities.supports_dolby_audio);
    // AVPlayer refuses PQ/HLG variants outright on SDR-only outputs, so HDR
    // sources are tone-mapped for clients that declare no HDR support.
    let video_transcoded = !capabilities.supports_hdr && probe.video_range != "SDR";
    // Master-playlist facts describe the *served* stream. Tone-mapping
    // re-encodes with libx264 (High profile, ≤1920 wide, 12M maxrate); copy
    // mode passes the probed parameters through.
    let served_video_codec = if video_transcoded {
        // High@L4.1 covers the ≤1080p output; a declared level above the
        // actual one is harmless (level support is backwards-compatible).
        Some("avc1.640029".to_string())
    } else {
        ffprobe::rfc6381_video_codec(
            probe.video_codec.as_deref(),
            probe.video_profile.as_deref(),
            probe.video_level,
        )
    };
    let served_audio_codec = if audio_transcoded {
        Some("mp4a.40.2".to_string())
    } else {
        ffprobe::rfc6381_audio_codec(audio_codec.as_deref())
    };
    let master_codecs = match (served_video_codec, served_audio_codec) {
        (Some(v), Some(a)) => Some(format!("{v},{a}")),
        _ => None,
    };
    let resolution = match (probe.width, probe.height) {
        (Some(w), Some(h)) if w > 0 && h > 0 => {
            if video_transcoded && w > 1920 {
                // Mirror the tonemap filter's `scale=min(iw,1920):-2`.
                Some((1920, (h * 1920 / w) / 2 * 2))
            } else {
                Some((w, h))
            }
        }
        _ => None,
    };
    let bandwidth_bps = if video_transcoded {
        // 12M video maxrate plus audio headroom.
        Some(14_000_000)
    } else {
        // Remuxes burst above the container average; pad by 25%.
        probe.bit_rate_bps.map(|b| b + b / 4)
    };
    // Embedded text subtitles matching the requested languages ride along as
    // extra WebVTT outputs of the same ffmpeg — release-accurate, no
    // OpenSubtitles quota. One stream per language, first match wins.
    let mut embedded_subtitles: Vec<(i64, String)> = Vec::new();
    for language in subtitle_languages {
        let lang = ffprobe::primary_language_code(language);
        if embedded_subtitles.iter().any(|(_, l)| *l == lang) {
            continue;
        }
        if let Some(stream) = ffprobe::select_embedded_subtitle(&probe.subtitle_streams, &lang) {
            embedded_subtitles.push((stream.stream_index, lang));
        }
    }
    session.set_info(MediaInfo {
        duration_secs: probe.duration_secs,
        video_codec: probe.video_codec.clone(),
        audio_codec,
        audio_transcoded,
        audio_stream_index,
        fps: probe.fps,
        video_range: if video_transcoded {
            "SDR".to_string()
        } else {
            probe.video_range
        },
        video_transcoded,
        master_codecs,
        resolution,
        bandwidth_bps,
        embedded_subtitles: embedded_subtitles.clone(),
        chapters: probe.chapters,
        intro_end_secs: probe.intro_end_secs,
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
            audio_stream_index,
            subtitle_extractions: embedded_subtitle_extractions(session, &embedded_subtitles, 0.0),
        },
    )
    .await?;
    if let Some(timer) = timer {
        timer.mark("ffmpeg_spawn");
    }
    register_embedded_subtitle_tracks(session, &embedded_subtitles).await;
    Ok(())
}

/// Extraction specs for [`SpawnOptions`]: one growing-VTT fragment file per
/// embedded stream, named by the (re)start position so fragments from seek
/// restarts coexist and the window slicer can merge them.
fn embedded_subtitle_extractions(
    session: &Arc<Session>,
    embedded: &[(i64, String)],
    start_secs: f64,
) -> Vec<(i64, std::path::PathBuf)> {
    embedded
        .iter()
        .map(|(index, lang)| {
            (
                *index,
                session
                    .temp_dir
                    .join(format!("sub_emb_{lang}_f{:06}.vtt", start_secs as u64)),
            )
        })
        .collect()
}

/// Write the windowed rendition playlist and record a [`SubtitleTrack`] per
/// embedded extraction, so the master playlist advertises them. Runs before
/// `auto_attach_subtitles`, which then skips languages already covered.
async fn register_embedded_subtitle_tracks(session: &Arc<Session>, embedded: &[(i64, String)]) {
    let duration = session.info().duration_secs;
    for (_, lang) in embedded {
        let playlist_name = format!("sub_emb_{lang}.m3u8");
        let playlist = subtitles::embedded_subtitle_playlist(lang, duration);
        if let Err(error) =
            tokio::fs::write(session.temp_dir.join(&playlist_name), playlist.as_bytes()).await
        {
            tracing::warn!(session = %session.id, %lang, %error, "writing embedded subtitle playlist failed");
            continue;
        }
        let default = session.subtitle_tracks().is_empty();
        session.add_subtitle_track(crate::subtitles::SubtitleTrack {
            language: lang.clone(),
            name: format!("{} (embedded)", subtitles::language_display_name(lang)),
            playlist_name,
            vtt_name: format!("sub_emb_{lang}_f000000.vtt"),
            key: format!("emb_{lang}"),
            base_vtt: String::new(),
            offset_ms: 0,
            default,
        });
        tracing::info!(session = %session.id, %lang, "attached embedded subtitle");
    }
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
    /// End (seconds) of the media's intro/opening chapter for "Skip Intro", or
    /// `null` when the release has no intro chapter.
    pub intro_end_secs: Option<f64>,
    /// Embedded chapter markers from the probe, in file order (empty when none).
    pub chapters: Vec<ChapterInfo>,
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
    // A client polling status is expressing interest: keep the idle reaper
    // away, like every other session route. This is what lets a client
    // pre-create the next episode's session (autoplay) minutes before it is
    // played — without the touch it would be reaped at the idle timeout.
    session.touch();
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
        intro_end_secs: session.intro_end_secs(),
        chapters: info
            .chapters
            .iter()
            .map(ChapterInfo::from_chapter)
            .collect(),
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
    // Grab the session before teardown removes it, so the final Trakt
    // scrobble (stop, with the last reported progress) can still fire.
    let session = state.sessions.get(&session_id);
    if state.sessions.teardown(&session_id).await {
        if let Some(session) = session {
            super::trakt::spawn_scrobble(&state, &session, crate::trakt::ScrobbleAction::Stop);
        }
        Ok(StatusCode::NO_CONTENT)
    } else {
        Err(AppError::NotFound(format!("session {session_id}")))
    }
}

// ---- HLS playlists and segments ----------------------------------------------

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

/// `?apikey=` propagation. Header-less players (AVPlayer) authenticate via
/// the query parameter, but they resolve child playlist/segment URIs
/// relative to the parent URL — which drops the query string (RFC 3986).
/// Since every /api/v1 route requires the key, the playlists must re-embed
/// the presented key into every URI they reference, or each follow-up
/// request 401s and playback is a black screen.
#[derive(Debug, Deserialize)]
pub struct ApiKeyParam {
    apikey: Option<String>,
    /// User bearer token — the web client's media requests authenticate by
    /// query because hls.js segment fetches cannot always carry headers.
    token: Option<String>,
}

impl ApiKeyParam {
    /// `?apikey=<encoded>` / `?token=<encoded>` when the request
    /// authenticated by query, else "". Header-authenticated clients keep
    /// sending the header themselves.
    fn uri_suffix(&self) -> String {
        if let Some(key) = &self.apikey {
            return format!("?apikey={}", percent_encode_component(key));
        }
        if let Some(token) = &self.token {
            return format!("?token={}", percent_encode_component(token));
        }
        String::new()
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
    // AVPlayer assumes SDR when VIDEO-RANGE is absent and rejects the stream
    // ("video range specified by playlist is less than actual format
    // description") once the format description says PQ/HLG. The builder also
    // advertises any attached external subtitle renditions; the ?apikey=
    // suffix is re-embedded into every URI so header-less players keep auth.
    let info = session.info();
    let variant = subtitles::hls::MasterVariant {
        bandwidth_bps: info.bandwidth_bps,
        codecs: info.master_codecs.clone(),
        resolution: info.resolution,
        video_range: if info.video_range.is_empty() {
            "SDR".to_string()
        } else {
            info.video_range.clone()
        },
    };
    let master = subtitles::master_playlist(&session.subtitle_tracks(), &variant);
    let master = playlist_with_suffix(&master, &auth.uri_suffix());
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
    let suffix = auth.uri_suffix();
    // With a known duration the playlist is synthesized as full VOD; the
    // ffmpeg-written playlist only backs sources ffprobe could not time.
    if let Some(duration) = session.info().duration_secs.filter(|d| *d > 0.0) {
        let body = vod_playlist(duration, &suffix);
        return Ok(([(header::CONTENT_TYPE, PLAYLIST_CONTENT_TYPE)], body).into_response());
    }
    match tokio::fs::read_to_string(session.playlist_path()).await {
        Ok(text) => {
            let body = playlist_with_suffix(&text, &suffix);
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

/// One file from the session dir: `init.mp4`, `seg_NNNNN.m4s`, or an external
/// subtitle rendition (`sub_<lang>_<n>.m3u8` / `sub_<lang>_<n>.vtt`).
#[utoipa::path(get, path = "/stream/{session_id}/{segment}", tag = "streaming",
    params(
        ("session_id" = Uuid, Path, description = "Session id"),
        ("segment" = String, Path, description = "`init.mp4`, `seg_NNNNN.m4s`, `sub_<lang>_<n>.m3u8` or `sub_<lang>_<n>.vtt`"),
    ),
    responses(
        (status = 200, description = "fMP4 init/media segment, subtitle playlist or WebVTT"),
        (status = 400, description = "Invalid file name"),
        (status = 404),
    ))]
pub async fn hls_segment(
    State(state): State<AppState>,
    Path((session_id, segment)): Path<(Uuid, String)>,
    Query(auth): Query<ApiKeyParam>,
) -> AppResult<Response> {
    let session = get_session(&state, &session_id)?;
    // Strict allowlist: the path parameter must never escape the temp dir.
    // Both regexes are fully anchored with no path separators.
    // Embedded-subtitle windows are sliced from the growing extraction
    // fragments at request time (they are not files on disk).
    if let Some(captures) = EMBEDDED_SUB_WINDOW.captures(&segment) {
        session.touch();
        let language = captures.get(1).expect("lang group").as_str();
        let window: u64 = captures
            .get(2)
            .expect("window group")
            .as_str()
            .parse()
            .map_err(|_| AppError::BadRequest(format!("invalid window in '{segment}'")))?;
        let vtt = embedded_window(&session, language, window).await;
        return Ok(([(header::CONTENT_TYPE, VTT_CONTENT_TYPE)], vtt).into_response());
    }
    let content_type = if SEGMENT_NAME.is_match(&segment) {
        "video/mp4"
    } else if SUBTITLE_FILE_NAME.is_match(&segment) || EMBEDDED_SUB_PLAYLIST.is_match(&segment) {
        if segment.ends_with(".vtt") {
            VTT_CONTENT_TYPE
        } else {
            PLAYLIST_CONTENT_TYPE
        }
    } else {
        return Err(AppError::BadRequest(format!(
            "invalid segment name '{segment}'"
        )));
    };
    session.touch();
    // Subtitle renditions are written straight to the session dir (not produced
    // by ffmpeg), so serve them from disk. The child playlist references the
    // .vtt by a relative URI, so header-less players need the ?apikey=
    // re-embedded like the media playlist; the .vtt itself is served as-is.
    if content_type != "video/mp4" {
        return match tokio::fs::read(session.temp_dir.join(&segment)).await {
            Ok(bytes) => {
                if segment.ends_with(".m3u8") {
                    let playlist =
                        playlist_with_suffix(&String::from_utf8_lossy(&bytes), &auth.uri_suffix());
                    Ok(([(header::CONTENT_TYPE, content_type)], playlist).into_response())
                } else {
                    Ok(([(header::CONTENT_TYPE, content_type)], bytes).into_response())
                }
            }
            Err(_) => Err(AppError::NotFound(format!("segment {segment}"))),
        };
    }

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

/// One embedded-subtitle window: merge all extraction fragments for the
/// language (one per ffmpeg (re)start position) and slice the window's time
/// range. Serves whatever has been extracted so far — the player requests
/// windows near the playhead, where extraction has already passed, so a
/// partially extracted window only occurs at the live edge (and yields the
/// cues known so far instead of an error).
async fn embedded_window(session: &Arc<Session>, language: &str, window: u64) -> String {
    let prefix = format!("sub_emb_{language}_f");
    let mut fragments = Vec::new();
    if let Ok(mut entries) = tokio::fs::read_dir(&session.temp_dir).await {
        while let Ok(Some(entry)) = entries.next_entry().await {
            let name = entry.file_name();
            let Some(name) = name.to_str() else { continue };
            if name.starts_with(&prefix) && name.ends_with(".vtt") {
                if let Ok(text) = tokio::fs::read_to_string(entry.path()).await {
                    fragments.push(text);
                }
            }
        }
    }
    let start = window as f64 * subtitles::EMBEDDED_WINDOW_SECS;
    subtitles::window_vtt(&fragments, start, start + subtitles::EMBEDDED_WINDOW_SECS)
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
                //
                // Restart damping: while a fresh ffmpeg has produced nothing
                // yet (empty window), hold restarts for 3s so a startup
                // burst can never wipe its own spawn. Once output exists, a
                // short 1s damper is enough — an out-of-window minimum
                // request then means the user really scrubbed somewhere
                // else, and every extra damping second is felt as "seeking
                // is stuck".
                let outside = target + seg <= window_start || target > window_end + 2.0 * seg;
                let producing = window_end > window_start;
                let damper = if producing {
                    std::time::Duration::from_secs(1)
                } else {
                    std::time::Duration::from_secs(3)
                };
                if outside
                    && session.min_requested() == Some(index)
                    && session.since_spawn() >= damper
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
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
}

// ---- Subtitle attach ----------------------------------------------------------

#[derive(Debug, Deserialize, ToSchema)]
pub struct AttachSubtitleRequest {
    /// OpenSubtitles `file_id` (from `GET /subtitles/search`).
    pub file_id: i64,
    /// ISO 639-1 language code (`en`, `de`, ...) for the track.
    pub language: String,
    /// Mark this as the default/auto-selected track (only honoured for the
    /// first attached track). Defaults to false.
    #[serde(default)]
    pub default: bool,
}

/// One subtitle track surfaced to the client (session responses).
#[derive(Debug, Serialize, ToSchema)]
pub struct SubtitleTrackInfo {
    pub language: String,
    pub name: String,
    /// Per-subtitle HLS media playlist URL (relative to the API root).
    pub playlist_url: String,
    pub default: bool,
}

impl SubtitleTrackInfo {
    fn from_track(session_id: &Uuid, track: &SubtitleTrack) -> Self {
        Self {
            language: track.language.clone(),
            name: track.name.clone(),
            playlist_url: format!("/api/v1/stream/{session_id}/{}", track.playlist_name),
            default: track.default,
        }
    }
}

#[derive(Debug, Serialize, ToSchema)]
pub struct AttachSubtitleResponse {
    /// The newly attached track.
    pub track: SubtitleTrackInfo,
    /// Master playlist URL to (re)load so the new track shows up.
    pub hls_master_url: String,
    /// All subtitle tracks now attached to the session.
    pub subtitle_tracks: Vec<SubtitleTrackInfo>,
}

/// Attach an OpenSubtitles subtitle to a live session. The subtitle is
/// downloaded, converted to WebVTT and written into the session as a
/// single-segment HLS subtitle rendition; the master playlist then advertises
/// it via `#EXT-X-MEDIA:TYPE=SUBTITLES` so AVPlayer offers it natively.
/// Reload `hls_master_url` after this call.
#[utoipa::path(post, path = "/stream/{session_id}/subtitles", tag = "subtitles",
    params(("session_id" = Uuid, Path, description = "Session id")),
    request_body = AttachSubtitleRequest,
    responses(
        (status = 200, body = AttachSubtitleResponse),
        (status = 400, description = "Bad language, or OpenSubtitles API key not configured"),
        (status = 404, description = "Unknown session"),
        (status = 502, description = "OpenSubtitles upstream error"),
    ))]
pub async fn attach_subtitle(
    State(state): State<AppState>,
    Path(session_id): Path<Uuid>,
    Json(request): Json<AttachSubtitleRequest>,
) -> AppResult<Json<AttachSubtitleResponse>> {
    let session = get_session(&state, &session_id)?;
    session.touch();
    // Validate the language up-front so a bad code fails before any download.
    let language = subtitles::normalize_language(&request.language)?;
    let client = opensubtitles_client(&state).await?;
    let srt = download_subtitle(&state, &client, request.file_id).await?;
    // Manual attach: the user picked this subtitle explicitly, so no automatic
    // fps rescale (the standalone search carries no fps/hash signal). Drift, if
    // any, is corrected via the manual offset endpoint below.
    let track =
        subtitles::attach_subtitle(&session, &language, &srt.text, request.default, None).await?;

    let tracks = session.subtitle_tracks();
    Ok(Json(AttachSubtitleResponse {
        track: SubtitleTrackInfo::from_track(&session_id, &track),
        hls_master_url: format!("/api/v1/stream/{session_id}/master.m3u8"),
        subtitle_tracks: tracks
            .iter()
            .map(|t| SubtitleTrackInfo::from_track(&session_id, t))
            .collect(),
    }))
}

// ---- Manual subtitle offset ---------------------------------------------------

#[derive(Debug, Deserialize, ToSchema)]
pub struct SubtitleOffsetRequest {
    /// Absolute cumulative offset in milliseconds, relative to the subtitle's
    /// original timing (positive = later, negative = earlier). Each call
    /// replaces the previous offset — it is not a delta — so cue times never
    /// compound. Negative cue times clamp to `0`.
    pub ms: i64,
}

/// Nudge an attached subtitle track's timing. The track is addressed by
/// **language** (the `{language}` path segment, matched case-insensitively on
/// the primary subtag, e.g. `en` also matches `en-US`); when several subtitles
/// of the same language are attached, the first/selected one is targeted.
///
/// The track's WebVTT is re-emitted from its pristine base timing shifted by
/// `ms` and written back to the same `.vtt` (the HLS playlist URI is
/// unchanged), so the player just reloads the rendition. `ms` is absolute, not
/// a delta, so repeated nudges never accumulate rounding drift.
#[utoipa::path(post, path = "/stream/{session_id}/subtitles/{language}/offset", tag = "subtitles",
    params(
        ("session_id" = Uuid, Path, description = "Session id"),
        ("language" = String, Path, description = "Track language (primary subtag, e.g. `en`)"),
    ),
    request_body = SubtitleOffsetRequest,
    responses(
        (status = 200, body = SubtitleTrackInfo, description = "The updated subtitle track"),
        (status = 404, description = "Unknown session, or no subtitle track with that language"),
    ))]
pub async fn offset_subtitle(
    State(state): State<AppState>,
    Path((session_id, language)): Path<(Uuid, String)>,
    Json(request): Json<SubtitleOffsetRequest>,
) -> AppResult<Json<SubtitleTrackInfo>> {
    let session = get_session(&state, &session_id)?;
    session.touch();
    let track = session
        .subtitle_track_by_language(&language)
        .ok_or_else(|| {
            AppError::NotFound(format!(
                "subtitle track '{language}' on session {session_id}"
            ))
        })?;
    // Embedded tracks come from the release itself — always in sync, and
    // their cues live in growing extraction fragments, not a base VTT.
    if track.key.starts_with("emb_") {
        return Err(AppError::BadRequest(
            "embedded subtitles are release-accurate and cannot be offset".into(),
        ));
    }
    // Re-shift from the pristine base VTT by the (absolute) offset.
    let updated = subtitles::set_subtitle_offset(&session, &track.key, request.ms)
        .await?
        .ok_or_else(|| {
            AppError::NotFound(format!(
                "subtitle track '{language}' on session {session_id}"
            ))
        })?;
    Ok(Json(SubtitleTrackInfo::from_track(&session_id, &updated)))
}

// ---- Bad-release blacklist ------------------------------------------------------

#[derive(Debug, Default, Deserialize, ToSchema)]
pub struct BlacklistReleaseRequest {
    /// Optional free-text reason (e.g. `audio out of sync`), kept for the
    /// blacklist listing.
    pub reason: Option<String>,
}

#[derive(Debug, Serialize, ToSchema)]
pub struct BlacklistReleaseResponse {
    /// The release title that was blacklisted.
    pub release_title: String,
    /// False when the title was already on the blacklist.
    pub created: bool,
}

/// Mark the session's release as bad: its title goes on the release
/// blacklist, so automatic selection rejects it from now on (a manual
/// guid pin still overrides). The session keeps playing — after this call
/// the client typically reports its position, ends the session and starts
/// a new one, which then resolves to the next-best release and resumes.
#[utoipa::path(post, path = "/stream/{session_id}/blacklist", tag = "streaming",
    params(("session_id" = Uuid, Path, description = "Session id")),
    request_body(content = BlacklistReleaseRequest, description = "Optional reason"),
    responses((status = 200, body = BlacklistReleaseResponse), (status = 404)))]
pub async fn blacklist_session_release(
    State(state): State<AppState>,
    Path(session_id): Path<Uuid>,
    request: Option<Json<BlacklistReleaseRequest>>,
) -> AppResult<Json<BlacklistReleaseResponse>> {
    let session = get_session(&state, &session_id)?;
    session.touch();
    let reason = request.and_then(|Json(r)| r.reason);
    let created = db::release_blacklist::add(
        &state.db,
        &db::release_blacklist::NewBlacklistedRelease {
            title: &session.release_title,
            tmdb_id: Some(session.tmdb_id),
            media_type: Some(session.media_type.as_str()),
            season: session.season,
            episode: session.episode,
            reason: reason.as_deref(),
        },
    )
    .await?;
    tracing::info!(
        session = %session.id,
        release = %session.release_title,
        created,
        "release marked as bad"
    );
    Ok(Json(BlacklistReleaseResponse {
        release_title: session.release_title.clone(),
        created,
    }))
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
            audio_stream_index: info.audio_stream_index,
            subtitle_extractions: embedded_subtitle_extractions(
                session,
                &info.embedded_subtitles,
                target,
            ),
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

/// Delete the video playlist + segments inside the session dir (keeping the
/// directory itself), but preserve attached external subtitle renditions
/// (`sub_*.m3u8` / `sub_*.vtt`) so they survive a seek restart.
async fn wipe_dir(dir: &FsPath) -> AppResult<()> {
    let mut entries = tokio::fs::read_dir(dir)
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("reading session dir: {e}")))?;
    while let Ok(Some(entry)) = entries.next_entry().await {
        if entry
            .file_name()
            .to_str()
            .is_some_and(|name| name.starts_with("sub_"))
        {
            continue;
        }
        if let Err(e) = tokio::fs::remove_file(entry.path()).await {
            tracing::warn!(path = %entry.path().display(), error = %e, "failed to remove file");
        }
    }
    Ok(())
}

/// One active streaming session, for the admin dashboard.
#[derive(Debug, Serialize, ToSchema)]
pub struct ActiveSession {
    pub session_id: Uuid,
    pub tmdb_id: i64,
    pub media_type: MediaType,
    pub season: Option<u32>,
    pub episode: Option<u32>,
    pub release_title: String,
    /// `starting`, `ready`, `failed` or `ended`.
    pub state: String,
    /// True while the video is being tone-mapped / transcoded (heavier CPU).
    pub video_transcoded: bool,
    pub audio_transcoded: bool,
    /// Finished HLS segments on disk (rough progress signal).
    pub segments_ready: usize,
    /// Seconds since the last client request touched this session.
    pub idle_secs: u64,
}

/// List the currently active streaming sessions (admin dashboard).
#[utoipa::path(get, path = "/stream/sessions", tag = "streaming",
    responses((status = 200, body = [ActiveSession])))]
pub async fn list_sessions(State(state): State<AppState>) -> AppResult<Json<Vec<ActiveSession>>> {
    let mut out = Vec::new();
    for session in state.sessions.snapshot() {
        let info = session.info();
        out.push(ActiveSession {
            session_id: session.id,
            tmdb_id: session.tmdb_id,
            media_type: session.media_type,
            season: session.season,
            episode: session.episode,
            release_title: session.release_title.clone(),
            state: session.state().label().to_string(),
            video_transcoded: info.video_transcoded,
            audio_transcoded: info.audio_transcoded,
            segments_ready: count_segments(&session.temp_dir).await,
            idle_secs: session.idle_for().as_secs(),
        });
    }
    // Most recently active first.
    out.sort_by_key(|s| s.idle_secs);
    Ok(Json(out))
}

pub fn router() -> OpenApiRouter<AppState> {
    OpenApiRouter::new()
        .routes(routes!(create_session, list_sessions))
        .routes(routes!(session_status, delete_session))
        .routes(routes!(master_playlist))
        .routes(routes!(media_playlist))
        .routes(routes!(raw_media))
        .routes(routes!(seek_session))
        .routes(routes!(blacklist_session_release))
        .routes(routes!(attach_subtitle))
        .routes(routes!(offset_subtitle))
        .routes(routes!(hls_segment))
}

#[cfg(test)]
mod tests {
    use super::*;

    use async_trait::async_trait;
    use bytes::Bytes;

    use crate::config::AppConfig;
    use crate::error::AppResult;
    use crate::vfs::VirtualFile;

    struct NullFile;

    #[async_trait]
    impl VirtualFile for NullFile {
        fn len(&self) -> u64 {
            0
        }
        async fn read_at(&self, _offset: u64, _buf_len: usize) -> AppResult<Bytes> {
            Ok(Bytes::new())
        }
    }

    async fn tv_session(
        dir: &std::path::Path,
        tmdb_id: i64,
        season: u32,
        episode: u32,
    ) -> Arc<Session> {
        Session::create(
            NewSession {
                media: Arc::new(NullFile),
                user_id: 1,
                tmdb_id,
                media_type: MediaType::Tv,
                season: Some(season),
                episode: Some(episode),
                release_title: "Show.S01E02".into(),
                inner_file_name: "ep.mkv".into(),
                resume_position_secs: 0.0,
            },
            Some(dir.to_str().unwrap()),
        )
        .await
        .expect("session")
    }

    /// A cached season intro is applied synchronously on session start (no
    /// fpcalc needed), so `intro_end_secs()` reflects it before the response is
    /// built.
    #[tokio::test]
    async fn cached_season_intro_is_applied_on_start() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = AppConfig::default();
        config.auth.api_key = "k".into();
        let state = AppState::for_tests(config).await.expect("state");

        // Seed a detected intro for (tmdb 500, season 1).
        db::fingerprints::upsert_season_intro(
            &state.db,
            500,
            1,
            db::fingerprints::SeasonIntro {
                intro_start_secs: 3.0,
                intro_end_secs: 88.0,
            },
        )
        .await
        .unwrap();

        // A fresh episode session of that season has no intro yet …
        let session = tv_session(dir.path(), 500, 1, 2).await;
        assert_eq!(session.intro_end_secs(), None);

        // … until intro detection applies the cached value (cache path is
        // synchronous and never touches fpcalc).
        apply_intro_detection(&state, &session).await;
        assert_eq!(session.intro_end_secs(), Some(88.0));
    }

    /// With no cached intro and no sibling fingerprint, detection stores this
    /// episode's fingerprint but leaves `intro_end_secs` unset (needs a 2nd
    /// episode). fpcalc is skip-gated: the test only asserts the no-cache path
    /// does not panic and leaves the intro unset when fingerprinting is a no-op.
    #[tokio::test]
    async fn missing_fpcalc_leaves_intro_unset() {
        let dir = tempfile::tempdir().unwrap();
        let mut config = AppConfig::default();
        config.auth.api_key = "k".into();
        // A binary that certainly does not exist → fingerprinting fails softly.
        config.analysis.fpcalc_path = "fpcalc-does-not-exist-xyz".into();
        let state = AppState::for_tests(config).await.expect("state");

        let session = tv_session(dir.path(), 501, 1, 1).await;
        apply_intro_detection(&state, &session).await;
        // The detached task may still be running; give it a moment to fail.
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
        assert_eq!(session.intro_end_secs(), None);
    }

    #[test]
    fn packaging_reorder_prefers_unpacked_within_band() {
        use crate::nzb::NzbFileRef;
        use crate::release::parse::parse_release_name;

        fn candidate(title: &str, score: i64) -> RankedRelease {
            let raw = RawRelease {
                title: title.into(),
                guid: format!("guid-{title}"),
                nzb_url: format!("https://x/{title}.nzb"),
                size_bytes: None,
                posted_at: None,
                indexer_id: 1,
                indexer_name: "test".into(),
                tvdb_id: None,
                imdb_id: None,
                file_count: None,
            };
            let parsed = parse_release_name(&raw.title);
            RankedRelease {
                raw,
                parsed,
                score,
                rejected: None,
            }
        }
        fn plain() -> (Nzb, MainContent) {
            (
                Nzb { files: Vec::new() },
                MainContent::Plain(NzbFileRef {
                    index: 0,
                    file_name: "movie.mkv".into(),
                    bytes: 1,
                }),
            )
        }
        fn rar() -> (Nzb, MainContent) {
            (Nzb { files: Vec::new() }, MainContent::RarSet(Vec::new()))
        }

        // An unpacked release within the band moves ahead of RAR sets.
        let mut to_try = vec![
            candidate("Rar.Top", 1850),
            candidate("Plain.Second", 1850),
            candidate("Rar.Third", 1800),
            candidate("Plain.Tail.Not.Pregrabbed", 1799),
        ];
        let pregrabbed = std::collections::HashMap::from([
            ("guid-Rar.Top".to_string(), rar()),
            ("guid-Plain.Second".to_string(), plain()),
            ("guid-Rar.Third".to_string(), rar()),
        ]);
        reorder_by_packaging(&mut to_try, &pregrabbed);
        let titles: Vec<&str> = to_try.iter().map(|c| c.raw.title.as_str()).collect();
        assert_eq!(
            titles,
            [
                "Plain.Second",
                "Rar.Top",
                "Rar.Third",
                "Plain.Tail.Not.Pregrabbed"
            ]
        );

        // Out of the score band, packaging never displaces a better release.
        let mut to_try = vec![
            candidate("Rar.Much.Better", 2000),
            candidate("Plain.Much.Worse", 1700),
        ];
        let pregrabbed = std::collections::HashMap::from([
            ("guid-Rar.Much.Better".to_string(), rar()),
            ("guid-Plain.Much.Worse".to_string(), plain()),
        ]);
        reorder_by_packaging(&mut to_try, &pregrabbed);
        assert_eq!(to_try[0].raw.title, "Rar.Much.Better");

        // Unknown packaging (pre-grab failed) keeps rank order.
        let mut to_try = vec![candidate("Rar.Top", 1850), candidate("Unknown", 1850)];
        let pregrabbed = std::collections::HashMap::from([("guid-Rar.Top".to_string(), rar())]);
        reorder_by_packaging(&mut to_try, &pregrabbed);
        assert_eq!(to_try[0].raw.title, "Rar.Top");
    }

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
            token: None,
            apikey: Some("k/e y+&=?".into()),
        };
        assert_eq!(auth.uri_suffix(), "?apikey=k%2Fe%20y%2B%26%3D%3F");
        assert_eq!(
            ApiKeyParam {
                apikey: None,
                token: None
            }
            .uri_suffix(),
            ""
        );
    }

    #[test]
    fn fps_rescale_rules() {
        // Hash-matched: never rescale, even with a known fps mismatch.
        assert_eq!(fps_rescale(Some(23.976), Some(25.0), true), None);
        // Matching (within 0.1) fps: no rescale.
        assert_eq!(fps_rescale(Some(23.976), Some(24.0), false), None);
        // Unknown media or subtitle fps: no rescale.
        assert_eq!(fps_rescale(None, Some(25.0), false), None);
        assert_eq!(fps_rescale(Some(23.976), None, false), None);
        // A genuine mismatch: scale by media/subtitle.
        let scale = fps_rescale(Some(23.976), Some(25.0), false).expect("rescale");
        assert!((scale - 23.976 / 25.0).abs() < 1e-9);
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
