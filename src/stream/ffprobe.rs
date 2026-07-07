//! Media probing via `ffprobe` against the internal loopback URL.

use std::process::Stdio;
use std::time::Duration;

use serde::Deserialize;

use crate::error::{AppError, AppResult};

const PROBE_TIMEOUT: Duration = Duration::from_secs(20);

/// The subset of ffprobe output the streaming layer cares about.
#[derive(Debug, Clone, Default)]
pub struct ProbeResult {
    pub duration_secs: Option<f64>,
    pub video_codec: Option<String>,
    /// Codec/channels of the FIRST audio stream; use
    /// [`select_audio_stream`] + `audio_streams` for language-aware picks.
    pub audio_codec: Option<String>,
    pub audio_channels: Option<i64>,
    /// All audio streams in file order, with their language tags. Dual-
    /// language releases conventionally put the dub first, so the caller
    /// picks by language instead of blindly taking stream 0.
    pub audio_streams: Vec<AudioStream>,
    /// Frames per second of the first video stream, when reported. Used to
    /// correct fps-mismatch drift when attaching external subtitles.
    pub fps: Option<f64>,
    /// HLS `VIDEO-RANGE` value derived from the color transfer: `PQ`
    /// (HDR10/DV), `HLG`, or `SDR`.
    pub video_range: String,
    /// Video profile as ffprobe names it (e.g. "Main 10", "High").
    pub video_profile: Option<String>,
    /// Codec level as ffprobe reports it (HEVC: 153 = 5.1; H.264: 41 = 4.1).
    pub video_level: Option<i64>,
    pub width: Option<i64>,
    pub height: Option<i64>,
    /// Container-level total bitrate in bits/s, when the format reports one.
    pub bit_rate_bps: Option<i64>,
    /// Text-based subtitle streams embedded in the container, in file order.
    /// Bitmap formats (PGS, VobSub) are excluded — they would need OCR.
    pub subtitle_streams: Vec<EmbeddedSubtitle>,
    /// Embedded chapter markers, in file order. Empty for the vast majority of
    /// releases (most have no chapters). Powers the client's "Skip Intro".
    pub chapters: Vec<Chapter>,
    /// End time (seconds) of the detected intro/opening chapter, when the
    /// release has one titled like an intro that starts within the first few
    /// minutes. `None` when there is no such chapter (the client then falls
    /// back to its own heuristic).
    pub intro_end_secs: Option<f64>,
}

/// One embedded chapter marker.
#[derive(Debug, Clone, PartialEq)]
pub struct Chapter {
    pub start_secs: f64,
    pub end_secs: f64,
    pub title: Option<String>,
}

/// A detected intro chapter must start no later than this into the file, so a
/// mid-episode chapter merely named "OP reprise" is not mistaken for the
/// opening.
const INTRO_MAX_START_SECS: f64 = 300.0;

/// One text-based subtitle stream embedded in the probed media.
#[derive(Debug, Clone, PartialEq)]
pub struct EmbeddedSubtitle {
    /// Global ffmpeg stream index (for `-map 0:<index>`).
    pub stream_index: i64,
    /// The stream's language tag, normalized to a primary ISO 639-1 code
    /// ("en", "de"); `None` when untagged.
    pub language: Option<String>,
    /// Forced-narrative track (translations of foreign dialogue only).
    pub forced: bool,
    /// SDH / hearing-impaired track.
    pub hearing_impaired: bool,
}

/// Codecs ffmpeg can convert to WebVTT as text. Bitmap subtitle formats
/// (`hdmv_pgs_subtitle`, `dvd_subtitle`) are deliberately absent.
const TEXT_SUBTITLE_CODECS: &[&str] =
    &["subrip", "srt", "ass", "ssa", "webvtt", "mov_text", "text"];

