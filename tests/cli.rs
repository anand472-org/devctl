mod support;

use assert_cmd::Command;
use predicates::str::contains;
use support::TestRepo;

fn devctl() -> Command {
    let mut cmd = Command::cargo_bin("devctl").expect("devctl binary should build");
    cmd.env("OTEL_SDK_DISABLED", "true");
    cmd
}

#[test]
fn version_subcommand_prints_version() {
    devctl()
        .arg("version")
        .assert()
        .success()
        .stdout(contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn missing_subcommand_fails_with_usage() {
    devctl().assert().failure().stderr(contains("Usage:"));
}

#[test]
fn version_works_outside_a_git_repository() {
    let dir = tempfile::tempdir().unwrap();
    devctl()
        .current_dir(dir.path())
        .env_remove("GIT_DIR")
        .arg("version")
        .assert()
        .success()
        .stdout(contains(env!("CARGO_PKG_VERSION")));
}

#[test]
fn repository_command_outside_a_repository_fails_cleanly() {
    let dir = tempfile::tempdir().unwrap();
    devctl()
        .current_dir(dir.path())
        .env_remove("GIT_DIR")
        .arg("probe")
        .assert()
        .failure()
        .stderr(contains("error: "))
        .stderr(contains("not inside a git repository"))
        .stderr(contains("git init"));
}

#[test]
fn repository_command_writes_its_log_under_the_common_dir() {
    let t = TestRepo::new();
    t.write("f", "1\n");
    t.commit("one");
    devctl()
        .current_dir(t.path())
        .env_remove("GIT_DIR")
        .arg("probe")
        .assert()
        .success();
    let logs = t.path().join(".git").join("devctl").join("logs");
    let names: Vec<String> = std::fs::read_dir(&logs)
        .unwrap()
        .map(|e| e.unwrap().file_name().to_string_lossy().into_owned())
        .collect();
    assert!(
        names.iter().any(|n| {
            n.starts_with("cli.")
                && std::path::Path::new(n)
                    .extension()
                    .is_some_and(|e| e == "log")
        }),
        "{names:?}"
    );
}
