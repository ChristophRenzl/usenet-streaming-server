//! Pre-flight availability check: STAT a sample of segments before starting
//! a stream, so dead releases fail fast instead of mid-playback.
//!
//! Beyond the binary streamable/not verdict, a release that is too damaged to
//! stream on the fly may still be recoverable by downloading everything and
//! running par2 repair (what SABnzbd does). [`assess_release`] classifies a
//! candidate into [`HealthVerdict::Streamable`], [`Repairable`] or
//! [`Unrecoverable`] so the API layer can pick the streaming or the
//! download-and-repair path.
//!
//! [`Repairable`]: HealthVerdict::Repairable
//! [`Unrecoverable`]: HealthVerdict::Unrecoverable

use std::collections::BTreeSet;

use serde::Serialize;

use crate::error::AppResult;
use crate::nntp::NntpPool;

use super::parse::{Nzb, Segment};
use super::select::{classify, extract_filename, FileKind, MainContent};

/// Streamable requires at least this fraction of the sampled segments present.
const STREAMABLE_RATIO: f64 = 0.95;
/// Repair is only attempted when at most this fraction of the main content is
/// missing (par2 sets rarely carry more than ~30% recovery volume).
const MAX_REPAIRABLE_MISSING: f64 = 0.30;

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

impl HealthReport {
    /// Fraction of the sampled segments that were missing (0.0 when nothing
    /// was checked).
    pub fn missing_fraction(&self) -> f64 {
        if self.checked == 0 {
            0.0
        } else {
            self.missing as f64 / self.checked as f64
        }
    }
}

/// How a candidate release can be consumed, after the pre-flight check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum HealthVerdict {
    /// Enough segments present to stream directly (current rule: >= 95% of the
    /// sample present, first and last segments present).
    Streamable,
    /// Too damaged to stream, but the missing fraction looks recoverable with
    /// the release's par2 recovery files (download-and-repair fallback).
    Repairable,
    /// Too much missing, or no par2 recovery available.
    Unrecoverable,
}

/// The full pre-flight assessment of a candidate: the sampled health of the
/// main content plus the recoverability verdict, with the par2 recovery
/// volume and estimated missing payload the verdict was based on.
#[derive(Debug, Clone, Serialize)]
pub struct RepairAssessment {
    pub verdict: HealthVerdict,
    /// Health of the main content sample (same as the legacy `health_check`).
    pub health: HealthReport,
    /// Total (encoded) size of the release's `.par2` recovery files.
    pub par2_recovery_bytes: u64,
    /// Estimated (encoded) size of the missing main-content payload,
    /// extrapolated from the sampled missing fraction.
    pub estimated_missing_bytes: u64,
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
    let ratio_ok = (checked - missing) as f64 / checked as f64 >= STREAMABLE_RATIO;

    Ok(HealthReport {
        checked,
        missing,
        ok: first_ok && last_ok && ratio_ok,
    })
}

/// Total encoded size of every `.par2` recovery file in the NZB. This is the
/// recovery volume available for repair (the actual par2 blocks; par2's own
/// index/main file is tiny and negligible).
pub fn par2_recovery_bytes(nzb: &Nzb) -> u64 {
    nzb.files
        .iter()
        .filter(|f| {
            extract_filename(&f.subject)
                .map(|name| classify(&name) == FileKind::Par2)
                .unwrap_or(false)
        })
        .map(|f| f.total_bytes())
        .sum()
}

/// Classify a candidate release into [`HealthVerdict`]: streamable directly,
/// repairable via par2 download, or unrecoverable.
///
/// Heuristic (deliberately conservative — the repair job either succeeds or
/// fails cleanly):
///   * `Streamable` — the existing streaming rule holds ([`HealthReport::ok`]).
///   * `Repairable` — the release is not streamable, but the sampled missing
///     fraction is at most [`MAX_REPAIRABLE_MISSING`] (~30%) AND the available
///     par2 recovery volume is at least as large as the estimated missing
///     payload. par2 can repair up to as much data as it has recovery blocks,
///     so recovery >= missing is the necessary condition; the 30% cap keeps us
///     away from releases so damaged that a sampled estimate is unreliable.
///   * `Unrecoverable` — anything else (too much missing, or no/insufficient
///     par2).
pub async fn assess_release(
    nzb: &Nzb,
    main: &MainContent,
    pool: &NntpPool,
    sample_size: usize,
) -> AppResult<RepairAssessment> {
    let segments = main_content_segments(nzb, main);
    let total_main_bytes: u64 = segments.iter().map(|s| s.bytes).sum();
    let health = health_check(&segments, pool, sample_size).await?;
    Ok(classify_verdict(
        health,
        total_main_bytes,
        par2_recovery_bytes(nzb),
    ))
}

