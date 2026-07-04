//! yEnc decoder for raw (already dot-unstuffed) article bodies.
//!
//! Supports single-part (`=ybegin` ... `=yend`) and multi-part
//! (`=ybegin part=` + `=ypart begin= end=`) articles. Part offsets are
//! normalized to 0-based, and `pcrc32`/`crc32` trailers are verified with
//! crc32fast when present.

use bytes::{BufMut, Bytes, BytesMut};
use memchr::memchr;

#[derive(Debug, thiserror::Error)]
pub enum YencError {
    #[error("missing =ybegin header")]
    MissingBegin,

    #[error("missing =yend trailer")]
    MissingEnd,

    #[error("malformed yEnc article: {0}")]
    Malformed(String),

    #[error("yEnc CRC mismatch: header {expected:08x}, decoded {actual:08x}")]
    CrcMismatch { expected: u32, actual: u32 },

    #[error("yEnc size mismatch: header says {expected} bytes, decoded {actual}")]
    SizeMismatch { expected: u64, actual: u64 },
}

impl From<YencError> for crate::error::AppError {
    fn from(e: YencError) -> Self {
        crate::error::AppError::Upstream(format!("yEnc decode failed: {e}"))
    }
}

/// One decoded yEnc part.
#[derive(Debug, Clone)]
pub struct DecodedPart {
    pub data: Bytes,
    /// 0-based offset of this part within the whole file.
    pub part_begin: u64,
    /// Decoded size of this part.
    pub part_size: u64,
    /// Size of the whole file (`size=` from `=ybegin`).
    pub file_size: u64,
    /// `name=` from `=ybegin`.
    pub file_name: String,
}

/// Iterate over lines, yielding them without the trailing CR/LF.
struct Lines<'a> {
    rest: &'a [u8],
}

impl<'a> Iterator for Lines<'a> {
    type Item = &'a [u8];

    fn next(&mut self) -> Option<&'a [u8]> {
        if self.rest.is_empty() {
            return None;
        }
        let (mut line, rest) = match memchr(b'\n', self.rest) {
            Some(i) => (&self.rest[..i], &self.rest[i + 1..]),
            None => (self.rest, &self.rest[self.rest.len()..]),
        };
        self.rest = rest;
        if line.last() == Some(&b'\r') {
            line = &line[..line.len() - 1];
        }
        Some(line)
    }
}

fn lines(input: &[u8]) -> Lines<'_> {
    Lines { rest: input }
}

/// Parse `key=<number>` from a whitespace-tokenized header line.
fn header_u64(line: &str, key: &str) -> Option<u64> {
    line.split_whitespace().find_map(|token| {
        token
            .strip_prefix(key)
            .and_then(|v| v.strip_prefix('='))
            .and_then(|v| v.parse().ok())
    })
}

fn header_hex_u32(line: &str, key: &str) -> Option<u32> {
    line.split_whitespace().find_map(|token| {
        token
            .strip_prefix(key)
            .and_then(|v| v.strip_prefix('='))
            .and_then(|v| u32::from_str_radix(v, 16).ok())
    })
}

/// `name=` takes the rest of the line (names may contain spaces).
fn header_name(line: &str) -> Option<String> {
    line.split_once("name=")
        .map(|(_, name)| name.trim().to_string())
}

