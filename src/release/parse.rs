//! Scene/P2P release-name parsing.
//!
//! Release names are dot/space/dash separated token soups like
//! `Movie.Name.2020.1080p.BluRay.x264-GROUP`. We tokenize case-insensitively
//! and match known quality/source/codec markers.

use std::{fmt, str::FromStr};

use serde::{Deserialize, Serialize};
use utoipa::ToSchema;

/// Video resolution, ordered ascending so `>` means "higher resolution".
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, ToSchema,
)]
pub enum Resolution {
    #[serde(rename = "480p")]
    R480p,
    #[serde(rename = "720p")]
    R720p,
    #[serde(rename = "1080p")]
    R1080p,
    #[serde(rename = "2160p")]
    R2160p,
}

impl Resolution {
    /// Index used for distance-based scoring.
    pub fn tier(self) -> i64 {
        match self {
            Self::R480p => 0,
            Self::R720p => 1,
            Self::R1080p => 2,
            Self::R2160p => 3,
        }
    }
}

impl fmt::Display for Resolution {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let s = match self {
            Self::R480p => "480p",
            Self::R720p => "720p",
            Self::R1080p => "1080p",
            Self::R2160p => "2160p",
        };
        f.write_str(s)
    }
}

impl FromStr for Resolution {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "480p" => Ok(Self::R480p),
            "720p" => Ok(Self::R720p),
            "1080p" => Ok(Self::R1080p),
            "2160p" | "4k" => Ok(Self::R2160p),
            other => Err(format!("unknown resolution '{other}'")),
        }
    }
}

/// Release source, ordered ascending by quality tier.
#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, ToSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum Source {
    Cam,
    Telesync,
    DvdRip,
    Hdtv,
    WebRip,
    WebDl,
    BluRay,
    Remux,
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, ToSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum VideoCodec {
    H264,
    Hevc,
    Xvid,
    Av1,
}

impl VideoCodec {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::H264 => "h264",
            Self::Hevc => "hevc",
            Self::Xvid => "xvid",
            Self::Av1 => "av1",
        }
    }
}

#[derive(
    Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize, ToSchema,
)]
#[serde(rename_all = "lowercase")]
pub enum AudioCodec {
    Aac,
    Ac3,
    Eac3,
    Dts,
    DtsHd,
    TrueHd,
    Flac,
    Opus,
    Mp3,
}

impl AudioCodec {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Aac => "aac",
            Self::Ac3 => "ac3",
            Self::Eac3 => "eac3",
            Self::Dts => "dts",
            Self::DtsHd => "dtshd",
            Self::TrueHd => "truehd",
            Self::Flac => "flac",
            Self::Opus => "opus",
            Self::Mp3 => "mp3",
        }
    }
}

/// Structured attributes extracted from a release name.
#[derive(Debug, Clone, Default, PartialEq, Serialize, ToSchema)]
pub struct ParsedRelease {
    pub resolution: Option<Resolution>,
    pub source: Option<Source>,
    pub video_codec: Option<VideoCodec>,
    pub audio_codec: Option<AudioCodec>,
    /// Language hints found in the name (e.g. "german", "multi").
    pub languages: Vec<String>,
    pub release_group: Option<String>,
    /// HDR10 / generic HDR marker present.
    pub hdr: bool,
    /// Dolby Vision marker present.
    pub dolby_vision: bool,
}

const LANGUAGE_HINTS: &[&str] = &[
    "german", "french", "italian", "spanish", "dutch", "nordic", "korean", "japanese", "hindi",
    "russian", "multi", "dual", "vostfr", "english",
];

/// Tokens that follow a `-` but are quality markers, not release groups.
const NOT_A_GROUP: &[&str] = &[
    "dl", "web", "rip", "hd", "ma", "hdtv", "bluray", "remux", "cam", "ts", "x264", "x265", "264",
    "265", "h264", "h265", "hevc", "av1", "xvid", "aac", "ac3", "eac3", "dts", "dv", "hdr",
    "hdr10", "atmos",
];