/// The best embedded subtitle stream for a requested language: prefers a
/// full track over forced/SDH variants, and only ever matches on an explicit
/// language tag (an untagged track could be anything).
pub fn select_embedded_subtitle<'a>(
    streams: &'a [EmbeddedSubtitle],
    language: &str,
) -> Option<&'a EmbeddedSubtitle> {
    let wanted = primary_language_code(language);
    let candidates: Vec<&EmbeddedSubtitle> = streams
        .iter()
        .filter(|s| s.language.as_deref() == Some(wanted.as_str()))
        .collect();
    candidates
        .iter()
        .find(|s| !s.forced && !s.hearing_impaired)
        .or_else(|| candidates.iter().find(|s| !s.forced))
        .copied()
}

/// One audio stream of the probed media.
#[derive(Debug, Clone, Default)]
pub struct AudioStream {
    pub codec: Option<String>,
    pub channels: Option<i64>,
    /// The stream's `language` tag as written in the container, usually an
    /// ISO 639-2 code ("eng", "ger", "jpn"); None when untagged.
    pub language: Option<String>,
}

/// Index (among audio streams) of the stream to feed the player: the first
/// stream whose language tag matches `preferred` (an ISO 639-1 code like
/// "en"/"de"), else the first stream. Untagged streams never match a
/// preference — a single untagged stream is simply index 0 via the fallback.
pub fn select_audio_stream(streams: &[AudioStream], preferred: Option<&str>) -> usize {
    let Some(preferred) = preferred else { return 0 };
    streams
        .iter()
        .position(|stream| {
            stream
                .language
                .as_deref()
                .is_some_and(|tag| primary_language_code(tag) == preferred)
        })
        .unwrap_or(0)
}

/// Normalize a container language tag to a lower-case ISO 639-1 code:
/// "GER"/"deu" → "de", "en-US" → "en". Unknown 3-letter codes pass through
/// lowercased (they simply won't match any 2-letter preference).
pub fn primary_language_code(tag: &str) -> String {
    let primary = tag
        .split(['-', '_'])
        .next()
        .unwrap_or(tag)
        .trim()
        .to_lowercase();
    match primary.as_str() {
        "eng" => "en",
        "deu" | "ger" => "de",
        "fra" | "fre" => "fr",
        "ita" => "it",
        "spa" => "es",
        "nld" | "dut" => "nl",
        "jpn" => "ja",
        "kor" => "ko",
        "hin" => "hi",
        "rus" => "ru",
        "por" => "pt",
        "swe" => "sv",
        "dan" => "da",
        "nor" | "nob" => "no",
        "fin" => "fi",
        "pol" => "pl",
        "ces" | "cze" => "cs",
        "zho" | "chi" => "zh",
        "ara" => "ar",
        "tur" => "tr",
        _ => return primary,
    }
    .to_string()
}

#[derive(Debug, Deserialize)]
struct ProbeDoc {
    format: Option<ProbeFormat>,
    #[serde(default)]
    streams: Vec<ProbeStream>,
    #[serde(default)]
    chapters: Vec<ProbeChapter>,
}

#[derive(Debug, Deserialize)]
struct ProbeChapter {
    /// Chapter start/end are seconds as strings (`start_time`, `end_time`).
    start_time: Option<String>,
    end_time: Option<String>,
    #[serde(default)]
    tags: ProbeChapterTags,
}

