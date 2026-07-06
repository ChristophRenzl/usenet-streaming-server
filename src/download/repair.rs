//! Download-and-repair fallback: for a release too damaged to stream, fetch
//! everything needed to reconstruct the media into a working directory, run
//! `par2 repair` over it, then produce the final media file exactly like a
//! normal download so the disk-playback path can serve it.
//!
//! This is what SABnzbd does and the streaming path cannot: missing articles
//! are *expected* here. Each data file is written best-effort — present parts
//! land at their decoded yEnc offsets, gaps stay zero-filled — so par2 sees a
//! full-size, damaged file it can repair from the `.par2` recovery blocks
//! (whose own articles are almost always present).

use std::path::{Path, PathBuf};
use std::process::Stdio;

use tokio::io::{AsyncSeekExt, AsyncWriteExt};
use tokio_util::sync::CancellationToken;

use crate::db;
use crate::error::{AppError, AppResult};
use crate::nntp::{NntpError, NntpPool};
use crate::nzb::{
    classify, extract_filename, yenc_decode, FileKind, MainContent, Nzb, NzbFile, Segment,
};
use crate::rar::{build_archive_map, ReadAt};
use crate::state::AppState;
use crate::vfs::DiskFile;

use super::name;

/// Files whose bytes we must have on disk for par2 to repair the media set:
/// the media itself, RAR volumes, and the par2 recovery files. Junk (nfo,
/// sfv, samples, subtitles, unknown) is skipped — par2 verifies only the
/// files listed in its recovery set, and skipping keeps the working dir lean.
fn is_needed_for_repair(kind: FileKind) -> bool {
    matches!(kind, FileKind::Media | FileKind::RarVolume | FileKind::Par2)
}

/// Outcome of a repair job's core work, mirroring the plain download's
/// `Ok(bool)` convention: `Ok(false)` == cancelled, `Ok(true)` == complete.
pub(super) async fn execute(
    state: &AppState,
    id: &uuid::Uuid,
    nzb: &Nzb,
    main: &MainContent,
    work_slot: &std::sync::Mutex<Option<PathBuf>>,
    partial_slot: &std::sync::Mutex<Option<PathBuf>>,
    cancel: &CancellationToken,
) -> AppResult<bool> {
    let id_text = id.to_string();

    // A dedicated working directory under the download dir, cleaned up on
    // failure/cancel via `work_slot` (like the plain job's `.partial`).
    let download_dir = PathBuf::from(&state.config.storage.download_dir);
    tokio::fs::create_dir_all(&download_dir)
        .await
        .map_err(|e| internal("creating download dir", &download_dir, e))?;
    let work_dir = download_dir.join(format!(".repair-{id_text}"));
    // Start clean in case a crashed previous attempt left one behind.
    let _ = tokio::fs::remove_dir_all(&work_dir).await;
    tokio::fs::create_dir_all(&work_dir)
        .await
        .map_err(|e| internal("creating repair work dir", &work_dir, e))?;
    *work_slot.lock().expect("work slot lock") = Some(work_dir.clone());

    db::downloads::mark_downloading(&state.db, &id_text, 0).await?;
    db::downloads::set_phase(&state.db, &id_text, "downloading").await?;

    // ---- Phase 1: download everything needed, best-effort --------------------
    let needed: Vec<&NzbFile> = nzb
        .files
        .iter()
        .filter(|f| {
            extract_filename(&f.subject)
                .map(|name| is_needed_for_repair(classify(&name)))
                .unwrap_or(false)
        })
        .collect();
    if needed.is_empty() {
        return Err(AppError::NoRelease(
            "release has no media/rar/par2 files to repair".into(),
        ));
    }

    let total_bytes: u64 = needed.iter().map(|f| f.total_bytes()).sum();
    db::downloads::mark_downloading(&state.db, &id_text, total_bytes as i64).await?;

    let mut written: u64 = 0;
    for file in &needed {
        let outcome = tokio::select! {
            _ = cancel.cancelled() => return Ok(false),
            r = fetch_file_to_disk(&state.nntp_pool, file, &work_dir, cancel) => r?,
        };
        match outcome {
            FetchOutcome::Cancelled => return Ok(false),
            FetchOutcome::Written { bytes } => {
                written += bytes;
                db::downloads::set_progress(&state.db, &id_text, written as i64).await?;
            }
        }
    }

    // ---- Phase 2: par2 repair -----------------------------------------------
    db::downloads::set_phase(&state.db, &id_text, "repairing").await?;
    tokio::select! {
        _ = cancel.cancelled() => return Ok(false),
        r = run_par2_repair(&state.config.streaming.par2_path, &work_dir) => r?,
    }

    // ---- Phase 3: produce the final media file ------------------------------
    let repaired_media = match main {
        MainContent::Plain(file_ref) => {
            // The repaired file on disk IS the media. Its on-disk name is the
            // decoded yEnc name.
            let media_name = repaired_plain_name(&nzb.files[file_ref.index]);
            let path = work_dir.join(name::sanitize_file_name(&media_name));
            if !tokio::fs::try_exists(&path).await.unwrap_or(false) {
                return Err(AppError::Internal(anyhow::anyhow!(
                    "par2 repair reported success but media file {} is missing",
                    path.display()
                )));
            }
            path
        }
        MainContent::RarSet(_) => {
            db::downloads::set_phase(&state.db, &id_text, "extracting").await?;
            extract_store_rar(&work_dir).await?
        }
    };

    // Move the repaired media into the download dir under a collision-free
    // name, exactly like a normal completed download, then mark complete.
    let inner_name = repaired_media
        .file_name()
        .map(|n| n.to_string_lossy().into_owned())
        .unwrap_or_else(|| "download".to_string());
    let (final_path, partial_file) = name::allocate_destination(&download_dir, &inner_name).await?;
    let partial = name::partial_path(&final_path);
    *partial_slot.lock().expect("partial slot lock") = Some(partial.clone());
    drop(partial_file); // we copy over it, not append

    tokio::fs::copy(&repaired_media, &partial)
        .await
        .map_err(|e| internal("copying repaired media", &partial, e))?;
    tokio::fs::rename(&partial, &final_path)
        .await
        .map_err(|e| internal("renaming repaired media into place", &partial, e))?;
    *partial_slot.lock().expect("partial slot lock") = None;

    // Working dir no longer needed.
    let _ = tokio::fs::remove_dir_all(&work_dir).await;
    *work_slot.lock().expect("work slot lock") = None;

    db::downloads::mark_complete(&state.db, &id_text, &final_path.to_string_lossy()).await?;
    Ok(true)
}

