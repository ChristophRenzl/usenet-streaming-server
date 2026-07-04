//! File classification within an NZB and selection of the main content
//! (the thing we actually stream): either a plain media file or a RAR set.

use std::collections::HashMap;

use crate::error::{AppError, AppResult};
use crate::rar::volumes::volume_sort_key;

use super::parse::Nzb;

/// Coarse classification of one file in an NZB, by extracted filename.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FileKind {
    /// PAR2 recovery data (`.par2`, `.volNNN+NN.par2`).
    Par2,
    /// Scene metadata: `.nfo`, `.sfv`, `.srr`, `.srs`.
    Metadata,
    /// Sample/proof clips.
    Sample,
    /// Subtitles: `.srt`, `.idx`, `.sub`.
    Subtitle,
    /// RAR volume (`.rar`, `.rNN`, `.partNN.rar`).
    RarVolume,
    /// Plain playable media (`.mkv`, `.mp4`, ...).
    Media,
    Other,
}

const MEDIA_EXTENSIONS: &[&str] = &["mkv", "mp4", "avi", "ts", "m2ts", "wmv"];
const METADATA_EXTENSIONS: &[&str] = &["nfo", "sfv", "srr", "srs"];
const SUBTITLE_EXTENSIONS: &[&str] = &["srt", "idx", "sub"];

/// Extract a probable filename from an article subject. Prefers the quoted
/// `"file.ext"` convention; otherwise falls back to the first token that
/// looks like it has a file extension.
pub fn extract_filename(subject: &str) -> Option<String> {
    // Quoted convention: [1/9] - "release.part1.rar" yEnc (1/50)
    if let Some(start) = subject.find('"') {
        if let Some(len) = subject[start + 1..].find('"') {
            let name = subject[start + 1..start + 1 + len].trim();
            if !name.is_empty() {
                return Some(name.to_string());
            }
        }
    }
    // Fallback: a whitespace token with a plausible extension.
    subject
        .split_whitespace()
        .map(|token| token.trim_matches(|c: char| "()[]{}'\",;".contains(c)))
        .find(|token| looks_like_filename(token))
        .map(|s| s.to_string())
}

fn looks_like_filename(token: &str) -> bool {
    let Some((stem, ext)) = token.rsplit_once('.') else {
        return false;
    };
    if stem.is_empty() || ext.len() < 2 || ext.len() > 4 {
        return false;
    }
    ext.chars().all(|c| c.is_ascii_alphanumeric())
}

fn extension(name: &str) -> Option<String> {
    name.rsplit_once('.').map(|(_, e)| e.to_ascii_lowercase())
}

fn is_rar_volume(lower: &str) -> bool {
    if lower.ends_with(".rar") {
        return true;
    }
    // .rNN / .rNNN old-style volumes
    if let Some((_, ext)) = lower.rsplit_once('.') {
        if let Some(digits) = ext.strip_prefix('r') {
            return (2..=3).contains(&digits.len()) && digits.bytes().all(|b| b.is_ascii_digit());
        }
    }
    false
}

/// Classify a filename. The checks are ordered: recovery/metadata first, then
/// samples and subtitles, then RAR volumes and plain media.
pub fn classify(file_name: &str) -> FileKind {
    let lower = file_name.to_ascii_lowercase();
    let ext = extension(&lower).unwrap_or_default();

    if ext == "par2" {
        return FileKind::Par2;
    }
    if METADATA_EXTENSIONS.contains(&ext.as_str()) {
        return FileKind::Metadata;
    }
    if lower.contains("sample") || lower.contains("proof") {
        return FileKind::Sample;
    }
    if SUBTITLE_EXTENSIONS.contains(&ext.as_str()) {
        return FileKind::Subtitle;
    }
    if is_rar_volume(&lower) {
        return FileKind::RarVolume;
    }
    if MEDIA_EXTENSIONS.contains(&ext.as_str()) {
        return FileKind::Media;
    }
    FileKind::Other
}

