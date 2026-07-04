//! Pre-flight availability check: STAT a sample of segments before starting
//! a stream, so dead releases fail fast instead of mid-playback.

use std::collections::BTreeSet;

use serde::Serialize;

use crate::error::AppResult;
use crate::nntp::NntpPool;

use super::parse::{Nzb, Segment};
use super::select::MainContent;

#[derive(Debug, Clone, Serialize)]
pub struct HealthReport {
    /// Number of segments actually STATed.
    pub checked: usize,
    /// How many of those were missing on every provider.
    pub missing: usize,
    /// True when >= 95% of the sample is present AND the first and last
    /// segments exist.
    pub ok: bool,
}

/// All segments of the main content, in playback order (all volumes of a RAR
/// set, in volume order).
pub fn main_content_segments<'a>(nzb: &'a Nzb, main: &MainContent) -> Vec<&'a Segment> {
    let indices: Vec<usize> = match main {
        MainContent::Plain(f) => vec![f.index],
        MainContent::RarSet(set) => set.iter().map(|f| f.index).collect(),
    };
    indices
        .into_iter()
        .filter_map(|i| nzb.files.get(i))
        .flat_map(|f| f.segments.iter())
        .collect()
}

/// Evenly-spaced sample indices over `total` items, always including the
/// first and last.
fn sample_indices(total: usize, sample_size: usize) -> Vec<usize> {
    if total == 0 {
        return Vec::new();
    }
    let n = sample_size.clamp(1, total);
    let mut set = BTreeSet::new();
    if n == 1 {
        set.insert(0);
    } else {
        for k in 0..n {
            set.insert(k * (total - 1) / (n - 1));
        }
    }
    set.into_iter().collect()
}

/// STAT an evenly-spaced sample (default size 10) of the given segments.
pub async fn health_check(
    segments: &[&Segment],
    pool: &NntpPool,
    sample_size: usize,
) -> AppResult<HealthReport> {
    let indices = sample_indices(segments.len(), sample_size);
    if indices.is_empty() {
        return Ok(HealthReport {
            checked: 0,
            missing: 0,
            ok: false,
        });
    }

    let checks = indices.iter().map(|&i| {
        let id = segments[i].message_id.as_str();
        async move { pool.stat_any(id).await }
    });
    let results = futures::future::join_all(checks).await;

    let mut present = Vec::with_capacity(results.len());
    for result in results {
        present.push(result.map_err(|e| {
            crate::error::AppError::Upstream(format!("health check STAT failed: {e}"))
        })?);
    }

    let checked = present.len();
    let missing = present.iter().filter(|&&p| !p).count();
    let first_ok = *present.first().unwrap_or(&false);
    let last_ok = *present.last().unwrap_or(&false);
    let ratio_ok = (checked - missing) as f64 / checked as f64 >= 0.95;

    Ok(HealthReport {
        checked,
        missing,
        ok: first_ok && last_ok && ratio_ok,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sampling_includes_endpoints_and_is_even() {
        assert_eq!(sample_indices(0, 10), Vec::<usize>::new());
        assert_eq!(sample_indices(1, 10), vec![0]);
        assert_eq!(sample_indices(5, 10), vec![0, 1, 2, 3, 4]);
        let s = sample_indices(1000, 10);
        assert_eq!(s.len(), 10);
        assert_eq!(s[0], 0);
        assert_eq!(*s.last().unwrap(), 999);
        assert_eq!(sample_indices(7, 1), vec![0]);
    }
}