#[derive(Debug, Default, Deserialize)]
struct ProbeChapterTags {
    title: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ProbeFormat {
    duration: Option<String>,
    bit_rate: Option<String>,
}

#[derive(Debug, Deserialize)]
struct ProbeStream {
    index: Option<i64>,
    codec_type: Option<String>,
    codec_name: Option<String>,
    profile: Option<String>,
    level: Option<i64>,
    width: Option<i64>,
    height: Option<i64>,
    channels: Option<i64>,
    #[serde(default)]
    disposition: Option<ProbeDisposition>,
    /// Rational frame rate `num/den`, e.g. `24000/1001`. `avg_frame_rate` is
    /// preferred (real average); `r_frame_rate` is the fallback base rate.
    avg_frame_rate: Option<String>,
    r_frame_rate: Option<String>,
    color_transfer: Option<String>,
    #[serde(default)]
    tags: Option<ProbeTags>,
}

#[derive(Debug, Deserialize)]
struct ProbeTags {
    language: Option<String>,
}

/// ffprobe stream disposition flags (0/1 integers).
#[derive(Debug, Default, Deserialize)]
struct ProbeDisposition {
    #[serde(default)]
    forced: i64,
    #[serde(default)]
    hearing_impaired: i64,
}

/// Parse an ffprobe rational frame rate (`num/den`, e.g. `24000/1001`) to fps.
/// Returns `None` for missing, zero (`0/0`) or unparseable values.
fn parse_frame_rate(rate: Option<&str>) -> Option<f64> {
    let rate = rate?.trim();
    let (num, den) = rate.split_once('/')?;
    let num: f64 = num.trim().parse().ok()?;
    let den: f64 = den.trim().parse().ok()?;
    if den == 0.0 || num == 0.0 {
        return None;
    }
    let fps = num / den;
    fps.is_finite().then_some(fps)
}

/// Run `ffprobe -v error -print_format json -show_format -show_streams`
/// against `url` with a 20s timeout. On failure the error carries ffprobe's
/// own stderr so the reason is actionable. Fails when no video stream is
/// found (nothing we could remux).
pub async fn probe_url(ffprobe_path: &str, url: &str) -> AppResult<ProbeResult> {
    let child = tokio::process::Command::new(ffprobe_path)
        // `-v error` (not `quiet`) so ffprobe writes the real failure reason to
        // stderr — captured below — while the JSON still goes to stdout. This
        // turns the opaque "is the media readable?" into an actionable message
        // (e.g. "Invalid data found" vs "Server returned 5XX" for a missing
        // article vs "moov atom not found").
        .args([
            "-v",
            "error",
            "-print_format",
            "json",
            "-show_format",
            "-show_streams",
            // Chapter markers power the client's "Skip Intro"; harmless (empty)
            // for the many releases without embedded chapters.
            "-show_chapters",
        ])
        .arg(url)
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true)
        .spawn()
        .map_err(|e| {
            AppError::Internal(anyhow::anyhow!("spawning ffprobe ({ffprobe_path}): {e}"))
        })?;

