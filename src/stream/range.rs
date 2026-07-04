//! RFC 7233 single-range responses over a [`VirtualFile`], streamed in
//! chunks so large ranges never sit in memory whole.

use std::sync::Arc;

use axum::body::Body;
use axum::http::{header, HeaderValue, StatusCode};
use axum::response::Response;

use crate::error::AppError;
use crate::vfs::VirtualFile;

/// Read size per chunk while streaming a range body.
const CHUNK_SIZE: usize = 1024 * 1024;

/// Decision derived from a `Range` request header.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RangeDecision {
    /// No (usable) range requested: respond 200 with the whole body.
    Full,
    /// Satisfiable single range: respond 206 with `start..=end`.
    Partial { start: u64, end: u64 },
    /// Invalid or unsatisfiable: respond 416.
    Unsatisfiable,
}

/// Interpret an optional `Range` header against a resource of `len` bytes.
///
/// Only single `bytes=` ranges are supported. A non-`bytes` unit is ignored
/// (200 full body, as RFC 7233 permits); a malformed or unsatisfiable
/// `bytes=` spec yields 416.
pub fn parse_range(header: Option<&str>, len: u64) -> RangeDecision {
    let Some(header) = header else {
        return RangeDecision::Full;
    };
    let Some(spec) = header.trim().strip_prefix("bytes=") else {
        // Unknown unit: a server MAY ignore the Range header.
        return RangeDecision::Full;
    };
    let spec = spec.trim();
    if spec.is_empty() || spec.contains(',') {
        // Multi-range is out of scope; treat as invalid.
        return RangeDecision::Unsatisfiable;
    }
    let Some((first, last)) = spec.split_once('-') else {
        return RangeDecision::Unsatisfiable;
    };
    let (first, last) = (first.trim(), last.trim());

    match (first.is_empty(), last.is_empty()) {
        // "bytes=-" is meaningless.
        (true, true) => RangeDecision::Unsatisfiable,
        // Suffix range: last `n` bytes.
        (true, false) => match last.parse::<u64>() {
            Ok(n) if n > 0 && len > 0 => RangeDecision::Partial {
                start: len.saturating_sub(n),
                end: len - 1,
            },
            _ => RangeDecision::Unsatisfiable,
        },
        // Open-ended range: from `start` to EOF.
        (false, true) => match first.parse::<u64>() {
            Ok(start) if start < len => RangeDecision::Partial {
                start,
                end: len - 1,
            },
            _ => RangeDecision::Unsatisfiable,
        },
        // Closed range.
        (false, false) => match (first.parse::<u64>(), last.parse::<u64>()) {
            (Ok(start), Ok(end)) if start <= end && start < len => RangeDecision::Partial {
                start,
                end: end.min(len - 1),
            },
            _ => RangeDecision::Unsatisfiable,
        },
    }
}

/// Content type by file extension of the (inner) media file name.
pub fn content_type_for(file_name: &str) -> &'static str {
    match file_name
        .rsplit_once('.')
        .map(|(_, ext)| ext.to_ascii_lowercase())
        .as_deref()
    {
        Some("mkv") => "video/x-matroska",
        Some("mp4") | Some("m4v") => "video/mp4",
        _ => "application/octet-stream",
    }
}

/// Build the full 200/206/416 response for a (possibly absent) `Range`
/// header. `on_error` is invoked when a chunk read fails mid-stream, right
/// before the connection is aborted.
pub fn range_response<F>(
    file: Arc<dyn VirtualFile>,
    file_name: &str,
    range_header: Option<&str>,
    on_error: F,
) -> Response
where
    F: Fn(&AppError) + Send + Sync + 'static,
{
    let len = file.len();
    let content_type = content_type_for(file_name);

    let (status, start, end) = match parse_range(range_header, len) {
        RangeDecision::Full => {
            if len == 0 {
                return response_builder(StatusCode::OK, content_type)
                    .header(header::CONTENT_LENGTH, "0")
                    .body(Body::empty())
                    .expect("static response");
            }
            (StatusCode::OK, 0, len - 1)
        }
        RangeDecision::Partial { start, end } => (StatusCode::PARTIAL_CONTENT, start, end),
        RangeDecision::Unsatisfiable => {
            return response_builder(StatusCode::RANGE_NOT_SATISFIABLE, content_type)
                .header(
                    header::CONTENT_RANGE,
                    HeaderValue::from_str(&format!("bytes */{len}")).expect("header"),
                )
                .header(header::CONTENT_LENGTH, "0")
                .body(Body::empty())
                .expect("static response");
        }
    };

    let mut builder = response_builder(status, content_type).header(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&(end - start + 1).to_string()).expect("header"),
    );
    if status == StatusCode::PARTIAL_CONTENT {
        builder = builder.header(
            header::CONTENT_RANGE,
            HeaderValue::from_str(&format!("bytes {start}-{end}/{len}")).expect("header"),
        );
    }
    builder
        .body(stream_body(file, start, end, on_error))
        .expect("range response")
}