/// Base name a RAR volume belongs to: `x.part03.rar` -> `x`, `x.r00` -> `x`,
/// `x.rar` -> `x`.
fn rar_set_base(lower: &str) -> String {
    if let Some(stripped) = lower.strip_suffix(".rar") {
        if let Some((base, part)) = stripped.rsplit_once(".part") {
            if !part.is_empty() && part.bytes().all(|b| b.is_ascii_digit()) {
                return base.to_string();
            }
        }
        return stripped.to_string();
    }
    lower.rsplit_once('.').map_or_else(
        || lower.to_string(),
        |(base, _ext)| base.to_string(), // .rNN
    )
}

/// Reference to one file inside an [`Nzb`], with its extracted filename.
#[derive(Debug, Clone)]
pub struct NzbFileRef {
    /// Index into `nzb.files`.
    pub index: usize,
    pub file_name: String,
    /// Approximate encoded size (sum of segment bytes).
    pub bytes: u64,
}

/// What the streamer should play from an NZB.
#[derive(Debug, Clone)]
pub enum MainContent {
    /// A plain media file posted directly.
    Plain(NzbFileRef),
    /// A RAR set, volumes in natural order.
    RarSet(Vec<NzbFileRef>),
}

/// Pick the main content of an NZB: the largest plain media file or the
/// largest RAR set (by total bytes), whichever is bigger.
pub fn select_main(nzb: &Nzb) -> AppResult<MainContent> {
    let mut best_media: Option<NzbFileRef> = None;
    let mut rar_sets: HashMap<String, Vec<NzbFileRef>> = HashMap::new();

    for (index, file) in nzb.files.iter().enumerate() {
        let Some(file_name) = extract_filename(&file.subject) else {
            continue;
        };
        let entry = NzbFileRef {
            index,
            bytes: file.total_bytes(),
            file_name,
        };
        match classify(&entry.file_name) {
            FileKind::Media => {
                if best_media.as_ref().is_none_or(|b| entry.bytes > b.bytes) {
                    best_media = Some(entry);
                }
            }
            FileKind::RarVolume => {
                let base = rar_set_base(&entry.file_name.to_ascii_lowercase());
                rar_sets.entry(base).or_default().push(entry);
            }
            _ => {}
        }
    }

    let best_set = rar_sets
        .into_values()
        .max_by_key(|set| set.iter().map(|f| f.bytes).sum::<u64>());

    match (best_media, best_set) {
        (Some(media), Some(mut set)) => {
            let set_bytes: u64 = set.iter().map(|f| f.bytes).sum();
            if media.bytes >= set_bytes {
                Ok(MainContent::Plain(media))
            } else {
                sort_volumes(&mut set);
                Ok(MainContent::RarSet(set))
            }
        }
        (Some(media), None) => Ok(MainContent::Plain(media)),
        (None, Some(mut set)) => {
            sort_volumes(&mut set);
            Ok(MainContent::RarSet(set))
        }
        (None, None) => Err(AppError::NoRelease(
            "NZB contains no playable media file or RAR set".into(),
        )),
    }
}

