//! `devctl hook pre-push` and `devctl gate` as processes, against a local
//! bare remote. Each test feeds git's pre-push lines on stdin.

mod support;

use std::fmt::Write as _;
use std::path::PathBuf;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use assert_cmd::Command;
use devctl::ctx::Ctx;
use devctl::git::ObjectId;
use devctl::record::queue;
use devctl_store::{RedbStore, RetentionPolicy, Store};
use predicates::prelude::PredicateBooleanExt;
use predicates::str::contains;
use support::TestRepo;

const ZERO: &str = "0000000000000000000000000000000000000000";
const JUNIT_PASS: &str =
    r#"<testsuites><testsuite name="s"><testcase name="a"/></testsuite></testsuites>"#;
const JUNIT_FAIL: &str = r#"<testsuites><testsuite name="s"><testcase name="a"/><testcase name="b"><failure message="no"/></testcase></testsuite></testsuites>"#;

/// LCOV for `src/a.rs` with lines `1..=n`, hit unless listed in `missed`.
fn cov(n: u32, missed: &[u32]) -> String {
    let mut s = String::from("SF:src/a.rs\n");
    for l in 1..=n {
        let hits = u32::from(!missed.contains(&l));
        let _ = writeln!(s, "DA:{l},{hits}");
    }
    s.push_str("end_of_record\n");
    s
}

/// A repository whose `dev` (three lines in `src/a.rs`, all covered) is
/// pushed to a bare `origin`. The `coverage` recipe counts its runs in
/// `runs.log`, copies the committed fixtures into place (`JUnit` only if
/// present) and exits with `fixture/exit-code`.
struct Fixture {
    t: TestRepo,
    _remote: tempfile::TempDir,
    url: String,
    base: String,
}

impl Fixture {
    fn new() -> Fixture {
        Fixture::with(|_| {})
    }

    /// `setup` runs before the base commit.
    fn with(setup: impl FnOnce(&TestRepo)) -> Fixture {
        let t = TestRepo::new();
        t.write(
            "justfile",
            "coverage:\n    echo run >> runs.log\n    mkdir -p coverage test-results\n    cp fixture/cov.info coverage/cov.info\n    if [ -f fixture/junit.xml ]; then cp fixture/junit.xml test-results/junit.xml; fi\n    exit \"$(cat fixture/exit-code)\"\n\ntest:\n    true\n",
        );
        t.write("fixture/cov.info", cov(3, &[]));
        t.write("fixture/junit.xml", JUNIT_PASS);
        t.write("fixture/exit-code", "0\n");
        t.write("src/a.rs", "a\nb\nc\n");
        setup(&t);
        let base = t.commit("base");
        let remote = tempfile::tempdir().unwrap();
        t.git_in(remote.path(), &["init", "-q", "--bare"]);
        let url = remote.path().to_string_lossy().into_owned();
        t.git(&["remote", "add", "origin", &url]);
        t.git(&["push", "-q", "-u", "origin", "dev"]);
        Fixture {
            t,
            _remote: remote,
            url,
            base,
        }
    }

    /// Checks out `feature/x` and commits lines 4-6 of `src/a.rs` with
    /// coverage missing `missed`.
    fn feature(&self, missed: &[u32]) -> String {
        self.t.checkout_new("feature/x");
        self.t.write("src/a.rs", "a\nb\nc\nd\ne\nf\n");
        self.t.write("fixture/cov.info", cov(6, missed));
        self.t.commit("feature")
    }

    fn devctl(&self) -> Command {
        let mut cmd = Command::cargo_bin("devctl").expect("devctl binary should build");
        cmd.current_dir(self.t.path())
            .env("OTEL_SDK_DISABLED", "true")
            .timeout(Duration::from_secs(120));
        for var in support::REPO_ENV {
            cmd.env_remove(var);
        }
        cmd
    }

