//! Scoring and ordering of release candidates against user preferences.

use serde::Serialize;
use utoipa::ToSchema;

use crate::{db::preferences::Preferences, indexer::RawRelease};

use super::parse::{parse_release_name, ParsedRelease, Source};

#[derive(Debug, Clone, Serialize, ToSchema)]
pub struct RankedRelease {
    pub raw: RawRelease,
    pub parsed: ParsedRelease,
    pub score: i64,
    /// Set when the release is hard-excluded; contains the human-readable reason.
    pub rejected: Option<String>,
}

/// Parse, score and order candidates. Accepted releases come first, sorted by
/// descending score; rejected ones follow with their reason. Ordering is
/// deterministic: score, then recency, then title.
pub fn rank(releases: Vec<RawRelease>, prefs: &Preferences) -> Vec<RankedRelease> {
    let mut ranked: Vec<RankedRelease> = releases
        .into_iter()
        .map(|raw| {
            let parsed = parse_release_name(&raw.title);
            let rejected = rejection_reason(&raw, &parsed, prefs);
            let score = score(&raw, &parsed, prefs);
            RankedRelease {
                raw,
                parsed,
                score,
                rejected,
            }
        })
        .collect();

    ranked.sort_by(|a, b| {
        a.rejected
            .is_some()
            .cmp(&b.rejected.is_some())
            .then_with(|| b.score.cmp(&a.score))
            .then_with(|| b.raw.posted_at.cmp(&a.raw.posted_at))
            .then_with(|| a.raw.title.cmp(&b.raw.title))
    });
    ranked
}

fn rejection_reason(
    raw: &RawRelease,
    parsed: &ParsedRelease,
    prefs: &Preferences,
) -> Option<String> {
    let title_lower = raw.title.to_lowercase();
    for term in prefs.blocked_terms.iter().filter(|t| !t.is_empty()) {
        if title_lower.contains(&term.to_lowercase()) {
            return Some(format!("blocked term '{term}'"));
        }
    }
    if let (Some(max), Some(size)) = (prefs.max_size_bytes, raw.size_bytes) {
        if size > max {
            return Some(format!("size {size} exceeds max {max} bytes"));
        }
    }
    if let Some(resolution) = parsed.resolution {
        if resolution > prefs.max_resolution {
            return Some(format!(
                "resolution {resolution} exceeds max {}",
                prefs.max_resolution
            ));
        }
    }
    None
}

fn score(raw: &RawRelease, parsed: &ParsedRelease, prefs: &Preferences) -> i64 {
    let mut score: i64 = 0;

    // Resolution: exact preferred match wins big; otherwise penalize by
    // distance so 1080p-preferred ranks 720p above 480p and above 2160p only
    // by distance, not absolute pixel count.
    score += match parsed.resolution {
        Some(res) if res == prefs.preferred_resolution => 1000,
        Some(res) => 1000 - 300 * (res.tier() - prefs.preferred_resolution.tier()).abs(),
        None => 100,
    };

    if let Some(codec) = parsed.video_codec {
        if contains_ci(&prefs.preferred_video_codecs, codec.as_str()) {
            score += 300;
        }
    }
    if let Some(codec) = parsed.audio_codec {
        if contains_ci(&prefs.preferred_audio_codecs, codec.as_str()) {
            score += 150;
        }
    }

    score += match parsed.source {
        Some(Source::Remux) => 500,
        Some(Source::BluRay) => 400,
        Some(Source::WebDl) => 300,
        Some(Source::WebRip) => 200,
        Some(Source::Hdtv) => 100,
        Some(Source::DvdRip) => 50,
        Some(Source::Telesync) | Some(Source::Cam) => -500,
        None => 0,
    };

    let title_lower = raw.title.to_lowercase();
    for term in prefs.allowed_terms.iter().filter(|t| !t.is_empty()) {
        if title_lower.contains(&term.to_lowercase()) {
            score += 200;
        }
    }

    // Mild size sanity: implausibly small files are usually spam or samples.
    if let Some(size) = raw.size_bytes {
        if size < 100 * 1024 * 1024 {
            score -= 100;
        }
    }

    score
}

fn contains_ci(haystack: &[String], needle: &str) -> bool {
    haystack.iter().any(|h| h.eq_ignore_ascii_case(needle))
}

#[cfg(test)]
mod tests {
    use chrono::{TimeZone, Utc};

    use super::*;
    use crate::release::parse::Resolution;

    fn prefs() -> Preferences {
        Preferences {
            preferred_resolution: Resolution::R1080p,
            max_resolution: Resolution::R2160p,
            preferred_video_codecs: vec!["h264".into(), "hevc".into()],
            preferred_audio_codecs: vec!["aac".into(), "ac3".into(), "eac3".into()],
            max_size_bytes: None,
            language: "en".into(),
            allowed_terms: vec![],
            blocked_terms: vec!["CAM".into(), "TELESYNC".into(), "HDCAM".into()],
        }
    }

