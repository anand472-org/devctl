use assert_cmd::Command;
use predicates::str::contains;

fn devctl() -> Command {
    Command::cargo_bin("devctl").expect("devctl binary should build")
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
