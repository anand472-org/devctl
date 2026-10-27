//! Recording commits (spec sections 4.1 and 7.1): the per-branch job queue,
//! measuring worktrees, the background worker and the `record` command.

pub mod measure;
pub mod queue;
pub mod worker;

use std::time::{SystemTime, UNIX_EPOCH};

use anyhow::{Context, Result, anyhow, bail};
use devctl_store::{Status, Store};

use crate::ctx::Ctx;
use crate::git::Repo;
use crate::status::format_summary;
use worker::{Outcome, Reuse};

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

/// `devctl record [--commit <rev>]`: measures `rev` (default `HEAD`) in the
/// current branch's measuring worktree in the foreground, waiting for a
/// background worker that holds the branch lock. It never reuses a record of
/// the same tree, so a flaky result can be re-run. A job queued meanwhile is
/// left to a freshly started background worker.
///
/// # Errors
/// On a detached HEAD, an unresolvable `rev`, a lock or store failure.
pub fn run_record(ctx: &Ctx, rev: Option<&str>) -> Result<RecordSummary> {
    let branch = ctx
        .repo
        .current_branch()?
        .ok_or_else(|| anyhow!("HEAD is detached; check out a branch, then run `devctl record`"))?;
    let rev = rev.unwrap_or("HEAD");
    let commit = ctx.repo.resolve(rev)?;
    // The measuring worktree belongs to whoever holds the branch lock.
    let lock = if let Some(lock) = worker::try_lock(ctx, &branch)? {
        lock
    } else {
        eprintln!("waiting for the background worker on {branch}...");
        worker::wait_lock(ctx, &branch)?
    };
    // Never reuse (always measure, so a flaky result can be re-run) and never
    // cancel: the developer asked for this commit.
    let outcome = worker::record_commit(ctx, &branch, commit, Reuse::Never, &|| false)?;
    drop(lock);
    // A hook may have queued a job while we held the lock, and its worker
    // exited seeing the lock held; hand the job to a fresh worker.
    if queue::peek(ctx, &branch)?.is_some()
        && let Err(e) = worker::spawn_worker(ctx, &branch)
    {
        tracing::warn!(error = %format!("{e:#}"), "cannot start a worker for the queued job");
    }
    let (id, record) = match outcome {
        Outcome::Recorded(id, r) | Outcome::Reused(id, r) => (id, r),
        Outcome::Superseded => bail!("recording {commit} was cancelled; run `devctl record` again"),
    };

    let history = ctx
        .store()
        .records_on(&branch, ctx.config.retention.records_per_branch.max(2))
        .with_context(|| {
            format!(
                "cannot read the record store {}; delete it and run `devctl record` again",
                ctx.dirs.store().display()
            )
        })?;
    // The next-newest record of a different commit (a re-record of the same
    // commit is not "previous").
    let previous = history
        .iter()
        .skip_while(|(i, _)| *i != id)
        .skip(1)
        .find(|(_, r)| r.commit != record.commit)
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
    use crate::record::worker::tests::{SharedFake, fixture_repo, runs};
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

        // A newer commit with lower coverage compares against the first.
        t.write(
            "fixture/cov.info",
            "SF:src/a.rs\nDA:1,1\nDA:2,0\nend_of_record\n",
        );
        let second = t.commit("two");
        let out = run_record(&ctx, Some(&second[..10])).unwrap();
        assert!(out.text.contains("50.0% (-25.0)"), "{}", out.text);

        // Recording `second` again skips its own earlier record when picking
        // the previous one.
        let out = run_record(&ctx, None).unwrap();
        assert!(out.text.contains("50.0% (-25.0)"), "{}", out.text);
    }

    #[test]
    fn explicit_record_never_reuses() {
        let t = fixture_repo(false);
        t.commit("one");
        let ctx = Ctx::open(t.path(), Box::new(FakePlatform::new())).unwrap();
        run_record(&ctx, None).unwrap();
        run_record(&ctx, None).unwrap();
        assert_eq!(runs(&ctx, "dev"), 2, "measured twice");
        let records = ctx.store().records_on("dev", 10).unwrap();
        assert_eq!(records.len(), 2);
    }

    #[test]
    fn explicit_record_leaves_a_queued_job_to_a_worker() {
        let t = fixture_repo(false);
        let first = t.commit("one");
        t.write("src/a.rs", "changed\n");
        t.commit("two");
        let fake = std::sync::Arc::new(FakePlatform::new());
        let ctx = Ctx::open(t.path(), Box::new(SharedFake(fake.clone()))).unwrap();
        // A hook queued HEAD; `record --commit <first>` records only `first`
        // and starts a worker for the queued job.
        let head = ctx.repo.resolve("HEAD").unwrap();
        queue::enqueue(&ctx, "dev", head).unwrap();
        run_record(&ctx, Some(&first)).unwrap();
        assert_eq!(peek(&ctx, "dev").unwrap(), Some(head));
        let spawned = fake.spawned();
        assert_eq!(spawned.len(), 1);
        assert_eq!(spawned[0].args, ["worker", "--branch", "dev"]);
    }

    #[test]
    fn lock_errors_are_reported_not_waited_on() {
        let t = fixture_repo(false);
        t.commit("one");
        let ctx = Ctx::open(t.path(), Box::new(FakePlatform::new())).unwrap();
        // A directory where the lock file should be cannot be opened.
        std::fs::create_dir_all(ctx.dirs.lock("dev")).unwrap();
        let err = run_record(&ctx, None).unwrap_err();
        assert!(format!("{err:#}").contains("cannot lock"), "{err:#}");
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
