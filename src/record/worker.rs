//! The background recorder (spec section 4.1, steps 3-7).

use std::process::Command;
use std::time::{Duration, Instant};

use anyhow::{Context, Result};
use devctl_platform::Lock;
use devctl_store::{
    CoverageSummary, RawReports, Record, RecordId, RetentionPolicy, Status, Store, TestSummary,
};

use crate::ctx::Ctx;
use crate::git::{self, ObjectId, Repo};
use crate::recipes::{ContractResult, RecipeRun, run_contract};
use crate::record::{now_ms, queue};
use crate::telemetry;

/// `just_version` when `just` could not be found.
const UNKNOWN: &str = "unknown";

fn status_name(s: &Status) -> &'static str {
    match s {
        Status::Ok => "ok",
        Status::TestsFailed => "tests_failed",
        Status::Error { .. } => "error",
    }
}

fn retention(ctx: &Ctx) -> RetentionPolicy {
    RetentionPolicy {
        records_per_branch: ctx.config.retention.records_per_branch,
        raw_reports_days: ctx.config.retention.raw_reports_days,
    }
}

/// Stores `record` (and `raw`, unless empty), then applies retention; a
/// pruning failure is only logged.
fn store_record(ctx: &Ctx, record: &Record, raw: Option<&RawReports>) -> Result<RecordId> {
    let store = ctx.store();
    let raw = raw.filter(|r| !r.lcov.is_empty() || !r.junit.is_empty());
    let id = store.put_record(record, raw).with_context(|| {
        format!(
            "cannot store the record for {} on {}",
            hex_of(&record.commit),
            record.branch
        )
    })?;
    if let Err(e) = store.prune(&retention(ctx), now_ms()) {
        tracing::warn!(error = %e, "cannot apply record retention");
    }
    Ok(id)
}

fn hex_of(bytes: &[u8]) -> String {
    use std::fmt::Write as _;
    bytes.iter().fold(String::new(), |mut s, b| {
        let _ = write!(s, "{b:02x}");
        s
    })
}

/// Copies the latest `ok`/`tests_failed` record of `tree` for `commit`.
fn reuse(
    ctx: &Ctx,
    branch: &str,
    commit: ObjectId,
    tree: ObjectId,
    started_at_ms: i64,
) -> Result<Option<Outcome>> {
    let store = ctx.store();
    let found = match store.record_for_tree(tree.as_bytes()) {
        Ok(found) => found,
        Err(e) => {
            tracing::warn!(error = %e, "cannot look up a record for this tree; recording instead");
            return Ok(None);
        }
    };
    let Some((found_id, found)) = found else {
        return Ok(None);
    };
    let raw = store.raw(found_id).unwrap_or_else(|e| {
        tracing::warn!(error = %e, "cannot read the reused record's raw reports");
        None
    });
    let record = Record {
        commit: commit.as_bytes().to_vec(),
        branch: branch.to_string(),
        started_at_ms,
        finished_at_ms: now_ms(),
        ..found
    };
    let id = store_record(ctx, &record, raw.as_ref())?;
    tracing::info!(
        from = found_id,
        id,
        "reused the record of an identical tree"
    );
    Ok(Some(Outcome::Reused(id, record)))
}

/// What measuring produced, before it becomes a [`Record`].
struct Measured {
    status: Status,
    coverage: CoverageSummary,
    tests: TestSummary,
    raw: RawReports,
    just_version: String,
}

impl Measured {
    fn error(message: &str, just_version: String) -> Measured {
        Measured {
            status: Status::error(message),
            coverage: CoverageSummary::default(),
            tests: TestSummary::default(),
            raw: RawReports::default(),
            just_version,
        }
    }
}

