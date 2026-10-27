//! Per-branch record status (spec section 7.1, `devctl status`).

use std::fmt::Write as _;
use std::path::Path;

use anyhow::{Context, Result};
use devctl_store::{Record, Status, Store};

use crate::ctx::Ctx;
use crate::git::{Repo, short_hex};
use crate::recipes::error_detail;
use crate::record::{now_ms, previous_record, worker};

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
/// when `record` is an error or has no coverable lines.
fn coverage(record: &Record, previous: Option<&Record>) -> String {
    let cur = match record.status {
        Status::Error { .. } => None,
        Status::Ok | Status::TestsFailed => percent(record),
    };
    let Some(cur) = cur else {
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

/// One summary line for `record`, e.g.
/// `feature/x  1a2b3c4  ok  87.3% (+1.2)  tests 9 passed, 0 failed, 0 errored, 1 skipped  5m ago`.
/// `previous` is the next-newest record on the branch. An `error` record adds
/// a second line, `    error: <detail>`, the detail from [`error_detail`].
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
        short_hex(&record.commit),
        coverage(record, previous),
        t.passed,
        t.failed,
        t.errored,
        t.skipped,
        age(record.finished_at_ms, now_ms),
    );
    if let Status::Error { stderr_tail } = &record.status {
        let _ = write!(line, "\n    error: {}", error_detail(stderr_tail));
    }
    line
}

/// `1.5 KB`-style size: 1024-based, one decimal; plain bytes below 1 KB.
fn human_size(bytes: u64) -> String {
    const UNITS: [&str; 4] = ["B", "KB", "MB", "GB"];
    #[allow(clippy::cast_precision_loss)]
    let mut v = bytes as f64;
    let mut unit = 0;
    while v >= 1024.0 && unit < UNITS.len() - 1 {
        v /= 1024.0;
        unit += 1;
    }
    if unit == 0 {
        format!("{bytes} B")
    } else {
        format!("{v:.1} {}", UNITS[unit])
    }
}

/// Total size of the files under `dir`, not following symlinks and ignoring
/// unreadable entries; `None` when `dir` does not exist.
fn dir_size(dir: &Path) -> Option<u64> {
    fn walk(dir: &Path) -> u64 {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return 0;
        };
        entries
            .filter_map(Result::ok)
            .map(|e| match e.file_type() {
                Ok(t) if t.is_dir() => walk(&e.path()),
                Ok(t) if t.is_file() => e.metadata().map_or(0, |m| m.len()),
                _ => 0,
            })
            .sum()
    }
    dir.is_dir().then(|| walk(dir))
}

