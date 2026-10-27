//! What every repository-bound command needs: the repository, the platform,
//! the policy config and devctl's directories under the git common dir.

use std::path::{Path, PathBuf};

use anyhow::{Context, Result, anyhow};
use devctl_platform::Platform;
use devctl_store::RedbStore;

use crate::config::Config;
use crate::git::{GixRepo, Repo};

/// A branch name made safe for one path component: `/` becomes `-`
/// (`feature/x` -> `feature-x`). `feature/x` and `feature-x` share a slug; that
/// is accepted, since `feature-x` does not match the branch pattern and cannot
/// pass the gate.
#[must_use]
pub fn slug(branch: &str) -> String {
    branch.replace('/', "-")
}

/// devctl's files under `<git common dir>/devctl` (spec section 3.2). Shared by
/// every worktree of the repository.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DevctlDirs {
    pub root: PathBuf,
}

impl DevctlDirs {
    /// `store.redb`: the records.
    #[must_use]
    pub fn store(&self) -> PathBuf {
        self.root.join("store.redb")
    }

    /// `queue/<slug>`: the newest commit waiting to be recorded on `b`.
    #[must_use]
    pub fn queue(&self, b: &str) -> PathBuf {
        self.root.join("queue").join(slug(b))
    }

    /// `locks/<slug>.lock`: the worker lock for `b`.
    #[must_use]
    pub fn lock(&self, b: &str) -> PathBuf {
        self.root.join("locks").join(format!("{}.lock", slug(b)))
    }

    /// `locks/<slug>.queue.lock`: guards read-modify-write of `b`'s queue file.
    #[must_use]
    pub fn queue_lock(&self, b: &str) -> PathBuf {
        self.root
            .join("locks")
            .join(format!("{}.queue.lock", slug(b)))
    }

    /// `measure/<slug>`: the measuring worktree for `b`.
    #[must_use]
    pub fn measure(&self, b: &str) -> PathBuf {
        self.root.join("measure").join(slug(b))
    }

    /// `logs/`: rotated log files.
    #[must_use]
    pub fn logs(&self) -> PathBuf {
        self.root.join("logs")
    }
}

/// The context of a repository-bound command.
pub struct Ctx {
    pub repo: GixRepo,
    pub platform: Box<dyn Platform>,
    pub config: Config,
    pub dirs: DevctlDirs,
}

impl Ctx {
    /// Opens the repository containing `cwd`, loads `.devctl.toml` from this
    /// worktree's top level and creates `<common dir>/devctl` if missing.
    ///
    /// # Errors
    /// Outside a git repository, in a bare repository, on an invalid
    /// `.devctl.toml`, or if the devctl directory cannot be created.
    pub fn open(cwd: &Path, platform: Box<dyn Platform>) -> Result<Ctx> {
        let repo = GixRepo::discover(cwd)?;
        let top = repo.workdir().ok_or_else(|| {
            anyhow!(
                "{} is a bare repository; devctl needs a working tree. Run it from a \
                 checkout (for example one made with `git worktree add`)",
                repo.common_dir().display()
            )
        })?;
        let config = Config::load(top)?;
        let dirs = DevctlDirs {
            root: repo.common_dir().join("devctl"),
        };
        std::fs::create_dir_all(&dirs.root).with_context(|| {
            format!(
                "cannot create {}; check the permissions of {}",
                dirs.root.display(),
                repo.common_dir().display()
            )
        })?;
        Ok(Ctx {
            repo,
            platform,
            config,
            dirs,
        })
    }

    /// The record store at [`DevctlDirs::store`].
    #[must_use]
    pub fn store(&self) -> RedbStore {
        RedbStore::new(self.dirs.store())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestRepo;
    use devctl_platform::fake::FakePlatform;

    #[test]
    fn dirs_follow_spec_layout() {
        let root = Path::new("g").join("devctl");
        let d = DevctlDirs { root: root.clone() };
        assert_eq!(d.store(), root.join("store.redb"));
        assert_eq!(d.queue("feature/x"), root.join("queue").join("feature-x"));
        assert_eq!(
            d.lock("feature/x"),
            root.join("locks").join("feature-x.lock")
        );
        assert_eq!(
            d.queue_lock("feature/x"),
            root.join("locks").join("feature-x.queue.lock")
        );
        assert_eq!(
            d.measure("feature/x"),
            root.join("measure").join("feature-x")
        );
        assert_eq!(d.logs(), root.join("logs"));
    }

    #[test]
    fn open_loads_config_from_worktree_top_and_creates_root() {
        let t = TestRepo::new();
        t.write(".devctl.toml", "[gate]\nnew_code_coverage = 80\n");
        t.write("sub/dir/f", "x\n");
        t.commit("one");
        let ctx = Ctx::open(&t.file("sub/dir"), Box::new(FakePlatform::new())).unwrap();
        assert_eq!(ctx.config.gate.new_code_coverage, 80);
        assert_eq!(ctx.dirs.root, ctx.repo.common_dir().join("devctl"));
        assert!(ctx.dirs.root.is_dir());
    }

    #[test]
    fn open_outside_a_repo_fails() {
        let dir = tempfile::tempdir().unwrap();
        let err = Ctx::open(dir.path(), Box::new(FakePlatform::new()))
            .err()
            .unwrap();
        assert!(
            format!("{err:#}").contains("not inside a git repository"),
            "{err:#}"
        );
    }

    #[test]
    fn open_rejects_invalid_config() {
        let t = TestRepo::new();
        t.write(".devctl.toml", "[gate]\nnew_code_coverage = 500\n");
        let err = Ctx::open(t.path(), Box::new(FakePlatform::new()))
            .err()
            .unwrap();
        assert!(format!("{err:#}").contains("new_code_coverage"), "{err:#}");
    }
}
