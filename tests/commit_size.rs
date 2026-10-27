//! `devctl hook pre-commit` as a process, and through real `git commit` with
//! the hook installed by hand.

mod support;

use std::time::Duration;

use assert_cmd::Command;
use predicates::prelude::PredicateBooleanExt;
use predicates::str::contains;
use support::TestRepo;

/// A repository with one committed file, `base.txt`.
fn repo() -> TestRepo {
    let t = TestRepo::new();
    t.write("base.txt", "base\n");
    t.commit("base");
    t
}

fn devctl(t: &TestRepo) -> Command {
    let mut cmd = Command::cargo_bin("devctl").expect("devctl binary should build");
    cmd.current_dir(t.path())
        .env("OTEL_SDK_DISABLED", "true")
        .timeout(Duration::from_secs(60));
    for var in support::REPO_ENV {
        cmd.env_remove(var);
    }
    cmd
}

fn hook(t: &TestRepo) -> assert_cmd::assert::Assert {
    devctl(t).args(["hook", "pre-commit"]).assert()
}

fn stage_files(t: &TestRepo, n: usize) {
    for i in 0..n {
        t.write(&format!("f{i:02}.txt"), "x\n");
    }
    t.git(&["add", "-A"]);
}

fn stage_lines(t: &TestRepo, name: &str, n: usize) {
    t.write(name, "line\n".repeat(n));
    t.git(&["add", "-A"]);
}

#[test]
fn ten_files_allowed_eleven_blocked() {
    let t = repo();
    stage_files(&t, 10);
    hook(&t).success().stdout("").stderr("");
    t.write("f10.txt", "x\n");
    t.git(&["add", "-A"]);
    hook(&t)
        .failure()
        .stderr(contains("BLOCK commit-size: 11 files (limit 10)\n"))
        .stderr(contains("      f00.txt: +1 -0\n"))
        .stderr(contains(
            "commit blocked: split this into smaller commits (`git commit --no-verify` bypasses)",
        ))
        .stderr(contains("lines (must").not());
}

#[test]
fn gross_change_counts_add_plus_remove() {
    let t = repo();
    t.write("big.txt", "old\n".repeat(200));
    t.commit("big");
    // Replace 200 lines with 300: +300 -200 = 500.
    t.write("big.txt", "new\n".repeat(300));
    t.git(&["add", "-A"]);
    hook(&t)
        .failure()
        .stderr(contains(
            "BLOCK commit-size: 500 changed lines (must be under 500)\n",
        ))
        .stderr(contains("      big.txt: +300 -200\n"))
        .stderr(contains("files (limit").not());
    // +300 -199 = 499.
    t.write("big.txt", format!("{}{}", "new\n".repeat(300), "old\n"));
    t.git(&["add", "-A"]);
    hook(&t).success().stderr("");
}

#[test]
fn binary_counts_as_file_with_zero_lines() {
    let t = repo();
    t.write(".devctl.toml", "[commit]\nmax_files = 1\n");
    t.git(&["add", "-A"]);
    t.git(&["commit", "-q", "--no-verify", "-m", "cfg"]);
    t.write("a.bin", b"\0\x01\x02");
    t.write("b.bin", b"\0\x03\x04");
    t.git(&["add", "-A"]);
    hook(&t)
        .failure()
        .stderr(contains("BLOCK commit-size: 2 files (limit 1)\n"))
        .stderr(contains("      a.bin: binary\n"));
}

#[test]
fn rename_counts_once() {
    let t = repo();
    t.write(".devctl.toml", "[commit]\nmax_files = 1\n");
    t.write("old.txt", "one\ntwo\nthree\nfour\n");
    t.commit("old");
    t.git(&["mv", "old.txt", "new.txt"]);
    hook(&t).success().stderr("");
}

#[test]
fn first_commit_compared_to_empty_tree() {
    let t = TestRepo::new();
    stage_files(&t, 11);
    hook(&t)
        .failure()
        .stderr(contains("BLOCK commit-size: 11 files (limit 10)"));
}

#[test]
fn merge_in_progress_is_skipped() {
    let t = repo();
    t.checkout_new("feature/x");
    t.write("f.txt", "feature\n");
    t.commit("feat");
    t.git(&["checkout", "-q", "dev"]);
    t.write("f.txt", "dev\n");
    t.commit("dev");
    let out = t
        .command(t.path())
        .args(["merge", "-q", "feature/x"])
        .output()
        .unwrap();
    assert!(!out.status.success(), "merge should conflict");
    stage_files(&t, 11);
    hook(&t).success().stdout("").stderr("");
}