/// `devctl status [--branch <b>]`: the newest record of each branch (the
/// current branch first, then the rest sorted), with whether a worker is
/// running and the size of the measuring worktree. Never creates the store.
///
/// # Errors
/// If the store, `HEAD` or a worker lock cannot be read.
pub fn run_status(ctx: &Ctx, branch: Option<&str>) -> Result<String> {
    const NONE: &str = "no records yet; run `devctl record`";
    let none_for = |b: &str| format!("no records for {b}; run `devctl record` on it");
    if !ctx.dirs.store().exists() {
        return Ok(branch.map_or_else(|| NONE.to_string(), none_for));
    }
    let store = ctx.store();
    let read_err = || ctx.store_unreadable();
    let branches = if let Some(b) = branch {
        vec![b.to_string()]
    } else {
        let mut all = store.branches().with_context(read_err)?;
        all.sort();
        if let Some(cur) = ctx.repo.current_branch()?
            && let Some(i) = all.iter().position(|b| *b == cur)
        {
            let cur = all.remove(i);
            all.insert(0, cur);
        }
        all
    };
    let limit = ctx.config.retention.records_per_branch.max(2);
    let now = now_ms();
    let mut lines = Vec::new();
    for b in &branches {
        let history = store.records_on(b, limit).with_context(read_err)?;
        let Some((id, record)) = history.first() else {
            if branch.is_some() {
                return Ok(none_for(b));
            }
            continue;
        };
        let mut text = format_summary(record, previous_record(&history, *id, record), now);
        // Markers go on the first line; an error record's second line stays last.
        let mut extra = String::new();
        if worker::worker_running(ctx, b)? {
            extra.push_str("  [worker running]");
        }
        if let Some(size) = dir_size(&ctx.dirs.measure(b)) {
            let _ = write!(extra, "  measure {}", human_size(size));
        }
        let at = text.find('\n').unwrap_or(text.len());
        text.insert_str(at, &extra);
        lines.push(text);
    }
    Ok(if lines.is_empty() {
        NONE.to_string()
    } else {
        lines.join("\n")
    })
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
            "feature/x  1a2b3c4  error  n/a  tests 9 passed, 0 failed, 0 errored, 1 skipped  2h ago\n    error: line two"
        );
    }

    #[test]
    fn error_line_names_the_recipe_error_and_coverage_is_na() {
        // The recipe wrote LCOV, then failed without JUnit.
        let cur = rec(
            5,
            10,
            Status::error(
                "`just coverage` failed:\ncompile error: x\nerror: recipe `coverage` failed on line 4 with exit code 2\n\
                 `just coverage` exited non-zero and wrote no JUnit XML; fix the error above, then run `devctl record`",
            ),
            NOW,
        );
        assert_eq!(
            format_summary(&cur, None, NOW),
            "feature/x  1a2b3c4  error  n/a  tests 9 passed, 0 failed, 0 errored, 1 skipped  0s ago\n    \
             error: compile error: x \u{2014} `just coverage` exited non-zero and wrote no JUnit XML; \
             fix the error above, then run `devctl record`"
        );
    }

    mod run {
        use super::super::*;
        use crate::record::run_record;
        use crate::record::worker::tests::fixture_repo;
        use devctl_platform::fake::FakePlatform;

        fn ctx_of(t: &crate::test_support::TestRepo) -> Ctx {
            Ctx::open(t.path(), Box::new(FakePlatform::new())).unwrap()
        }

        #[test]
        fn human_sizes() {
            assert_eq!(human_size(0), "0 B");
            assert_eq!(human_size(1023), "1023 B");
            assert_eq!(human_size(1536), "1.5 KB");
            assert_eq!(human_size(5 * 1024 * 1024), "5.0 MB");
            assert_eq!(human_size(3 * 1024 * 1024 * 1024), "3.0 GB");
            assert_eq!(human_size(2048 * 1024 * 1024 * 1024), "2048.0 GB");
        }

        #[test]
        fn no_store_prints_hint_and_creates_nothing() {
            let t = fixture_repo(false);
            t.commit("one");
            let ctx = ctx_of(&t);
            assert_eq!(
                run_status(&ctx, None).unwrap(),
                "no records yet; run `devctl record`"
            );
            assert_eq!(
                run_status(&ctx, Some("dev")).unwrap(),
                "no records for dev; run `devctl record` on it"
            );
            assert!(!ctx.dirs.store().exists());
        }

        #[test]
        fn lists_current_branch_first_then_sorted() {
            let t = fixture_repo(false);
            t.commit("one");
            let ctx = ctx_of(&t);
            run_record(&ctx, None).unwrap();
            t.checkout_new("a/other");
            run_record(&ctx, None).unwrap();
            t.git(&["checkout", "-q", "dev"]);
            t.checkout_new("z/last");
            run_record(&ctx, None).unwrap();
            t.git(&["checkout", "-q", "dev"]);
            let out = run_status(&ctx, None).unwrap();
            let lines: Vec<&str> = out.lines().collect();
            assert_eq!(lines.len(), 3, "{out}");
            assert!(lines[0].starts_with("dev  "), "{out}");
            assert!(lines[1].starts_with("a/other  "), "{out}");
            assert!(lines[2].starts_with("z/last  "), "{out}");
            assert!(lines[0].contains("  ok  75.0% (new)  tests 1 passed"));
            assert!(lines[0].contains("s ago  measure "), "{out}");
            assert!(!out.contains("[worker running]"));

            let one = run_status(&ctx, Some("z/last")).unwrap();
            assert!(one.starts_with("z/last  ") && !one.contains('\n'), "{one}");
            assert_eq!(
                run_status(&ctx, Some("nope")).unwrap(),
                "no records for nope; run `devctl record` on it"
            );
        }

        #[test]
        fn shows_worker_running_while_the_lock_is_held() {
            let t = fixture_repo(false);
            t.commit("one");
            let ctx = ctx_of(&t);
            run_record(&ctx, None).unwrap();
            let held = worker::try_lock(&ctx, "dev").unwrap().unwrap();
            let out = run_status(&ctx, None).unwrap();
            assert!(out.contains("s ago  [worker running]  measure "), "{out}");
            drop(held);
            assert!(!run_status(&ctx, None).unwrap().contains("[worker running]"));
        }

        #[test]
        fn missing_measure_dir_is_omitted_and_error_markers_stay_on_line_one() {
            let t = crate::test_support::TestRepo::new();
            t.write("f", "x\n");
            t.commit("one");
            let ctx = ctx_of(&t);
            run_record(&ctx, None).unwrap();
            let out = run_status(&ctx, None).unwrap();
            let (first, second) = out.split_once('\n').unwrap();
            assert!(first.contains("  error  n/a  "), "{out}");
            assert!(second.starts_with("    error: "), "{out}");
            std::fs::remove_dir_all(ctx.dirs.measure("dev")).ok();
            let out = run_status(&ctx, None).unwrap();
            assert!(!out.contains("measure"), "{out}");
        }

        #[test]
        fn compares_with_the_previous_other_commit() {
            let t = fixture_repo(false);
            t.commit("one");
            let ctx = ctx_of(&t);
            run_record(&ctx, None).unwrap();
            t.write(
                "fixture/cov.info",
                "SF:src/a.rs\nDA:1,1\nDA:2,0\nend_of_record\n",
            );
            t.commit("two");
            run_record(&ctx, None).unwrap();
            run_record(&ctx, None).unwrap();
            let out = run_status(&ctx, None).unwrap();
            assert!(out.contains("50.0% (-25.0)"), "{out}");
        }

        #[test]
        fn delta_skips_error_records() {
            let t = fixture_repo(false);
            t.commit("one");
            let ctx = ctx_of(&t);
            run_record(&ctx, None).unwrap();
            // An error record (with coverage) in between is not "previous".
            t.write("fixture/junit.xml", "not xml");
            t.write("fixture/cov.info", "SF:src/a.rs\nDA:1,1\nend_of_record\n");
            t.commit("two");
            assert!(matches!(
                run_record(&ctx, None).unwrap().status,
                Status::Error { .. }
            ));
            t.write(
                "fixture/junit.xml",
                r#"<testsuites><testsuite name="s"><testcase name="a"/></testsuite></testsuites>"#,
            );
            t.write(
                "fixture/cov.info",
                "SF:src/a.rs\nDA:1,1\nDA:2,0\nend_of_record\n",
            );
            t.commit("three");
            let out = run_record(&ctx, None).unwrap();
            assert!(out.text.contains("50.0% (-25.0)"), "{}", out.text);
            let out = run_status(&ctx, None).unwrap();
            assert!(out.contains("50.0% (-25.0)"), "{out}");
        }

        #[test]
        fn dir_size_sums_files_and_ignores_missing() {
            let d = tempfile::tempdir().unwrap();
            std::fs::create_dir(d.path().join("s")).unwrap();
            std::fs::write(d.path().join("a"), [0u8; 10]).unwrap();
            std::fs::write(d.path().join("s").join("b"), [0u8; 5]).unwrap();
            assert_eq!(dir_size(d.path()), Some(15));
            assert_eq!(dir_size(&d.path().join("missing")), None);
        }
    }

    #[test]
    fn future_finish_reads_as_zero_seconds() {
        let cur = rec(1, 1, Status::Ok, NOW + 10_000);
        assert!(format_summary(&cur, None, NOW).ends_with("  0s ago"));
    }
}
