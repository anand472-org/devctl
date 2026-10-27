//! Fixture git repositories for tests, built with the `git` CLI.
//!
//! Kept in sync with `src/test_support.rs` (integration tests cannot see
//! `#[cfg(test)]` items of the library, so the helper exists twice).
//!
//! Every git invocation is isolated from the developer's setup: no system or
//! global config, `HOME` pointing at an empty temp dir, repository-selecting
//! environment variables removed, and identity, line-ending and default-branch
//! settings passed on each command.
#![allow(dead_code)] // each test binary uses a different subset

use std::path::{Path, PathBuf};
use std::process::Command;

/// Variables that would make git operate on some other repository or index
/// than the one the test names (they are set when tests run inside a hook).
const REPO_ENV: [&str; 8] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_CEILING_DIRECTORIES",
];

/// A temporary repository, initialised on branch `dev`, deleted on drop.
pub struct TestRepo {
    dir: tempfile::TempDir,
    home: tempfile::TempDir,
}

impl TestRepo {
    /// `git init -b dev` in a fresh temp dir.
    ///
    /// # Panics
    /// If the temp dirs cannot be created or git fails.
    #[must_use]
    pub fn new() -> TestRepo {
        let dir = tempfile::tempdir().expect("create repo temp dir");
        let home = tempfile::tempdir().expect("create HOME temp dir");
        std::fs::write(home.path().join(".gitconfig"), "").expect("write empty global config");
        let repo = TestRepo { dir, home };
        repo.git(&["init", "-q", "-b", "dev"]);
        // Also in the repo config, so in-process readers (gix) see it.
        repo.git(&["config", "core.autocrlf", "false"]);
        repo
    }

    /// The working tree root.
    #[must_use]
    pub fn path(&self) -> &Path {
        self.dir.path()
    }

    /// An isolated `git` command running in `dir`.
    #[must_use]
    pub fn command(&self, dir: &Path) -> Command {
        let mut cmd = Command::new("git");
        for var in REPO_ENV {
            cmd.env_remove(var);
        }
        cmd.current_dir(dir)
            .env("GIT_CONFIG_NOSYSTEM", "1")
            .env("GIT_CONFIG_GLOBAL", self.home.path().join(".gitconfig"))
            .env("HOME", self.home.path())
            .env("XDG_CONFIG_HOME", self.home.path())
            .env("GIT_TERMINAL_PROMPT", "0")
            .args([
                "-c",
                "user.name=t",
                "-c",
                "user.email=t@t",
                "-c",
                "core.autocrlf=false",
                "-c",
                "init.defaultBranch=dev",
                "-c",
                "commit.gpgsign=false",
            ]);
        cmd
    }

    /// Runs git in `dir`, returning trimmed stdout.
    ///
    /// # Panics
    /// If git cannot run or exits non-zero.
    pub fn git_in(&self, dir: &Path, args: &[&str]) -> String {
        let out = self.command(dir).args(args).output().expect("run git");
        assert!(
            out.status.success(),
            "git {args:?} failed: {}",
            String::from_utf8_lossy(&out.stderr)
        );
        String::from_utf8_lossy(&out.stdout).trim().to_string()
    }

    /// Runs git in the repo root, returning trimmed stdout.
    ///
    /// # Panics
    /// If git cannot run or exits non-zero.
    pub fn git(&self, args: &[&str]) -> String {
        self.git_in(self.path(), args)
    }

    /// Writes `contents` to `rel` (`/`-separated), creating parent dirs.
    ///
    /// # Panics
    /// If the file cannot be written.
    pub fn write(&self, rel: &str, contents: impl AsRef<[u8]>) {
        let path = self.file(rel);
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent).expect("create parent dirs");
        }
        std::fs::write(path, contents).expect("write file");
    }

    /// The absolute path of `rel` (`/`-separated) inside the repo.
    #[must_use]
    pub fn file(&self, rel: &str) -> PathBuf {
        rel.split('/')
            .fold(self.path().to_path_buf(), |p, c| p.join(c))
    }

    /// Stages everything (`git add -A`) and commits; returns the commit hex.
    ///
    /// # Panics
    /// If git fails.
    pub fn commit(&self, msg: &str) -> String {
        self.git(&["add", "-A"]);
        self.git(&["commit", "-q", "--allow-empty", "--no-verify", "-m", msg]);
        self.git(&["rev-parse", "HEAD"])
    }

    /// `git checkout -b <branch>`.
    ///
    /// # Panics
    /// If git fails.
    pub fn checkout_new(&self, branch: &str) {
        self.git(&["checkout", "-q", "-b", branch]);
    }

    /// `git merge --no-ff <branch>` into the current branch.
    ///
    /// # Panics
    /// If git fails.
    pub fn merge_no_ff(&self, branch: &str) {
        self.git(&["merge", "-q", "--no-ff", "--no-edit", branch]);
    }
}

impl Default for TestRepo {
    fn default() -> Self {
        Self::new()
    }
}