pub fn parse_release_name(name: &str) -> ParsedRelease {
    let lower = name.to_lowercase();
    let tokens: Vec<&str> = lower
        .split(['.', ' ', '-', '_', '[', ']', '(', ')'])
        .filter(|t| !t.is_empty())
        .collect();

    ParsedRelease {
        resolution: detect_resolution(&tokens),
        source: detect_source(&tokens),
        video_codec: detect_video_codec(&tokens),
        audio_codec: detect_audio_codec(&tokens),
        languages: LANGUAGE_HINTS
            .iter()
            .filter(|l| has(&tokens, l))
            .map(|l| l.to_string())
            .collect(),
        release_group: detect_release_group(name),
        hdr: tokens.iter().any(|t| t.starts_with("hdr")),
        dolby_vision: has(&tokens, "dv")
            || has(&tokens, "dovi")
            || has_seq(&tokens, &["dolby", "vision"]),
    }
}

fn has(tokens: &[&str], wanted: &str) -> bool {
    tokens.contains(&wanted)
}

/// Adjacent-token sequence match, so `WEB-DL` / `WEB.DL` both hit ["web","dl"].
fn has_seq(tokens: &[&str], seq: &[&str]) -> bool {
    tokens.windows(seq.len()).any(|w| w == seq)
}

fn detect_resolution(tokens: &[&str]) -> Option<Resolution> {
    if has(tokens, "2160p") || has(tokens, "4k") || has(tokens, "uhd") {
        Some(Resolution::R2160p)
    } else if has(tokens, "1080p") || has(tokens, "1080i") {
        Some(Resolution::R1080p)
    } else if has(tokens, "720p") {
        Some(Resolution::R720p)
    } else if has(tokens, "480p") || has(tokens, "576p") {
        Some(Resolution::R480p)
    } else {
        None
    }
}

fn detect_source(tokens: &[&str]) -> Option<Source> {
    if has(tokens, "remux") {
        Some(Source::Remux)
    } else if has(tokens, "bluray")
        || has_seq(tokens, &["blu", "ray"])
        || has(tokens, "bdrip")
        || has(tokens, "brrip")
    {
        Some(Source::BluRay)
    } else if has(tokens, "webrip") || has_seq(tokens, &["web", "rip"]) {
        Some(Source::WebRip)
    } else if has(tokens, "webdl") || has_seq(tokens, &["web", "dl"]) || has(tokens, "web") {
        Some(Source::WebDl)
    } else if has(tokens, "hdtv") || has(tokens, "pdtv") {
        Some(Source::Hdtv)
    } else if has(tokens, "dvdrip")
        || has(tokens, "dvdr")
        || has(tokens, "dvd")
        || has(tokens, "dvdscr")
    {
        Some(Source::DvdRip)
    } else if has(tokens, "cam") || has(tokens, "hdcam") || has(tokens, "camrip") {
        Some(Source::Cam)
    } else if has(tokens, "ts") || has(tokens, "hdts") || has(tokens, "telesync") {
        Some(Source::Telesync)
    } else {
        None
    }
}

fn detect_video_codec(tokens: &[&str]) -> Option<VideoCodec> {
    if has(tokens, "x265")
        || has(tokens, "h265")
        || has(tokens, "hevc")
        || has_seq(tokens, &["h", "265"])
    {
        Some(VideoCodec::Hevc)
    } else if has(tokens, "x264")
        || has(tokens, "h264")
        || has(tokens, "avc")
        || has_seq(tokens, &["h", "264"])
    {
        Some(VideoCodec::H264)
    } else if has(tokens, "av1") {
        Some(VideoCodec::Av1)
    } else if has(tokens, "xvid") || has(tokens, "divx") {
        Some(VideoCodec::Xvid)
    } else {
        None
    }
}