enum FetchOutcome {
    Written { bytes: u64 },
    Cancelled,
}

/// Fetch one NZB file's segments to `<work_dir>/<decoded name>`, best-effort:
/// present parts are written at their decoded yEnc offsets, missing articles
/// leave a zero-filled gap (par2 repairs those). Returns the number of bytes
/// actually written (for progress).
async fn fetch_file_to_disk(
    pool: &NntpPool,
    file: &NzbFile,
    work_dir: &Path,
    cancel: &CancellationToken,
) -> AppResult<FetchOutcome> {
    // Decode the first present segment to learn the file name and total size.
    let Some((first_idx, first_part)) = first_decodable(pool, &file.segments, cancel).await? else {
        // Every segment missing: nothing to write. par2 will treat the file as
        // absent and either recreate it from recovery blocks or fail cleanly.
        return Ok(FetchOutcome::Written { bytes: 0 });
    };
    if cancel.is_cancelled() {
        return Ok(FetchOutcome::Cancelled);
    }

    let file_name = name::sanitize_file_name(&first_part.file_name);
    let file_size = first_part.file_size;
    let path = work_dir.join(&file_name);

    let mut out = tokio::fs::File::create(&path)
        .await
        .map_err(|e| internal("creating repair file", &path, e))?;
    // Pre-size so gaps from missing parts stay zero-filled at the right place.
    out.set_len(file_size)
        .await
        .map_err(|e| internal("sizing repair file", &path, e))?;

    let mut written = 0u64;
    for (idx, segment) in file.segments.iter().enumerate() {
        if cancel.is_cancelled() {
            return Ok(FetchOutcome::Cancelled);
        }
        let part = if idx == first_idx {
            first_part.clone()
        } else {
            match fetch_decoded(pool, &segment.message_id).await {
                Ok(part) => part,
                // Missing / undecodable article → leave the gap for par2.
                Err(_) => continue,
            }
        };
        out.seek(std::io::SeekFrom::Start(part.part_begin))
            .await
            .map_err(|e| internal("seeking in repair file", &path, e))?;
        out.write_all(&part.data)
            .await
            .map_err(|e| internal("writing repair file", &path, e))?;
        written += part.data.len() as u64;
    }
    out.flush()
        .await
        .map_err(|e| internal("flushing repair file", &path, e))?;
    Ok(FetchOutcome::Written { bytes: written })
}

