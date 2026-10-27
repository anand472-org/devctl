//! End to end (spec sections 7.4 and 9): a sample repository whose `justfile`
//! copies prepared LCOV and `JUnit` files into place, driven through real
//! `git commit` and `git push` to a local bare remote with devctl's hooks
//! installed. The hooks call `devctl` from `PATH`, so the directory of the
//! built binary goes first on it for every git command.
//!
//! Needs `git` and `just` on `PATH`.

mod support;

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::process::Output;
use std::time::{Duration, Instant};

use devctl::ctx::Ctx;
use devctl::git::ObjectId;
use devctl::record::worker;
use devctl_store::{RedbStore, Store as _};
use support::TestRepo;

/// How long to wait for a background record, or for workers to go idle.
const DEADLINE: Duration = Duration::from_secs(60);
/// How often to poll while waiting.
const POLL: Duration = Duration::from_millis(100);
/// A safety net for any single git command (a push may record in the
/// foreground; the sample's record timeout is 2 minutes).
const GIT_TIMEOUT: Duration = Duration::from_secs(300);

const FEATURE: &str = "feature/e2e";

/// `src/lib.txt` after a feature commit: the base's two lines plus four new
/// ones (lines 3-6), which every case's LCOV has `DA` entries for.
const LIB_CHANGED: &str = "line 1\nline 2\nline 3\nline 4\nline 5\nline 6\n";

fn fixtures() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("tests")
        .join("fixtures")
        .join("e2e")
}

/// A working repository with hooks installed, on `dev`, whose `origin` is a
/// bare repository holding the same `dev`. Waits for every background worker
/// to finish when dropped, so none outlives the test.
struct Sample {
    repo: TestRepo,
    remote: tempfile::TempDir,
    path_env: OsString,
}

impl Sample {
    /// The base commit (sample `justfile`, `.devctl.toml`, `src/lib.txt` and the
    /// `pass` case) is committed and pushed to `dev` before the hooks are
    /// installed: it is setup, not under test, and pushing it through the
    /// gate would only add a foreground record to every test.
    fn new() -> Sample {
        let repo = TestRepo::new();
        let remote = tempfile::tempdir().expect("create remote temp dir");
        let bin = Path::new(env!("CARGO_BIN_EXE_devctl"));
        let old = std::env::var_os("PATH").unwrap_or_default();
        let path_env = std::env::join_paths(
            std::iter::once(bin.parent().expect("binary dir").to_path_buf())
                .chain(std::env::split_paths(&old)),
        )
        .expect("join PATH");
        let s = Sample {
            repo,
            remote,
            path_env,
        };
        s.git_in(s.remote.path(), &["init", "-q", "--bare", "-b", "dev"]);
        for rel in ["justfile", ".devctl.toml", "src/lib.txt"] {
            let from = rel.split('/').fold(fixtures(), |p, c| p.join(c));
            s.repo
                .write(rel, std::fs::read(&from).expect("read fixture"));
        }
        s.use_case("pass");
        s.git(&["add", "-A"]);
        s.git(&["commit", "-q", "-m", "Base"]);
        let url = s
            .remote
            .path()
            .to_str()
            .expect("UTF-8 temp path")
            .to_string();
        s.git(&["remote", "add", "origin", &url]);
        s.git(&["push", "-q", "origin", "dev"]);
        s.devctl(&["hooks", "install"]);
        s
    }

    /// An isolated git command in `dir` with `devctl` first on `PATH`.
    fn run_git_in(&self, dir: &Path, args: &[&str]) -> Output {
        let mut cmd = self.repo.command(dir);
        cmd.env("PATH", &self.path_env)
            .env("OTEL_SDK_DISABLED", "true")
            .args(args);
        assert_cmd::Command::from_std(cmd)
            .timeout(GIT_TIMEOUT)
            .output()
            .expect("run git")
    }

