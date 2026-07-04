//! A [`VirtualFile`] over a local file, for playing back finished downloads
//! (and for file-backed RAR parsing in tests).

use std::path::Path;
use std::sync::Arc;

use async_trait::async_trait;
use bytes::Bytes;

use crate::error::{AppError, AppResult};

use super::VirtualFile;

pub struct DiskFile {
    file: Arc<std::fs::File>,
    len: u64,
}

impl DiskFile {
    pub async fn open(path: impl AsRef<Path>) -> AppResult<Self> {
        let path = path.as_ref().to_owned();
        let (file, len) = tokio::task::spawn_blocking(move || -> std::io::Result<_> {
            let file = std::fs::File::open(&path)?;
            let len = file.metadata()?.len();
            Ok((file, len))
        })
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("blocking task failed: {e}")))?
        .map_err(|e| AppError::Internal(anyhow::anyhow!("opening file: {e}")))?;
        Ok(Self {
            file: Arc::new(file),
            len,
        })
    }
}

#[async_trait]
impl VirtualFile for DiskFile {
    fn len(&self) -> u64 {
        self.len
    }

    async fn read_at(&self, offset: u64, buf_len: usize) -> AppResult<Bytes> {
        let end = offset.saturating_add(buf_len as u64).min(self.len);
        if offset >= end {
            return Ok(Bytes::new());
        }
        let want = (end - offset) as usize;
        let file = self.file.clone();
        let buf = tokio::task::spawn_blocking(move || -> std::io::Result<Vec<u8>> {
            use std::os::unix::fs::FileExt;
            let mut buf = vec![0u8; want];
            let mut filled = 0usize;
            while filled < want {
                let n = file.read_at(&mut buf[filled..], offset + filled as u64)?;
                if n == 0 {
                    break; // EOF (file shrank underneath us)
                }
                filled += n;
            }
            buf.truncate(filled);
            Ok(buf)
        })
        .await
        .map_err(|e| AppError::Internal(anyhow::anyhow!("blocking task failed: {e}")))?
        .map_err(|e| AppError::Internal(anyhow::anyhow!("reading file: {e}")))?;
        Ok(Bytes::from(buf))
    }
}

/// Disk files double as [`crate::rar::ReadAt`] sources for header parsing.
#[async_trait]
impl crate::rar::ReadAt for DiskFile {
    fn len(&self) -> u64 {
        VirtualFile::len(self)
    }

    async fn read_at(&self, offset: u64, len: usize) -> AppResult<Bytes> {
        VirtualFile::read_at(self, offset, len).await
    }
}
