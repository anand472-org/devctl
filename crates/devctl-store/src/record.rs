use std::collections::BTreeMap;

use serde::{Deserialize, Serialize};

pub type RecordId = u64;

/// How many trailing bytes of stderr an `Error` status keeps.
const STDERR_TAIL_BYTES: usize = 4096;

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct Record {
    pub commit: Vec<u8>,
    pub tree: Vec<u8>,
    pub branch: String,
    pub started_at_ms: i64,
    pub finished_at_ms: i64,
    pub status: Status,
    pub coverage: CoverageSummary,
    pub tests: TestSummary,
    pub devctl_version: String,
    pub just_version: String,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug)]
pub enum Status {
    Ok,
    TestsFailed,
    Error { stderr_tail: String },
}

impl Status {
    /// An `Error` status keeping the last 4096 bytes of `message`, cut on a
    /// char boundary.
    #[must_use]
    pub fn error(message: &str) -> Status {
        let mut start = message.len().saturating_sub(STDERR_TAIL_BYTES);
        while !message.is_char_boundary(start) {
            start += 1;
        }
        Status::Error {
            stderr_tail: message[start..].to_string(),
        }
    }
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct LineCounts {
    pub coverable: u32,
    pub covered: u32,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug, Default)]
pub struct CoverageSummary {
    pub total: LineCounts,
    pub files: BTreeMap<String, LineCounts>,
}

#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct TestSummary {
    pub passed: u32,
    pub failed: u32,
    pub errored: u32,
    pub skipped: u32,
    pub duration_ms: u64,
}

#[derive(Serialize, Deserialize, Clone, PartialEq, Eq, Debug, Default)]
pub struct RawReports {
    pub lcov: String,
    pub junit: Vec<String>,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub struct RetentionPolicy {
    pub records_per_branch: usize,
    pub raw_reports_days: u32,
}

#[derive(Clone, Copy, PartialEq, Eq, Debug, Default)]
pub struct PruneStats {
    pub records_removed: usize,
    pub raw_removed: usize,
}