/// Prepares the measuring worktree and runs the recipe contract. `None` when
/// cancelled. Every failure becomes an `Error` status.
fn measure(
    ctx: &Ctx,
    branch: &str,
    commit: ObjectId,
    cancel: &dyn Fn() -> bool,
) -> Option<Measured> {
    let just = match ctx.platform.just() {
        Ok(j) => j,
        Err(e) => return Some(Measured::error(&e.to_string(), UNKNOWN.into())),
    };
    let version = just.version_string();
    let root = match super::measure::prepare(ctx, branch, commit) {
        Ok(r) => r,
        Err(e) => return Some(Measured::error(&format!("{e:#}"), version)),
    };
    let hex = commit.to_string();
    let minutes = u64::from(ctx.config.record.timeout_minutes);
    let run = RecipeRun {
        platform: ctx.platform.as_ref(),
        just: &just,
        root: &root,
        commit: &hex,
        timeout: Duration::from_secs(minutes * 60),
        cancel,
    };
    match run_contract(&run) {
        Ok(ContractResult::Cancelled) => None,
        Ok(ContractResult::Done {
            status,
            report,
            tests,
            raw,
        }) => {
            let coverage = report.map_or_else(CoverageSummary::default, |r| {
                let (total, files) = r.line_counts();
                CoverageSummary {
                    total: devctl_store::LineCounts {
                        coverable: total.coverable,
                        covered: total.covered,
                    },
                    files: files
                        .into_iter()
                        .map(|(path, c)| {
                            (
                                path,
                                devctl_store::LineCounts {
                                    coverable: c.coverable,
                                    covered: c.covered,
                                },
                            )
                        })
                        .collect(),
                }
            });
            let tests = tests.map_or_else(TestSummary::default, |t| {
                let c = t.counts();
                TestSummary {
                    passed: c.passed,
                    failed: c.failed,
                    errored: c.errored,
                    skipped: c.skipped,
                    duration_ms: u64::try_from(c.duration.as_millis()).unwrap_or(u64::MAX),
                }
            });
            Some(Measured {
                status,
                coverage,
                tests,
                raw,
                just_version: version,
            })
        }
        Err(e) => Some(Measured::error(&format!("{e:#}"), version)),
    }
}

/// What [`record_commit`] did.
#[derive(Debug, Clone, PartialEq)]
pub enum Outcome {
    /// The recipes ran and this record was stored.
    Recorded(RecordId, Record),
    /// A record for the same tree was copied for this commit.
    Reused(RecordId, Record),
    /// A newer job replaced this one mid-run; nothing was stored.
    Superseded,
}

/// Records `commit` on `branch`.
///
/// # Errors
/// If the record cannot be stored.
pub fn record_commit(
    ctx: &Ctx,
    branch: &str,
    commit: ObjectId,
    cancel: &dyn Fn() -> bool,
) -> Result<Outcome> {
    let span = tracing::info_span!("record.commit", branch, commit = %commit);
    let _entered = span.enter();
    let started_at_ms = now_ms();
    let clock = Instant::now();

    let tree = match ctx.repo.tree_of(commit) {
        Ok(tree) => Some(tree),
        Err(e) => {
            tracing::warn!(error = %format!("{e:#}"), "cannot read the commit's tree");
            None
        }
    };
    let measured = match tree {
        Some(tree) => {
            if let Some(outcome) = reuse(ctx, branch, commit, tree, started_at_ms)? {
                return Ok(outcome);
            }
            let Some(m) = measure(ctx, branch, commit, cancel) else {
                tracing::info!(monotonic_counter.devctl.worker.cancelled = 1_u64);
                tracing::info!("superseded by a newer commit");
                return Ok(Outcome::Superseded);
            };
            m
        }
        None => Measured::error(
            &format!(
                "commit {commit} is not in this repository (or cannot be read); \
                 run `devctl record` on an existing commit"
            ),
            ctx.platform
                .just()
                .map_or_else(|_| UNKNOWN.into(), |j| j.version_string()),
        ),
    };

    let record = Record {
        commit: commit.as_bytes().to_vec(),
        tree: tree.map(|t| t.as_bytes().to_vec()).unwrap_or_default(),
        branch: branch.to_string(),
        started_at_ms,
        finished_at_ms: now_ms(),
        status: measured.status,
        coverage: measured.coverage,
        tests: measured.tests,
        devctl_version: env!("CARGO_PKG_VERSION").to_string(),
        just_version: measured.just_version,
    };
    let id = store_record(ctx, &record, Some(&measured.raw))?;
    let status = status_name(&record.status);
    tracing::info!(
        histogram.devctl.record.duration = clock.elapsed().as_secs_f64(),
        status,
        id,
        "recorded"
    );
    Ok(Outcome::Recorded(id, record))
}