    /// `devctl hook pre-push origin <url>` with `stdin`.
    fn hook(&self, stdin: &str) -> assert_cmd::assert::Assert {
        self.devctl()
            .args(["hook", "pre-push", "origin", &self.url])
            .write_stdin(stdin.to_string())
            .assert()
    }

    fn runs(&self, branch_slug: &str) -> usize {
        std::fs::read_to_string(self.measure(branch_slug).join("runs.log"))
            .map_or(0, |s| s.lines().count())
    }

    fn measure(&self, branch_slug: &str) -> PathBuf {
        self.devctl_dir().join("measure").join(branch_slug)
    }

    fn devctl_dir(&self) -> PathBuf {
        self.t.path().join(".git").join("devctl")
    }
}

fn line(local_ref: &str, local: &str, remote_ref: &str, remote: &str) -> String {
    format!("{local_ref} {local} {remote_ref} {remote}\n")
}

/// A push of `feature/x` at `head` as a new remote branch.
fn new_feature(head: &str) -> String {
    line("refs/heads/feature/x", head, "refs/heads/feature/x", ZERO)
}

fn oid(hex: &str) -> ObjectId {
    ObjectId::from_hex(hex.as_bytes()).unwrap()
}

fn out(a: &assert_cmd::assert::Assert) -> String {
    String::from_utf8_lossy(&a.get_output().stdout).into_owned()
}

#[test]
fn bad_branch_name_blocks() {
    let f = Fixture::new();
    f.t.checkout_new("wip/x");
    let head = f.t.commit("wip");
    f.hook(&line("refs/heads/wip/x", &head, "refs/heads/wip/x", ZERO))
        .failure()
        .stdout(contains("BLOCK branch-name: "))
        .stdout(contains("push blocked for wip/x: "))
        .stderr(contains("push blocked by devctl"));
    assert_eq!(f.runs("wip-x"), 0, "nothing recorded for a bad name");
}

#[test]
fn force_push_to_dev_blocks() {
    let f = Fixture::new();
    f.t.git(&["commit", "-q", "--amend", "--no-verify", "-m", "rewritten"]);
    let rewritten = f.t.git(&["rev-parse", "HEAD"]);
    f.hook(&line(
        "refs/heads/dev",
        &rewritten,
        "refs/heads/dev",
        &f.base,
    ))
    .failure()
    .stdout(contains("BLOCK no-rewrite: "))
    .stdout(contains("pass  linear: "))
    .stdout(contains(
        "push blocked for dev: pull and rebase onto `origin/dev`, then push without --force",
    ));
}

#[test]
fn merge_commit_to_dev_blocks() {
    let f = Fixture::new();
    f.t.checkout_new("feature/y");
    f.t.write("f.txt", "f\n");
    f.t.commit("side");
    f.t.git(&["checkout", "-q", "dev"]);
    f.t.write("g.txt", "g\n");
    f.t.commit("main");
    f.t.merge_no_ff("feature/y");
    let merge = f.t.git(&["rev-parse", "HEAD"]);
    f.hook(&line("refs/heads/dev", &merge, "refs/heads/dev", &f.base))
        .failure()
        .stdout(contains("pass  no-rewrite: "))
        .stdout(contains(format!(
            "BLOCK linear: merge commits {}",
            &merge[..7]
        )))
        .stdout(contains(
            "push blocked for dev: rebase to drop the merge commits",
        ));
}

#[test]
fn deleting_dev_blocks() {
    let f = Fixture::new();
    f.hook(&line("(delete)", ZERO, "refs/heads/dev", &f.base))
        .failure()
        .stdout(contains("BLOCK no-delete: "))
        .stdout(contains("skip  record: "));
}

#[test]
fn deleting_a_feature_branch_passes() {
    let f = Fixture::new();
    f.hook(&line("(delete)", ZERO, "refs/heads/feature/old", &f.base))
        .success()
        .stdout(contains("BLOCK").not());
}

