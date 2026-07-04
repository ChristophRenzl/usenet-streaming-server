//! A [`VirtualFile`] view of one store-mode inner file of a RAR set, backed
//! by an [`ArchiveMap`] and the (virtual) volume files.

use std::sync::Arc;

use async_trait::async_trait;
use bytes::{Bytes, BytesMut};

use crate::error::{AppError, AppResult};
use crate::rar::ArchiveMap;

use super::VirtualFile;

pub struct RarInnerFile {
    map: ArchiveMap,
    volumes: Vec<Arc<dyn VirtualFile>>,
}

impl RarInnerFile {
    /// Validates that the map's extents are contiguous, cover the whole file
    /// and stay within the given volumes.
    pub fn new(map: ArchiveMap, volumes: Vec<Arc<dyn VirtualFile>>) -> AppResult<Self> {
        let mut expected_start = 0u64;
        for (i, extent) in map.extents.iter().enumerate() {
            if extent.unpacked_start != expected_start {
                return Err(AppError::InvalidRarArchive(format!(
                    "extent {i} starts at {} but {expected_start} bytes are covered",
                    extent.unpacked_start
                )));
            }
            let volume = volumes.get(extent.volume_index).ok_or_else(|| {
                AppError::InvalidRarArchive(format!(
                    "extent {i} references missing volume {}",
                    extent.volume_index
                ))
            })?;
            if extent.volume_offset + extent.len > volume.len() {
                return Err(AppError::InvalidRarArchive(format!(
                    "extent {i} extends past the end of volume {}",
                    extent.volume_index
                )));
            }
            expected_start += extent.len;
        }
        if expected_start != map.unpacked_size {
            return Err(AppError::InvalidRarArchive(format!(
                "extents cover {expected_start} of {} bytes",
                map.unpacked_size
            )));
        }
        Ok(Self { map, volumes })
    }

    pub fn inner_file_name(&self) -> &str {
        &self.map.inner_file_name
    }
}

#[async_trait]
impl VirtualFile for RarInnerFile {
    fn len(&self) -> u64 {
        self.map.unpacked_size
    }

    async fn read_at(&self, offset: u64, buf_len: usize) -> AppResult<Bytes> {
        let end = offset.saturating_add(buf_len as u64).min(self.len());
        if offset >= end {
            return Ok(Bytes::new());
        }

        // First extent containing `offset`.
        let extents = &self.map.extents;
        let mut i = extents.partition_point(|e| e.unpacked_start + e.len <= offset);

        let mut out = BytesMut::with_capacity((end - offset) as usize);
        let mut pos = offset;
        while pos < end {
            let extent = extents.get(i).ok_or_else(|| {
                AppError::InvalidRarArchive(format!("no extent covers byte {pos}"))
            })?;
            let within = pos - extent.unpacked_start;
            let take = (extent.len - within).min(end - pos);
            let chunk = self.volumes[extent.volume_index]
                .read_at(extent.volume_offset + within, take as usize)
                .await?;
            if chunk.len() as u64 != take {
                return Err(AppError::Upstream(format!(
                    "short read from volume {}: wanted {take} bytes, got {}",
                    extent.volume_index,
                    chunk.len()
                )));
            }
            out.extend_from_slice(&chunk);
            pos += take;
            i += 1;
        }
        Ok(out.freeze())
    }
}
