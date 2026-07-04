//! Building the streamable [`VirtualFile`] for an NZB's main content:
//! plain media files directly, RAR sets through the store-mode archive map.

use std::sync::Arc;

use crate::error::{AppError, AppResult};
use crate::nntp::NntpPool;
use crate::nzb::{MainContent, Nzb};
use crate::rar::{build_archive_map, ReadAt};
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
            let mut volumes: Vec<Arc<NzbBackedFile>> = Vec::with_capacity(set.len());
            for file_ref in set {
                volumes.push(Arc::new(
                    NzbBackedFile::open(
                        &nzb.files[file_ref.index],
                        pool.clone(),
                        cache.clone(),
                        readahead_segments,
                    )
                    .await?,
                ));
            }
            let refs: Vec<&dyn ReadAt> =
                volumes.iter().map(|v| v.as_ref() as &dyn ReadAt).collect();
            let map = build_archive_map(&refs).await?;
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
