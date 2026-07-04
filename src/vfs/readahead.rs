//! Sequential-access detection for prefetching.
//!
//! Prefetch work runs on a deliberately small separate semaphore and uses
//! the pool's `try_fetch_body` (permit `try_acquire`), so it never starves
//! demand reads — when the pool is contended, prefetch simply skips.

use std::sync::{Arc, Mutex};

use tokio::sync::Semaphore;

/// Maximum prefetch tasks in flight per file, regardless of readahead depth.
const MAX_PREFETCH_CONCURRENCY: usize = 4;

pub struct ReadaheadState {
    last_end: Mutex<Option<u64>>,
    /// Segments to prefetch ahead of a sequential reader
    /// (`StreamingConfig::readahead_segments`); 0 disables readahead.
    depth: usize,
    semaphore: Arc<Semaphore>,
}

impl ReadaheadState {
    pub fn new(depth: usize) -> Self {
        Self {
            last_end: Mutex::new(None),
            depth,
            semaphore: Arc::new(Semaphore::new(depth.clamp(1, MAX_PREFETCH_CONCURRENCY))),
        }
    }

    pub fn depth(&self) -> usize {
        self.depth
    }

    /// Record a read and report whether it looks sequential: it starts within
    /// one `window` (part size) of where the previous read ended.
    pub fn observe(&self, start: u64, end: u64, window: u64) -> bool {
        let mut last = self.last_end.lock().expect("readahead lock");
        let sequential = match *last {
            Some(prev_end) => {
                start >= prev_end.saturating_sub(window) && start <= prev_end.saturating_add(window)
            }
            None => false,
        };
        *last = Some(end);
        sequential && self.depth > 0
    }

    /// Try to reserve a prefetch task slot; `None` when prefetch is already
    /// running at full tilt (callers just skip — prefetch is best-effort).
    pub fn try_reserve(&self) -> Option<tokio::sync::OwnedSemaphorePermit> {
        self.semaphore.clone().try_acquire_owned().ok()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn detects_sequential_reads() {
        let ra = ReadaheadState::new(8);
        assert!(!ra.observe(0, 100, 50)); // first read: no history
        assert!(ra.observe(100, 200, 50)); // contiguous
        assert!(ra.observe(180, 260, 50)); // small overlap still sequential
        assert!(!ra.observe(10_000, 10_100, 50)); // seek
        assert!(ra.observe(10_120, 10_200, 50)); // resumes after the seek
    }

    #[test]
    fn zero_depth_disables() {
        let ra = ReadaheadState::new(0);
        ra.observe(0, 100, 50);
        assert!(!ra.observe(100, 200, 50));
    }
}
