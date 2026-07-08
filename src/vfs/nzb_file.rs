//! A [`VirtualFile`] backed by one NZB file's segments, fetched over NNTP
//! and yEnc-decoded through the shared segment cache.
//!
//! Segment placement: yEnc parts are near-uniform, so the segment holding an
//! offset is estimated as `offset / nominal_part_size` and corrected using
//! the actual `=ypart begin=` of the decoded article (the corrections are
//! remembered in a lazily-filled offset table).

use std::sync::{Arc, Mutex};

use async_trait::async_trait;
use bytes::{Bytes, BytesMut};

use crate::error::{AppError, AppResult};
use crate::nntp::{NntpError, NntpPool};
use crate::nzb::parse::{NzbFile, Segment};
use crate::nzb::yenc;

use super::cache::{CachedSegment, SegmentCache};
use super::readahead::ReadaheadState;
use super::VirtualFile;

#[derive(Debug, Clone, Copy, Default)]
struct SegMeta {
    begin: Option<u64>,
    size: Option<u64>,
}

struct Inner {
    segments: Vec<Segment>,
    pool: NntpPool,
    cache: SegmentCache,
    file_size: u64,
    nominal_part_size: u64,
    file_name: String,
    /// Lazily-corrected decoded-offset table, indexed like `segments`.
    meta: Mutex<Vec<SegMeta>>,
    readahead: ReadaheadState,
}

/// Cheap to clone; clones share the offset table and readahead state.
#[derive(Clone)]
pub struct NzbBackedFile {
    inner: Arc<Inner>,
}

impl std::fmt::Debug for NzbBackedFile {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("NzbBackedFile")
            .field("file_name", &self.inner.file_name)
            .field("file_size", &self.inner.file_size)
            .field("segments", &self.inner.segments.len())
            .finish_non_exhaustive()
    }
}

impl NzbBackedFile {
    /// Open an NZB file for random access. Fetches and decodes the first
    /// segment to learn the file size, name and nominal part size.
    pub async fn open(
        file: &NzbFile,
        pool: NntpPool,
        cache: SegmentCache,
        readahead_segments: usize,
    ) -> AppResult<Self> {
        let segments = file.segments.clone();
        if segments.is_empty() {
            return Err(AppError::BadRequest("NZB file has no segments".to_string()));
        }
        let first = fetch_decoded(&pool, &cache, &segments[0].message_id).await?;
        if first.begin != 0 {
            return Err(AppError::Upstream(format!(
                "first segment of '{}' starts at offset {}, expected 0",
                first.file_name, first.begin
            )));
        }

        let mut meta = vec![SegMeta::default(); segments.len()];
        meta[0] = SegMeta {
            begin: Some(0),
            size: Some(first.data.len() as u64),
        };

        Ok(Self {
            inner: Arc::new(Inner {
                nominal_part_size: (first.data.len() as u64).max(1),
                file_size: first.file_size,
                file_name: first.file_name.to_string(),
                segments,
                pool,
                cache,
                meta: Mutex::new(meta),
                readahead: ReadaheadState::new(readahead_segments),
            }),
        })
    }

    /// Decoded file name from the yEnc headers.
    pub fn file_name(&self) -> &str {
        &self.inner.file_name
    }

    pub fn segment_count(&self) -> usize {
        self.inner.segments.len()
    }

    /// Decoded size of a typical segment (the first one).
    pub fn nominal_part_size(&self) -> u64 {
        self.inner.nominal_part_size
    }

    /// Fetch + decode segment `idx` through the cache and remember its
    /// actual offset.
    async fn segment(&self, idx: usize) -> AppResult<CachedSegment> {
        let inner = &self.inner;
        let seg = fetch_decoded(&inner.pool, &inner.cache, &inner.segments[idx].message_id).await?;
        record_meta(inner, idx, &seg);
        Ok(seg)
    }

    /// Find the segment containing `pos`: estimate by nominal part size,
    /// then walk using actual decoded offsets.
    async fn locate(&self, pos: u64) -> AppResult<CachedSegment> {
        let inner = &self.inner;
        let count = inner.segments.len();
        let mut idx = usize::try_from(pos / inner.nominal_part_size)
            .unwrap_or(usize::MAX)
            .min(count - 1);

        for _ in 0..=count.saturating_mul(2) {
            // Fast path: a previously recorded offset tells us to move on
            // without fetching the wrong segment.
            let known = inner.meta.lock().expect("offset table lock")[idx];
            if let SegMeta {
                begin: Some(begin),
                size: Some(size),
            } = known
            {
                if pos < begin {
                    if idx == 0 {
                        break;
                    }
                    idx -= 1;
                    continue;
                }
                if pos >= begin + size {
                    if idx + 1 >= count {
                        break;
                    }
                    idx += 1;
                    continue;
                }
            }

            let seg = self.segment(idx).await?;
            let begin = seg.begin;
            let size = seg.data.len() as u64;
            if pos < begin {
                if idx == 0 {
                    break;
                }
                idx -= 1;
            } else if pos >= begin + size {
                if idx + 1 >= count {
                    break;
                }
                idx += 1;
            } else {
                return Ok(seg);
            }
        }
        Err(AppError::Upstream(format!(
            "inconsistent yEnc part offsets in '{}' around byte {pos}",
            inner.file_name
        )))
    }