    let output = tokio::time::timeout(PROBE_TIMEOUT, child.wait_with_output())
        .await
        .map_err(|_| {
            AppError::Upstream(format!(
                "ffprobe timed out after {}s",
                PROBE_TIMEOUT.as_secs()
            ))
        })?
        .map_err(|e| AppError::Internal(anyhow::anyhow!("waiting for ffprobe: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        let detail = stderr.trim();
        // Keep the tail (the last line is usually the actionable one) bounded.
        let detail: String = detail
            .chars()
            .rev()
            .take(400)
            .collect::<String>()
            .chars()
            .rev()
            .collect();
        return Err(AppError::Upstream(if detail.is_empty() {
            format!(
                "ffprobe exited with {} (is the media readable?)",
                output.status
            )
        } else {
            format!("ffprobe exited with {}: {detail}", output.status)
        }));
    }

    let doc: ProbeDoc = serde_json::from_slice(&output.stdout)
        .map_err(|e| AppError::Upstream(format!("unparseable ffprobe output: {e}")))?;
    parse_probe(doc)
}

fn parse_probe(doc: ProbeDoc) -> AppResult<ProbeResult> {
    let (duration_secs, bit_rate_bps) = doc
        .format
        .map(|f| {
            (
                f.duration.and_then(|d| d.parse::<f64>().ok()),
                f.bit_rate.and_then(|b| b.parse::<i64>().ok()),
            )
        })
        .unwrap_or((None, None));

    let mut has_video = false;
    let mut video_codec = None;
    let mut video_profile = None;
    let mut video_level = None;
    let mut width = None;
    let mut height = None;
    let mut audio_streams = Vec::new();
    let mut subtitle_streams = Vec::new();
    let mut fps = None;
    let mut color_transfer = None;
    for stream in &doc.streams {
        match stream.codec_type.as_deref() {
            Some("video") if !has_video => {
                has_video = true;
                video_codec = stream.codec_name.clone();
                video_profile = stream.profile.clone();
                video_level = stream.level;
                width = stream.width;
                height = stream.height;
                fps = parse_frame_rate(stream.avg_frame_rate.as_deref())
                    .or_else(|| parse_frame_rate(stream.r_frame_rate.as_deref()));
                color_transfer = stream.color_transfer.clone();
            }
            Some("audio") => {
                audio_streams.push(AudioStream {
                    codec: stream.codec_name.clone(),
                    channels: stream.channels,
                    language: stream.tags.as_ref().and_then(|t| t.language.clone()),
                });
            }
            Some("subtitle") => {
                let text = stream
                    .codec_name
                    .as_deref()
                    .is_some_and(|c| TEXT_SUBTITLE_CODECS.contains(&c));
                if let (true, Some(index)) = (text, stream.index) {
                    let disposition = stream.disposition.as_ref();
                    subtitle_streams.push(EmbeddedSubtitle {
                        stream_index: index,
                        language: stream
                            .tags
                            .as_ref()
                            .and_then(|t| t.language.as_deref())
                            .map(primary_language_code),
                        forced: disposition.is_some_and(|d| d.forced != 0),
                        hearing_impaired: disposition.is_some_and(|d| d.hearing_impaired != 0),
                    });
                }
            }
            _ => {}
        }
    }
    let audio_codec = audio_streams.first().and_then(|s| s.codec.clone());
    let audio_channels = audio_streams.first().and_then(|s| s.channels);

    if !has_video {
        return Err(AppError::Upstream(
            "media contains no video stream".to_string(),
        ));
    }

    let video_range = match color_transfer.as_deref() {
        Some("smpte2084") => "PQ",
        Some("arib-std-b67") => "HLG",
        _ => "SDR",
    }
    .to_string();

    let chapters: Vec<Chapter> = doc
        .chapters
        .into_iter()
        .map(|c| Chapter {
            start_secs: c.start_time.and_then(|s| s.parse().ok()).unwrap_or(0.0),
            end_secs: c.end_time.and_then(|s| s.parse().ok()).unwrap_or(0.0),
            title: c.tags.title,
        })
        .collect();
    let intro_end_secs = detect_intro_end(&chapters);

    Ok(ProbeResult {
        duration_secs,
        video_codec,
        audio_codec,
        audio_channels,
        audio_streams,
        fps,
        video_range,
        video_profile,
        video_level,
        width,
        height,
        bit_rate_bps,
        subtitle_streams,
        chapters,
        intro_end_secs,
    })
}

/// RFC 6381 codec string for the master playlist's `CODECS` attribute.
///
/// AVPlayer refuses a variant whose `VIDEO-RANGE` is PQ/HLG unless it can
/// also validate decoder support from `CODECS` — HDR releases fail with
/// "unsupported URL" (-1002) without it. `None` when the combination is not
/// confidently mappable; the playlist then omits the attribute (declaring a
/// wrong string is worse than declaring none).
pub fn rfc6381_video_codec(
    codec: Option<&str>,
    profile: Option<&str>,
    level: Option<i64>,
) -> Option<String> {
    match codec? {
        "hevc" => {
            let level = level?;
            // Main tier assumed — high-tier releases are vanishingly rare and
            // a too-low declared tier only makes AVPlayer probe, not reject.
            match profile? {
                "Main" => Some(format!("hvc1.1.6.L{level}.B0")),
                "Main 10" => Some(format!("hvc1.2.4.L{level}.B0")),
                _ => None,
            }
        }
        "h264" => {
            let level = level?;
            let prefix = match profile? {
                "Baseline" | "Constrained Baseline" => "4240",
                "Main" => "4D40",
                "High" => "6400",
                _ => return None,
            };
            Some(format!("avc1.{prefix}{level:02X}"))
        }
        _ => None,
    }
}

/// RFC 6381 string for the audio half of `CODECS`.
pub fn rfc6381_audio_codec(codec: Option<&str>) -> Option<String> {
    match codec? {
        "aac" => Some("mp4a.40.2".to_string()),
        "ac3" => Some("ac-3".to_string()),
        "eac3" => Some("ec-3".to_string()),
        _ => None,
    }
}

/// Find the end of the intro/opening among the chapters: the end time of the
/// first chapter whose title looks like an intro (`intro`, `opening`, `op`, or
/// `avant`, case-insensitively) and that starts within the first
/// [`INTRO_MAX_START_SECS`]. `None` when no chapter qualifies.
fn detect_intro_end(chapters: &[Chapter]) -> Option<f64> {
    chapters
        .iter()
        .find(|c| c.start_secs <= INTRO_MAX_START_SECS && title_is_intro(c.title.as_deref()))
        .map(|c| c.end_secs)
}

/// Whether a chapter title names an intro/opening. Matches whole words so a
/// title like "Recap" is not caught by the substring "op".
fn title_is_intro(title: Option<&str>) -> bool {
    let Some(title) = title else { return false };
    let lower = title.to_ascii_lowercase();
    lower
        .split(|c: char| !c.is_alphanumeric())
        .any(|word| matches!(word, "intro" | "opening" | "op" | "avant"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_typical_probe_output() {
        let doc: ProbeDoc = serde_json::from_str(
            r#"{
                "streams": [
                    {"codec_type": "video", "codec_name": "h264", "avg_frame_rate": "24000/1001"},
                    {"codec_type": "audio", "codec_name": "ac3", "channels": 6},
                    {"codec_type": "audio", "codec_name": "aac", "channels": 2}
                ],
                "format": {"duration": "5400.123"}
            }"#,
        )
        .expect("parse");
        let result = parse_probe(doc).expect("probe");
        assert_eq!(result.duration_secs, Some(5400.123));
        assert_eq!(result.video_codec.as_deref(), Some("h264"));
        // First audio stream wins.
        assert_eq!(result.audio_codec.as_deref(), Some("ac3"));
        assert_eq!(result.audio_channels, Some(6));
        // 24000/1001 ~= 23.976 fps.
        assert!((result.fps.unwrap() - 23.976).abs() < 0.001);
        // No color transfer reported → SDR.
        assert_eq!(result.video_range, "SDR");
    }

    #[test]
    fn audio_streams_carry_language_tags() {
        let doc: ProbeDoc = serde_json::from_str(
            r#"{
                "streams": [
                    {"codec_type": "video", "codec_name": "h264"},
                    {"codec_type": "audio", "codec_name": "ac3", "channels": 6, "tags": {"language": "ger"}},
                    {"codec_type": "audio", "codec_name": "eac3", "channels": 6, "tags": {"language": "eng"}},
                    {"codec_type": "audio", "codec_name": "aac", "channels": 2}
                ],
                "format": {}
            }"#,
        )
        .expect("parse");
        let result = parse_probe(doc).expect("probe");
        assert_eq!(result.audio_streams.len(), 3);
        assert_eq!(result.audio_streams[0].language.as_deref(), Some("ger"));
        assert_eq!(result.audio_streams[1].language.as_deref(), Some("eng"));
        assert_eq!(result.audio_streams[2].language, None);
        // Back-compat: the summary fields still describe the first stream.
        assert_eq!(result.audio_codec.as_deref(), Some("ac3"));
    }

    #[test]
    fn audio_selection_prefers_the_requested_language() {
        let stream = |codec: &str, language: Option<&str>| AudioStream {
            codec: Some(codec.into()),
            channels: Some(6),
            language: language.map(Into::into),
        };
        // German-first dual-language release: an English preference picks
        // the second stream.
        let dual = [stream("ac3", Some("ger")), stream("eac3", Some("eng"))];
        assert_eq!(select_audio_stream(&dual, Some("en")), 1);
        assert_eq!(select_audio_stream(&dual, Some("de")), 0);
        // Anime with original-language preference resolving to Japanese.
        let anime = [stream("aac", Some("eng")), stream("aac", Some("jpn"))];
        assert_eq!(select_audio_stream(&anime, Some("ja")), 1);
        // No match, no preference or no tags → first stream.
        assert_eq!(select_audio_stream(&dual, Some("fr")), 0);
        assert_eq!(select_audio_stream(&dual, None), 0);
        assert_eq!(select_audio_stream(&[stream("aac", None)], Some("en")), 0);
        assert_eq!(select_audio_stream(&[], Some("en")), 0);
    }

    #[test]
    fn language_tags_normalize_to_two_letter_codes() {
        for (tag, code) in [
            ("eng", "en"),
            ("ENG", "en"),
            ("ger", "de"),
            ("deu", "de"),
            ("jpn", "ja"),
            ("en-US", "en"),
            ("de_AT", "de"),
            ("en", "en"),
            ("und", "und"),
        ] {
            assert_eq!(primary_language_code(tag), code, "tag {tag}");
        }
    }

    #[test]
    fn frame_rate_parsing_handles_rationals_and_junk() {
        assert!((parse_frame_rate(Some("25/1")).unwrap() - 25.0).abs() < 1e-9);
        assert!((parse_frame_rate(Some("24000/1001")).unwrap() - 23.976).abs() < 0.001);
        // Zero numerator/denominator and junk yield None.
        assert_eq!(parse_frame_rate(Some("0/0")), None);
        assert_eq!(parse_frame_rate(Some("30/0")), None);
        assert_eq!(parse_frame_rate(Some("nonsense")), None);
        assert_eq!(parse_frame_rate(None), None);
    }

    #[test]
    fn falls_back_to_r_frame_rate_when_avg_missing() {
        let doc: ProbeDoc = serde_json::from_str(
            r#"{"streams": [{"codec_type": "video", "codec_name": "h264", "r_frame_rate": "25/1"}]}"#,
        )
        .expect("parse");
        let result = parse_probe(doc).expect("probe");
        assert!((result.fps.unwrap() - 25.0).abs() < 1e-9);
    }

    #[test]
    fn embedded_subtitle_selection_prefers_full_tracks() {
        let streams = vec![
            EmbeddedSubtitle {
                stream_index: 2,
                language: Some("en".into()),
                forced: true,
                hearing_impaired: false,
            },
            EmbeddedSubtitle {
                stream_index: 3,
                language: Some("en".into()),
                forced: false,
                hearing_impaired: true,
            },
            EmbeddedSubtitle {
                stream_index: 4,
                language: Some("en".into()),
                forced: false,
                hearing_impaired: false,
            },
            EmbeddedSubtitle {
                stream_index: 5,
                language: None,
                forced: false,
                hearing_impaired: false,
            },
        ];
        // Full track wins over forced and SDH variants.
        assert_eq!(
            select_embedded_subtitle(&streams, "en").map(|s| s.stream_index),
            Some(4)
        );
        // Without a full track, SDH beats forced.
        assert_eq!(
            select_embedded_subtitle(&streams[..2], "en").map(|s| s.stream_index),
            Some(3)
        );
        // Untagged streams never match; unknown languages find nothing.
        assert_eq!(select_embedded_subtitle(&streams, "de"), None);
    }

    #[test]
    fn probe_collects_text_subtitle_streams_only() {
        let doc: ProbeDoc = serde_json::from_str(
            r#"{
                "streams": [
                    {"index": 0, "codec_type": "video", "codec_name": "h264"},
                    {"index": 1, "codec_type": "audio", "codec_name": "aac"},
                    {"index": 2, "codec_type": "subtitle", "codec_name": "subrip",
                     "tags": {"language": "eng"},
                     "disposition": {"forced": 0, "hearing_impaired": 0}},
                    {"index": 3, "codec_type": "subtitle", "codec_name": "hdmv_pgs_subtitle",
                     "tags": {"language": "ger"}},
                    {"index": 4, "codec_type": "subtitle", "codec_name": "ass",
                     "tags": {"language": "ger"},
                     "disposition": {"forced": 1, "hearing_impaired": 0}}
                ],
                "format": {}
            }"#,
        )
        .expect("parse");
        let probe = parse_probe(doc).expect("probe");
        let langs: Vec<_> = probe
            .subtitle_streams
            .iter()
            .map(|s| (s.stream_index, s.language.clone(), s.forced))
            .collect();
        // PGS (bitmap) is excluded; language tags normalize to 639-1.
        assert_eq!(
            langs,
            vec![
                (2, Some("en".to_string()), false),
                (4, Some("de".to_string()), true)
            ]
        );
    }

    #[test]
    fn rfc6381_strings_cover_apple_playable_codecs() {
        // UHD BluRay remux: HEVC Main 10 @ L5.1.
        assert_eq!(
            rfc6381_video_codec(Some("hevc"), Some("Main 10"), Some(153)).as_deref(),
            Some("hvc1.2.4.L153.B0")
        );
        assert_eq!(
            rfc6381_video_codec(Some("hevc"), Some("Main"), Some(120)).as_deref(),
            Some("hvc1.1.6.L120.B0")
        );
        // 1080p web rip: H.264 High @ 4.1 (41 = 0x29).
        assert_eq!(
            rfc6381_video_codec(Some("h264"), Some("High"), Some(41)).as_deref(),
            Some("avc1.640029")
        );
        assert_eq!(
            rfc6381_video_codec(Some("h264"), Some("Main"), Some(40)).as_deref(),
            Some("avc1.4D4028")
        );
        // Unknown combinations stay unmapped rather than guessed.
        assert_eq!(
            rfc6381_video_codec(Some("hevc"), Some("Main 10"), None),
            None
        );
        assert_eq!(
            rfc6381_video_codec(Some("av1"), Some("Main"), Some(8)),
            None
        );
        assert_eq!(rfc6381_video_codec(None, Some("High"), Some(41)), None);

        assert_eq!(
            rfc6381_audio_codec(Some("aac")).as_deref(),
            Some("mp4a.40.2")
        );
        assert_eq!(rfc6381_audio_codec(Some("ac3")).as_deref(), Some("ac-3"));
        assert_eq!(rfc6381_audio_codec(Some("eac3")).as_deref(), Some("ec-3"));
        assert_eq!(rfc6381_audio_codec(Some("truehd")), None);
        assert_eq!(rfc6381_audio_codec(None), None);
    }

    #[test]
    fn probe_captures_codec_parameters_for_master_playlist() {
        let doc: ProbeDoc = serde_json::from_str(
            r#"{
                "streams": [{
                    "codec_type": "video", "codec_name": "hevc",
                    "profile": "Main 10", "level": 153,
                    "width": 3840, "height": 2160,
                    "color_transfer": "smpte2084"
                }],
                "format": {"duration": "6519.776", "bit_rate": "73744240"}
            }"#,
        )
        .expect("parse");
        let probe = parse_probe(doc).expect("probe");
        assert_eq!(probe.video_profile.as_deref(), Some("Main 10"));
        assert_eq!(probe.video_level, Some(153));
        assert_eq!(probe.width, Some(3840));
        assert_eq!(probe.height, Some(2160));
        assert_eq!(probe.bit_rate_bps, Some(73_744_240));
    }

