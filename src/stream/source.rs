//! Building the streamable [`VirtualFile`] for an NZB's main content:
//! plain media files directly, RAR sets through the store-mode archive map.

use std::sync::Arc;

use crate::error::{AppError, AppResult};
use crate::nntp::NntpPool;
use crate::nzb::{MainContent, Nzb};
use crate::rar::{build_archive_map_from_entries, parse_volume, FileEntry, ReadAt};
use crate::vfs::{NzbBackedFile, RarInnerFile, SegmentCache, VirtualFile};

/// A ready-to-stream media source.
pub struct MediaSource {
    pub file: Arc<dyn VirtualFile>,
    /// Actual media file name (yEnc name or RAR inner file name).
    pub inner_file_name: String,
}

/// Open the main content as a random-access virtual file. Compressed or
/// encrypted RAR sets surface their typed errors so callers can move on to
/// the next candidate.
pub async fn open_media_source(
    nzb: &Nzb,
    main: &MainContent,
    pool: &NntpPool,
    cache: &SegmentCache,
    readahead_segments: usize,
) -> AppResult<MediaSource> {
    match main {
        MainContent::Plain(file_ref) => {
            let file = NzbBackedFile::open(
                &nzb.files[file_ref.index],
                pool.clone(),
                cache.clone(),
                readahead_segments,
            )
            .await?;
            let inner_file_name = file.file_name().to_string();
            Ok(MediaSource {
                file: Arc::new(file),
                inner_file_name,
            })
        }
        MainContent::RarSet(set) => {
            if set.is_empty() {
                return Err(AppError::NoRelease("empty RAR set".into()));
            }
            // Opening a volume fetches its first article and header walking
            // touches its last one — 2-3 NNTP round trips per volume, which
            // walked serially is the dominant part of session start for a
            // typical multi-volume release (60 volumes ≈ 10s+). Fan every
            // volume out as its own task; the NNTP pool's per-provider
            // connection limits cap the actual parallelism.
            let handles: Vec<_> = set
                .iter()
                .map(|file_ref| {
                    let file = nzb.files[file_ref.index].clone();
                    let pool = pool.clone();
                    let cache = cache.clone();
                    tokio::spawn(async move {
                        let volume = Arc::new(
                            NzbBackedFile::open(&file, pool, cache, readahead_segments).await?,
                        );
                        let entries = parse_volume(volume.as_ref() as &dyn ReadAt).await?;
                        Ok::<_, AppError>((volume, entries))
                    })
                })
                .collect();
            let mut volumes: Vec<Arc<NzbBackedFile>> = Vec::with_capacity(handles.len());
            let mut per_volume: Vec<Vec<FileEntry>> = Vec::with_capacity(handles.len());
            for handle in handles {
                let (volume, entries) = handle
                    .await
                    .map_err(|e| AppError::Internal(anyhow::anyhow!("volume task: {e}")))??;
                volumes.push(volume);
                per_volume.push(entries);
            }
            let lens: Vec<u64> = volumes.iter().map(|v| ReadAt::len(v.as_ref())).collect();
            let map = build_archive_map_from_entries(per_volume, &lens)?;
            let inner_file_name = map.inner_file_name.clone();
            let inner = RarInnerFile::new(
                map,
                volumes
                    .into_iter()
                    .map(|v| v as Arc<dyn VirtualFile>)
                    .collect(),
            )?;
            Ok(MediaSource {
                file: Arc::new(inner),
                inner_file_name,
            })
        }
    }
}
