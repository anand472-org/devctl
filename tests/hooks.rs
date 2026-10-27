//! `devctl hook post-commit` as a process: it must exit 0 whatever happens.
//! Only paths that start no worker are run here (the unit tests cover the
//! spawn with a fake platform).

mod support;

use std::time::Duration;

use assert_cmd::Command;
use support::TestRepo;

fn post_commit(dir: &std::path::Path) -> assert_cmd::assert::Assert {
    let mut cmd = Command::cargo_bin("devctl").expect("devctl binary should build");
    cmd.current_dir(dir)
        .env("OTEL_SDK_DISABLED", "true")
        .timeout(Duration::from_secs(60));
    for var in support::REPO_ENV {
        cmd.env_remove(var);
    }
    cmd.args(["hook", "post-commit"]).assert()
}

fn log_text(t: &TestRepo) -> String {
    let dir = t.path().join(".git").join("devctl").join("logs");
    std::fs::read_dir(dir)
        .map(|entries| {
            entries
                .flatten()
                .filter_map(|e| std::fs::read_to_string(e.path()).ok())
                .collect()
        })
        .unwrap_or_default()
}

#[test]
fn post_commit_detached_is_noop_and_exits_zero() {
    let t = TestRepo::new();
    let sha = t.commit("one");
    t.git(&["checkout", "-q", "--detach", &sha]);
    post_commit(t.path()).success().stdout("");
    assert!(!t.path().join(".git/devctl/queue").exists());
}

#[test]
fn post_commit_never_fails() {
    let t = TestRepo::new();
    t.commit("one");
    // A corrupt store (post-commit never opens it), and a queue "directory"
    // that is a file, which is what makes the enqueue fail.
    let root = t.path().join(".git").join("devctl");
    std::fs::create_dir_all(&root).unwrap();
    std::fs::write(root.join("store.redb"), "not a database").unwrap();
    std::fs::write(root.join("queue"), "in the way").unwrap();
    post_commit(t.path()).success();
    let log = log_text(&t);
    assert!(log.contains("cannot write the record queue"), "{log}");
}

#[test]
fn installed_pre_commit_is_executable_under_real_git() {
    let t = TestRepo::new();
    t.write("base.txt", "base\n");
    t.commit("base");
    let bin = std::path::Path::new(env!("CARGO_BIN_EXE_devctl"));
    let mut cmd = Command::cargo_bin("devctl").unwrap();
    cmd.current_dir(t.path())
        .env("OTEL_SDK_DISABLED", "true")
        .args(["hooks", "install"]);
    for var in support::REPO_ENV {
        cmd.env_remove(var);
    }
    cmd.assert().success();
    // 11 staged files: if the installed pre-commit is executable and finds
    // `devctl` on PATH, git blocks the commit (so post-commit never runs).
    for i in 0..11 {
        t.write(&format!("f{i:02}.txt"), "x\n");
    }
    t.git(&["add", "-A"]);
    let old = std::env::var_os("PATH").unwrap_or_default();
    let path = std::env::join_paths(
        std::iter::once(bin.parent().unwrap().to_path_buf()).chain(std::env::split_paths(&old)),
    )
    .unwrap();
    let out = t
        .command(t.path())
        .env("PATH", path)
        .env("OTEL_SDK_DISABLED", "true")
        .args(["commit", "-q", "-m", "m"])
        .output()
        .unwrap();
    assert!(!out.status.success());
    assert!(String::from_utf8_lossy(&out.stderr).contains("BLOCK commit-size: 11 files"));
}

#[test]
fn post_commit_outside_a_repo_exits_zero() {
    let dir = tempfile::tempdir().unwrap();
    post_commit(dir.path())
        .success()
        .stderr(predicates::str::contains("not inside a git repository"));
}

#[test]
fn post_commit_with_invalid_config_exits_zero() {
    let t = TestRepo::new();
    t.commit("one");
    t.write(".devctl.toml", "[gate]\nnew_code_coverage = 500\n");
    post_commit(t.path()).success();
}

#[test]
fn unknown_hook_name_is_a_usage_error() {
    let t = TestRepo::new();
    Command::cargo_bin("devctl")
        .unwrap()
        .current_dir(t.path())
        .args(["hook", "bogus"])
        .assert()
        .failure();
}

#[test]
fn post_commit_does_not_wait_for_a_dead_collector() {
    let t = TestRepo::new();
    let sha = t.commit("one");
    t.git(&["checkout", "-q", "--detach", &sha]);
    // A listener that accepts and never answers.
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let endpoint = format!("http://{}", listener.local_addr().unwrap());
    let start = std::time::Instant::now();
    let mut cmd = Command::cargo_bin("devctl").unwrap();
    cmd.current_dir(t.path())
        .env("OTEL_EXPORTER_OTLP_ENDPOINT", &endpoint)
        .timeout(Duration::from_secs(30));
    for var in support::REPO_ENV {
        cmd.env_remove(var);
    }
    cmd.args(["hook", "post-commit"]).assert().success();
    assert!(
        start.elapsed() < Duration::from_secs(1),
        "took {:?}",
        start.elapsed()
    );
    drop(listener);
}

#[test]
fn install_repairs_crlf_and_rejects_a_hooks_file() {
    let t = TestRepo::new();
    let hooks = t.path().join(".git").join("hooks");
    std::fs::create_dir_all(&hooks).unwrap();
    std::fs::write(
        hooks.join("pre-push"),
        "#!/bin/sh\r\nexec devctl hook pre-push \"$@\"\r\n",
    )
    .unwrap();
    let install = || {
        let mut cmd = Command::cargo_bin("devctl").unwrap();
        cmd.current_dir(t.path()).env("OTEL_SDK_DISABLED", "true");
        for var in support::REPO_ENV {
            cmd.env_remove(var);
        }
        cmd.args(["hooks", "install"]);
        cmd
    };
    install().assert().success();
    let text = std::fs::read_to_string(hooks.join("pre-push")).unwrap();
    assert_eq!(text, "#!/bin/sh\nexec devctl hook pre-push \"$@\"\n");
    std::fs::remove_dir_all(&hooks).unwrap();
    std::fs::write(&hooks, "file").unwrap();
    install()
        .assert()
        .failure()
        .stderr(predicates::str::contains("is not a directory; remove it"));
}