/// Pure verdict logic, factored out of [`assess_release`] so it can be unit
/// tested without an NNTP pool. See [`assess_release`] for the heuristic.
fn classify_verdict(
    health: HealthReport,
    total_main_bytes: u64,
    recovery_bytes: u64,
) -> RepairAssessment {
    let missing_fraction = health.missing_fraction();
    let estimated_missing_bytes = (total_main_bytes as f64 * missing_fraction) as u64;

    let verdict = if health.ok {
        HealthVerdict::Streamable
    } else if missing_fraction <= MAX_REPAIRABLE_MISSING
        && estimated_missing_bytes > 0
        && recovery_bytes >= estimated_missing_bytes
    {
        HealthVerdict::Repairable
    } else {
        HealthVerdict::Unrecoverable
    };

    RepairAssessment {
        verdict,
        health,
        par2_recovery_bytes: recovery_bytes,
        estimated_missing_bytes,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nzb::parse::NzbFile;

    fn seg(id: &str, bytes: u64) -> Segment {
        Segment {
            number: 1,
            bytes,
            message_id: id.to_string(),
        }
    }

    fn data_file(name: &str, seg_count: usize, seg_bytes: u64) -> NzbFile {
        NzbFile {
            subject: format!(r#"Rel [1/1] - "{name}" yEnc"#),
            poster: String::new(),
            date: 0,
            groups: vec![],
            segments: (0..seg_count)
                .map(|i| seg(&format!("{name}-{i}@mock"), seg_bytes))
                .collect(),
        }
    }

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

    #[test]
    fn par2_recovery_bytes_sums_only_par2_files() {
        let nzb = Nzb {
            files: vec![
                data_file("movie.mkv", 10, 1000),
                data_file("movie.vol000+01.par2", 3, 500),
                data_file("movie.vol001+02.par2", 4, 500),
                data_file("movie.nfo", 1, 100),
            ],
        };
        // 3*500 + 4*500 = 3500, ignoring the .mkv and .nfo.
        assert_eq!(par2_recovery_bytes(&nzb), 3500);
    }

    fn report(checked: usize, missing: usize, ok: bool) -> HealthReport {
        HealthReport {
            checked,
            missing,
            ok,
        }
    }

    #[test]
    fn verdict_streamable_when_health_ok() {
        let a = classify_verdict(report(10, 0, true), 1_000_000, 0);
        assert_eq!(a.verdict, HealthVerdict::Streamable);
    }

    #[test]
    fn verdict_repairable_when_par2_covers_missing() {
        // 10% missing of 1 MiB = ~100 KiB missing; 200 KiB par2 recovery.
        let a = classify_verdict(report(10, 1, false), 1_000_000, 200_000);
        assert_eq!(a.verdict, HealthVerdict::Repairable);
        assert_eq!(a.estimated_missing_bytes, 100_000);
    }

    #[test]
    fn verdict_unrecoverable_when_par2_insufficient() {
        // 10% missing = ~100 KiB, but only 50 KiB recovery available.
        let a = classify_verdict(report(10, 1, false), 1_000_000, 50_000);
        assert_eq!(a.verdict, HealthVerdict::Unrecoverable);
    }

    #[test]
    fn verdict_unrecoverable_when_no_par2() {
        let a = classify_verdict(report(10, 1, false), 1_000_000, 0);
        assert_eq!(a.verdict, HealthVerdict::Unrecoverable);
    }

    #[test]
    fn verdict_unrecoverable_when_too_much_missing() {
        // 40% missing exceeds the 30% repairable cap even with ample par2.
        let a = classify_verdict(report(10, 4, false), 1_000_000, 10_000_000);
        assert_eq!(a.verdict, HealthVerdict::Unrecoverable);
    }
}
