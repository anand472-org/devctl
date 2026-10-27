//! Per-branch record status (spec section 7.1, `devctl status`).

use std::fmt::Write as _;

use devctl_store::{Record, Status};

/// Line coverage in percent rounded once to one decimal (half away from
/// zero), or `None` when no line is coverable. Both the shown value and the
/// delta come from this, so they always agree.
fn percent(r: &Record) -> Option<f64> {
    let t = r.coverage.total;
    (t.coverable > 0).then(|| {
        let x = f64::from(t.covered) * 100.0 / f64::from(t.coverable);
        (x * 10.0).round() / 10.0
    })
}

/// `87.3% (+1.2)`, `50.0% (new)` without a previous percentage, or `n/a`
/// when `record` has no coverable lines.
fn coverage(record: &Record, previous: Option<&Record>) -> String {
    let Some(cur) = percent(record) else {
        return "n/a".to_string();
    };
    match previous.and_then(percent) {
        // `+ 0.0` turns -0.0 into +0.0.
        Some(prev) => {
            let delta = cur - prev + 0.0;
            format!("{cur:.1}% ({delta:+.1})")
        }
        None => format!("{cur:.1}% (new)"),
    }
}

/// `42s ago`, `5m ago`, `3h ago` or `2d ago`, rounded down; a finish in the
/// future (clock skew) reads as `0s ago`.
fn age(finished_at_ms: i64, now_ms: i64) -> String {
    let secs = now_ms.saturating_sub(finished_at_ms).max(0) / 1000;
    match secs {
        s if s < 60 => format!("{s}s ago"),
        s if s < 3600 => format!("{}m ago", s / 60),
        s if s < 86_400 => format!("{}h ago", s / 3600),
        s => format!("{}d ago", s / 86_400),
    }
}

/// The first seven hex digits of a commit ID.
fn short(commit: &[u8]) -> String {
    let mut hex = String::with_capacity(8);
    for b in commit.iter().take(4) {
        let _ = write!(hex, "{b:02x}");
    }
    hex.truncate(7);
    hex
}

/// One summary line for `record`, e.g.
/// `feature/x  1a2b3c4  ok  87.3% (+1.2)  tests 9 passed, 0 failed, 0 errored, 1 skipped  5m ago`.
/// `previous` is the next-newest record on the branch. An `error` record adds
/// a second line, `    error: <first non-blank line of its stderr tail>`.
#[must_use]
pub fn format_summary(record: &Record, previous: Option<&Record>, now_ms: i64) -> String {
    let status = match &record.status {
        Status::Ok => "ok",
        Status::TestsFailed => "tests failed",
        Status::Error { .. } => "error",
    };
    let t = &record.tests;
    let mut line = format!(
        "{}  {}  {status}  {}  tests {} passed, {} failed, {} errored, {} skipped  {}",
        record.branch,
        short(&record.commit),
        coverage(record, previous),
        t.passed,
        t.failed,
        t.errored,
        t.skipped,
        age(record.finished_at_ms, now_ms),
    );
    if let Status::Error { stderr_tail } = &record.status {
        let first = stderr_tail
            .lines()
            .map(str::trim)
            .find(|l| !l.is_empty())
            .unwrap_or("(no output)");
        let _ = write!(line, "\n    error: {first}");
    }
    line
}

#[cfg(test)]
mod tests {
    use super::*;
    use devctl_store::{CoverageSummary, LineCounts, Status, TestSummary};

    const NOW: i64 = 1_800_000_000_000;

    fn rec(covered: u32, coverable: u32, status: Status, finished_at_ms: i64) -> Record {
        Record {
            commit: vec![0x1a, 0x2b, 0x3c, 0x4d, 0x5e],
            tree: vec![0xff; 20],
            branch: "feature/x".into(),
            started_at_ms: finished_at_ms - 1000,
            finished_at_ms,
            status,
            coverage: CoverageSummary {
                total: LineCounts { coverable, covered },
                files: std::collections::BTreeMap::new(),
            },
            tests: TestSummary {
                passed: 9,
                failed: 0,
                errored: 0,
                skipped: 1,
                duration_ms: 12,
            },
            devctl_version: "0.1.0".into(),
            just_version: "1.59.0".into(),
        }
    }

    #[test]
    fn ok_with_previous_shows_delta() {
        let cur = rec(873, 1000, Status::Ok, NOW - 5 * 60_000);
        let prev = rec(861, 1000, Status::Ok, NOW - 3_600_000);
        assert_eq!(
            format_summary(&cur, Some(&prev), NOW),
            "feature/x  1a2b3c4  ok  87.3% (+1.2)  tests 9 passed, 0 failed, 0 errored, 1 skipped  5m ago"
        );
    }

    #[test]
    fn negative_and_zero_deltas() {
        let cur = rec(850, 1000, Status::TestsFailed, NOW - 59_000);
        let prev = rec(861, 1000, Status::Ok, NOW);
        assert_eq!(
            format_summary(&cur, Some(&prev), NOW),
            "feature/x  1a2b3c4  tests failed  85.0% (-1.1)  tests 9 passed, 0 failed, 0 errored, 1 skipped  59s ago"
        );
        let same = rec(1, 3, Status::Ok, NOW - 2 * 3_600_000);
        let prev = rec(2, 6, Status::Ok, NOW);
        assert!(
            format_summary(&same, Some(&prev), NOW).contains("  33.3% (+0.0)  "),
            "{}",
            format_summary(&same, Some(&prev), NOW)
        );
    }

    #[test]
    fn value_and_delta_use_the_same_rounding() {
        // 87.25% shows as 87.3 (half away from zero), and the delta from
        // 86.0 is computed from the shown 87.3.
        let cur = rec(349, 400, Status::Ok, NOW);
        let prev = rec(344, 400, Status::Ok, NOW);
        assert!(
            format_summary(&cur, Some(&prev), NOW).contains("  87.3% (+1.3)  "),
            "{}",
            format_summary(&cur, Some(&prev), NOW)
        );
    }

    #[test]
    fn first_record_is_new() {
        let cur = rec(1, 2, Status::Ok, NOW - 3 * 86_400_000);
        assert_eq!(
            format_summary(&cur, None, NOW),
            "feature/x  1a2b3c4  ok  50.0% (new)  tests 9 passed, 0 failed, 0 errored, 1 skipped  3d ago"
        );
    }

    #[test]
    fn previous_without_coverage_counts_as_new() {
        let cur = rec(1, 2, Status::Ok, NOW);
        let prev = rec(0, 0, Status::error("boom"), NOW);
        assert!(format_summary(&cur, Some(&prev), NOW).contains("  50.0% (new)  "));
    }

    #[test]
    fn error_adds_second_line_and_no_coverage_is_na() {
        let cur = rec(
            0,
            0,
            Status::error("\n`just coverage` failed:\nline two\n"),
            NOW - 2 * 3_600_000 - 1,
        );
        let prev = rec(873, 1000, Status::Ok, NOW);
        assert_eq!(
            format_summary(&cur, Some(&prev), NOW),
            "feature/x  1a2b3c4  error  n/a  tests 9 passed, 0 failed, 0 errored, 1 skipped  2h ago\n    error: `just coverage` failed:"
        );
    }

    #[test]
    fn future_finish_reads_as_zero_seconds() {
        let cur = rec(1, 1, Status::Ok, NOW + 10_000);
        assert!(format_summary(&cur, None, NOW).ends_with("  0s ago"));
    }
}
