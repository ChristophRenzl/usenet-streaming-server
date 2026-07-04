//! NZB XML parsing via quick-xml's serde support.

use serde::Deserialize;

use crate::error::{AppError, AppResult};

/// A parsed NZB document.
#[derive(Debug, Clone)]
pub struct Nzb {
    pub files: Vec<NzbFile>,
}

/// One `<file>` element: a Usenet post split into ordered segments.
#[derive(Debug, Clone)]
pub struct NzbFile {
    pub subject: String,
    pub poster: String,
    pub date: i64,
    pub groups: Vec<String>,
    /// Sorted by segment number, ascending.
    pub segments: Vec<Segment>,
}

impl NzbFile {
    /// Approximate file size: the sum of the (encoded) segment sizes.
    pub fn total_bytes(&self) -> u64 {
        self.segments.iter().map(|s| s.bytes).sum()
    }
}

/// One `<segment>`: a single article. `message_id` has no angle brackets.
#[derive(Debug, Clone)]
pub struct Segment {
    pub number: u32,
    pub bytes: u64,
    pub message_id: String,
}

#[derive(Debug, Deserialize)]
struct RawNzb {
    #[serde(rename = "file", default)]
    files: Vec<RawFile>,
}

#[derive(Debug, Deserialize)]
struct RawFile {
    #[serde(rename = "@subject", default)]
    subject: String,
    #[serde(rename = "@poster", default)]
    poster: String,
    #[serde(rename = "@date", default)]
    date: i64,
    #[serde(default)]
    groups: RawGroups,
    #[serde(default)]
    segments: RawSegments,
}

#[derive(Debug, Default, Deserialize)]
struct RawGroups {
    #[serde(rename = "group", default)]
    groups: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
struct RawSegments {
    #[serde(rename = "segment", default)]
    segments: Vec<RawSegment>,
}

#[derive(Debug, Deserialize)]
struct RawSegment {
    #[serde(rename = "@number", default)]
    number: u32,
    #[serde(rename = "@bytes", default)]
    bytes: u64,
    #[serde(rename = "$text", default)]
    message_id: String,
}

/// Parse NZB XML. File order is preserved; segments are sorted by number.
pub fn parse_nzb(xml: &str) -> AppResult<Nzb> {
    let raw: RawNzb = quick_xml::de::from_str(xml)
        .map_err(|e| AppError::BadRequest(format!("invalid NZB XML: {e}")))?;

    let files = raw
        .files
        .into_iter()
        .map(|f| {
            let mut segments: Vec<Segment> = f
                .segments
                .segments
                .into_iter()
                .map(|s| Segment {
                    number: s.number,
                    bytes: s.bytes,
                    message_id: s.message_id.trim().to_string(),
                })
                .collect();
            segments.sort_by_key(|s| s.number);
            NzbFile {
                subject: f.subject,
                poster: f.poster,
                date: f.date,
                groups: f.groups.groups,
                segments,
            }
        })
        .collect();

    Ok(Nzb { files })
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE nzb PUBLIC "-//newzBin//DTD NZB 1.1//EN" "http://www.newzbin.com/DTD/nzb/nzb-1.1.dtd">
<nzb xmlns="http://www.newzbin.com/DTD/2003/nzb">
  <head>
    <meta type="title">Some.Release.1080p</meta>
  </head>
  <file poster="poster@example.com" date="1719000000" subject="Some.Release [1/2] - &quot;archive.part1.rar&quot; yEnc (1/2)">
    <groups>
      <group>alt.binaries.example</group>
      <group>alt.binaries.other</group>
    </groups>
    <segments>
      <segment bytes="750000" number="2">seg2@news.example</segment>
      <segment bytes="750000" number="1">seg1@news.example</segment>
    </segments>
  </file>
</nzb>"#;

    #[test]
    fn parses_files_groups_and_sorted_segments() {
        let nzb = parse_nzb(SAMPLE).expect("parse");
        assert_eq!(nzb.files.len(), 1);
        let f = &nzb.files[0];
        assert_eq!(
            f.subject,
            "Some.Release [1/2] - \"archive.part1.rar\" yEnc (1/2)"
        );
        assert_eq!(f.poster, "poster@example.com");
        assert_eq!(f.date, 1719000000);
        assert_eq!(f.groups, vec!["alt.binaries.example", "alt.binaries.other"]);
        assert_eq!(f.segments.len(), 2);
        assert_eq!(f.segments[0].number, 1);
        assert_eq!(f.segments[0].message_id, "seg1@news.example");
        assert_eq!(f.segments[1].number, 2);
        assert_eq!(f.total_bytes(), 1_500_000);
    }

    #[test]
    fn rejects_invalid_xml() {
        assert!(parse_nzb("this is not xml <<<").is_err());
    }
}