#[test]
fn limits_come_from_config() {
    let t = repo();
    t.write(".devctl.toml", "[commit]\nmax_files = 2\nmax_lines = 3\n");
    t.git(&["add", "-A"]);
    t.git(&["commit", "-q", "--no-verify", "-m", "cfg"]);
    stage_files(&t, 3);
    hook(&t)
        .failure()
        .stderr(contains("BLOCK commit-size: 3 files (limit 2)"));
    t.git(&["reset", "-q"]);
    t.git(&["clean", "-fdq"]);
    stage_lines(&t, "a.txt", 3);
    hook(&t)
        .failure()
        .stderr(contains("3 changed lines (must be under 3)"));
}

#[test]
fn lists_only_five_largest_files() {
    let t = repo();
    t.write(".devctl.toml", "[commit]\nmax_files = 6\n");
    for i in 1..=7 {
        t.write(&format!("f{i}.txt"), "x\n".repeat(i * 10));
    }
    t.git(&["add", "-A"]);
    let out = hook(&t).failure();
    let err = String::from_utf8_lossy(&out.get_output().stderr).into_owned();
    let listed: Vec<&str> = err.lines().filter(|l| l.starts_with("      ")).collect();
    assert_eq!(
        listed,
        [
            "      f7.txt: +70 -0",
            "      f6.txt: +60 -0",
            "      f5.txt: +50 -0",
            "      f4.txt: +40 -0",
            "      f3.txt: +30 -0",
        ]
    );
}

#[test]
fn fails_closed_on_invalid_config() {
    let t = repo();
    t.write(".devctl.toml", "[commit]\nmax_files = 0\n");
    hook(&t).failure().stderr(contains("commit.max_files"));
}

#[test]
fn fails_closed_outside_a_repository() {
    let dir = tempfile::tempdir().unwrap();
    let mut cmd = Command::cargo_bin("devctl").unwrap();
    cmd.current_dir(dir.path())
        .env("OTEL_SDK_DISABLED", "true")
        .env("GIT_CEILING_DIRECTORIES", dir.path().parent().unwrap())
        .args(["hook", "pre-commit"]);
    for var in support::REPO_ENV {
        cmd.env_remove(var);
    }
    cmd.assert()
        .failure()
        .stderr(contains("the commit is blocked"));
}

mod real_git {
    use super::{TestRepo, repo, stage_files};

    fn install_hook(t: &TestRepo) {
        let bin = env!("CARGO_BIN_EXE_devctl");
        let path = t.path().join(".git").join("hooks").join("pre-commit");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(
            &path,
            format!("#!/bin/sh\nOTEL_SDK_DISABLED=true exec '{bin}' hook pre-commit\n"),
        )
        .unwrap();
        // A no-op on Windows: Git for Windows runs hooks through its shell
        // and ignores the mode.
        devctl_platform::current()
            .make_executable(&path)
            .expect("make the hook executable");
    }

    fn commit(t: &TestRepo, args: &[&str]) -> std::process::Output {
        t.command(t.path())
            .args(["commit", "-q", "-m", "m"])
            .args(args)
            .output()
            .unwrap()
    }

    #[test]
    fn plain_commit_is_blocked_and_allowed_commit_is_quiet() {
        let t = repo();
        install_hook(&t);
        stage_files(&t, 11);
        let out = commit(&t, &[]);
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains("BLOCK commit-size: 11 files"));
        t.git(&["reset", "-q"]);
        t.write("one.txt", "x\n");
        t.git(&["add", "one.txt"]);
        let out = commit(&t, &[]);
        assert!(out.status.success());
        assert!(out.stdout.is_empty() && out.stderr.is_empty());
    }

    #[test]
    fn commit_paths_checks_the_temporary_index_not_the_real_one() {
        let t = repo();
        install_hook(&t);
        // 11 files are staged in the real index, but only one path is committed.
        stage_files(&t, 11);
        let out = commit(&t, &["f00.txt"]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn amend_measures_the_index_against_head() {
        let t = repo();
        install_hook(&t);
        stage_files(&t, 11);
        t.git(&["commit", "-q", "--no-verify", "-m", "big"]);
        // Nothing newly staged: rewording an 11-file commit is allowed.
        let out = commit(&t, &["--amend"]);
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
        // Newly staged changes are what count, not the amended commit's own.
        for i in 0..11 {
            t.write(&format!("g{i:02}.txt"), "x\n");
        }
        t.git(&["add", "-A"]);
        let out = commit(&t, &["--amend"]);
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains("BLOCK commit-size: 11 files"));
    }

    #[test]
    fn commit_all_checks_what_it_will_commit() {
        let t = repo();
        install_hook(&t);
        for i in 0..11 {
            t.write(&format!("t{i:02}.txt"), "1\n");
        }
        t.git(&["add", "-A"]);
        t.git(&["commit", "-q", "--no-verify", "-m", "tracked"]);
        for i in 0..11 {
            t.write(&format!("t{i:02}.txt"), "2\n");
        }
        // Nothing is staged; `-a` stages the 11 changes into a temporary index.
        let out = commit(&t, &["-a"]);
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains("BLOCK commit-size: 11 files"));
    }
}
