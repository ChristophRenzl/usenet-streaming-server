//! Session state and the [`SessionManager`] registry with its idle reaper.

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::{Duration, Instant};

use dashmap::DashMap;
use rand::Rng;
use uuid::Uuid;

use crate::error::{AppError, AppResult};
use crate::tmdb::models::MediaType;
use crate::vfs::VirtualFile;

/// How many trailing ffmpeg stderr lines are kept for error reporting.
const STDERR_TAIL_LINES: usize = 50;

/// Lifecycle of a playback session.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionState {
    /// ffmpeg is spawned but has not produced a playlist yet.
    Starting,
    /// The media playlist exists; clients can play.
    Ready,
    /// ffmpeg exited non-zero or a mid-stream read failed hard.
    Failed(String),
    /// ffmpeg finished cleanly (VOD complete) or the session was torn down.
    Ended,
}

impl SessionState {
    pub fn label(&self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::Ready => "ready",
            Self::Failed(_) => "failed",
            Self::Ended => "ended",
        }
    }
}

/// Probe-derived facts about the media plus the transcode decision.
#[derive(Debug, Clone, Default)]
pub struct MediaInfo {
    pub duration_secs: Option<f64>,
    pub video_codec: Option<String>,
    pub audio_codec: Option<String>,
    pub audio_transcoded: bool,
    /// HLS `VIDEO-RANGE` (`PQ`, `HLG` or `SDR`); empty until probed.
    /// Reflects the *served* stream: `SDR` when tone-mapping.
    pub video_range: String,
    /// True when the video is tone-mapped to SDR for an HDR-incapable client.
    pub video_transcoded: bool,
}

/// Everything needed to register a new session.
pub struct NewSession {
    pub media: Arc<dyn VirtualFile>,
    pub tmdb_id: i64,
    pub media_type: MediaType,
    pub season: Option<u32>,
    pub episode: Option<u32>,
    /// Title of the chosen release (for status/history).
    pub release_title: String,
    /// Name of the file actually being served (yEnc / RAR inner name).
    pub inner_file_name: String,
    pub resume_position_secs: f64,
}

/// One playback session. Shared between the HTTP handlers, the ffmpeg
/// monitor tasks and the reaper via `Arc`.
pub struct Session {
    pub id: Uuid,
    /// Per-session secret guarding the internal loopback VFS route.
    pub token: String,
    pub media: Arc<dyn VirtualFile>,
    pub tmdb_id: i64,
    pub media_type: MediaType,
    pub season: Option<u32>,
    pub episode: Option<u32>,
    pub release_title: String,
    pub inner_file_name: String,
    /// Container extension of the inner file (`mkv`, `mp4`, ...).
    pub container: String,
    /// Where ffmpeg writes `media.m3u8`, `init.mp4` and `seg_*.m4s`.
    pub temp_dir: PathBuf,
    pub resume_position_secs: f64,
    pub created_at: Instant,
    /// Set once after ffprobe ran.
    info: OnceLock<MediaInfo>,
    state: Mutex<SessionState>,
    last_access: Mutex<Instant>,
    /// `-ss` offset of the currently running ffmpeg (changes on seek).
    start_offset: Mutex<f64>,
    /// When ffmpeg was last (re)spawned; debounces restart storms when a
    /// scrub fires several out-of-window segment requests at once.
    last_spawn: Mutex<Instant>,
    /// Segment indexes with an in-flight request. The player fetches an
    /// ascending burst in parallel; only the LOWEST outstanding index may
    /// restart ffmpeg, everyone above waits for the sweep to reach them.
    requested: Mutex<std::collections::BTreeMap<u64, u32>>,
    /// Bumped on every ffmpeg (re)spawn so stale monitor tasks stand down.
    generation: AtomicU64,
    /// The running ffmpeg child; taken by whoever reaps it (monitor on
    /// natural exit, seek/teardown when killing).
    pub(crate) child: tokio::sync::Mutex<Option<tokio::process::Child>>,
    stderr_tail: Mutex<VecDeque<String>>,
    /// Serializes seek/teardown so kill+wipe+respawn is atomic.
    pub(crate) control: tokio::sync::Mutex<()>,
}

