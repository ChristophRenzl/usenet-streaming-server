//! Audio-fingerprint intro detection, Jellyfin-style.
//!
//! Chromaprint (via `fpcalc`) turns the first few minutes of an episode's
//! audio into a sequence of 32-bit "sub-fingerprints", one every
//! [`SECS_PER_POINT`] seconds. Two episodes of the same season share the same
//! opening, so their fingerprints contain a long, near-identical contiguous run
//! near the start. [`find_intro`] locates that run and turns it into an
//! `intro_end_secs` for the client's "Skip Intro".
//!
//! This module is deliberately pure (no I/O): it takes two `&[u32]`
//! fingerprints and returns the intro bounds it finds in the *first* one. The
//! fingerprint extraction and caching live in [`crate::stream::fingerprint`] and
//! the session wiring in [`crate::api::stream`].

/// Seconds of audio each chromaprint sub-fingerprint (one `u32`) represents.
/// Chromaprint's default frame configuration advances by 4096 samples at
/// 11025 Hz with a 2/3 overlap → one point per ~0.1238s. This constant maps
/// point indices back to wall-clock seconds.
pub const SECS_PER_POINT: f64 = 0.1238;

/// Two sub-fingerprints "match" when at most this many of their 32 bits differ
/// (Hamming distance ≤ threshold). Chromaprint points are noisy even for
/// identical audio (different encoders, bitrates), so an exact match is far too
/// strict; ~6 bits tolerates real-world encode noise while still rejecting
/// unrelated audio (which averages ~16 differing bits — half the word).
const BIT_THRESHOLD: u32 = 6;

/// Candidate alignment offsets searched, in points, either side of zero
/// (~30s worth). Two episodes rarely start their opening at the exact same
/// timestamp — a cold open, a "previously on", or a few seconds of network
/// logo shift it — so the sibling fingerprint is slid across this window to
/// find the alignment that maximises the shared run.
const MAX_OFFSET_POINTS: i64 = (30.0 / SECS_PER_POINT) as i64; // ~242 points

/// Shortest run (in points) accepted as an intro (~10s). Below this it is more
/// likely a coincidental match (shared silence, a common stinger) than a real
/// shared opening.
const MIN_INTRO_POINTS: usize = (10.0 / SECS_PER_POINT) as usize; // ~80 points

/// The detected intro must begin no later than this into episode A (~150s in
/// points). A matching run that only starts deep into the episode is not the
/// opening (e.g. shared end-credits music), so it is rejected.
const MAX_INTRO_START_POINTS: usize = (150.0 / SECS_PER_POINT) as usize; // ~1211 points

/// Hamming distance between two sub-fingerprints: the number of differing bits.
#[inline]
fn hamming(a: u32, b: u32) -> u32 {
    (a ^ b).count_ones()
}

/// The longest contiguous run of matching points, evaluated at a single fixed
/// alignment `offset` (A[i] vs B[i + offset]). Returns the run as a half-open
/// range of indices *into A* (`start..end`), or `None` when no point matches.
///
/// Only the *longest* run at this offset is returned; a couple of noisy points
/// in the middle of an otherwise-shared opening would split it, so the caller
/// relies on [`BIT_THRESHOLD`] being loose enough to keep the run whole.
fn longest_run_at_offset(a: &[u32], b: &[u32], offset: i64) -> Option<std::ops::Range<usize>> {
    // A-indices `i` for which B[i + offset] is in bounds: need
    // 0 <= i+offset < b.len() and 0 <= i < a.len().
    let lo = if offset < 0 { (-offset) as usize } else { 0 };
    let hi = {
        let by_b = (b.len() as i64 - offset).max(0) as usize;
        a.len().min(by_b)
    };

    let mut best: Option<std::ops::Range<usize>> = None;
    let mut cur_start: Option<usize> = None;
    // The loop indexes two slices in lockstep — `a[i]` and `b[i + offset]` — so
    // a single `enumerate()` over one of them cannot express it cleanly.
    #[allow(clippy::needless_range_loop)]
    for i in lo..hi {
        let j = (i as i64 + offset) as usize;
        let matches = hamming(a[i], b[j]) <= BIT_THRESHOLD;
        match (matches, cur_start) {
            (true, None) => cur_start = Some(i),
            (true, Some(_)) => {}
            (false, Some(start)) => {
                let run = start..i;
                if best.as_ref().is_none_or(|b| run.len() > b.len()) {
                    best = Some(run);
                }
                cur_start = None;
            }
            (false, None) => {}
        }
    }
    if let Some(start) = cur_start {
        let run = start..hi;
        if best.as_ref().is_none_or(|b| run.len() > b.len()) {
            best = Some(run);
        }
    }
    best
}