#[test]
fn failed_test_blocks() {
    let f = Fixture::new();
    f.t.checkout_new("feature/x");
    f.t.write("fixture/junit.xml", JUNIT_FAIL);
    f.t.write("fixture/exit-code", "1\n");
    let head = f.t.commit("failing test");
    f.hook(&new_feature(&head))
        .failure()
        .stdout(contains("pass  record: "))
        .stdout(contains("BLOCK tests: 1 failed, 0 errored\n"));
}

#[test]
fn several_refs_get_headers_and_earlier_results_survive_an_error() {
    let f = Fixture::new();
    let head = f.feature(&[]);
    // The second ref pushes a commit this repository does not have to a
    // protected branch, so walking its history fails.
    let missing = "1111111111111111111111111111111111111111";
    let stdin = format!(
        "{}{}",
        new_feature(&head),
        line("refs/heads/dev", missing, "refs/heads/dev", ZERO)
    );
    let a = f.hook(&stdin).failure().stderr(contains(
        "the pre-push gate could not run, so the push is blocked",
    ));
    let text = out(&a);
    assert!(
        text.starts_with("refs/heads/feature/x:\npass  branch-name: "),
        "{text}"
    );
    assert!(text.contains("pass  new-code-coverage: "), "{text}");
    assert!(text.contains("\nrefs/heads/dev:\n"), "{text}");
}

#[test]
fn one_ref_has_no_header() {
    let f = Fixture::new();
    let head = f.feature(&[]);
    let a = f.hook(&new_feature(&head)).success();
    assert!(out(&a).starts_with("pass  branch-name: "), "{}", out(&a));
}

#[test]
fn non_zero_exit_without_failed_test_blocks() {
    let f = Fixture::new();
    f.t.checkout_new("feature/x");
    f.t.write("fixture/exit-code", "1\n");
    let head = f.t.commit("recipe exits 1");
    f.hook(&new_feature(&head)).failure().stdout(contains(
        "BLOCK tests: a recipe exited non-zero with no failed test\n",
    ));
}

#[test]
fn low_new_code_coverage_blocks() {
    let f = Fixture::new();
    let head = f.feature(&[4, 5, 6]);
    f.hook(&new_feature(&head))
        .failure()
        .stdout(contains("BLOCK new-code-coverage: 0/3 new lines covered"))
        .stdout(contains("\n      src/a.rs: 4-6\n"))
        .stdout(contains("push blocked for feature/x: "));
}

#[test]
fn coverage_measured_from_split_point() {
    // `dev` has three uncovered lines; only the feature's lines count.
    let f = Fixture::with(|t| t.write("fixture/cov.info", cov(3, &[1, 2, 3])));
    // `dev` moves on after the split, with more uncovered lines.
    f.t.checkout_new("feature/x");
    f.t.git(&["checkout", "-q", "dev"]);
    f.t.write("src/b.rs", "x\ny\n");
    f.t.commit("more on dev");
    f.t.git(&["push", "-q", "origin", "dev"]);
    f.t.git(&["checkout", "-q", "feature/x"]);
    f.t.write("src/a.rs", "a\nb\nc\nd\ne\nf\n");
    f.t.write("fixture/cov.info", cov(6, &[1, 2, 3]));
    let head = f.t.commit("feature");
    f.hook(&new_feature(&head))
        .success()
        .stdout(contains("pass  new-code-coverage: 3/3 new lines covered"));
}

#[test]
fn record_error_blocks() {
    let f = Fixture::new();
    f.t.checkout_new("feature/x");
    std::fs::remove_file(f.t.file("fixture/junit.xml")).unwrap();
    f.t.write("fixture/exit-code", "1\n");
    let head = f.t.commit("broken recipe");
    f.hook(&new_feature(&head))
        .failure()
        .stdout(contains(
            "BLOCK record: exit \"$(cat fixture/exit-code)\" \u{2014} `just coverage` exited \
             non-zero and wrote no JUnit XML; fix the error above, then run `devctl record`",
        ))
        .stdout(contains("skip  tests: "))
        .stdout(contains(
            "push blocked for feature/x: fix the recipe, then run `devctl record`",
        ));
}