    fn run_git(&self, args: &[&str]) -> Output {
        self.run_git_in(self.repo.path(), args)
    }

    /// Runs git in `dir`; panics unless it succeeds. Returns trimmed stdout.
    fn git_in(&self, dir: &Path, args: &[&str]) -> String {
        let out = self.run_git_in(dir, args);
        assert!(out.status.success(), "git {args:?} failed:\n{}", text(&out));
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    fn git(&self, args: &[&str]) -> String {
        self.git_in(self.repo.path(), args)
    }

    fn head(&self) -> String {
        self.git(&["rev-parse", "HEAD"])
    }

    /// Runs `devctl` in the working repository; panics unless it succeeds.
    fn devctl(&self, args: &[&str]) -> String {
        let mut cmd = assert_cmd::Command::cargo_bin("devctl").expect("devctl binary");
        cmd.current_dir(self.repo.path())
            .env("OTEL_SDK_DISABLED", "true")
            .timeout(DEADLINE);
        for var in support::REPO_ENV {
            cmd.env_remove(var);
        }
        let out = cmd.args(args).output().expect("run devctl");
        assert!(
            out.status.success(),
            "devctl {args:?} failed:\n{}",
            text(&out)
        );
        String::from_utf8_lossy(&out.stdout).into_owned()
    }

    /// Replaces `fixture/` with the files of `cases/<case>`.
    fn use_case(&self, case: &str) {
        let dir = self.repo.file("fixture");
        if dir.exists() {
            std::fs::remove_dir_all(&dir).expect("remove fixture/");
        }
        let from = fixtures().join("cases").join(case);
        for entry in std::fs::read_dir(&from).expect("read case dir") {
            let entry = entry.expect("case entry");
            let name = entry.file_name();
            let name = name.to_str().expect("UTF-8 fixture name");
            self.repo.write(
                &format!("fixture/{name}"),
                std::fs::read(entry.path()).expect("read case file"),
            );
        }
    }

    /// Commits everything with the hooks running; panics unless git
    /// succeeds. Returns the new commit.
    fn commit(&self, msg: &str) -> String {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "-m", msg]);
        self.head()
    }

    /// Checks out a new `feature/e2e`, adds lines 3-6 to `src/lib.txt` with
    /// `case`'s fixtures, and commits it.
    fn feature_commit(&self, case: &str) -> String {
        self.git(&["checkout", "-q", "-b", FEATURE]);
        self.repo.write("src/lib.txt", LIB_CHANGED);
        self.use_case(case);
        self.commit(&format!("Add lines ({case})"))
    }

    /// `git push origin <args>`: whether it succeeded, and everything it
    /// printed (git forwards a hook's output to stderr).
    fn push(&self, args: &[&str]) -> (bool, String) {
        let mut all = vec!["push", "origin"];
        all.extend_from_slice(args);
        let out = self.run_git(&all);
        (out.status.success(), text(&out))
    }

    /// The commit `branch` points at on the remote, if it exists.
    fn remote_branch(&self, branch: &str) -> Option<String> {
        let out = self.run_git_in(
            self.remote.path(),
            &[
                "rev-parse",
                "-q",
                "--verify",
                &format!("refs/heads/{branch}"),
            ],
        );
        out.status
            .success()
            .then(|| String::from_utf8_lossy(&out.stdout).trim().to_string())
    }

    fn devctl_dir(&self) -> PathBuf {
        self.repo.path().join(".git").join("devctl")
    }

    /// Polls `devctl status` until it contains `needle`; returns the output.
    fn wait_status(&self, needle: &str) -> String {
        let start = Instant::now();
        loop {
            let out = self.devctl(&["status"]);
            if out.contains(needle) {
                return out;
            }
            assert!(
                start.elapsed() < DEADLINE,
                "no `{needle}` in `devctl status` after {DEADLINE:?}:\n{out}\nlogs:\n{}",
                self.logs()
            );
            std::thread::sleep(POLL);
        }
    }

    /// Every log file's text.
    fn logs(&self) -> String {
        let Ok(entries) = std::fs::read_dir(self.devctl_dir().join("logs")) else {
            return String::new();
        };
        let mut files: Vec<PathBuf> = entries.filter_map(Result::ok).map(|e| e.path()).collect();
        files.sort();
        files
            .iter()
            .filter_map(|p| std::fs::read_to_string(p).ok())
            .collect()
    }

    /// Polls the logs until they contain `needle`. A process's log lines are
    /// written by a background thread and flushed when it exits, after it has
    /// stored its record and released its lock, so neither `devctl status` nor
    /// [`Sample::idle`] implies they are on disk yet.
    fn wait_log(&self, needle: &str) {
        let start = Instant::now();
        while !self.logs().contains(needle) {
            assert!(
                start.elapsed() < DEADLINE,
                "no `{needle}` in the logs after {DEADLINE:?}:\n{}",
                self.logs()
            );
            std::thread::sleep(POLL);
        }
    }

    /// Whether no job is queued and no worker holds a branch lock.
    fn idle(&self) -> bool {
        let names = |dir: &str| -> Vec<String> {
            std::fs::read_dir(self.devctl_dir().join(dir))
                .map(|entries| {
                    entries
                        .filter_map(Result::ok)
                        .map(|e| e.file_name().to_string_lossy().into_owned())
                        .collect()
                })
                .unwrap_or_default()
        };
        // Queue files are named by branch slug; `.<slug>.tmp` is a write in
        // progress.
        if names("queue").iter().any(|n| !n.starts_with('.')) {
            return false;
        }
        let Ok(ctx) = Ctx::open(self.repo.path(), devctl_platform::current()) else {
            return false;
        };
        // `<slug>.lock` is a worker lock (`<slug>.queue.lock` is not);
        // `worker_running` takes a branch and slugs it, and a slug is its own
        // slug.
        names("locks")
            .iter()
            .filter(|n| !n.ends_with(".queue.lock"))
            .filter_map(|n| n.strip_suffix(".lock"))
            .all(|slug| matches!(worker::worker_running(&ctx, slug), Ok(false)))
    }

    /// Waits until [`Sample::idle`]; `false` if it never is within the
    /// deadline.
    fn wait_idle(&self) -> bool {
        let start = Instant::now();
        while start.elapsed() < DEADLINE {
            if self.idle() {
                return true;
            }
            std::thread::sleep(POLL);
        }
        false
    }

    /// The store's newest record of `commit`, if any.
    fn has_record(&self, commit: &str) -> bool {
        let id = ObjectId::from_hex(commit.as_bytes()).expect("commit hex");
        RedbStore::new(self.devctl_dir().join("store.redb"))
            .record_for(id.as_bytes())
            .expect("read the store")
            .is_some()
    }
}

