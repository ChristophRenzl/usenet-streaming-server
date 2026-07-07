//! Header-only RAR parsing for store-mode (`-m0`) archives.
//!
//! The goal is not extraction but a byte mapping: where inside which volume
//! do the bytes of the largest inner file live ([`ArchiveMap`]), so the
//! virtual file layer can serve reads straight from NZB-backed volumes.

pub mod rar4;
pub mod rar5;
pub mod volumes;

use async_trait::async_trait;
use bytes::Bytes;

use crate::error::{AppError, AppResult};

pub use volumes::{
    build_archive_map, build_archive_map_from_entries, detect_format, parse_volume, volume_sort_key,
};

/// Random-access byte source a RAR volume is parsed from. Implemented for
/// in-memory buffers, disk files and NZB-backed virtual files.
#[async_trait]
pub trait ReadAt: Send + Sync {
    fn len(&self) -> u64;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Read up to `len` bytes at `offset`. Short reads only happen at EOF.
    async fn read_at(&self, offset: u64, len: usize) -> AppResult<Bytes>;
}

/// In-memory [`ReadAt`] for tests and small buffers.
pub struct SliceReadAt(pub Bytes);

#[async_trait]
impl ReadAt for SliceReadAt {
    fn len(&self) -> u64 {
        self.0.len() as u64
    }

    async fn read_at(&self, offset: u64, len: usize) -> AppResult<Bytes> {
        let start = (offset.min(self.0.len() as u64)) as usize;
        let end = (start + len).min(self.0.len());
        Ok(self.0.slice(start..end))
    }
}

/// Read exactly `len` bytes or fail (used for headers, which must be whole).
pub(crate) async fn read_exact_at(r: &dyn ReadAt, offset: u64, len: usize) -> AppResult<Bytes> {
    let bytes = r.read_at(offset, len).await?;
    if bytes.len() != len {
        return Err(AppError::InvalidRarArchive(format!(
            "truncated archive: wanted {len} bytes at offset {offset}, got {}",
            bytes.len()
        )));
    }
    Ok(bytes)
}

/// Supported archive format generations.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RarFormat {
    Rar4,
    Rar5,
}

/// One file (or file part, in multi-volume sets) found in a volume's headers.
#[derive(Debug, Clone)]
pub struct FileEntry {
    pub name: String,
    /// Size of the whole unpacked file (same in every part of a split file).
    pub unpacked_size: u64,
    /// Size of the data area in *this* volume.
    pub packed_size: u64,
    /// Absolute offset of the data area within this volume.
    pub data_offset: u64,
    /// Compression method is "store" (only mode we can stream).
    pub method_store: bool,
    /// File continues from the previous volume.
    pub split_before: bool,
    /// File continues into the next volume.
    pub split_after: bool,
    pub is_directory: bool,
}

/// A contiguous run of an inner file's bytes inside one volume.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Extent {
    /// Offset within the unpacked inner file.
    pub unpacked_start: u64,
    pub len: u64,
    /// Index into the ordered volume list.
    pub volume_index: usize,
    /// Absolute offset within that volume.
    pub volume_offset: u64,
}

/// Byte mapping for the largest inner file of a RAR set.
#[derive(Debug, Clone)]
pub struct ArchiveMap {
    /// Sorted by `unpacked_start`, contiguous, covering the whole file.
    pub extents: Vec<Extent>,
    pub inner_file_name: String,
    pub unpacked_size: u64,
}

pub(crate) fn malformed(msg: impl Into<String>) -> AppError {
    AppError::InvalidRarArchive(msg.into())
}