impl Session {
    /// Register a new session: allocate id + token and create the temp dir
    /// (under `session_dir_base` or the OS temp dir).
    pub async fn create(
        params: NewSession,
        session_dir_base: Option<&str>,
    ) -> AppResult<Arc<Self>> {
        let id = Uuid::new_v4();
        let token = format!("{:032x}", rand::rng().random::<u128>());
        let base = match session_dir_base {
            Some(dir) => PathBuf::from(dir),
            None => std::env::temp_dir().join("usenet-streamer"),
        };
        let temp_dir = base.join(id.to_string());
        tokio::fs::create_dir_all(&temp_dir).await.map_err(|e| {
            AppError::Internal(anyhow::anyhow!(
                "creating session dir {}: {e}",
                temp_dir.display()
            ))
        })?;

        let container = params
            .inner_file_name
            .rsplit_once('.')
            .map(|(_, ext)| ext.to_ascii_lowercase())
            .unwrap_or_default();

        Ok(Arc::new(Self {
            id,
            token,
            media: params.media,
            tmdb_id: params.tmdb_id,
            media_type: params.media_type,
            season: params.season,
            episode: params.episode,
            release_title: params.release_title,
            inner_file_name: params.inner_file_name,
            container,
            temp_dir,
            resume_position_secs: params.resume_position_secs,
            created_at: Instant::now(),
            info: OnceLock::new(),
            state: Mutex::new(SessionState::Starting),
            last_access: Mutex::new(Instant::now()),
            start_offset: Mutex::new(0.0),
            last_spawn: Mutex::new(Instant::now()),
            requested: Mutex::new(std::collections::BTreeMap::new()),
            generation: AtomicU64::new(0),
            child: tokio::sync::Mutex::new(None),
            stderr_tail: Mutex::new(VecDeque::new()),
            control: tokio::sync::Mutex::new(()),
        }))
    }

    pub fn playlist_path(&self) -> PathBuf {
        self.temp_dir.join("media.m3u8")
    }

    /// Set the probe result; only the first call wins.
    pub fn set_info(&self, info: MediaInfo) {
        let _ = self.info.set(info);
    }

    pub fn info(&self) -> MediaInfo {
        self.info.get().cloned().unwrap_or_default()
    }

    pub fn state(&self) -> SessionState {
        self.state.lock().expect("session state lock").clone()
    }

    pub fn set_state(&self, state: SessionState) {
        *self.state.lock().expect("session state lock") = state;
    }

    /// Starting -> Ready, only while `generation` is still current.
    pub fn mark_ready(&self, generation: u64) {
        if self.generation() != generation {
            return;
        }
        let mut state = self.state.lock().expect("session state lock");
        if *state == SessionState::Starting {
            *state = SessionState::Ready;
        }
    }

    /// Final state after a natural ffmpeg exit, only while `generation` is
    /// still current. A previously recorded failure is never overwritten.
    pub fn finish(&self, generation: u64, result: Result<(), String>) {
        if self.generation() != generation {
            return;
        }
        let mut state = self.state.lock().expect("session state lock");
        if matches!(*state, SessionState::Failed(_)) {
            return;
        }
        *state = match result {
            Ok(()) => SessionState::Ended,
            Err(message) => SessionState::Failed(message),
        };
    }

    /// Record a hard mid-stream failure (e.g. a segment missing on every
    /// provider) unless the session already ended.
    pub fn mark_stream_failure(&self, message: String) {
        let mut state = self.state.lock().expect("session state lock");
        if matches!(*state, SessionState::Starting | SessionState::Ready) {
            *state = SessionState::Failed(message);
        }
    }

