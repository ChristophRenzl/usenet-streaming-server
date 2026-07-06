//! Chromaprint audio fingerprinting via `fpcalc`, for intro detection.
//!
//! Fingerprints the first [`ANALYZE_SECS`] of an episode's audio into a list of
//! 32-bit chromaprint sub-fingerprints ([`fingerprint_url`]) and (de)serializes
//! that list to/from the compact little-endian BLOB stored in the database
//! ([`to_bytes`] / [`from_bytes`]).
//!
//! Everything here is best-effort: a missing `fpcalc`, an unreadable URL or a
//! surprising output all surface as an `Err` the caller logs and ignores —
//! playback never depends on any of it.

use std::process::Stdio;
use std::time::Duration;

use anyhow::{anyhow, bail, Context};

/// How many seconds of audio from the start we fingerprint. The opening we look
/// for sits within the first ~2-3 minutes; capping the analysis bounds both the
/// audio we fetch over the loopback and the `fpcalc` runtime.
pub const ANALYZE_SECS: u32 = 240;

/// Hard ceiling on the `fpcalc` (and any transcode) subprocess. Well above the
/// time to fetch/decode ~240s of already-cached audio; a runaway is killed.
const FPCALC_TIMEOUT: Duration = Duration::from_secs(120);

/// Serialize a chromaprint fingerprint to bytes: each `u32` little-endian, so
/// the BLOB is exactly `4 * points.len()` bytes and round-trips via
/// [`from_bytes`].
pub fn to_bytes(points: &[u32]) -> Vec<u8> {
    let mut out = Vec::with_capacity(points.len() * 4);
    for &p in points {
        out.extend_from_slice(&p.to_le_bytes());
    }
    out
}

/// Deserialize a fingerprint stored by [`to_bytes`]. A byte length that is not
/// a multiple of 4 is corrupt; the trailing partial word is ignored.
pub fn from_bytes(bytes: &[u8]) -> Vec<u32> {
    bytes
        .chunks_exact(4)
        .map(|c| u32::from_le_bytes([c[0], c[1], c[2], c[3]]))
        .collect()
}

/// Fingerprint the first [`ANALYZE_SECS`] of the audio at `url` (the session's
/// loopback VFS URL) into a list of chromaprint sub-fingerprints.
///
/// `fpcalc` reads its input through ffmpeg, so it can open the loopback HTTP URL
/// directly — we call `fpcalc -raw -length <secs> <url>` and parse the
/// `FINGERPRINT=` line (a comma-separated list of unsigned 32-bit ints with
/// `-raw`). Best-effort: any failure (binary missing, non-zero exit, timeout,
/// unparseable output) returns an `Err`.
pub async fn fingerprint_url(fpcalc_path: &str, url: &str) -> anyhow::Result<Vec<u32>> {
    let child = tokio::process::Command::new(fpcalc_path)
        .args(["-raw", "-length", &ANALYZE_SECS.to_string()])
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("spawning fpcalc ({fpcalc_path})"))?;

    let output = tokio::time::timeout(FPCALC_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| anyhow!("fpcalc timed out after {}s", FPCALC_TIMEOUT.as_secs()))?
        .context("waiting for fpcalc")?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        bail!("fpcalc exited with {}: {}", output.status, stderr.trim());
    }

    let stdout = String::from_utf8_lossy(&output.stdout);
    parse_fpcalc_raw(&stdout)
}

/// Parse the `FINGERPRINT=` line of `fpcalc -raw` output into a `Vec<u32>`.
/// The value is a comma-separated list of unsigned 32-bit integers.
fn parse_fpcalc_raw(stdout: &str) -> anyhow::Result<Vec<u32>> {
    let line = stdout
        .lines()
        .find_map(|l| l.strip_prefix("FINGERPRINT="))
        .ok_or_else(|| anyhow!("no FINGERPRINT= line in fpcalc output"))?;
    let trimmed = line.trim();
    if trimmed.is_empty() {
        bail!("empty fpcalc fingerprint");
    }
    let points: Result<Vec<u32>, _> = trimmed
        .split(',')
        .map(|n| n.trim().parse::<u32>())
        .collect();
    let points = points.context("parsing fpcalc fingerprint integers")?;
    if points.is_empty() {
        bail!("fpcalc produced no fingerprint points");
    }
    Ok(points)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bytes_round_trip() {
        let points = vec![0u32, 1, 2, u32::MAX, 0x0A0B_0C0D, 4242];
        let bytes = to_bytes(&points);
        assert_eq!(bytes.len(), points.len() * 4);
        assert_eq!(from_bytes(&bytes), points);
    }

    #[test]
    fn bytes_are_little_endian() {
        // 0x04030201 → bytes 01 02 03 04.
        assert_eq!(to_bytes(&[0x0403_0201]), vec![0x01, 0x02, 0x03, 0x04]);
    }

    #[test]
    fn empty_round_trips() {
        assert!(to_bytes(&[]).is_empty());
        assert!(from_bytes(&[]).is_empty());
    }

    #[test]
    fn from_bytes_ignores_trailing_partial_word() {
        // 5 bytes → one full u32, the stray 5th byte dropped.
        assert_eq!(from_bytes(&[0x01, 0x00, 0x00, 0x00, 0xFF]), vec![1]);
    }

    #[test]
    fn parses_raw_fingerprint_line() {
        let out = "DURATION=240\nFINGERPRINT=1,2,3,4000000000\n";
        assert_eq!(parse_fpcalc_raw(out).unwrap(), vec![1, 2, 3, 4_000_000_000]);
    }

    #[test]
    fn parse_rejects_missing_line() {
        assert!(parse_fpcalc_raw("DURATION=240\n").is_err());
    }

    #[test]
    fn parse_rejects_non_numeric() {
        assert!(parse_fpcalc_raw("FINGERPRINT=1,2,notanumber\n").is_err());
    }

    #[test]
    fn parse_rejects_empty_value() {
        assert!(parse_fpcalc_raw("FINGERPRINT=\n").is_err());
    }
}
