//! `devctl record` and `devctl worker` as processes.

mod support;

use std::path::{Path, PathBuf};
use std::process::{Child, Command as StdCommand, Stdio};
use std::time::{Duration, Instant};

use assert_cmd::Command;
use devctl::ctx::Ctx;
use devctl::git::ObjectId;
use devctl::record::{queue, worker};
use predicates::str::contains;
use support::TestRepo;

const COV: &str = "SF:src/a.rs\nDA:1,1\nDA:2,0\nDA:3,1\nDA:4,1\nend_of_record\n";
const JUNIT: &str =
    r#"<testsuites><testsuite name="s"><testcase name="a"/></testsuite></testsuites>"#;

/// A repository on `dev` whose `coverage` recipe runs `pre` first, then copies
/// the committed LCOV and `JUnit` fixtures into place.
fn fixture(pre: &str) -> TestRepo {
    let t = TestRepo::new();
    t.write(
        "justfile",
        format!(
            "coverage:\n{pre}    mkdir -p coverage test-results\n    cp fixture/cov.info coverage/cov.info\n    cp fixture/junit.xml test-results/junit.xml\n\ntest:\n    true\n"
        ),
    );
    t.write("fixture/cov.info", COV);
    t.write("fixture/junit.xml", JUNIT);
    t
}

fn devctl(dir: &Path) -> Command {
    let mut cmd = Command::cargo_bin("devctl").expect("devctl binary should build");
    cmd.current_dir(dir).env("OTEL_SDK_DISABLED", "true");
    for var in support::REPO_ENV {
        cmd.env_remove(var);
    }
    cmd
}

fn logs_containing(t: &TestRepo, prefix: &str, needle: &str) -> bool {
    let logs = t.path().join(".git").join("devctl").join("logs");
    std::fs::read_dir(logs).is_ok_and(|entries| {
        entries.filter_map(Result::ok).any(|e| {
            e.file_name().to_string_lossy().starts_with(prefix)
                && std::fs::read_to_string(e.path()).is_ok_and(|s| s.contains(needle))
        })
    })
}

#[test]
fn record_prints_the_summary() {
    let t = fixture("");
    let head = t.commit("one");
    devctl(t.path())
        .arg("record")
        .timeout(Duration::from_secs(60))
        .assert()
        .success()
        .stdout(contains(format!(
            "dev  {}  ok  75.0% (new)  tests 1 passed, 0 failed, 0 errored, 0 skipped  ",
            &head[..7]
        )));
}

#[test]
fn record_with_error_status_exits_non_zero() {
    let t = TestRepo::new();
    t.write("f", "x\n");
    t.commit("one");
    devctl(t.path())
        .arg("record")
        .timeout(Duration::from_secs(60))
        .assert()
        .failure()
        .stdout(contains("  error  n/a  "))
        .stdout(contains(
            "    error: missing recipe `coverage`; run `devctl init`",
        ))
        .stderr(contains("error: the record ended with status error"));
}

#[test]
fn record_on_detached_head_fails_and_logs_the_error() {
    let t = fixture("");
    t.commit("one");
    t.git(&["checkout", "-q", "--detach"]);
    let msg = "HEAD is detached; check out a branch, then run `devctl record`";
    devctl(t.path())
        .arg("record")
        .assert()
        .failure()
        .stderr(format!("error: {msg}\n"));
    assert!(logs_containing(&t, "cli.", msg), "error logged in cli log");
}

/// Kills the child and stops the recipe it left waiting, however the test
/// ends. The recipe removes `started` on its way out; waiting for that keeps
/// the temp repo from being deleted before the recipe sees `stop`.
struct Reaper {
    child: Child,
    dir: PathBuf,
}

impl Drop for Reaper {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if self.dir.join("started").exists() {
            let _ = std::fs::write(self.dir.join("stop"), "");
            let deadline = Instant::now() + Duration::from_secs(10);
            while self.dir.join("started").exists() && Instant::now() < deadline {
                std::thread::sleep(Duration::from_millis(50));
            }
        }
    }
}

fn wait_until(what: &str, limit: Duration, mut f: impl FnMut() -> bool) {
    let deadline = Instant::now() + limit;
    while !f() {
        assert!(Instant::now() < deadline, "timed out waiting for {what}");
        std::thread::sleep(Duration::from_millis(50));
    }
}

#[test]
fn killed_worker_releases_lock() {
    // The recipe signals that it started, waits (bounded) for `stop`, then
    // removes `started` in the same shell (once the worker is gone, `just`
    // dies of SIGPIPE echoing its next line to the closed stderr pipe).
    let t = fixture(
        "    touch started\n    i=0; while [ ! -f stop ] && [ $i -lt 300 ]; do sleep 0.1; i=$((i+1)); done; rm -f started\n",
    );
    let head = t.commit("one");
    let ctx = Ctx::open(t.path(), devctl_platform::current()).unwrap();
    queue::enqueue(&ctx, "dev", ObjectId::from_hex(head.as_bytes()).unwrap()).unwrap();
    let measure = ctx.dirs.measure("dev");

    let config = tempfile::tempdir().unwrap();
    let global = config.path().join("gitconfig");
    std::fs::write(&global, "").unwrap();
    let mut cmd = StdCommand::new(env!("CARGO_BIN_EXE_devctl"));
    for var in support::REPO_ENV {
        cmd.env_remove(var);
    }
    let child = cmd
        .args(["worker", "--branch", "dev"])
        .current_dir(t.path())
        .env("OTEL_SDK_DISABLED", "true")
        .env("GIT_CONFIG_NOSYSTEM", "1")
        .env("GIT_CONFIG_GLOBAL", &global)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .unwrap();
    let mut reaper = Reaper {
        child,
        dir: measure.clone(),
    };

    wait_until("the recipe to start", Duration::from_secs(30), || {
        measure.join("started").exists()
    });
    assert!(worker::worker_running(&ctx, "dev").unwrap());
    reaper.child.kill().unwrap();
    reaper.child.wait().unwrap();
    wait_until("the lock to be released", Duration::from_secs(5), || {
        !worker::worker_running(&ctx, "dev").unwrap()
    });
    // The job survives the crash for the next worker.
    assert_eq!(
        queue::peek(&ctx, "dev").unwrap(),
        Some(ObjectId::from_hex(head.as_bytes()).unwrap())
    );
}

#[test]
fn status_reports_records_and_empty_repos() {
    let t = fixture("");
    t.commit("one");
    devctl(t.path())
        .arg("status")
        .assert()
        .success()
        .stdout(contains("no records yet; run `devctl record`"));
    devctl(t.path())
        .arg("record")
        .timeout(Duration::from_secs(60))
        .assert()
        .success();
    devctl(t.path())
        .args(["status", "--branch", "dev"])
        .assert()
        .success()
        .stdout(contains("dev  "))
        .stdout(contains("75.0% (new)"))
        .stdout(contains("measure "));
    devctl(t.path())
        .args(["status", "--branch", "nope"])
        .assert()
        .success()
        .stdout(contains("no records for nope; run `devctl record` on it"));
}