impl Drop for Sample {
    fn drop(&mut self) {
        let idle = self.wait_idle();
        if !std::thread::panicking() {
            assert!(
                idle,
                "background workers still busy after {DEADLINE:?}; logs:\n{}",
                self.logs()
            );
        }
    }
}

fn text(out: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&out.stdout),
        String::from_utf8_lossy(&out.stderr)
    )
}

fn short(commit: &str) -> &str {
    &commit[..7]
}

#[test]
fn commit_is_recorded_in_background() {
    let s = Sample::new();
    let sha = s.feature_commit("pass");
    let status = s.wait_status(&format!("{FEATURE}  {}  ok", short(&sha)));
    assert!(
        status.contains("100.0% (new)  tests 2 passed, 0 failed, 0 errored, 0 skipped"),
        "{status}"
    );
    s.wait_log("started worker");
    s.wait_log("job done");
}

#[test]
fn push_blocks_bad_branch_name() {
    let s = Sample::new();
    s.git(&["checkout", "-q", "-b", "wip"]);
    let (ok, out) = s.push(&["wip"]);
    assert!(!ok, "{out}");
    assert!(
        out.contains("BLOCK branch-name: `wip` does not match branches.pattern"),
        "{out}"
    );
    assert!(out.contains("push blocked for wip"), "{out}");
    assert_eq!(s.remote_branch("wip"), None);
}