fn detect_audio_codec(tokens: &[&str]) -> Option<AudioCodec> {
    let has_pref = |p: &str| tokens.iter().any(|t| t.starts_with(p));
    if has(tokens, "truehd") || has_seq(tokens, &["true", "hd"]) {
        Some(AudioCodec::TrueHd)
    } else if has(tokens, "dtshd")
        || has_seq(tokens, &["dts", "hd"])
        || has_seq(tokens, &["dts", "x"])
    {
        Some(AudioCodec::DtsHd)
    } else if has(tokens, "dts") {
        Some(AudioCodec::Dts)
    } else if has(tokens, "eac3") || has_pref("ddp") || has_seq(tokens, &["e", "ac3"]) {
        Some(AudioCodec::Eac3)
    } else if has(tokens, "ac3")
        || has_pref("dd5")
        || has_pref("dd7")
        || has_pref("dd2")
        || has_seq(tokens, &["dd", "5", "1"])
    {
        Some(AudioCodec::Ac3)
    } else if has(tokens, "flac") {
        Some(AudioCodec::Flac)
    } else if has(tokens, "opus") {
        Some(AudioCodec::Opus)
    } else if has(tokens, "aac") || has(tokens, "aac2") {
        Some(AudioCodec::Aac)
    } else if has(tokens, "mp3") {
        Some(AudioCodec::Mp3)
    } else {
        None
    }
}