/// A decoded yEnc part with just the fields the repair writer needs.
#[derive(Clone)]
struct DecodedFilePart {
    data: bytes::Bytes,
    part_begin: u64,
    file_size: u64,
    file_name: String,
}

/// Find and decode the first segment that is actually present, so we can learn
/// the file name/size even when the leading segments are missing.
async fn first_decodable(
    pool: &NntpPool,
    segments: &[Segment],
    cancel: &CancellationToken,
) -> AppResult<Option<(usize, DecodedFilePart)>> {
    for (idx, segment) in segments.iter().enumerate() {
        if cancel.is_cancelled() {
            return Ok(None);
        }
        match fetch_decoded(pool, &segment.message_id).await {
            Ok(part) => return Ok(Some((idx, part))),
            Err(_) => continue,
        }
    }
    Ok(None)
}

/// Fetch and yEnc-decode one article. Errors (missing/undecodable) are the
/// caller's cue to leave a gap.
async fn fetch_decoded(pool: &NntpPool, message_id: &str) -> AppResult<DecodedFilePart> {
    let raw = pool.fetch_body(message_id).await.map_err(|e| match e {
        NntpError::ArticleNotFound => AppError::MissingSegment(message_id.to_string()),
        other => AppError::Upstream(format!("NNTP fetch of <{message_id}> failed: {other}")),
    })?;
    let part = yenc_decode(&raw)?;
    Ok(DecodedFilePart {
        data: part.data,
        part_begin: part.part_begin,
        file_size: part.file_size,
        file_name: part.file_name,
    })
}

/// Decoded on-disk name for a plain media file. We cannot decode headers here
/// (that happens during download), so fall back to the NZB subject filename;
/// the actual file was written under its decoded yEnc name, which par2 uses.
fn repaired_plain_name(file: &NzbFile) -> String {
    extract_filename(&file.subject).unwrap_or_else(|| "download".to_string())
}

/// Run `par2 repair` over every `.par2` set in the working dir. par2cmdline
/// exits 0 when files verify or repair succeeds, non-zero when the damage
/// exceeds the available recovery blocks.
async fn run_par2_repair(par2_path: &str, work_dir: &Path) -> AppResult<()> {
    // Repair each index file (`*.par2` without a `.volNNN+NN.` recovery
    // suffix). Passing the main index lets par2 discover its recovery volumes.
    let mut par2_indexes = Vec::new();
    let mut entries = tokio::fs::read_dir(work_dir)
        .await
        .map_err(|e| internal("reading repair work dir", work_dir, e))?;
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name().to_string_lossy().into_owned();
        let lower = name.to_ascii_lowercase();
        // The index file is `<base>.par2`; recovery volumes are
        // `<base>.volNNN+NN.par2`. Repairing from the index covers the set.
        if lower.ends_with(".par2") && !lower.contains(".vol") {
            par2_indexes.push(entry.path());
        }
    }
    if par2_indexes.is_empty() {
        // Some sets name every file `*.vol...par2` with no plain index; fall
        // back to any `.par2`.
        let mut entries = tokio::fs::read_dir(work_dir)
            .await
            .map_err(|e| internal("reading repair work dir", work_dir, e))?;
        while let Ok(Some(entry)) = entries.next_entry().await {
            if entry
                .file_name()
                .to_string_lossy()
                .to_ascii_lowercase()
                .ends_with(".par2")
            {
                par2_indexes.push(entry.path());
                break;
            }
        }
    }
    if par2_indexes.is_empty() {
        return Err(AppError::NoRelease(
            "no par2 recovery files present to repair with".into(),
        ));
    }

    for index in par2_indexes {
        run_one_par2(par2_path, work_dir, &index).await?;
    }
    Ok(())
}

