//! Virtual file layer: random-access, byte-addressable views over content
//! that may live on disk, inside NZB segments on Usenet, or inside a
//! store-mode RAR archive spread across NZB-backed volumes.

pub mod cache;
pub mod disk_file;
pub mod nzb_file;
pub mod rar_file;
pub mod readahead;

use async_trait::async_trait;
use bytes::Bytes;

use crate::error::AppResult;

pub use cache::{CachedSegment, SegmentCache};
pub use disk_file::DiskFile;
pub use nzb_file::NzbBackedFile;
pub use rar_file::RarInnerFile;
pub use readahead::ReadaheadState;

/// A random-access read-only file. The unit the streaming/HLS layer consumes.
#[async_trait]
pub trait VirtualFile: Send + Sync {
    fn len(&self) -> u64;

    fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Read up to `buf_len` bytes starting at `offset`. Returns fewer bytes
    /// only at EOF; an offset at/past EOF yields an empty buffer.
    async fn read_at(&self, offset: u64, buf_len: usize) -> AppResult<Bytes>;
}