/// Records jobs on `branch` until its queue is empty, then returns. Returns
/// at once if another worker holds the branch lock.
///
/// # Errors
/// If the lock or queue cannot be read.
pub fn run_worker(ctx: &Ctx, branch: &str) -> Result<()> {
    run_worker_with(ctx, branch, &mut || {})
}

/// [`run_worker`], calling `between` after each drain while the lock is
/// still held (tests use it to queue a job at that moment).
fn run_worker_with(ctx: &Ctx, branch: &str, between: &mut dyn FnMut()) -> Result<()> {
    loop {
        let Some(lock) = try_lock(ctx, branch)? else {
            tracing::info!(
                branch,
                "another worker holds the lock; leaving the job to it"
            );
            return Ok(());
        };
        drain(ctx, branch, &lock)?;
        between();
        drop(lock);
        // A hook may have queued a job (and its worker exited, seeing the
        // lock held) between the last drain and the release.
        if queue::peek(ctx, branch)?.is_none() {
            return Ok(());
        }
    }
}

fn lock_error(ctx: &Ctx, branch: &str) -> String {
    format!(
        "cannot lock {}; check the permissions of {}",
        ctx.dirs.lock(branch).display(),
        ctx.dirs.root.display()
    )
}

fn try_lock(ctx: &Ctx, branch: &str) -> Result<Option<Lock>> {
    ctx.platform
        .try_lock(&ctx.dirs.lock(branch))
        .with_context(|| lock_error(ctx, branch))
}

/// Takes `branch`'s worker lock, waiting for a running worker to finish.
///
/// # Errors
/// If the lock file cannot be opened or locked.
pub fn wait_lock(ctx: &Ctx, branch: &str) -> Result<Lock> {
    ctx.platform
        .lock(&ctx.dirs.lock(branch))
        .with_context(|| lock_error(ctx, branch))
}

/// Records queued jobs until the queue is empty. The caller holds `branch`'s
/// worker lock. A job whose recording fails (e.g. the store is unwritable) is
/// logged and dropped, so it cannot loop forever.
///
/// # Errors
/// If the queue cannot be read or updated.
pub fn drain(ctx: &Ctx, branch: &str, _held: &Lock) -> Result<()> {
    while let Some(commit) = queue::peek(ctx, branch)? {
        // run_contract polls this once a second; a small unlocked read.
        let cancel = || match queue::peek(ctx, branch) {
            Ok(queued) => queued != Some(commit),
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "cannot re-read the record queue");
                false
            }
        };
        match record_commit(ctx, branch, commit, &cancel) {
            Ok(Outcome::Recorded(id, r) | Outcome::Reused(id, r)) => {
                tracing::info!(id, status = status_name(&r.status), %commit, "job done");
            }
            Ok(Outcome::Superseded) => {}
            Err(e) => tracing::error!(error = %format!("{e:#}"), %commit, "recording failed"),
        }
        queue::complete(ctx, branch, commit)?;
    }
    Ok(())
}