fn sort_volumes(set: &mut [NzbFileRef]) {
    set.sort_by_key(|f| volume_sort_key(&f.file_name));
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::nzb::parse::{NzbFile, Segment};

    fn file(subject: &str, bytes: u64) -> NzbFile {
        NzbFile {
            subject: subject.into(),
            poster: String::new(),
            date: 0,
            groups: vec![],
            segments: vec![Segment {
                number: 1,
                bytes,
                message_id: format!("{}@test", subject.len()),
            }],
        }
    }

    #[test]
    fn extracts_quoted_filename() {
        assert_eq!(
            extract_filename(r#"Rel [1/9] - "My Movie.part01.rar" yEnc (1/50)"#).as_deref(),
            Some("My Movie.part01.rar")
        );
    }

    #[test]
    fn extracts_token_filename_without_quotes() {
        assert_eq!(
            extract_filename("Movie.2024.1080p.mkv yEnc (1/40)").as_deref(),
            Some("Movie.2024.1080p.mkv")
        );
        assert_eq!(extract_filename("no filename here at all"), None);
    }

    #[test]
    fn classification_matrix() {
        assert_eq!(classify("x.par2"), FileKind::Par2);
        assert_eq!(classify("x.vol031+32.PAR2"), FileKind::Par2);
        assert_eq!(classify("x.nfo"), FileKind::Metadata);
        assert_eq!(classify("x.sfv"), FileKind::Metadata);
        assert_eq!(classify("movie.sample.mkv"), FileKind::Sample);
        assert_eq!(classify("movie-proof.jpg"), FileKind::Sample);
        assert_eq!(classify("x.srt"), FileKind::Subtitle);
        assert_eq!(classify("x.rar"), FileKind::RarVolume);
        assert_eq!(classify("x.r00"), FileKind::RarVolume);
        assert_eq!(classify("x.part42.rar"), FileKind::RarVolume);
        assert_eq!(classify("x.mkv"), FileKind::Media);
        assert_eq!(classify("x.m2ts"), FileKind::Media);
        assert_eq!(classify("x.exe"), FileKind::Other);
        assert_eq!(classify("noext"), FileKind::Other);
    }

    #[test]
    fn selects_largest_media_ignoring_samples_and_par2() {
        let nzb = Nzb {
            files: vec![
                file(r#""movie.sample.mkv""#, 5_000),
                file(r#""movie.mkv""#, 4_000_000),
                file(r#""movie.par2""#, 100_000),
                file(r#""movie.nfo""#, 1_000),
            ],
        };
        match select_main(&nzb).expect("main") {
            MainContent::Plain(f) => assert_eq!(f.file_name, "movie.mkv"),
            other => panic!("expected Plain, got {other:?}"),
        }
    }

    #[test]
    fn selects_rar_set_in_natural_order() {
        let nzb = Nzb {
            files: vec![
                file(r#""movie.part10.rar""#, 1_000_000),
                file(r#""movie.part2.rar""#, 1_000_000),
                file(r#""movie.part1.rar""#, 1_000_000),
                file(r#""movie.par2""#, 50_000),
            ],
        };
        match select_main(&nzb).expect("main") {
            MainContent::RarSet(set) => {
                let names: Vec<_> = set.iter().map(|f| f.file_name.as_str()).collect();
                assert_eq!(
                    names,
                    ["movie.part1.rar", "movie.part2.rar", "movie.part10.rar"]
                );
            }
            other => panic!("expected RarSet, got {other:?}"),
        }
    }

    #[test]
    fn old_style_volumes_order_rar_first() {
        let nzb = Nzb {
            files: vec![
                file(r#""movie.r01""#, 1_000_000),
                file(r#""movie.rar""#, 1_000_000),
                file(r#""movie.r00""#, 1_000_000),
            ],
        };
        match select_main(&nzb).expect("main") {
            MainContent::RarSet(set) => {
                let names: Vec<_> = set.iter().map(|f| f.file_name.as_str()).collect();
                assert_eq!(names, ["movie.rar", "movie.r00", "movie.r01"]);
            }
            other => panic!("expected RarSet, got {other:?}"),
        }
    }

    #[test]
    fn rar_set_beats_smaller_media_and_vice_versa() {
        let nzb = Nzb {
            files: vec![
                file(r#""movie.part1.rar""#, 3_000_000),
                file(r#""movie.part2.rar""#, 3_000_000),
                file(r#""extras.mkv""#, 1_000_000),
            ],
        };
        assert!(matches!(
            select_main(&nzb).expect("main"),
            MainContent::RarSet(_)
        ));

        let nzb = Nzb {
            files: vec![
                file(r#""tiny.part1.rar""#, 10_000),
                file(r#""movie.mkv""#, 9_000_000),
            ],
        };
        assert!(matches!(
            select_main(&nzb).expect("main"),
            MainContent::Plain(_)
        ));
    }

    #[test]
    fn nothing_playable_is_an_error() {
        let nzb = Nzb {
            files: vec![file(r#""readme.nfo""#, 100), file(r#""x.par2""#, 100)],
        };
        assert!(matches!(select_main(&nzb), Err(AppError::NoRelease(_))));
    }
}