    fn release(title: &str) -> RawRelease {
        RawRelease {
            title: title.into(),
            guid: format!("guid-{title}"),
            nzb_url: format!("https://indexer.example/getnzb/{title}"),
            size_bytes: Some(4 * 1024 * 1024 * 1024),
            posted_at: Some(Utc.with_ymd_and_hms(2026, 6, 1, 12, 0, 0).unwrap()),
            indexer_id: 1,
            indexer_name: "test".into(),
        }
    }

    #[test]
    fn blocked_term_is_rejected_with_reason() {
        let ranked = rank(
            vec![
                release("Movie.2026.1080p.BluRay.x264-GOOD"),
                release("Movie.2026.HDCAM.x264-BAD"),
            ],
            &prefs(),
        );
        assert_eq!(ranked.len(), 2);
        assert!(ranked[0].rejected.is_none());
        let reason = ranked[1].rejected.as_deref().expect("must be rejected");
        assert!(reason.contains("blocked term"), "reason was: {reason}");
        // Rejected releases sort after accepted ones.
        assert_eq!(ranked[1].raw.title, "Movie.2026.HDCAM.x264-BAD");
    }

    #[test]
    fn max_resolution_is_enforced() {
        let mut p = prefs();
        p.max_resolution = Resolution::R1080p;
        let ranked = rank(vec![release("Movie.2026.2160p.WEB-DL.HEVC-X")], &p);
        let reason = ranked[0]
            .rejected
            .as_deref()
            .expect("2160p must be rejected");
        assert!(reason.contains("2160p"), "reason was: {reason}");
        assert!(reason.contains("1080p"), "reason was: {reason}");
    }

    #[test]
    fn max_size_is_enforced() {
        let mut p = prefs();
        p.max_size_bytes = Some(1024);
        let ranked = rank(vec![release("Movie.2026.1080p.BluRay.x264-BIG")], &p);
        assert!(ranked[0].rejected.as_deref().unwrap().contains("size"));
    }

    #[test]
    fn preferred_resolution_outranks_higher_resolution() {
        let ranked = rank(
            vec![
                release("Movie.2026.2160p.BluRay.x265-UHD"),
                release("Movie.2026.1080p.BluRay.x264-FHD"),
            ],
            &prefs(),
        );
        assert!(
            ranked[0].raw.title.contains("1080p"),
            "preferred 1080p must win"
        );
        assert!(ranked[0].score > ranked[1].score);
    }

    #[test]
    fn source_tier_breaks_resolution_ties() {
        let ranked = rank(
            vec![
                release("Movie.2026.1080p.HDTV.x264-TV"),
                release("Movie.2026.1080p.BluRay.REMUX.AVC-REM"),
                release("Movie.2026.1080p.WEB-DL.H.264-WEB"),
            ],
            &prefs(),
        );
        let titles: Vec<&str> = ranked.iter().map(|r| r.raw.title.as_str()).collect();
        assert!(titles[0].contains("REMUX"));
        assert!(titles[1].contains("WEB-DL"));
        assert!(titles[2].contains("HDTV"));
    }

    #[test]
    fn allowed_terms_boost_score() {
        let mut p = prefs();
        p.allowed_terms = vec!["ATMOS".into()];
        let ranked = rank(
            vec![
                release("Movie.2026.1080p.BluRay.x264-PLAIN"),
                release("Movie.2026.1080p.BluRay.Atmos.x264-BOOSTED"),
            ],
            &p,
        );
        assert!(ranked[0].raw.title.contains("BOOSTED"));
    }

    #[test]
    fn deterministic_order_with_equal_scores() {
        let a = release("Movie.2026.1080p.BluRay.x264-AAA");
        let b = release("Movie.2026.1080p.BluRay.x264-BBB");
        let first = rank(vec![a.clone(), b.clone()], &prefs());
        let second = rank(vec![b, a], &prefs());
        let order1: Vec<&str> = first.iter().map(|r| r.raw.title.as_str()).collect();
        let order2: Vec<&str> = second.iter().map(|r| r.raw.title.as_str()).collect();
        assert_eq!(order1, order2, "input order must not matter");
        assert_eq!(
            order1[0], "Movie.2026.1080p.BluRay.x264-AAA",
            "title tiebreak"
        );
    }

    #[test]
    fn newer_release_wins_score_tie() {
        let mut old = release("Movie.2026.1080p.BluRay.x264-OLD");
        old.posted_at = Some(Utc.with_ymd_and_hms(2025, 1, 1, 0, 0, 0).unwrap());
        let newer = release("Movie.2026.1080p.BluRay.x264-NEW");
        let ranked = rank(vec![old, newer], &prefs());
        assert!(ranked[0].raw.title.ends_with("NEW"));
    }

    #[test]
    fn tiny_size_is_penalized() {
        let mut small = release("Movie.2026.1080p.BluRay.x264-SMALL");
        small.size_bytes = Some(10 * 1024 * 1024);
        let normal = release("Movie.2026.1080p.BluRay.x264-NORMAL");
        let ranked = rank(vec![small, normal], &prefs());
        assert!(ranked[0].raw.title.ends_with("NORMAL"));
    }
}