/// Starts `devctl worker --branch <branch>` detached, in the worktree's top
/// level, with `TRACEPARENT` set when telemetry export is on and git's
/// repository-selecting variables removed.
///
/// # Errors
/// If the current executable cannot be found or the process cannot start.
pub fn spawn_worker(ctx: &Ctx, branch: &str) -> Result<()> {
    let exe = std::env::current_exe()
        .context("cannot find the devctl executable to start the background worker")?;
    let mut cmd = Command::new(&exe);
    cmd.args(["worker", "--branch", branch]);
    if let Some(top) = ctx.repo.workdir() {
        cmd.current_dir(top);
    }
    for var in git::REPO_ENV {
        cmd.env_remove(var);
    }
    if let Some(tp) = telemetry::traceparent() {
        cmd.env("TRACEPARENT", tp);
    }
    let pid = ctx.platform.spawn_detached(cmd).with_context(|| {
        format!(
            "cannot start `{} worker --branch {branch}`; run `devctl record` to record in the foreground",
            exe.display()
        )
    })?;
    tracing::info!(pid, branch, "started worker");
    Ok(())
}

/// Whether a worker holds `branch`'s lock.
///
/// # Errors
/// If the lock file cannot be opened.
pub fn worker_running(ctx: &Ctx, branch: &str) -> Result<bool> {
    Ok(try_lock(ctx, branch)?.is_none())
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::git::Repo;
    use crate::record::queue::{enqueue, peek};
    use crate::test_support::TestRepo;
    use devctl_platform::JustError;
    use devctl_platform::fake::FakePlatform;
    use devctl_store::{RawReports, Status, Store};
    use std::time::{Duration, Instant};

    pub(crate) const COV: &str =
        "SF:src/a.rs\nDA:1,1\nDA:2,0\nDA:3,1\nend_of_record\nSF:src/b.rs\nDA:1,1\nend_of_record\n";
    pub(crate) const JUNIT: &str = r#"<testsuites><testsuite name="s"><testcase name="a" time="0.5"/><testcase name="b"><skipped/></testcase></testsuite></testsuites>"#;

    /// `coverage` copies the committed fixtures into place; `test` is a no-op
    /// (skipped anyway, since `coverage` writes `JUnit`).
    pub(crate) fn justfile(slow: bool) -> String {
        let wait = if slow { "    sleep 30\n" } else { "" };
        format!(
            "coverage:\n{wait}    mkdir -p coverage test-results\n    cp fixture/cov.info coverage/cov.info\n    cp fixture/junit.xml test-results/junit.xml\n\ntest:\n    true\n"
        )
    }

    pub(crate) fn fixture_repo(slow: bool) -> TestRepo {
        let t = TestRepo::new();
        t.write("justfile", justfile(slow));
        t.write("fixture/cov.info", COV);
        t.write("fixture/junit.xml", JUNIT);
        t.write("src/a.rs", "a\nb\nc\n");
        t
    }

    fn oid(hex: &str) -> ObjectId {
        ObjectId::from_hex(hex.as_bytes()).unwrap()
    }

    fn ctx_of(t: &TestRepo) -> Ctx {
        Ctx::open(t.path(), Box::new(FakePlatform::new())).unwrap()
    }

    fn never() -> bool {
        false
    }

    #[test]
    fn records_head() {
        let t = fixture_repo(false);
        let head = oid(&t.commit("one"));
        let ctx = ctx_of(&t);
        let Outcome::Recorded(id, rec) = record_commit(&ctx, "dev", head, &never).unwrap() else {
            panic!("expected Recorded");
        };
        assert_eq!(rec.status, Status::Ok);
        assert_eq!(rec.commit, head.as_bytes());
        assert_eq!(rec.tree, ctx.repo.tree_of(head).unwrap().as_bytes());
        assert_eq!(rec.branch, "dev");
        assert_eq!(rec.coverage.total.coverable, 4);
        assert_eq!(rec.coverage.total.covered, 3);
        assert_eq!(rec.coverage.files["src/a.rs"].coverable, 3);
        assert_eq!(rec.tests.passed, 1);
        assert_eq!(rec.tests.skipped, 1);
        assert_eq!(rec.tests.duration_ms, 500);
        assert_eq!(rec.devctl_version, env!("CARGO_PKG_VERSION"));
        assert!(rec.just_version.starts_with("1."), "{}", rec.just_version);
        assert!(rec.started_at_ms <= rec.finished_at_ms && rec.started_at_ms > 0);
        let store = ctx.store();
        assert_eq!(store.record_for(head.as_bytes()).unwrap(), Some((id, rec)));
        let raw = store.raw(id).unwrap().expect("raw stored");
        assert!(raw.lcov.contains("SF:src/a.rs"));
        assert_eq!(raw.junit, vec![JUNIT.to_string()]);
    }

    #[test]
    fn amend_message_only_reuses() {
        let t = fixture_repo(false);
        let first = oid(&t.commit("one"));
        let ctx = ctx_of(&t);
        let Outcome::Recorded(_, before) = record_commit(&ctx, "dev", first, &never).unwrap()
        else {
            panic!("expected Recorded");
        };
        t.git(&[
            "commit",
            "-q",
            "--amend",
            "--no-verify",
            "-m",
            "new message",
        ]);
        let amended = oid(&t.git(&["rev-parse", "HEAD"]));
        assert_ne!(amended, first);
        let Outcome::Reused(id, after) = record_commit(&ctx, "feature/y", amended, &never).unwrap()
        else {
            panic!("expected Reused");
        };
        assert_eq!(after.commit, amended.as_bytes());
        assert_eq!(after.branch, "feature/y");
        assert_eq!(after.coverage, before.coverage);
        assert_eq!(after.tests, before.tests);
        assert!(after.finished_at_ms >= before.finished_at_ms);
        let store = ctx.store();
        let raw: RawReports = store.raw(id).unwrap().expect("raw copied");
        assert!(raw.lcov.contains("SF:src/a.rs"));
        assert_eq!(
            store.record_for(amended.as_bytes()).unwrap().map(|r| r.0),
            Some(id)
        );
    }

    #[test]
    fn missing_just_becomes_error_record() {
        let t = fixture_repo(false);
        let head = oid(&t.commit("one"));
        let ctx = Ctx::open(
            t.path(),
            Box::new(FakePlatform::new().with_just(Err(JustError::Missing))),
        )
        .unwrap();
        let Outcome::Recorded(_, rec) = record_commit(&ctx, "dev", head, &never).unwrap() else {
            panic!("expected Recorded");
        };
        let Status::Error { stderr_tail } = &rec.status else {
            panic!("expected Error, got {:?}", rec.status);
        };
        assert!(stderr_tail.contains("just.systems"), "{stderr_tail}");
        assert_eq!(rec.just_version, "unknown");
        assert_eq!(
            ctx.store()
                .record_for(head.as_bytes())
                .unwrap()
                .map(|r| r.1.status),
            Some(rec.status.clone())
        );
    }

    #[test]
    fn missing_recipe_becomes_error_record() {
        let t = TestRepo::new();
        t.write("f", "x\n");
        let head = oid(&t.commit("one"));
        let ctx = ctx_of(&t);
        let Outcome::Recorded(_, rec) = record_commit(&ctx, "dev", head, &never).unwrap() else {
            panic!("expected Recorded");
        };
        let Status::Error { stderr_tail } = &rec.status else {
            panic!("expected Error, got {:?}", rec.status);
        };
        assert!(
            stderr_tail.contains("missing recipe `coverage`"),
            "{stderr_tail}"
        );
    }

    #[test]
    fn newer_job_supersedes() {
        let t = fixture_repo(true);
        let a = oid(&t.commit("slow"));
        t.write("justfile", justfile(false));
        let b = oid(&t.commit("fast"));
        let ctx = ctx_of(&t);
        enqueue(&ctx, "dev", a).unwrap();
        let start = Instant::now();
        std::thread::scope(|s| {
            let worker = s.spawn(|| run_worker(&ctx, "dev"));
            std::thread::sleep(Duration::from_secs(1));
            enqueue(&ctx, "dev", b).unwrap();
            worker.join().unwrap().unwrap();
        });
        assert!(
            start.elapsed() < Duration::from_secs(10),
            "{:?}",
            start.elapsed()
        );
        let store = ctx.store();
        assert!(store.record_for(a.as_bytes()).unwrap().is_none());
        let (_, rec) = store
            .record_for(b.as_bytes())
            .unwrap()
            .expect("record for B");
        assert_eq!(rec.status, Status::Ok);
        assert_eq!(peek(&ctx, "dev").unwrap(), None);
    }

    #[test]
    fn second_worker_exits_when_locked() {
        let t = fixture_repo(false);
        let a = oid(&t.commit("one"));
        let ctx = ctx_of(&t);
        let held = ctx.platform.lock(&ctx.dirs.lock("dev")).unwrap();
        assert!(worker_running(&ctx, "dev").unwrap());
        enqueue(&ctx, "dev", a).unwrap();
        let start = Instant::now();
        run_worker(&ctx, "dev").unwrap();
        assert!(start.elapsed() < Duration::from_secs(1));
        assert_eq!(
            peek(&ctx, "dev").unwrap(),
            Some(a),
            "job left for the holder"
        );
        assert!(ctx.store().record_for(a.as_bytes()).unwrap().is_none());
        drop(held);
        assert!(!worker_running(&ctx, "dev").unwrap());
    }

    #[test]
    fn job_queued_during_exit_is_picked_up() {
        let t = fixture_repo(false);
        let a = oid(&t.commit("one"));
        t.write("src/a.rs", "changed\n");
        let b = oid(&t.commit("two"));
        let ctx = ctx_of(&t);
        enqueue(&ctx, "dev", a).unwrap();
        let mut queued = false;
        run_worker_with(&ctx, "dev", &mut || {
            if !queued {
                queued = true;
                // A hook queues B; its worker finds the lock held and exits.
                enqueue(&ctx, "dev", b).unwrap();
                run_worker(&ctx, "dev").unwrap();
                assert!(ctx.store().record_for(b.as_bytes()).unwrap().is_none());
            }
        })
        .unwrap();
        let store = ctx.store();
        assert!(store.record_for(a.as_bytes()).unwrap().is_some());
        assert!(
            store.record_for(b.as_bytes()).unwrap().is_some(),
            "re-check recorded B"
        );
        assert_eq!(peek(&ctx, "dev").unwrap(), None);
    }

    #[test]
    fn drain_records_while_lock_held_by_caller() {
        let t = fixture_repo(false);
        let a = oid(&t.commit("one"));
        let ctx = ctx_of(&t);
        let held = ctx.platform.lock(&ctx.dirs.lock("dev")).unwrap();
        enqueue(&ctx, "dev", a).unwrap();
        drain(&ctx, "dev", &held).unwrap();
        assert!(ctx.store().record_for(a.as_bytes()).unwrap().is_some());
        assert_eq!(peek(&ctx, "dev").unwrap(), None);
    }

    #[test]
    fn unknown_commit_becomes_error_record_and_job_is_dropped() {
        let t = fixture_repo(false);
        t.commit("one");
        let ctx = ctx_of(&t);
        let bogus = oid("1111111111111111111111111111111111111111");
        enqueue(&ctx, "dev", bogus).unwrap();
        run_worker(&ctx, "dev").unwrap();
        assert_eq!(peek(&ctx, "dev").unwrap(), None);
        let (_, rec) = ctx
            .store()
            .record_for(bogus.as_bytes())
            .unwrap()
            .expect("error record");
        assert!(
            matches!(rec.status, Status::Error { .. }),
            "{:?}",
            rec.status
        );
    }

    #[test]
    fn spawn_worker_passes_branch_and_traceparent() {
        use crate::telemetry;
        use opentelemetry_sdk::logs::{InMemoryLogExporter, SdkLoggerProvider};
        use opentelemetry_sdk::metrics::{
            InMemoryMetricExporter, PeriodicReader, SdkMeterProvider,
        };
        use opentelemetry_sdk::trace::{InMemorySpanExporter, SdkTracerProvider};

        let t = TestRepo::new();
        t.commit("one");
        let fake = std::sync::Arc::new(FakePlatform::new());
        let ctx = Ctx::open(t.path(), Box::new(SharedFake(fake.clone()))).unwrap();

        let cfg = crate::config::TelemetryCfg {
            endpoint: String::new(),
            include_paths: false,
            log_level: "info".into(),
        };
        let settings = telemetry::Settings {
            process: "hook-post-commit",
            logs_dir: None,
            cfg: &cfg,
            flush_timeout: Duration::from_secs(1),
            repo: None,
        };
        let exporters = telemetry::Exporters {
            tracer: SdkTracerProvider::builder()
                .with_simple_exporter(InMemorySpanExporter::default())
                .build(),
            logger: SdkLoggerProvider::builder()
                .with_simple_exporter(InMemoryLogExporter::default())
                .build(),
            meter: SdkMeterProvider::builder()
                .with_reader(PeriodicReader::builder(InMemoryMetricExporter::default()).build())
                .build(),
        };
        let (dispatch, guard) = telemetry::build_with(&settings, Some(exporters));
        let header = tracing::dispatcher::with_default(&dispatch, || {
            let span = tracing::info_span!("hook");
            span.in_scope(|| {
                spawn_worker(&ctx, "feature/x").unwrap();
                telemetry::traceparent().expect("traceparent")
            })
        });
        drop(guard);

        let spawned = fake.spawned();
        assert_eq!(spawned.len(), 1);
        let s = &spawned[0];
        assert_eq!(s.program, std::env::current_exe().unwrap().into_os_string());
        assert_eq!(s.args, ["worker", "--branch", "feature/x"]);
        assert_eq!(s.current_dir.as_deref(), ctx.repo.workdir());
        let env = |k: &str| s.envs.iter().find(|(n, _)| n == k).map(|(_, v)| v.clone());
        assert_eq!(env("TRACEPARENT"), Some(Some(header.into())));
        for var in crate::git::REPO_ENV {
            assert_eq!(env(var), Some(None), "{var} removed");
        }
    }

    #[test]
    fn spawn_worker_without_export_sets_no_traceparent() {
        let t = TestRepo::new();
        t.commit("one");
        let fake = std::sync::Arc::new(FakePlatform::new());
        let ctx = Ctx::open(t.path(), Box::new(SharedFake(fake.clone()))).unwrap();
        spawn_worker(&ctx, "dev").unwrap();
        let s = &fake.spawned()[0];
        assert!(s.envs.iter().all(|(n, _)| n != "TRACEPARENT"));
    }

    /// Lets a test keep a handle on the `FakePlatform` inside a `Ctx`.
    struct SharedFake(std::sync::Arc<FakePlatform>);

    impl devctl_platform::Platform for SharedFake {
        fn spawn_detached(&self, cmd: std::process::Command) -> std::io::Result<u32> {
            self.0.spawn_detached(cmd)
        }
        fn spawn_group(&self, cmd: std::process::Command) -> std::io::Result<std::process::Child> {
            self.0.spawn_group(cmd)
        }
        fn kill_tree(&self, pid: u32) -> std::io::Result<()> {
            self.0.kill_tree(pid)
        }
        fn lock(&self, path: &std::path::Path) -> std::io::Result<Lock> {
            self.0.lock(path)
        }
        fn try_lock(&self, path: &std::path::Path) -> std::io::Result<Option<Lock>> {
            self.0.try_lock(path)
        }
        fn open_browser(&self, url: &str) -> std::io::Result<()> {
            self.0.open_browser(url)
        }
        fn just(&self) -> Result<devctl_platform::JustCommand, JustError> {
            self.0.just()
        }
        fn data_dir(&self) -> Option<std::path::PathBuf> {
            self.0.data_dir()
        }
        fn normalize_path(&self, p: &std::path::Path) -> String {
            self.0.normalize_path(p)
        }
        fn make_executable(&self, path: &std::path::Path) -> std::io::Result<()> {
            self.0.make_executable(path)
        }
    }
}