#[test]
fn compliant_push_passes() {
    let f = Fixture::new();
    let head = f.feature(&[]);
    // git may set repository variables for hooks; they must not redirect the
    // gate (or the git and recipe processes it starts) to another repository
    // or index.
    let other = TestRepo::new();
    other.commit("unrelated");
    let a = f
        .devctl()
        .args(["hook", "pre-push", "origin", &f.url])
        .env("GIT_DIR", other.path().join(".git"))
        .env(
            "GIT_INDEX_FILE",
            f.t.path().join("no-such-dir").join("index"),
        )
        .write_stdin(new_feature(&head))
        .assert()
        .success()
        .stdout(contains("BLOCK").not());
    let text = out(&a);
    assert_eq!(text.lines().count(), 7, "{text}");
    assert!(text.contains("pass  branch-name: "), "{text}");
    assert!(text.contains("skip  no-rewrite: "), "{text}");
    assert!(text.contains("pass  tests: "), "{text}");
    assert!(
        text.contains("pass  new-code-coverage: 3/3 new lines covered"),
        "{text}"
    );
    assert_eq!(f.runs("feature-x"), 1);
}

#[test]
fn compliant_push_to_dev_passes() {
    let f = Fixture::new();
    f.t.write("src/a.rs", "a\nb\nc\nd\n");
    f.t.write("fixture/cov.info", cov(4, &[]));
    let head = f.t.commit("on dev");
    f.hook(&line("refs/heads/dev", &head, "refs/heads/dev", &f.base))
        .success()
        .stdout(contains("pass  no-rewrite: "))
        .stdout(contains("pass  linear: "))
        .stdout(contains("pass  new-code-coverage: 1/1 new lines covered"));
}

#[test]
fn tag_push_is_skipped() {
    let f = Fixture::new();
    f.hook(&line("refs/tags/v1", &f.base, "refs/tags/v1", ZERO))
        .success()
        .stdout(contains("skip  branch-name: not a branch\n"))
        .stdout(contains("pass").not());
}

#[test]
fn unknown_prefix_blocks_with_fix() {
    let f = Fixture::with(|t| {
        t.write(
            ".devctl.toml",
            "[branches]\npattern = \"^(feature|wip)/[a-z0-9._-]+$\"\n",
        );
    });
    f.t.checkout_new("wip/x");
    let head = f.t.commit("wip");
    f.hook(&line("refs/heads/wip/x", &head, "refs/heads/wip/x", ZERO))
        .failure()
        .stdout(contains("pass  branch-name: "))
        .stdout(contains("BLOCK new-code-coverage: "))
        .stdout(contains(
            "push blocked for wip/x: add `wip` to branches.targets in .devctl.toml",
        ));
}

#[test]
fn pruned_raw_is_rerecorded() {
    let f = Fixture::new();
    let head = f.feature(&[]);
    f.hook(&new_feature(&head)).success();
    assert_eq!(f.runs("feature-x"), 1);
    // Drop every raw report, keeping the record summaries.
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_millis();
    let store = RedbStore::new(f.devctl_dir().join("store.redb"));
    let stats = store
        .prune(
            &RetentionPolicy {
                records_per_branch: 100,
                raw_reports_days: 0,
            },
            i64::try_from(now).unwrap() + 1000,
        )
        .unwrap();
    assert!(stats.raw_removed > 0);
    f.hook(&new_feature(&head))
        .success()
        .stdout(contains("pass  new-code-coverage: 3/3 new lines covered"));
    assert_eq!(f.runs("feature-x"), 2, "recorded again");
}

