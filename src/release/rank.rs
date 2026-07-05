//! Scoring and ordering of release candidates against user preferences.

use serde::Serialize;
use utoipa::ToSchema;

use crate::{db::preferences::Preferences, indexer::RawRelease};

use super::parse::{parse_release_name, ParsedRelease, Resolution, Source};

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
///
/// `device_cap` is a per-request hard resolution ceiling reported by the
/// client (what its display supports). Releases above the cap are rejected,
/// and scoring treats `min(preferred, effective max)` as the preferred
/// resolution so the best supported quality ranks first.
pub fn rank(
    releases: Vec<RawRelease>,
    prefs: &Preferences,
    device_cap: Option<Resolution>,
) -> Vec<RankedRelease> {
    let scoring_prefs = effective_prefs(prefs, device_cap);
    let mut ranked: Vec<RankedRelease> = releases
        .into_iter()
        .map(|raw| {
            let parsed = parse_release_name(&raw.title);
            let rejected = rejection_reason(&raw, &parsed, prefs, device_cap);
            let score = score(&raw, &parsed, &scoring_prefs);
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

/// A copy of the preferences clamped to the device cap for scoring:
/// `max_resolution` never exceeds the cap and `preferred_resolution` never
/// exceeds the effective max, so the best supported quality gets the
/// exact-match bonus. Without a cap the preferences are used as stored.
fn effective_prefs(prefs: &Preferences, device_cap: Option<Resolution>) -> Preferences {
    let mut effective = prefs.clone();
    if let Some(cap) = device_cap {
        effective.max_resolution = effective.max_resolution.min(cap);
        effective.preferred_resolution =
            effective.preferred_resolution.min(effective.max_resolution);
    }
    effective
}

fn rejection_reason(
    raw: &RawRelease,
    parsed: &ParsedRelease,
    prefs: &Preferences,
    device_cap: Option<Resolution>,
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
        if let Some(cap) = device_cap {
            if resolution > cap {
                return Some(format!("resolution {resolution} exceeds device max {cap}"));
            }
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
            None,
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
        let ranked = rank(vec![release("Movie.2026.2160p.WEB-DL.HEVC-X")], &p, None);
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
        let ranked = rank(vec![release("Movie.2026.1080p.BluRay.x264-BIG")], &p, None);
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
            None,
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
            None,
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
            None,
        );
        assert!(ranked[0].raw.title.contains("BOOSTED"));
    }

    #[test]
    fn deterministic_order_with_equal_scores() {
        let a = release("Movie.2026.1080p.BluRay.x264-AAA");
        let b = release("Movie.2026.1080p.BluRay.x264-BBB");
        let first = rank(vec![a.clone(), b.clone()], &prefs(), None);
        let second = rank(vec![b, a], &prefs(), None);
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
        let ranked = rank(vec![old, newer], &prefs(), None);
        assert!(ranked[0].raw.title.ends_with("NEW"));
    }

    #[test]
    fn device_cap_rejects_above_cap_with_device_reason() {
        // User allows up to 2160p, but the device only supports 1080p.
        let ranked = rank(
            vec![
                release("Movie.2026.2160p.WEB-DL.HEVC-UHD"),
                release("Movie.2026.1080p.WEB-DL.x264-FHD"),
            ],
            &prefs(),
            Some(Resolution::R1080p),
        );
        assert!(ranked[0].raw.title.contains("1080p"));
        assert!(ranked[0].rejected.is_none());
        let reason = ranked[1].rejected.as_deref().expect("2160p rejected");
        assert!(reason.contains("device max 1080p"), "reason was: {reason}");
        assert!(reason.contains("2160p"), "reason was: {reason}");
    }

    #[test]
    fn preferred_resolution_clamps_to_device_cap() {
        // User prefers 2160p; a 1080p device cap must make 1080p score as
        // the exact preferred match.
        let mut p = prefs();
        p.preferred_resolution = Resolution::R2160p;
        let capped = rank(
            vec![release("Movie.2026.1080p.BluRay.x264-FHD")],
            &p,
            Some(Resolution::R1080p),
        );
        let native = rank(
            vec![release("Movie.2026.1080p.BluRay.x264-FHD")],
            &prefs(), // prefers 1080p natively
            None,
        );
        assert!(capped[0].rejected.is_none());
        assert_eq!(
            capped[0].score, native[0].score,
            "capped preferred must score like a native 1080p preference"
        );
    }

    #[test]
    fn device_capped_preferred_wins_over_higher_and_lower() {
        // User prefers 2160p, device caps at 1080p: 2160p is rejected and
        // 1080p (the best supported quality) outranks 720p.
        let mut p = prefs();
        p.preferred_resolution = Resolution::R2160p;
        let ranked = rank(
            vec![
                release("Movie.2026.720p.BluRay.x264-HD"),
                release("Movie.2026.2160p.BluRay.x265-UHD"),
                release("Movie.2026.1080p.BluRay.x264-FHD"),
            ],
            &p,
            Some(Resolution::R1080p),
        );
        let titles: Vec<&str> = ranked.iter().map(|r| r.raw.title.as_str()).collect();
        assert!(titles[0].contains("1080p"), "order was: {titles:?}");
        assert!(titles[1].contains("720p"), "order was: {titles:?}");
        assert!(ranked[2].rejected.is_some(), "2160p must be rejected");
    }

    #[test]
    fn device_cap_above_user_max_keeps_user_reason() {
        // The stricter of the two limits wins; a generous device cap must
        // not relax the user's max and the reason stays the user one.
        let mut p = prefs();
        p.max_resolution = Resolution::R1080p;
        let ranked = rank(
            vec![release("Movie.2026.2160p.WEB-DL.HEVC-X")],
            &p,
            Some(Resolution::R2160p),
        );
        let reason = ranked[0].rejected.as_deref().expect("must be rejected");
        assert!(reason.contains("exceeds max 1080p"), "reason was: {reason}");
        assert!(!reason.contains("device"), "reason was: {reason}");
    }

    #[test]
    fn tiny_size_is_penalized() {
        let mut small = release("Movie.2026.1080p.BluRay.x264-SMALL");
        small.size_bytes = Some(10 * 1024 * 1024);
        let normal = release("Movie.2026.1080p.BluRay.x264-NORMAL");
        let ranked = rank(vec![small, normal], &prefs(), None);
        assert!(ranked[0].raw.title.ends_with("NORMAL"));
    }
}