    /// Bump the idle clock; called on playlist/segment/raw hits.
    pub fn touch(&self) {
        *self.last_access.lock().expect("session access lock") = Instant::now();
    }

    pub fn idle_for(&self) -> Duration {
        self.last_access
            .lock()
            .expect("session access lock")
            .elapsed()
    }

    pub fn start_offset(&self) -> f64 {
        *self.start_offset.lock().expect("session offset lock")
    }

    pub fn mark_spawned(&self) {
        *self.last_spawn.lock().expect("session spawn lock") = Instant::now();
    }

    pub fn begin_segment_request(&self, index: u64) {
        *self
            .requested
            .lock()
            .expect("session requested lock")
            .entry(index)
            .or_insert(0) += 1;
    }

    pub fn end_segment_request(&self, index: u64) {
        let mut requested = self.requested.lock().expect("session requested lock");
        if let Some(count) = requested.get_mut(&index) {
            *count -= 1;
            if *count == 0 {
                requested.remove(&index);
            }
        }
    }

    /// Lowest segment index with an in-flight request.
    pub fn min_requested(&self) -> Option<u64> {
        self.requested
            .lock()
            .expect("session requested lock")
            .keys()
            .next()
            .copied()
    }

    pub fn since_spawn(&self) -> std::time::Duration {
        self.last_spawn
            .lock()
            .expect("session spawn lock")
            .elapsed()
    }

    pub fn set_start_offset(&self, secs: f64) {
        *self.start_offset.lock().expect("session offset lock") = secs;
    }

    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    pub fn bump_generation(&self) -> u64 {
        self.generation.fetch_add(1, Ordering::SeqCst) + 1
    }

    pub fn push_stderr(&self, line: String) {
        let mut tail = self.stderr_tail.lock().expect("stderr tail lock");
        if tail.len() >= STDERR_TAIL_LINES {
            tail.pop_front();
        }
        tail.push_back(line);
    }

    pub fn clear_stderr(&self) {
        self.stderr_tail.lock().expect("stderr tail lock").clear();
    }

    /// Last `n` captured ffmpeg stderr lines, joined with newlines.
    pub fn stderr_tail(&self, n: usize) -> String {
        let tail = self.stderr_tail.lock().expect("stderr tail lock");
        let skip = tail.len().saturating_sub(n);
        tail.iter()
            .skip(skip)
            .cloned()
            .collect::<Vec<_>>()
            .join("\n")
    }

    /// Kill the running ffmpeg (if any) and reap it.
    pub async fn kill_ffmpeg(&self) {
        let child = self.child.lock().await.take();
        if let Some(mut child) = child {
            let _ = child.start_kill();
            let _ = child.wait().await;
        }
    }
}

impl std::fmt::Debug for Session {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Session")
            .field("id", &self.id)
            .field("release_title", &self.release_title)
            .field("inner_file_name", &self.inner_file_name)
            .field("state", &self.state())
            .finish_non_exhaustive()
    }
}

/// Kill ffmpeg and delete the session's temp dir. The session should already
/// be removed from the manager so no new requests can reach it.
pub async fn teardown_session(session: &Arc<Session>) {
    let _guard = session.control.lock().await;
    session.set_state(SessionState::Ended);
    session.kill_ffmpeg().await;
    if let Err(e) = tokio::fs::remove_dir_all(&session.temp_dir).await {
        if e.kind() != std::io::ErrorKind::NotFound {
            tracing::warn!(session = %session.id, error = %e, "failed to remove session dir");
        }
    }
}

/// Registry of live sessions plus the background idle reaper. Cheap to clone.
#[derive(Clone)]
pub struct SessionManager {
    sessions: Arc<DashMap<Uuid, Arc<Session>>>,
    idle_timeout: Duration,
}

