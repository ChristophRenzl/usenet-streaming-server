//! Server-side download jobs: sequentially copy a resolved release's
//! virtual file (the same source the streaming layer plays from) into the
//! download directory, with DB-backed progress, cancellation and crash
//! recovery.

pub mod name;

use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use dashmap::DashMap;
use tokio::io::AsyncWriteExt;
use tokio_util::sync::CancellationToken;
use uuid::Uuid;

use crate::db;
use crate::error::{AppError, AppResult};
use crate::nzb::{MainContent, Nzb};
use crate::state::AppState;
use crate::stream::open_media_source;

pub use name::sanitize_file_name;

/// Sequential read size. Large enough to keep the NNTP pipeline busy, small
/// enough to react to cancellation promptly.
const CHUNK_SIZE: usize = 4 * 1024 * 1024;
/// Progress rows are written at most once per interval ...
const PROGRESS_INTERVAL: Duration = Duration::from_secs(1);
/// ... unless this many new bytes arrived first.
const PROGRESS_BYTES: u64 = 16 * 1024 * 1024;

/// Everything a spawned job needs besides the shared state: the parsed NZB
/// and its selected main content (already health-checked by the API layer).
pub struct DownloadJob {
    pub nzb: Nzb,
    pub main: MainContent,
}

struct RunningJob {
    cancel: CancellationToken,
    handle: tokio::task::JoinHandle<()>,
}

/// In-process registry of running download tasks. Cheap to clone; lives on
/// [`AppState`]. Completed/failed jobs remove themselves.
#[derive(Clone, Default)]
pub struct DownloadManager {
    jobs: Arc<DashMap<Uuid, RunningJob>>,
}

impl DownloadManager {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn is_running(&self, id: &Uuid) -> bool {
        self.jobs.contains_key(id)
    }

    /// Spawn the job task for an already-inserted `pending` row.
    pub fn spawn(&self, state: AppState, id: Uuid, job: DownloadJob) {
        let cancel = CancellationToken::new();
        let token = cancel.clone();
        let jobs = self.jobs.clone();
        // The task waits for its registration below, so a job that finishes
        // instantly cannot leave a stale entry behind.
        let (registered_tx, registered_rx) = tokio::sync::oneshot::channel::<()>();
        let handle = tokio::spawn(async move {
            let _ = registered_rx.await;
            run(state, id, job, token).await;
            jobs.remove(&id);
        });
        self.jobs.insert(id, RunningJob { cancel, handle });
        let _ = registered_tx.send(());
    }

    /// Cancel a running job and wait for it to clean up (partial file
    /// removed, row marked cancelled). Returns false when no job with this
    /// id is running.
    pub async fn cancel(&self, id: &Uuid) -> bool {
        let Some((_, job)) = self.jobs.remove(id) else {
            return false;
        };
        job.cancel.cancel();
        if let Err(error) = job.handle.await {
            tracing::error!(download = %id, %error, "download task panicked during cancel");
        }
        true
    }
}

enum Outcome {
    Complete,
    Failed(AppError),
    Cancelled,
}

async fn run(state: AppState, id: Uuid, job: DownloadJob, cancel: CancellationToken) {
    // `execute` records the partial path here as soon as it exists so the
    // cancellation/failure paths below can always clean it up.
    let partial_slot: Arc<Mutex<Option<PathBuf>>> = Arc::default();

    // Cancellation is cooperative inside `execute`: only network waits race
    // against the token. Local file operations always run to completion —
    // dropping a tokio::fs future mid-await would leave its spawn_blocking
    // op running detached, e.g. creating the `.partial` *after* cleanup.
    let outcome = match execute(&state, &id, &job, &partial_slot, &cancel).await {
        Ok(false) => Outcome::Cancelled,
        Ok(true) => Outcome::Complete,
        Err(error) => Outcome::Failed(error),
    };

    let id_text = id.to_string();
    let db_result = match outcome {
        Outcome::Complete => {
            tracing::info!(download = %id, "download complete");
            Ok(()) // execute already marked the row complete
        }
        Outcome::Failed(error) => {
            tracing::warn!(download = %id, %error, "download failed");
            remove_partial(&partial_slot).await;
            db::downloads::mark_failed(&state.db, &id_text, &error.to_string()).await
        }
        Outcome::Cancelled => {
            tracing::info!(download = %id, "download cancelled");
            remove_partial(&partial_slot).await;
            db::downloads::mark_cancelled(&state.db, &id_text).await
        }
    };
    if let Err(error) = db_result {
        tracing::error!(download = %id, %error, "failed to record download outcome");
    }
}

