//! Playback sessions: a virtual file resolved from a release, an ffmpeg HLS
//! remux writing into a per-session temp dir, byte-range access to the raw
//! media, and lifecycle management (idle reaping, seek restarts, teardown).

pub mod ffmpeg;
pub mod ffprobe;
pub mod fingerprint;
pub mod intro;
pub mod range;
pub mod session;
pub mod source;

pub use ffprobe::{Chapter, ProbeResult};
pub use session::{MediaInfo, NewSession, Session, SessionManager, SessionState};
pub use source::{open_media_source, MediaSource};