async fn run_one_par2(par2_path: &str, work_dir: &Path, index: &Path) -> AppResult<()> {
    let output = tokio::process::Command::new(par2_path)
        .arg("repair")
        .arg("-q")
        .arg(index)
        .current_dir(work_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| {
            if e.kind() == std::io::ErrorKind::NotFound {
                AppError::Internal(anyhow::anyhow!(
                    "par2 not installed (looked for '{par2_path}'): {e}"
                ))
            } else {
                AppError::Internal(anyhow::anyhow!("running par2 '{par2_path}': {e}"))
            }
        })?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let stdout = String::from_utf8_lossy(&output.stdout);
        let detail = stderr
            .trim()
            .lines()
            .last()
            .or_else(|| stdout.trim().lines().last())
            .unwrap_or("unknown error");
        return Err(AppError::NoRelease(format!(
            "par2 repair failed for {}: {detail}",
            index.display()
        )));
    }
    Ok(())
}

/// After a successful repair of a store-mode RAR set, parse the on-disk
/// volumes and copy the largest inner file out into a standalone media file.
async fn extract_store_rar(work_dir: &Path) -> AppResult<PathBuf> {
    // Collect and naturally order the RAR volumes now sitting in the work dir.
    let mut volume_paths: Vec<PathBuf> = Vec::new();
    let mut entries = tokio::fs::read_dir(work_dir)
        .await
        .map_err(|e| internal("reading repair work dir", work_dir, e))?;
    while let Ok(Some(entry)) = entries.next_entry().await {
        let name = entry.file_name().to_string_lossy().into_owned();
        if classify(&name) == FileKind::RarVolume {
            volume_paths.push(entry.path());
        }
    }
    if volume_paths.is_empty() {
        return Err(AppError::NoRelease(
            "repaired release has no RAR volumes to extract".into(),
        ));
    }
    volume_paths.sort_by(|a, b| {
        crate::rar::volume_sort_key(&a.file_name().unwrap_or_default().to_string_lossy()).cmp(
            &crate::rar::volume_sort_key(&b.file_name().unwrap_or_default().to_string_lossy()),
        )
    });

    // Open each volume as a DiskFile and build the store-mode byte map.
    let mut volumes: Vec<DiskFile> = Vec::with_capacity(volume_paths.len());
    for path in &volume_paths {
        volumes.push(DiskFile::open(path).await?);
    }
    let refs: Vec<&dyn ReadAt> = volumes.iter().map(|v| v as &dyn ReadAt).collect();
    let map = build_archive_map(&refs).await?;

    // Copy the inner file out by streaming its extents from the volumes.
    let out_path = work_dir.join(name::sanitize_file_name(&map.inner_file_name));
    let mut out = tokio::fs::File::create(&out_path)
        .await
        .map_err(|e| internal("creating extracted media", &out_path, e))?;
    for extent in &map.extents {
        let volume = &volumes[extent.volume_index];
        let mut remaining = extent.len;
        let mut off = extent.volume_offset;
        while remaining > 0 {
            let want = remaining.min(4 * 1024 * 1024) as usize;
            let chunk = VirtualFileExt::read_at(volume, off, want).await?;
            if chunk.is_empty() {
                return Err(AppError::Internal(anyhow::anyhow!(
                    "unexpected EOF extracting {} from repaired volume",
                    map.inner_file_name
                )));
            }
            out.write_all(&chunk)
                .await
                .map_err(|e| internal("writing extracted media", &out_path, e))?;
            off += chunk.len() as u64;
            remaining -= chunk.len() as u64;
        }
    }
    out.flush()
        .await
        .map_err(|e| internal("flushing extracted media", &out_path, e))?;
    Ok(out_path)
}

// DiskFile implements the VFS read_at; use it by that trait for extraction.
use crate::vfs::VirtualFile as VirtualFileExt;

fn internal(what: &str, path: &Path, e: std::io::Error) -> AppError {
    AppError::Internal(anyhow::anyhow!("{what} {}: {e}", path.display()))
}