    #[test]
    fn color_transfer_maps_to_video_range() {
        for (transfer, expected) in [
            ("smpte2084", "PQ"),
            ("arib-std-b67", "HLG"),
            ("bt709", "SDR"),
        ] {
            let doc: ProbeDoc = serde_json::from_str(&format!(
                r#"{{"streams": [{{"codec_type": "video", "codec_name": "hevc", "color_transfer": "{transfer}"}}], "format": {{}}}}"#,
            ))
            .expect("parse");
            assert_eq!(parse_probe(doc).expect("probe").video_range, expected);
        }
    }

    #[test]
    fn missing_video_stream_is_an_error() {
        let doc: ProbeDoc = serde_json::from_str(
            r#"{"streams": [{"codec_type": "audio", "codec_name": "mp3"}], "format": {}}"#,
        )
        .expect("parse");
        assert!(parse_probe(doc).is_err());
    }

    #[test]
    fn tolerates_missing_fields() {
        let doc: ProbeDoc =
            serde_json::from_str(r#"{"streams": [{"codec_type": "video"}]}"#).expect("parse");
        let result = parse_probe(doc).expect("probe");
        assert_eq!(result.duration_secs, None);
        assert_eq!(result.video_codec, None);
        assert_eq!(result.audio_codec, None);
        // No chapters element → empty, and no intro marker.
        assert!(result.chapters.is_empty());
        assert_eq!(result.intro_end_secs, None);
    }

    #[test]
    fn parses_chapters_and_detects_intro() {
        let doc: ProbeDoc = serde_json::from_str(
            r#"{
                "streams": [{"codec_type": "video", "codec_name": "hevc"}],
                "format": {"duration": "1440.0"},
                "chapters": [
                    {"start_time": "0.000", "end_time": "90.000", "tags": {"title": "Intro"}},
                    {"start_time": "90.000", "end_time": "1440.000", "tags": {"title": "Episode"}}
                ]
            }"#,
        )
        .expect("parse");
        let result = parse_probe(doc).expect("probe");
        assert_eq!(result.chapters.len(), 2);
        assert_eq!(result.chapters[0].start_secs, 0.0);
        assert_eq!(result.chapters[0].end_secs, 90.0);
        assert_eq!(result.chapters[0].title.as_deref(), Some("Intro"));
        // The intro chapter's end time is the intro end.
        assert_eq!(result.intro_end_secs, Some(90.0));
    }