#[test]
fn waits_then_runs_when_worker_dies() {
    let f = Fixture::new();
    let head = f.feature(&[]);
    // A job left behind, and a "worker" holding the lock that dies without
    // recording it.
    let ctx = Ctx::open(f.t.path(), devctl_platform::current()).unwrap();
    queue::enqueue(&ctx, "feature/x", oid(&head)).unwrap();
    let lock = devctl_platform::current()
        .try_lock(&ctx.dirs.lock("feature/x"))
        .unwrap()
        .expect("lock is free");
    let release = std::thread::spawn(move || {
        std::thread::sleep(Duration::from_millis(1500));
        drop(lock);
    });
    let a = f.hook(&new_feature(&head));
    release.join().unwrap();
    a.success()
        .stderr(contains(format!(
            "waiting for record of {} (0s)",
            &head[..7]
        )))
        .stdout(contains("pass  record: "));
    assert_eq!(f.runs("feature-x"), 1);
    assert_eq!(queue::peek(&ctx, "feature/x").unwrap(), None);
}

#[test]
fn gate_command_matches_hook() {
    let f = Fixture::new();
    let head = f.feature(&[4]);
    let gate = f.devctl().arg("gate").assert().failure();
    let hook = f.hook(&new_feature(&head)).failure();
    let text = out(&gate);
    assert!(text.contains("\n      src/a.rs: 4\n"), "{text}");
    assert_eq!(text, out(&hook));
    // `--target` names the branch the push would update.
    f.devctl()
        .args(["gate", "--target", "dev"])
        .assert()
        .stdout(contains("pass  branch-name: `dev` is protected"));
}

#[test]
fn hook_fails_closed_on_bad_input() {
    let f = Fixture::new();
    f.hook("refs/heads/dev nothex\n").failure().stderr(contains(
        "the pre-push gate could not run, so the push is blocked",
    ));
    f.devctl()
        .args(["hook", "pre-push"])
        .write_stdin("")
        .assert()
        .failure()
        .stderr(contains("without the remote name"));
}

#[test]
fn lcov_outside_the_worktree_blocks() {
    let f = Fixture::new();
    f.t.checkout_new("feature/x");
    f.t.write("src/a.rs", "a\nb\nc\nd\n");
    // Every SF path points outside the worktree, so all are dropped.
    f.t.write(
        "fixture/cov.info",
        "SF:../outside/a.rs\nDA:1,1\nend_of_record\n",
    );
    let head = f.t.commit("lcov elsewhere");
    f.hook(&new_feature(&head))
        .failure()
        .stdout(contains("pass  record: "))
        .stdout(contains(
            "BLOCK new-code-coverage: the record's LCOV covers no files in this worktree",
        ))
        .stdout(contains("check the `coverage` recipe's SF paths"));
}

#[test]
fn zero_raw_retention_still_passes() {
    let f = Fixture::with(|t| t.write(".devctl.toml", "[retention]\nraw_reports_days = 0\n"));
    let head = f.feature(&[]);
    f.hook(&new_feature(&head))
        .success()
        .stdout(contains("pass  new-code-coverage: 3/3 new lines covered"));
    assert_eq!(f.runs("feature-x"), 1, "recorded once");
}

#[test]
fn push_to_a_url_uses_the_local_target_branch() {
    let f = Fixture::new();
    let head = f.feature(&[]);
    // Only `origin/dev` is left as the target.
    f.t.git(&["branch", "-q", "-D", "dev"]);
    f.hook(&new_feature(&head))
        .success()
        .stdout(contains("pass  new-code-coverage: 3/3 new lines covered"));
    // git passes the URL twice when the push target is a URL; a URL has no
    // remote-tracking refs, so the local `dev` is the target, and it is gone.
    f.devctl()
        .args(["hook", "pre-push", &f.url, &f.url])
        .write_stdin(new_feature(&head))
        .assert()
        .failure()
        .stdout(contains(
            "BLOCK new-code-coverage: target branch `dev` not found",
        ));
}