    /// Kick off best-effort prefetch of the segments following `end` when
    /// the access pattern looks sequential.
    fn maybe_readahead(&self, start: u64, end: u64) {
        let inner = &self.inner;
        if !inner.readahead.observe(start, end, inner.nominal_part_size) {
            return;
        }
        let next = usize::try_from(end / inner.nominal_part_size).unwrap_or(usize::MAX);
        for idx in next..(next.saturating_add(inner.readahead.depth())) {
            if idx >= inner.segments.len() {
                break;
            }
            if inner.cache.contains(&inner.segments[idx].message_id) {
                continue;
            }
            let Some(permit) = inner.readahead.try_reserve() else {
                break;
            };
            let inner = self.inner.clone();
            tokio::spawn(async move {
                let _permit = permit;
                prefetch(&inner, idx).await;
            });
        }
    }
}

fn record_meta(inner: &Inner, idx: usize, seg: &CachedSegment) {
    let mut meta = inner.meta.lock().expect("offset table lock");
    meta[idx] = SegMeta {
        begin: Some(seg.begin),
        size: Some(seg.data.len() as u64),
    };
}

/// Sentinel error a contended prefetch resolves its single-flight cache slot
/// with. The cache shares in-flight results with every concurrent waiter, so
/// a *demand* read racing a skipped prefetch of the same segment can be
/// handed this error — it must retry with a real fetch instead of failing
/// the read (which would fail a session start or a mid-stream request).
const PREFETCH_SKIPPED: &str = "prefetch skipped";

/// Demand fetch: waits for a pool connection, falls back across providers,
/// maps a missing article to `AppError::MissingSegment`.
async fn fetch_decoded(
    pool: &NntpPool,
    cache: &SegmentCache,
    message_id: &str,
) -> AppResult<CachedSegment> {
    // Bounded retry for the prefetch-skip race (see PREFETCH_SKIPPED): the
    // sentinel is never cached, so the retry performs the real fetch.
    for _ in 0..3 {
        let pool = pool.clone();
        let id = message_id.to_string();
        let result = cache
            .get_or_fetch(message_id, async move {
                let raw = pool.fetch_body(&id).await.map_err(|e| match e {
                    NntpError::ArticleNotFound => AppError::MissingSegment(id.clone()),
                    other => AppError::Upstream(format!("NNTP fetch of <{id}> failed: {other}")),
                })?;
                decode_segment(&raw)
            })
            .await;
        match result {
            Err(AppError::Upstream(message)) if message.contains(PREFETCH_SKIPPED) => continue,
            other => return other,
        }
    }
    Err(AppError::Upstream(format!(
        "fetch of <{message_id}> kept losing the race against skipped prefetches"
    )))
}

/// Prefetch: skips instead of waiting when the pool is contended; errors are
/// swallowed (the demand path will retry and surface them).
async fn prefetch(inner: &Arc<Inner>, idx: usize) {
    let message_id = inner.segments[idx].message_id.clone();
    if inner.cache.contains(&message_id) {
        return;
    }
    let pool = inner.pool.clone();
    let id = message_id.clone();
    let result = inner
        .cache
        .get_or_fetch(&message_id, async move {
            match pool.try_fetch_body(&id).await {
                Some(raw) => decode_segment(&raw),
                // Not cached (errors are never cached), so a later demand
                // read fetches for real; a concurrent demand read sharing
                // this in-flight slot retries on the sentinel.
                None => Err(AppError::Upstream(PREFETCH_SKIPPED.into())),
            }
        })
        .await;
    if let Ok(seg) = result {
        record_meta(inner, idx, &seg);
    }
}

fn decode_segment(raw: &[u8]) -> AppResult<CachedSegment> {
    let part = yenc::decode(raw)?;
    Ok(CachedSegment {
        begin: part.part_begin,
        file_size: part.file_size,
        file_name: part.file_name.into(),
        data: part.data,
    })
}

#[async_trait]
impl VirtualFile for NzbBackedFile {
    fn len(&self) -> u64 {
        self.inner.file_size
    }

    async fn read_at(&self, offset: u64, buf_len: usize) -> AppResult<Bytes> {
        let end = offset.saturating_add(buf_len as u64).min(self.len());
        if offset >= end {
            return Ok(Bytes::new());
        }

        let mut out = BytesMut::with_capacity((end - offset) as usize);
        let mut pos = offset;
        while pos < end {
            let seg = self.locate(pos).await?;
            let begin = seg.begin;
            let take_from = (pos - begin) as usize;
            let take_to = ((end - begin) as usize).min(seg.data.len());
            out.extend_from_slice(&seg.data[take_from..take_to]);
            pos = begin + take_to as u64;
        }

        self.maybe_readahead(offset, end);
        Ok(out.freeze())
    }
}

/// RAR volumes fetched from Usenet are parsed through the same view.
#[async_trait]
impl crate::rar::ReadAt for NzbBackedFile {
    fn len(&self) -> u64 {
        VirtualFile::len(self)
    }

    async fn read_at(&self, offset: u64, len: usize) -> AppResult<Bytes> {
        VirtualFile::read_at(self, offset, len).await
    }
}