#[test]
fn push_blocks_force_push_to_dev() {
    let s = Sample::new();
    let base = s.head();
    s.git(&["commit", "-q", "--amend", "-m", "Rewritten base"]);
    let (ok, out) = s.push(&["--force", "dev"]);
    assert!(!ok, "{out}");
    assert!(out.contains("BLOCK no-rewrite"), "{out}");
    assert_eq!(s.remote_branch("dev"), Some(base));
}

#[test]
fn push_blocks_merge_commit_to_dev() {
    let s = Sample::new();
    let base = s.head();
    s.git(&["checkout", "-q", "-b", "feature/side"]);
    s.repo.write("notes/side.txt", "side\n");
    s.commit("Add side notes");
    s.git(&["checkout", "-q", "dev"]);
    s.repo.write("notes/main.txt", "main\n");
    s.commit("Add main notes");
    s.git(&[
        "merge",
        "-q",
        "--no-ff",
        "-m",
        "Merge feature/side",
        "feature/side",
    ]);
    let (ok, out) = s.push(&["dev"]);
    assert!(!ok, "{out}");
    assert!(out.contains("BLOCK linear: merge commits"), "{out}");
    assert_eq!(s.remote_branch("dev"), Some(base));
}

#[test]
fn push_blocks_failed_test() {
    let s = Sample::new();
    s.feature_commit("failed-test");
    let (ok, out) = s.push(&["-u", FEATURE]);
    assert!(!ok, "{out}");
    assert!(out.contains("BLOCK tests: 1 failed, 0 errored"), "{out}");
    assert_eq!(s.remote_branch(FEATURE), None);
}

#[test]
fn push_blocks_low_new_code_coverage() {
    let s = Sample::new();
    s.feature_commit("low-coverage");
    let (ok, out) = s.push(&["-u", FEATURE]);
    assert!(!ok, "{out}");
    assert!(
        out.contains("BLOCK new-code-coverage: 2/4 new lines covered (50.0%, threshold 95%)"),
        "{out}"
    );
    assert!(out.contains("src/lib.txt: 3-4"), "{out}");
    assert_eq!(s.remote_branch(FEATURE), None);
}

#[test]
fn push_blocks_record_error() {
    let s = Sample::new();
    let sha = s.feature_commit("error");
    let (ok, out) = s.push(&["-u", FEATURE]);
    assert!(!ok, "{out}");
    assert!(out.contains("BLOCK record:"), "{out}");
    assert!(
        out.contains("fix the recipe, then run `devctl record`"),
        "{out}"
    );
    assert_eq!(s.remote_branch(FEATURE), None);
    let status = s.devctl(&["status"]);
    assert!(
        status.contains(&format!("{FEATURE}  {}  error", short(&sha))),
        "{status}"
    );
}

#[test]
fn compliant_push_passes() {
    let s = Sample::new();
    let sha = s.feature_commit("pass");
    let (ok, out) = s.push(&["-u", FEATURE]);
    assert!(ok, "{out}");
    assert!(!out.contains("BLOCK"), "{out}");
    assert!(
        out.contains("pass  new-code-coverage: 4/4 new lines covered"),
        "{out}"
    );
    assert_eq!(s.remote_branch(FEATURE), Some(sha.clone()));
    // Fast-forwarding the protected `dev` to it passes too.
    s.git(&["checkout", "-q", "dev"]);
    s.git(&["merge", "-q", "--ff-only", FEATURE]);
    let (ok, out) = s.push(&["dev"]);
    assert!(ok, "{out}");
    assert!(out.contains("pass  no-rewrite"), "{out}");
    assert!(out.contains("pass  linear"), "{out}");
    assert_eq!(s.remote_branch("dev"), Some(sha));
}

