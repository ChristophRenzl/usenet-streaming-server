//! Shared test support: deterministic payload generator, yEnc encoder, NZB
//! XML builder, ffmpeg test-clip generation, the in-process mock NNTP
//! server and a real-socket app harness for streaming tests.

#![allow(dead_code)] // helpers are shared across many test modules

pub mod mock_nntp;

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::process::Stdio;

use usenet_streaming_server::{api, state::AppState};

pub use mock_nntp::MockNntp;

pub fn ffmpeg_available() -> bool {
    let works = |bin: &str| {
        std::process::Command::new(bin)
            .arg("-version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|status| status.success())
            .unwrap_or(false)
    };
    works("ffmpeg") && works("ffprobe")
}

/// Encode a deterministic test clip (testsrc2 + sine) into an MKV with 1s
/// GOPs (so HLS can cut ~6s segments). Returns None when encoding fails
/// (e.g. the audio encoder is unavailable).
pub fn generate_media(dir: &Path, duration: u32, fps: u32, audio_args: &[&str]) -> Option<PathBuf> {
    let out = dir.join(format!(
        "source-{}.mkv",
        audio_args.join("").replace(':', "-")
    ));
    let status = std::process::Command::new("ffmpeg")
        .args([
            "-y",
            "-f",
            "lavfi",
            "-i",
            &format!("testsrc2=duration={duration}:size=320x180:rate={fps}"),
            "-f",
            "lavfi",
            "-i",
            &format!("sine=frequency=440:duration={duration}"),
            "-map",
            "0:v",
            "-map",
            "1:a",
            "-c:v",
            "libx264",
            "-preset",
            "ultrafast",
            "-g",
            &fps.to_string(),
            "-pix_fmt",
            "yuv420p",
        ])
        .args(audio_args)
        .arg(&out)
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .ok()?;
    (status.success() && out.exists()).then_some(out)
}

/// Serve the full app on an ephemeral loopback port (with connect-info, as
/// `run()` does) and point the state's loopback base at it. Returns the base
/// URL, the final state (shared with the server) and the serve task handle.
pub async fn spawn_app(state: AppState) -> (String, AppState, tokio::task::JoinHandle<()>) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind app listener");
    let addr = listener.local_addr().expect("app addr");
    let base = format!("http://{addr}");
    let state = state.with_loopback_base(&base);
    let router = api::router(state.clone());
    let handle = tokio::spawn(async move {
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .await
        .expect("serve app");
    });
    (base, state, handle)
}

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
