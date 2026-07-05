//! NZB documents: XML parsing, yEnc decoding, file classification/selection
//! and pre-flight segment health checks.

pub mod health;
pub mod parse;
pub mod select;
pub mod yenc;

pub use health::{
    assess_release, health_check, main_content_segments, par2_recovery_bytes, HealthReport,
    HealthVerdict, RepairAssessment,
};
pub use parse::{parse_nzb, Nzb, NzbFile, Segment};
pub use select::{classify, extract_filename, select_main, FileKind, MainContent, NzbFileRef};
pub use yenc::{decode as yenc_decode, DecodedPart, YencError};