async fn remove_partial(slot: &Mutex<Option<PathBuf>>) {
    let path = slot.lock().expect("partial slot lock").take();
    if let Some(path) = path {
        if let Err(error) = tokio::fs::remove_file(&path).await {
            if error.kind() != std::io::ErrorKind::NotFound {
                tracing::warn!(path = %path.display(), %error, "failed to remove partial file");
            }
        }
    }
}

/// The happy path: open the virtual file, stream it into `.partial`,
/// atomically rename and mark the row complete. Returns `Ok(false)` when the
/// cancellation token fired first. Only network-bound awaits are raced
/// against the token; file-system operations run to completion so no path
/// escapes the cleanup slot (see `run`).
async fn execute(
    state: &AppState,
    id: &Uuid,
    job: &DownloadJob,
    partial_slot: &Mutex<Option<PathBuf>>,
    cancel: &CancellationToken,
) -> AppResult<bool> {
    let id_text = id.to_string();
    let open = open_media_source(
        &job.nzb,
        &job.main,
        &state.nntp_pool,
        &state.segment_cache,
        state.config.streaming.readahead_segments,
    );
    let source = tokio::select! {
        _ = cancel.cancelled() => return Ok(false),
        source = open => source?,
    };
    let total = source.file.len();
    db::downloads::mark_downloading(&state.db, &id_text, total as i64).await?;

    let dir = PathBuf::from(&state.config.storage.download_dir);
    let (final_path, mut file) = name::allocate_destination(&dir, &source.inner_file_name).await?;
    let partial = name::partial_path(&final_path);
    *partial_slot.lock().expect("partial slot lock") = Some(partial.clone());

    let io_error = |what: &'static str| {
        let path = partial.display().to_string();
        move |e: std::io::Error| AppError::Internal(anyhow::anyhow!("{what} {path}: {e}"))
    };

    let mut offset: u64 = 0;
    let mut last_report = Instant::now();
    let mut last_reported: u64 = 0;
    while offset < total {
        let chunk = tokio::select! {
            _ = cancel.cancelled() => return Ok(false),
            chunk = source.file.read_at(offset, CHUNK_SIZE) => chunk?,
        };
        if chunk.is_empty() {
            return Err(AppError::Internal(anyhow::anyhow!(
                "unexpected end of media at byte {offset} of {total}"
            )));
        }
        file.write_all(&chunk).await.map_err(io_error("writing"))?;
        offset += chunk.len() as u64;

        if offset - last_reported >= PROGRESS_BYTES || last_report.elapsed() >= PROGRESS_INTERVAL {
            db::downloads::set_progress(&state.db, &id_text, offset as i64).await?;
            last_reported = offset;
            last_report = Instant::now();
        }
    }

    file.flush().await.map_err(io_error("flushing"))?;
    file.sync_all().await.map_err(io_error("syncing"))?;
    drop(file);
    tokio::fs::rename(&partial, &final_path)
        .await
        .map_err(|e| {
            AppError::Internal(anyhow::anyhow!(
                "renaming {} into place: {e}",
                partial.display()
            ))
        })?;
    *partial_slot.lock().expect("partial slot lock") = None;

    db::downloads::mark_complete(&state.db, &id_text, &final_path.to_string_lossy()).await?;
    Ok(true)
}