/// Detect the intro in episode `a` by comparing it against a sibling `b` from
/// the same season.
///
/// Slides `b` across `a` over `±`[`MAX_OFFSET_POINTS`] and keeps the longest
/// contiguous run of matching points (Hamming ≤ [`BIT_THRESHOLD`]). If that run
/// is at least [`MIN_INTRO_POINTS`] long **and** begins within the first
/// [`MAX_INTRO_START_POINTS`] of `a`, it is taken to be the shared opening.
///
/// Returns `Some((intro_start_secs, intro_end_secs))` — the run's bounds *in
/// episode A*, converted to seconds via [`SECS_PER_POINT`] — or `None` when no
/// qualifying run is found.
pub fn find_intro(a: &[u32], b: &[u32]) -> Option<(f64, f64)> {
    if a.is_empty() || b.is_empty() {
        return None;
    }
    let mut best: Option<std::ops::Range<usize>> = None;
    for offset in -MAX_OFFSET_POINTS..=MAX_OFFSET_POINTS {
        if let Some(run) = longest_run_at_offset(a, b, offset) {
            if best.as_ref().is_none_or(|b| run.len() > b.len()) {
                best = Some(run);
            }
        }
    }
    let run = best?;
    if run.len() < MIN_INTRO_POINTS || run.start > MAX_INTRO_START_POINTS {
        return None;
    }
    let start_secs = run.start as f64 * SECS_PER_POINT;
    let end_secs = run.end as f64 * SECS_PER_POINT;
    Some((start_secs, end_secs))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Deterministic pseudo-random u32 stream (xorshift), for synthetic
    /// "unrelated audio" — its points differ from any other stream in ~16 bits.
    struct Rng(u64);
    impl Rng {
        fn new(seed: u64) -> Self {
            Self(seed | 1)
        }
        fn next_u32(&mut self) -> u32 {
            self.0 ^= self.0 >> 12;
            self.0 ^= self.0 << 25;
            self.0 ^= self.0 >> 27;
            (self.0.wrapping_mul(0x2545F4914F6CDD1D) >> 32) as u32
        }
    }

    fn random_vec(seed: u64, len: usize) -> Vec<u32> {
        let mut rng = Rng::new(seed);
        (0..len).map(|_| rng.next_u32()).collect()
    }

    /// Flip `n` low bits of `x` (deterministically), to simulate encode noise
    /// on a shared point without exceeding the match threshold.
    fn flip_bits(x: u32, n: u32) -> u32 {
        let mut out = x;
        for bit in 0..n {
            out ^= 1 << (bit * 3 % 32);
        }
        out
    }

    #[test]
    fn hamming_counts_differing_bits() {
        assert_eq!(hamming(0, 0), 0);
        assert_eq!(hamming(0b1011, 0b0010), 2);
        assert_eq!(hamming(u32::MAX, 0), 32);
    }

    #[test]
    fn detects_shared_block_at_zero_offset() {
        // A shared opening of 120 points (~15s) starting at point 20, embedded
        // in otherwise-unrelated audio, with a few bit flips (noise).
        let shared: Vec<u32> = random_vec(1, 120);
        let noisy: Vec<u32> = shared.iter().map(|&x| flip_bits(x, 3)).collect();

        let mut a = random_vec(2, 20);
        a.extend_from_slice(&shared);
        a.extend(random_vec(3, 400));

        let mut b = random_vec(4, 20);
        b.extend_from_slice(&noisy);
        b.extend(random_vec(5, 400));

        let (start, end) = find_intro(&a, &b).expect("shared block detected");
        // Run starts at point 20, spans 120 points.
        assert!(
            (start - 20.0 * SECS_PER_POINT).abs() < 2.0 * SECS_PER_POINT,
            "start {start}"
        );
        assert!(
            (end - 140.0 * SECS_PER_POINT).abs() < 2.0 * SECS_PER_POINT,
            "end {end}"
        );
    }

    #[test]
    fn detects_shared_block_at_nonzero_offset() {
        // The opening sits at point 10 in A but point 45 in B (a 35-point /
        // ~4.3s shift, e.g. B has a longer cold-open). The offset search must
        // still line them up.
        let shared: Vec<u32> = random_vec(10, 150);
        let noisy: Vec<u32> = shared.iter().map(|&x| flip_bits(x, 4)).collect();

        let mut a = random_vec(11, 10);
        a.extend_from_slice(&shared);
        a.extend(random_vec(12, 300));

        let mut b = random_vec(13, 45);
        b.extend_from_slice(&noisy);
        b.extend(random_vec(14, 300));

        let (start, end) = find_intro(&a, &b).expect("shared block at offset detected");
        assert!(
            (start - 10.0 * SECS_PER_POINT).abs() < 2.0 * SECS_PER_POINT,
            "start {start}"
        );
        assert!(
            (end - 160.0 * SECS_PER_POINT).abs() < 3.0 * SECS_PER_POINT,
            "end {end}"
        );
    }

    #[test]
    fn no_shared_block_returns_none() {
        let a = random_vec(100, 500);
        let b = random_vec(200, 500);
        assert_eq!(find_intro(&a, &b), None);
    }

    #[test]
    fn short_shared_block_is_rejected() {
        // A shared run of only 40 points (~5s) is below MIN_INTRO_POINTS.
        let shared: Vec<u32> = random_vec(300, 40);
        let mut a = random_vec(301, 20);
        a.extend_from_slice(&shared);
        a.extend(random_vec(302, 400));
        let mut b = random_vec(303, 20);
        b.extend_from_slice(&shared);
        b.extend(random_vec(304, 400));
        assert_eq!(find_intro(&a, &b), None);
    }

    #[test]
    fn block_starting_too_late_is_rejected() {
        // A long shared run, but it only begins ~200s into A (past
        // MAX_INTRO_START_POINTS ~150s) — shared end-credits music, not the
        // opening.
        let lead = MAX_INTRO_START_POINTS + 200;
        let shared: Vec<u32> = random_vec(400, 150);
        let mut a = random_vec(401, lead);
        a.extend_from_slice(&shared);
        a.extend(random_vec(402, 100));
        let mut b = random_vec(403, lead);
        b.extend_from_slice(&shared);
        b.extend(random_vec(404, 100));
        assert_eq!(find_intro(&a, &b), None);
    }

    #[test]
    fn empty_inputs_return_none() {
        assert_eq!(find_intro(&[], &[1, 2, 3]), None);
        assert_eq!(find_intro(&[1, 2, 3], &[]), None);
    }

    #[test]
    fn identical_fingerprints_detect_a_full_length_intro() {
        // Two identical fingerprints: the whole thing matches at offset 0, so
        // the run is the full length (clamped by the start window, which the
        // run at 0 satisfies).
        let fp = random_vec(500, 200);
        let (start, end) = find_intro(&fp, &fp).expect("identical detected");
        assert!((start - 0.0).abs() < 1e-9, "start {start}");
        assert!(
            (end - 200.0 * SECS_PER_POINT).abs() < 1e-6,
            "end {end} should be full length"
        );
    }
}