#[test]
fn amend_reuses_record() {
    let s = Sample::new();
    let first = s.feature_commit("pass");
    s.wait_status(&format!("{FEATURE}  {}  ok", short(&first)));
    s.git(&["commit", "-q", "--amend", "-m", "Add lines, reworded"]);
    let second = s.head();
    assert_ne!(first, second);
    s.wait_status(&format!("{FEATURE}  {}  ok", short(&second)));
    s.wait_log("reused the record of an identical tree");
    // The coverage recipe ran once: for the first commit only.
    assert!(s.wait_idle(), "workers still busy; logs:\n{}", s.logs());
    s.wait_log("recipe exited");
    let runs = s.logs().matches("recipe exited").count();
    assert_eq!(runs, 1, "{}", s.logs());
}

#[test]
fn second_commit_cancels_first() {
    let s = Sample::new();
    s.git(&["checkout", "-q", "-b", FEATURE]);
    s.repo.write("src/lib.txt", LIB_CHANGED);
    s.use_case("pass");
    // Long enough that only cancellation ends it within the test's deadline.
    s.repo.write("fixture/slow", "30\n");
    let first = s.commit("Add lines slowly");
    // The recipe touches started-<commit> in the measuring worktree just
    // before it sleeps: commit again only once the first run is under way.
    let marker = s
        .devctl_dir()
        .join("measure")
        .join("feature-e2e")
        .join(format!("started-{first}"));
    let start = Instant::now();
    while !marker.exists() {
        assert!(
            start.elapsed() < DEADLINE,
            "the first record never started; logs:\n{}",
            s.logs()
        );
        std::thread::sleep(POLL);
    }
    std::fs::remove_file(s.repo.file("fixture/slow")).expect("remove fixture/slow");
    let second = s.commit("Add lines quickly");
    let status = s.wait_status(&format!("{FEATURE}  {}  ok", short(&second)));
    assert!(!status.contains(short(&first)), "{status}");
    assert!(s.wait_idle(), "workers still busy; logs:\n{}", s.logs());
    assert!(!s.has_record(&first), "the cancelled commit has a record");
    assert!(s.has_record(&second));
    s.wait_log("recipe cancelled");
    s.wait_log("superseded by a newer commit");
}

#[test]
fn oversized_commit_is_blocked() {
    let s = Sample::new();
    s.git(&["checkout", "-q", "-b", FEATURE]);
    let base = s.head();

    // 500 changed lines: the limit is "under 500".
    let big = "line\n".repeat(500);
    s.repo.write("big.txt", big);
    s.git(&["add", "-A"]);
    let out = s.run_git(&["commit", "-q", "-m", "Big"]);
    assert!(!out.status.success(), "{}", text(&out));
    assert!(
        text(&out).contains("BLOCK commit-size: 500 changed lines (must be under 500)"),
        "{}",
        text(&out)
    );
    assert_eq!(s.head(), base);
    s.git(&["rm", "-q", "--cached", "big.txt"]);
    std::fs::remove_file(s.repo.file("big.txt")).expect("remove big.txt");

    // 11 files: the limit is at most 10.
    for i in 0..11 {
        s.repo.write(&format!("many/f{i:02}.txt"), "x\n");
    }
    s.git(&["add", "-A"]);
    let out = s.run_git(&["commit", "-q", "-m", "Many"]);
    assert!(!out.status.success(), "{}", text(&out));
    assert!(
        text(&out).contains("BLOCK commit-size: 11 files (limit 10)"),
        "{}",
        text(&out)
    );
    assert!(text(&out).contains("`git commit --no-verify` bypasses"));
    assert_eq!(s.head(), base);

    // --no-verify bypasses it.
    s.git(&["commit", "-q", "--no-verify", "-m", "Many, bypassed"]);
    assert_ne!(s.head(), base);
}