/// The release group conventionally follows the final `-` and contains no
/// separators: `...x264-GROUP`. `...WEB-DL` must not yield a group "DL".
fn detect_release_group(name: &str) -> Option<String> {
    let mut stem = name.trim();
    for ext in [".mkv", ".mp4", ".avi", ".nzb"] {
        if let Some(s) = stem.strip_suffix(ext) {
            stem = s;
        }
    }
    // Strip trailing tracker tags like "[rartv]".
    if stem.ends_with(']') {
        if let Some(open) = stem.rfind('[') {
            stem = stem[..open].trim_end();
        }
    }
    let idx = stem.rfind('-')?;
    let group = stem[idx + 1..].trim();
    if group.is_empty()
        || group.len() > 24
        || group.contains(['.', ' ', '/'])
        || NOT_A_GROUP.contains(&group.to_lowercase().as_str())
    {
        return None;
    }
    Some(group.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(name: &str) -> ParsedRelease {
        parse_release_name(name)
    }

    #[test]
    fn classic_bluray_movie() {
        let r = p("Movie.Name.2020.1080p.BluRay.x264-GROUP");
        assert_eq!(r.resolution, Some(Resolution::R1080p));
        assert_eq!(r.source, Some(Source::BluRay));
        assert_eq!(r.video_codec, Some(VideoCodec::H264));
        assert_eq!(r.release_group.as_deref(), Some("GROUP"));
    }

    #[test]
    fn webdl_episode_with_dd51_and_dotted_h264() {
        let r = p("Show.S01E02.720p.WEB-DL.DD5.1.H.264");
        assert_eq!(r.resolution, Some(Resolution::R720p));
        assert_eq!(r.source, Some(Source::WebDl));
        assert_eq!(r.video_codec, Some(VideoCodec::H264));
        assert_eq!(r.audio_codec, Some(AudioCodec::Ac3));
        assert_eq!(r.release_group, None, "WEB-DL must not become group 'DL'");
    }

    #[test]
    fn remux_beats_bluray_marker() {
        let r = p("Epic.Film.1999.2160p.BluRay.REMUX.HEVC.TrueHD.7.1.Atmos-FraMeSToR");
        assert_eq!(r.source, Some(Source::Remux));
        assert_eq!(r.resolution, Some(Resolution::R2160p));
        assert_eq!(r.video_codec, Some(VideoCodec::Hevc));
        assert_eq!(r.audio_codec, Some(AudioCodec::TrueHd));
        assert_eq!(r.release_group.as_deref(), Some("FraMeSToR"));
    }

    #[test]
    fn dolby_vision_and_hdr10() {
        let r = p("Blockbuster.2023.2160p.WEB-DL.DV.HDR10.HEVC.DDP5.1-FLUX");
        assert!(r.dolby_vision);
        assert!(r.hdr);
        assert_eq!(r.resolution, Some(Resolution::R2160p));
        assert_eq!(r.audio_codec, Some(AudioCodec::Eac3));
        assert_eq!(r.release_group.as_deref(), Some("FLUX"));
    }

    #[test]
    fn webrip_x265() {
        let r = p("Some.Documentary.2021.1080p.WEBRip.x265-RARBG");
        assert_eq!(r.source, Some(Source::WebRip));
        assert_eq!(r.video_codec, Some(VideoCodec::Hevc));
        assert_eq!(r.release_group.as_deref(), Some("RARBG"));
    }

    #[test]
    fn hdtv_episode() {
        let r = p("Late.Show.2024.01.15.720p.HDTV.x264-CROOKS");
        assert_eq!(r.source, Some(Source::Hdtv));
        assert_eq!(r.resolution, Some(Resolution::R720p));
        assert_eq!(r.release_group.as_deref(), Some("CROOKS"));
    }

    #[test]
    fn dvdrip_xvid() {
        let r = p("Old.Comedy.1995.DVDRip.XviD-CLASSIC");
        assert_eq!(r.source, Some(Source::DvdRip));
        assert_eq!(r.video_codec, Some(VideoCodec::Xvid));
        assert_eq!(r.resolution, None);
    }

    #[test]
    fn cam_release() {
        let r = p("New.Blockbuster.2026.HDCAM.x264-QRips");
        assert_eq!(r.source, Some(Source::Cam));
    }

    #[test]
    fn telesync_release() {
        let r = p("Another.Movie.2026.1080p.TS.x264-BadQuality");
        assert_eq!(r.source, Some(Source::Telesync));
        assert_eq!(r.resolution, Some(Resolution::R1080p));
    }

    #[test]
    fn dts_hd_ma() {
        let r = p("Space.Saga.2017.1080p.BluRay.DTS-HD.MA.5.1.x264-VETO");
        assert_eq!(r.audio_codec, Some(AudioCodec::DtsHd));
        // Group after "MA.5.1..." — final dash segment is VETO.
        assert_eq!(r.release_group.as_deref(), Some("VETO"));
    }

    #[test]
    fn plain_dts() {
        let r = p("War.Drama.2005.720p.BluRay.DTS.x264-ESiR");
        assert_eq!(r.audio_codec, Some(AudioCodec::Dts));
    }

    #[test]
    fn german_language_hint() {
        let r = p("Der.Film.2019.German.DL.1080p.BluRay.x264-EXQUiSiTE");
        assert_eq!(r.languages, vec!["german".to_string()]);
    }

    #[test]
    fn multi_language_webdl_av1() {
        let r = p("Anime.Series.S02E05.MULTi.1080p.WEB-DL.AV1.OPUS-Kawaii");
        assert!(r.languages.contains(&"multi".to_string()));
        assert_eq!(r.video_codec, Some(VideoCodec::Av1));
        assert_eq!(r.audio_codec, Some(AudioCodec::Opus));
    }

    #[test]
    fn space_separated_name() {
        let r = p("Movie Name 2020 2160p UHD BluRay x265 HDR10 AAC-Group");
        assert_eq!(r.resolution, Some(Resolution::R2160p));
        assert_eq!(r.source, Some(Source::BluRay));
        assert_eq!(r.video_codec, Some(VideoCodec::Hevc));
        assert_eq!(r.audio_codec, Some(AudioCodec::Aac));
        assert!(r.hdr);
    }

    #[test]
    fn rartv_tag_stripped_before_group() {
        let r = p("Show.S03E07.1080p.WEB-DL.DDP5.1.H.264-NTb[rartv]");
        assert_eq!(r.release_group.as_deref(), Some("NTb"));
        assert_eq!(r.audio_codec, Some(AudioCodec::Eac3));
    }

    #[test]
    fn flac_and_480p() {
        let r = p("Concert.Film.2010.480p.DVDRip.FLAC.x264-MELON");
        assert_eq!(r.resolution, Some(Resolution::R480p));
        assert_eq!(r.audio_codec, Some(AudioCodec::Flac));
    }

    #[test]
    fn eac3_spelled_out() {
        let r = p("Thriller.2022.1080p.WEB.EAC3.5.1.h264-GOSSIP");
        assert_eq!(r.audio_codec, Some(AudioCodec::Eac3));
        assert_eq!(r.source, Some(Source::WebDl));
        assert_eq!(r.video_codec, Some(VideoCodec::H264));
    }

    #[test]
    fn no_markers_at_all() {
        let r = p("Totally Ambiguous Name");
        assert_eq!(r, ParsedRelease::default());
    }

    #[test]
    fn file_extension_stripped_for_group() {
        let r = p("Movie.2020.1080p.BluRay.x264-GROUP.mkv");
        assert_eq!(r.release_group.as_deref(), Some("GROUP"));
    }
}