impl SessionManager {
    /// Must be called from within a tokio runtime (spawns the idle reaper).
    pub fn new(idle_timeout: Duration) -> Self {
        let sessions: Arc<DashMap<Uuid, Arc<Session>>> = Arc::new(DashMap::new());
        spawn_reaper(&sessions, idle_timeout);
        Self {
            sessions,
            idle_timeout,
        }
    }

    pub fn insert(&self, session: Arc<Session>) {
        self.sessions.insert(session.id, session);
    }

    pub fn get(&self, id: &Uuid) -> Option<Arc<Session>> {
        self.sessions.get(id).map(|s| s.clone())
    }

    pub fn len(&self) -> usize {
        self.sessions.len()
    }

    pub fn is_empty(&self) -> bool {
        self.sessions.is_empty()
    }

    pub fn idle_timeout(&self) -> Duration {
        self.idle_timeout
    }

    /// Remove and fully tear down a session. Returns false when unknown.
    pub async fn teardown(&self, id: &Uuid) -> bool {
        match self.sessions.remove(id) {
            Some((_, session)) => {
                teardown_session(&session).await;
                true
            }
            None => false,
        }
    }
}

fn spawn_reaper(sessions: &Arc<DashMap<Uuid, Arc<Session>>>, idle_timeout: Duration) {
    let weak: Weak<DashMap<Uuid, Arc<Session>>> = Arc::downgrade(sessions);
    // Scan often enough that tiny timeouts (tests) are honored promptly.
    let interval = (idle_timeout / 2).clamp(Duration::from_millis(250), Duration::from_secs(15));
    tokio::spawn(async move {
        loop {
            tokio::time::sleep(interval).await;
            let Some(sessions) = weak.upgrade() else {
                break;
            };
            let expired: Vec<Arc<Session>> = sessions
                .iter()
                .filter(|entry| entry.value().idle_for() > idle_timeout)
                .map(|entry| entry.value().clone())
                .collect();
            for session in expired {
                if sessions.remove(&session.id).is_some() {
                    tracing::info!(session = %session.id, "reaping idle session");
                    teardown_session(&session).await;
                }
            }
        }
    });
}

#[cfg(test)]
mod tests {
    use async_trait::async_trait;
    use bytes::Bytes;

    use super::*;

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

    async fn test_session(dir: &std::path::Path) -> Arc<Session> {
        Session::create(
            NewSession {
                media: Arc::new(NullFile),
                tmdb_id: 1,
                media_type: MediaType::Movie,
                season: None,
                episode: None,
                release_title: "t".into(),
                inner_file_name: "movie.mkv".into(),
                resume_position_secs: 0.0,
            },
            Some(dir.to_str().unwrap()),
        )
        .await
        .expect("session")
    }

    #[tokio::test]
    async fn stderr_tail_is_bounded_and_ordered() {
        let dir = tempfile::tempdir().unwrap();
        let session = test_session(dir.path()).await;
        for i in 0..200 {
            session.push_stderr(format!("line {i}"));
        }
        let tail = session.stderr_tail(5);
        assert_eq!(tail, "line 195\nline 196\nline 197\nline 198\nline 199");
        assert_eq!(session.stderr_tail(1000).lines().count(), STDERR_TAIL_LINES);
    }

    #[tokio::test]
    async fn state_transitions_respect_generation_and_failures() {
        let dir = tempfile::tempdir().unwrap();
        let session = test_session(dir.path()).await;
        assert_eq!(session.state(), SessionState::Starting);
        assert_eq!(session.container, "mkv");

        // A stale generation cannot mark ready or finish.
        session.mark_ready(99);
        assert_eq!(session.state(), SessionState::Starting);
        session.mark_ready(0);
        assert_eq!(session.state(), SessionState::Ready);

        session.mark_stream_failure("segment gone".into());
        session.finish(0, Ok(()));
        assert!(matches!(session.state(), SessionState::Failed(_)));

        let generation = session.bump_generation();
        assert_eq!(generation, 1);
    }
}