fn response_builder(
    status: StatusCode,
    content_type: &'static str,
) -> axum::http::response::Builder {
    Response::builder()
        .status(status)
        .header(header::CONTENT_TYPE, content_type)
        .header(header::ACCEPT_RANGES, "bytes")
}

/// Stream `start..=end` of the file in [`CHUNK_SIZE`] reads. A failed read
/// aborts the body (connection error to the client) after calling
/// `on_error`.
fn stream_body<F>(file: Arc<dyn VirtualFile>, start: u64, end: u64, on_error: F) -> Body
where
    F: Fn(&AppError) + Send + Sync + 'static,
{
    let on_error = Arc::new(on_error);
    let stream = futures::stream::try_unfold(start, move |pos| {
        let file = file.clone();
        let on_error = on_error.clone();
        async move {
            if pos > end {
                return Ok(None);
            }
            let want = usize::try_from(end - pos + 1)
                .unwrap_or(usize::MAX)
                .min(CHUNK_SIZE);
            match file.read_at(pos, want).await {
                Ok(bytes) if bytes.is_empty() => Err(std::io::Error::other(format!(
                    "unexpected EOF at byte {pos} while streaming"
                ))),
                Ok(bytes) => {
                    let next = pos + bytes.len() as u64;
                    Ok(Some((bytes, next)))
                }
                Err(e) => {
                    on_error(&e);
                    Err(std::io::Error::other(e.to_string()))
                }
            }
        }
    });
    Body::from_stream(stream)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_header_is_full() {
        assert_eq!(parse_range(None, 100), RangeDecision::Full);
    }

    #[test]
    fn non_bytes_unit_is_ignored() {
        assert_eq!(parse_range(Some("items=0-4"), 100), RangeDecision::Full);
    }

    #[test]
    fn closed_open_and_suffix_ranges() {
        assert_eq!(
            parse_range(Some("bytes=0-49"), 100),
            RangeDecision::Partial { start: 0, end: 49 }
        );
        assert_eq!(
            parse_range(Some("bytes=10-"), 100),
            RangeDecision::Partial { start: 10, end: 99 }
        );
        assert_eq!(
            parse_range(Some("bytes=-10"), 100),
            RangeDecision::Partial { start: 90, end: 99 }
        );
        // Suffix longer than the file: whole file.
        assert_eq!(
            parse_range(Some("bytes=-1000"), 100),
            RangeDecision::Partial { start: 0, end: 99 }
        );
        // End clamped to len-1.
        assert_eq!(
            parse_range(Some("bytes=90-1000"), 100),
            RangeDecision::Partial { start: 90, end: 99 }
        );
    }

    #[test]
    fn invalid_and_unsatisfiable_ranges() {
        for header in [
            "bytes=",
            "bytes=-",
            "bytes=abc",
            "bytes=5-2",
            "bytes=100-",
            "bytes=100-200",
            "bytes=-0",
            "bytes=0-1,5-6",
        ] {
            assert_eq!(
                parse_range(Some(header), 100),
                RangeDecision::Unsatisfiable,
                "header: {header}"
            );
        }
        // Everything but a full-body request is unsatisfiable on empty files.
        assert_eq!(
            parse_range(Some("bytes=0-"), 0),
            RangeDecision::Unsatisfiable
        );
        assert_eq!(
            parse_range(Some("bytes=-5"), 0),
            RangeDecision::Unsatisfiable
        );
        assert_eq!(parse_range(None, 0), RangeDecision::Full);
    }

    #[test]
    fn content_types() {
        assert_eq!(content_type_for("a.mkv"), "video/x-matroska");
        assert_eq!(content_type_for("a.MKV"), "video/x-matroska");
        assert_eq!(content_type_for("a.mp4"), "video/mp4");
        assert_eq!(content_type_for("a.avi"), "application/octet-stream");
        assert_eq!(content_type_for("noext"), "application/octet-stream");
    }
}