/// Decode a raw article body into a yEnc part. Robust against garbage lines
/// before `=ybegin` and after `=yend`.
pub fn decode(article_body: &[u8]) -> Result<DecodedPart, YencError> {
    let mut it = lines(article_body);

    // Locate =ybegin, skipping any leading noise.
    let begin_line = loop {
        match it.next() {
            Some(line) if line.starts_with(b"=ybegin ") => {
                break String::from_utf8_lossy(line).into_owned()
            }
            Some(_) => continue,
            None => return Err(YencError::MissingBegin),
        }
    };

    let file_size = header_u64(&begin_line, "size")
        .ok_or_else(|| YencError::Malformed("=ybegin lacks size=".into()))?;
    let file_name = header_name(&begin_line)
        .ok_or_else(|| YencError::Malformed("=ybegin lacks name=".into()))?;
    let is_multipart = header_u64(&begin_line, "part").is_some();

    // Multi-part articles carry an =ypart line with a 1-based inclusive range.
    let mut ypart: Option<(u64, u64)> = None;
    if is_multipart {
        let line = it
            .next()
            .ok_or_else(|| YencError::Malformed("missing =ypart after =ybegin part=".into()))?;
        if !line.starts_with(b"=ypart ") {
            return Err(YencError::Malformed("expected =ypart line".into()));
        }
        let line = String::from_utf8_lossy(line).into_owned();
        let begin = header_u64(&line, "begin")
            .ok_or_else(|| YencError::Malformed("=ypart lacks begin=".into()))?;
        let end = header_u64(&line, "end")
            .ok_or_else(|| YencError::Malformed("=ypart lacks end=".into()))?;
        if begin == 0 || end < begin {
            return Err(YencError::Malformed(format!(
                "invalid =ypart range {begin}..{end}"
            )));
        }
        ypart = Some((begin - 1, end - begin + 1)); // normalize to 0-based offset + length
    }

    // Decode data lines until =yend.
    let mut data = BytesMut::new();
    let mut escape = false;
    let end_line = loop {
        let Some(line) = it.next() else {
            return Err(YencError::MissingEnd);
        };
        if line.starts_with(b"=yend") {
            break String::from_utf8_lossy(line).into_owned();
        }
        for &b in line {
            if escape {
                data.put_u8(b.wrapping_sub(64).wrapping_sub(42));
                escape = false;
            } else if b == b'=' {
                escape = true;
            } else {
                data.put_u8(b.wrapping_sub(42));
            }
        }
        // A '=' as the last byte of a line escapes the first byte of the next
        // line (never produced by sane encoders, but cheap to tolerate).
    };
    let data = data.freeze();

    // Verify =yend metadata.
    if let Some(expected) = header_u64(&end_line, "size") {
        if expected != data.len() as u64 {
            return Err(YencError::SizeMismatch {
                expected,
                actual: data.len() as u64,
            });
        }
    }
    let actual_crc = crc32fast::hash(&data);
    if let Some(expected) = header_hex_u32(&end_line, "pcrc32") {
        if expected != actual_crc {
            return Err(YencError::CrcMismatch {
                expected,
                actual: actual_crc,
            });
        }
    } else if !is_multipart {
        if let Some(expected) = header_hex_u32(&end_line, "crc32") {
            if expected != actual_crc {
                return Err(YencError::CrcMismatch {
                    expected,
                    actual: actual_crc,
                });
            }
        }
    }

    let (part_begin, part_size) = match ypart {
        Some((begin, len)) => {
            if len != data.len() as u64 {
                return Err(YencError::SizeMismatch {
                    expected: len,
                    actual: data.len() as u64,
                });
            }
            (begin, len)
        }
        None => (0, data.len() as u64),
    };

    Ok(DecodedPart {
        data,
        part_begin,
        part_size,
        file_size,
        file_name,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Hand-built single-part article: "Hello" encodes to byte+42 each.
    /// H=0x48->0x72 'r', e=0x65->0x8F, l=0x6C->0x96, o=0x6F->0x99.
    #[test]
    fn decodes_simple_single_part() {
        let mut body = Vec::new();
        body.extend_from_slice(b"=ybegin line=128 size=5 name=hello.txt\r\n");
        body.extend_from_slice(&[0x72, 0x8F, 0x96, 0x96, 0x99]);
        body.extend_from_slice(b"\r\n");
        body.extend_from_slice(
            format!("=yend size=5 crc32={:08x}\r\n", crc32fast::hash(b"Hello")).as_bytes(),
        );

        let part = decode(&body).expect("decode");
        assert_eq!(&part.data[..], b"Hello");
        assert_eq!(part.part_begin, 0);
        assert_eq!(part.part_size, 5);
        assert_eq!(part.file_size, 5);
        assert_eq!(part.file_name, "hello.txt");
    }

    /// Escape sequences: encoded stream contains `=` followed by byte+64.
    /// Critical bytes: 0x00 -> enc 0x2A ... reference raw bytes that ENCODE to
    /// 0x00/0x0A/0x0D/0x3D and must therefore be escaped:
    ///   raw 0xD6 -> 0x00 -> "=" 0x40
    ///   raw 0xE0 -> 0x0A -> "=" 0x4A
    ///   raw 0xE3 -> 0x0D -> "=" 0x4D
    ///   raw 0x13 -> 0x3D -> "=" 0x7D
    #[test]
    fn decodes_escaped_critical_bytes() {
        let raw: &[u8] = &[0xD6, 0xE0, 0xE3, 0x13];
        let mut body = Vec::new();
        body.extend_from_slice(b"=ybegin line=128 size=4 name=crit.bin\r\n");
        body.extend_from_slice(&[b'=', 0x40, b'=', 0x4A, b'=', 0x4D, b'=', 0x7D]);
        body.extend_from_slice(b"\r\n");
        body.extend_from_slice(
            format!("=yend size=4 crc32={:08x}\r\n", crc32fast::hash(raw)).as_bytes(),
        );

        let part = decode(&body).expect("decode");
        assert_eq!(&part.data[..], raw);
    }

    /// A leading '.' in the encoded stream: raw 0x04 encodes to '.' (0x2E).
    /// The NNTP layer has already un-stuffed, so the decoder just sees '.'.
    #[test]
    fn decodes_line_starting_with_dot() {
        let raw: &[u8] = &[0x04, 0x04, 0x41];
        let mut body = Vec::new();
        body.extend_from_slice(b"=ybegin line=128 size=3 name=dots.bin\r\n");
        body.extend_from_slice(b"..k\r\n"); // 0x2E 0x2E 0x6B
        body.extend_from_slice(
            format!("=yend size=3 crc32={:08x}\r\n", crc32fast::hash(raw)).as_bytes(),
        );
        let part = decode(&body).expect("decode");
        assert_eq!(&part.data[..], raw);
    }

    #[test]
    fn multipart_offsets_are_zero_based() {
        let raw = b"WORLD";
        let encoded: Vec<u8> = raw.iter().map(|b| b.wrapping_add(42)).collect();
        let mut body = Vec::new();
        body.extend_from_slice(
            b"=ybegin part=2 total=3 line=128 size=1000 name=multi part.bin\r\n",
        );
        body.extend_from_slice(b"=ypart begin=501 end=505\r\n");
        body.extend_from_slice(&encoded);
        body.extend_from_slice(b"\r\n");
        body.extend_from_slice(
            format!(
                "=yend size=5 part=2 pcrc32={:08x}\r\n",
                crc32fast::hash(raw)
            )
            .as_bytes(),
        );
        body.extend_from_slice(b"trailing garbage the decoder must ignore\r\n");

        let part = decode(&body).expect("decode");
        assert_eq!(&part.data[..], raw);
        assert_eq!(part.part_begin, 500);
        assert_eq!(part.part_size, 5);
        assert_eq!(part.file_size, 1000);
        assert_eq!(part.file_name, "multi part.bin");
    }

    #[test]
    fn pcrc32_mismatch_is_an_error() {
        let raw = b"WORLD";
        let encoded: Vec<u8> = raw.iter().map(|b| b.wrapping_add(42)).collect();
        let mut body = Vec::new();
        body.extend_from_slice(b"=ybegin part=1 total=1 line=128 size=5 name=x.bin\r\n");
        body.extend_from_slice(b"=ypart begin=1 end=5\r\n");
        body.extend_from_slice(&encoded);
        body.extend_from_slice(b"\r\n=yend size=5 part=1 pcrc32=deadbeef\r\n");

        match decode(&body) {
            Err(YencError::CrcMismatch { expected, .. }) => assert_eq!(expected, 0xdeadbeef),
            other => panic!("expected CrcMismatch, got {other:?}"),
        }
    }

    #[test]
    fn size_mismatch_is_an_error() {
        let mut body = Vec::new();
        body.extend_from_slice(b"=ybegin line=128 size=9 name=x.bin\r\n");
        body.extend_from_slice(&[0x72, 0x8F]);
        body.extend_from_slice(b"\r\n=yend size=9\r\n");
        assert!(matches!(decode(&body), Err(YencError::SizeMismatch { .. })));
    }

    #[test]
    fn missing_headers_are_errors() {
        assert!(matches!(
            decode(b"random\r\n"),
            Err(YencError::MissingBegin)
        ));
        assert!(matches!(
            decode(b"=ybegin line=128 size=5 name=x\r\nabc\r\n"),
            Err(YencError::MissingEnd)
        ));
    }

    #[test]
    fn tolerates_leading_garbage_lines() {
        let raw = b"Hello";
        let encoded: Vec<u8> = raw.iter().map(|b| b.wrapping_add(42)).collect();
        let mut body = Vec::new();
        body.extend_from_slice(b"X-Header: injected by some gateway\r\n\r\n");
        body.extend_from_slice(b"=ybegin line=128 size=5 name=x.bin\r\n");
        body.extend_from_slice(&encoded);
        body.extend_from_slice(
            format!("\r\n=yend size=5 crc32={:08x}\r\n", crc32fast::hash(raw)).as_bytes(),
        );
        assert_eq!(&decode(&body).expect("decode").data[..], raw);
    }
}
