//! Recording commits (spec sections 4.1 and 7.1): the per-branch job queue,
//! measuring worktrees, the background worker and the `record` command.

pub mod measure;
pub mod queue;
pub mod worker;

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow};
use devctl_store::{Status, Store};

use crate::ctx::Ctx;
use crate::git::Repo;
use crate::status::format_summary;

/// Milliseconds since the Unix epoch.
#[must_use]
pub fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| i64::try_from(d.as_millis()).unwrap_or(i64::MAX))
}

/// What `devctl record` prints and how it ended.
#[derive(Debug, Clone, PartialEq)]
pub struct RecordSummary {
    /// The `format_summary` line(s).
    pub text: String,
    pub status: Status,
}

/// `devctl record [--commit <rev>]`: queues `rev` (default `HEAD`) on the
/// current branch and runs the worker in the foreground, waiting for a
/// background worker that holds the branch lock.
///
/// # Errors
/// On a detached HEAD, an unresolvable `rev`, or when no record was written.
pub fn run_record(ctx: &Ctx, rev: Option<&str>) -> Result<RecordSummary> {
    let branch = ctx
        .repo
        .current_branch()?
        .ok_or_else(|| anyhow!("HEAD is detached; check out a branch, then run `devctl record`"))?;
    let rev = rev.unwrap_or("HEAD");
    let commit = ctx.repo.resolve(rev)?;
    queue::enqueue(ctx, &branch, commit)?;
    loop {
        let lock = if let Ok(Some(lock)) = ctx.platform.try_lock(&ctx.dirs.lock(&branch)) {
            lock
        } else {
            eprintln!("waiting for the background worker on {branch}...");
            worker::wait_lock(ctx, &branch)?
        };
        worker::drain(ctx, &branch, &lock)?;
        drop(lock);
        // Same re-check as `run_worker`: a job queued while we held the lock.
        if queue::peek(ctx, &branch)?.is_none() {
            break;
        }
    }

    let store = ctx.store();
    let cannot_read = || {
        format!(
            "cannot read the record store {}; delete it and run `devctl record` again",
            ctx.dirs.store().display()
        )
    };
    let (id, record) = store
        .record_for(commit.as_bytes())
        .with_context(cannot_read)?
        .ok_or_else(|| {
            anyhow!(
                "no record was written for {commit}: a newer commit queued on {branch} \
                 replaced it, or recording failed (see the logs in {}); run \
                 `devctl record --commit {commit}` again",
                ctx.dirs.logs().display()
            )
        })?;
    let history = store
        .records_on(&branch, ctx.config.retention.records_per_branch.max(2))
        .with_context(cannot_read)?;
    let previous = history
        .iter()
        .skip_while(|(i, _)| *i != id)
        .nth(1)
        .map(|(_, r)| r);
    Ok(RecordSummary {
        text: format_summary(&record, previous, now_ms()),
        status: record.status,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::queue::peek;
    use crate::record::worker::tests::fixture_repo;
    use devctl_platform::fake::FakePlatform;

    #[test]
    fn records_head_and_prints_summary() {
        let t = fixture_repo(false);
        t.checkout_new("feature/x");
        let head = t.commit("one");
        let ctx = Ctx::open(t.path(), Box::new(FakePlatform::new())).unwrap();
        let out = run_record(&ctx, None).unwrap();
        assert_eq!(out.status, Status::Ok);
        assert!(
            out.text.starts_with(&format!(
                "feature/x  {}  ok  75.0% (new)  tests 1 passed",
                &head[..7]
            )),
            "{}",
            out.text
        );
        assert_eq!(peek(&ctx, "feature/x").unwrap(), None);
        // The measuring worktree is per branch.
        assert!(ctx.dirs.measure("feature/x").join("justfile").exists());

        // A second record of an older commit compares against the first.
        let first = t.git(&["rev-parse", "HEAD~0"]);
        t.write("src/a.rs", "z\n");
        t.commit("two");
        let out = run_record(&ctx, Some(&first[..10])).unwrap();
        assert!(out.text.contains("75.0% (+0.0)"), "{}", out.text);
    }

    #[test]
    fn error_status_is_returned() {
        let t = crate::test_support::TestRepo::new();
        t.write("f", "x\n");
        t.commit("one");
        let ctx = Ctx::open(t.path(), Box::new(FakePlatform::new())).unwrap();
        let out = run_record(&ctx, None).unwrap();
        assert!(matches!(out.status, Status::Error { .. }));
        assert!(
            out.text.contains("\n    error: missing recipe `coverage`"),
            "{}",
            out.text
        );
    }

    #[test]
    fn record_on_detached_head_fails() {
        let t = fixture_repo(false);
        t.commit("one");
        t.git(&["checkout", "-q", "--detach"]);
        let ctx = Ctx::open(t.path(), Box::new(FakePlatform::new())).unwrap();
        let err = run_record(&ctx, None).unwrap_err();
        assert_eq!(
            format!("{err:#}"),
            "HEAD is detached; check out a branch, then run `devctl record`"
        );
    }

    #[test]
    fn unknown_rev_fails_with_next_step() {
        let t = fixture_repo(false);
        t.commit("one");
        let ctx = Ctx::open(t.path(), Box::new(FakePlatform::new())).unwrap();
        let err = run_record(&ctx, Some("no-such-rev")).unwrap_err();
        assert!(format!("{err:#}").contains("no-such-rev"), "{err:#}");
    }
}
