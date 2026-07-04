//! Shared test support: deterministic payload generator, yEnc encoder, NZB
//! XML builder and the in-process mock NNTP server.

#![allow(dead_code)] // helpers are shared across many test modules

pub mod mock_nntp;

pub use mock_nntp::MockNntp;

/// CRC32 of `payload_3mib()`; must match the value printed by
/// `scripts/gen-rar-fixtures.sh` (the fixtures pack the same payload).
pub const PAYLOAD_CRC32: u32 = 0x04ED7586;

/// Deterministic byte stream: xorshift64* with a fixed seed. Mirrors the
/// python generator in scripts/gen-rar-fixtures.sh, byte for byte.
pub fn xorshift_bytes(n: usize) -> Vec<u8> {
    let mut x: u64 = 0x9E3779B97F4A7C15;
    let mut out = vec![0u8; n];
    for byte in &mut out {
        x ^= x >> 12;
        x ^= x << 25;
        x ^= x >> 27;
        *byte = (x.wrapping_mul(0x2545F4914F6CDD1D) >> 56) as u8;
    }
    out
}

/// The exact payload packed into the committed RAR fixtures.
pub fn payload_3mib() -> Vec<u8> {
    xorshift_bytes(3 * 1024 * 1024)
}

/// Simple deterministic pseudo-random u64 stream for fuzzing offsets.
pub struct TestRng(u64);

impl TestRng {
    pub fn new(seed: u64) -> Self {
        Self(seed | 1)
    }

    pub fn next_u64(&mut self) -> u64 {
        self.0 ^= self.0 >> 12;
        self.0 ^= self.0 << 25;
        self.0 ^= self.0 >> 27;
        self.0.wrapping_mul(0x2545F4914F6CDD1D)
    }
}

/// yEnc-encode one part of a file. `begin0` is the 0-based offset of `data`
/// within the whole file. Produces a single-part article when
/// `total_parts == 1`, otherwise `=ybegin part=`/`=ypart` framing.
pub fn yenc_encode_part(
    data: &[u8],
    name: &str,
    part_num: usize,
    total_parts: usize,
    begin0: u64,
    file_size: u64,
) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + data.len() / 50 + 256);
    if total_parts > 1 {
        out.extend_from_slice(
            format!(
                "=ybegin part={part_num} total={total_parts} line=128 size={file_size} name={name}\r\n"
            )
            .as_bytes(),
        );
        out.extend_from_slice(
            format!(
                "=ypart begin={} end={}\r\n",
                begin0 + 1,
                begin0 + data.len() as u64
            )
            .as_bytes(),
        );
    } else {
        out.extend_from_slice(
            format!("=ybegin line=128 size={file_size} name={name}\r\n").as_bytes(),
        );
    }

    let mut col = 0usize;
    for &b in data {
        let enc = b.wrapping_add(42);
        match enc {
            0x00 | 0x0A | 0x0D | b'=' => {
                out.push(b'=');
                out.push(enc.wrapping_add(64));
                col += 2;
            }
            _ => {
                // NOTE: '.' is deliberately not escaped so the NNTP layer's
                // dot-stuffing round-trip is exercised.
                out.push(enc);
                col += 1;
            }
        }
        if col >= 128 {
            out.extend_from_slice(b"\r\n");
            col = 0;
        }
    }
    if col > 0 {
        out.extend_from_slice(b"\r\n");
    }

    let crc = crc32fast::hash(data);
    if total_parts > 1 {
        out.extend_from_slice(
            format!(
                "=yend size={} part={part_num} pcrc32={crc:08x}\r\n",
                data.len()
            )
            .as_bytes(),
        );
    } else {
        out.extend_from_slice(format!("=yend size={} crc32={crc:08x}\r\n", data.len()).as_bytes());
    }
    out
}

/// Split `payload` into yEnc parts of `part_size`, register each as an
/// article on the mock server (message-ids `{prefix}-{n}@mock`), and return
/// `(message_id, encoded_bytes)` per segment for NZB building.
pub fn add_yenc_file(
    server: &MockNntp,
    prefix: &str,
    payload: &[u8],
    part_size: usize,
    name: &str,
) -> Vec<(String, u64)> {
    let total = payload.len().div_ceil(part_size).max(1);
    let mut segments = Vec::with_capacity(total);
    for (i, chunk) in payload.chunks(part_size).enumerate() {
        let article = yenc_encode_part(
            chunk,
            name,
            i + 1,
            total,
            (i * part_size) as u64,
            payload.len() as u64,
        );
        let message_id = format!("{prefix}-{}@mock", i + 1);
        let bytes = article.len() as u64;
        server.add_article(&message_id, article);
        segments.push((message_id, bytes));
    }
    segments
}

fn xml_escape(s: &str) -> String {
    s.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
        .replace('"', "&quot;")
}

/// Build an NZB document from `(subject, segments)` pairs.
pub fn build_nzb_xml(files: &[(String, Vec<(String, u64)>)]) -> String {
    let mut xml = String::from(
        "<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n\
         <nzb xmlns=\"http://www.newzbin.com/DTD/2003/nzb\">\n",
    );
    for (subject, segments) in files {
        xml.push_str(&format!(
            "  <file poster=\"tester@example.com\" date=\"1719000000\" subject=\"{}\">\n",
            xml_escape(subject)
        ));
        xml.push_str("    <groups><group>alt.binaries.test</group></groups>\n    <segments>\n");
        for (i, (id, bytes)) in segments.iter().enumerate() {
            xml.push_str(&format!(
                "      <segment bytes=\"{bytes}\" number=\"{}\">{}</segment>\n",
                i + 1,
                xml_escape(id)
            ));
        }
        xml.push_str("    </segments>\n  </file>\n");
    }
    xml.push_str("</nzb>\n");
    xml
}

#[test]
fn payload_generator_matches_fixture_script() {
    // Guards against drift between the python generator in
    // scripts/gen-rar-fixtures.sh and this Rust port.
    assert_eq!(crc32fast::hash(&payload_3mib()), PAYLOAD_CRC32);
}