    #[test]
    fn no_intro_chapter_leaves_intro_end_none() {
        let doc: ProbeDoc = serde_json::from_str(
            r#"{
                "streams": [{"codec_type": "video", "codec_name": "h264"}],
                "chapters": [
                    {"start_time": "0.0", "end_time": "600.0", "tags": {"title": "Chapter 1"}},
                    {"start_time": "600.0", "end_time": "1200.0", "tags": {"title": "Chapter 2"}}
                ]
            }"#,
        )
        .expect("parse");
        let result = parse_probe(doc).expect("probe");
        assert_eq!(result.chapters.len(), 2);
        assert_eq!(result.intro_end_secs, None);
    }

    #[test]
    fn intro_detection_matches_common_titles_and_respects_start_window() {
        // Whole-word intro synonyms match.
        for title in ["Intro", "OPENING", "op", "Avant"] {
            assert!(
                title_is_intro(Some(title)),
                "'{title}' should be an intro title"
            );
        }
        // "Recap" must not match on the "op"/"cap" substring.
        assert!(!title_is_intro(Some("Recap")));
        assert!(!title_is_intro(Some("Episode")));
        assert!(!title_is_intro(None));

        // An intro-titled chapter that starts too late is not the opening.
        let late = vec![Chapter {
            start_secs: INTRO_MAX_START_SECS + 60.0,
            end_secs: INTRO_MAX_START_SECS + 120.0,
            title: Some("Opening".into()),
        }];
        assert_eq!(detect_intro_end(&late), None);
    }
}
