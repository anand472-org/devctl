//! Measuring worktrees (spec section 3.2): one hidden, detached worktree per
//! branch at `.git/devctl/measure/<branch>`, reused across records so build
//! caches survive.

use std::path::{Component, Path, PathBuf};

use anyhow::{Context, Result, anyhow};

use crate::ctx::Ctx;
use crate::git::{ObjectId, Repo, git};

/// One entry of `git worktree list --porcelain`.
struct Listed {
    path: String,
    bare: bool,
}

fn list(dir: &Path) -> Result<Vec<Listed>> {
    let out = git(dir, &["worktree", "list", "--porcelain"])?;
    let mut entries: Vec<Listed> = Vec::new();
    for line in out.lines() {
        let line = line.trim_end_matches('\r');
        if let Some(path) = line.strip_prefix("worktree ") {
            entries.push(Listed {
                path: path.to_string(),
                bare: false,
            });
        } else if line == "bare"
            && let Some(last) = entries.last_mut()
        {
            last.bare = true;
        }
    }
    Ok(entries)
}

/// `p` normalized by the platform (absolute, `/`, symlinks resolved), also
/// when `p` no longer exists: its nearest existing ancestor is resolved and
/// the rest appended.
fn normalized(ctx: &Ctx, p: &Path) -> String {
    let mut base = p;
    let mut rest: Vec<String> = Vec::new();
    while !base.exists() {
        match (base.parent(), base.components().next_back()) {
            (Some(parent), Some(Component::Normal(name))) => {
                rest.push(name.to_string_lossy().into_owned());
                base = parent;
            }
            _ => break,
        }
    }
    let mut out = ctx.platform.normalize_path(base);
    for name in rest.iter().rev() {
        if !out.ends_with('/') {
            out.push('/');
        }
        out.push_str(name);
    }
    out
}

/// Where to run `git worktree` commands: the main worktree's top level, or
/// this worktree's if the main one is bare.
fn main_top(here: &Path) -> Result<PathBuf> {
    Ok(list(here)?
        .into_iter()
        .next()
        .filter(|w| !w.bare && Path::new(&w.path).is_dir())
        .map_or_else(|| here.to_path_buf(), |w| PathBuf::from(w.path)))
}

fn is_registered(ctx: &Ctx, top: &Path, path: &Path) -> Result<bool> {
    let want = normalized(ctx, path);
    Ok(list(top)?
        .iter()
        .any(|w| normalized(ctx, Path::new(&w.path)) == want))
}

fn utf8(path: &Path) -> Result<&str> {
    path.to_str().ok_or_else(|| {
        anyhow!(
            "the measuring worktree path {} is not valid UTF-8; move the repository to a UTF-8 path",
            path.display()
        )
    })
}

/// `core.hooksPath=<devctl dir>/no-hooks`, a directory that does not exist:
/// the developer's hooks (e.g. a failing `post-checkout`) must not run, or
/// fail, in the measuring worktree.
fn no_hooks(ctx: &Ctx) -> Result<String> {
    let dir = ctx.dirs.root.join("no-hooks");
    Ok(format!("core.hooksPath={}", utf8(&dir)?))
}

fn checkout(ctx: &Ctx, path: &Path, hex: &str) -> Result<()> {
    let hooks = no_hooks(ctx)?;
    git(
        path,
        &[
            "-c",
            "core.longpaths=true",
            "-c",
            &hooks,
            "checkout",
            "-q",
            "--detach",
            "--force",
            hex,
        ],
    )
    .map(drop)
}

fn add(ctx: &Ctx, top: &Path, path: &Path, hex: &str) -> Result<()> {
    let target = utf8(path)?;
    let hooks = no_hooks(ctx)?;
    let args = [
        "-c",
        "core.longpaths=true",
        "-c",
        &hooks,
        "worktree",
        "add",
        "--detach",
        target,
        hex,
    ];
    if is_registered(ctx, top, path)? {
        // Registered but its directory is gone: a stale entry that would
        // make `worktree add` refuse.
        tracing::info!(path = %path.display(), "pruning a stale measuring worktree registration");
        git(top, &["worktree", "prune"])?;
    }
    match git(top, &args) {
        Ok(_) => Ok(()),
        Err(e) if format!("{e:#}").contains("already registered") => {
            git(top, &["worktree", "prune"])?;
            git(top, &args).map(drop)
        }
        Err(e) => Err(e),
    }
}

/// Names the measuring worktree's `index.lock` (left by a crashed or still
/// running git) and how to clear it. It is never deleted automatically.
fn index_lock_message(path: &Path) -> String {
    let lock = git(
        path,
        &[
            "rev-parse",
            "--path-format=absolute",
            "--git-path",
            "index.lock",
        ],
    )
    .map_or_else(
        |_| "its index.lock".to_string(),
        |out| out.trim().to_string(),
    );
    format!(
        "cannot check out in the measuring worktree {}: {lock} exists (another git \
         process is using it, or one crashed); delete it if no git process is running, \
         then run `devctl record`",
        path.display()
    )
}

fn remove_dir(path: &Path) -> Result<()> {
    std::fs::remove_dir_all(path).with_context(|| {
        format!(
            "cannot remove the broken measuring worktree {}; delete it, then run `devctl record`",
            path.display()
        )
    })
}

/// Checks `commit` out in `branch`'s measuring worktree, creating it on first
/// use, and returns its path. Untracked build output is kept.
///
/// # Errors
/// If git fails to create the worktree or check out the commit.
pub fn prepare(ctx: &Ctx, branch: &str, commit: ObjectId) -> Result<PathBuf> {
    let _span = tracing::info_span!("measure.prepare", branch).entered();
    let path = ctx.dirs.measure(branch);
    let hex = commit.to_string();
    let here = ctx
        .repo
        .workdir()
        .ok_or_else(|| anyhow!("the repository has no working tree"))?;
    let failed = || {
        format!(
            "cannot check out {} in the measuring worktree {}; run `git worktree prune`, then `devctl record`",
            &hex[..hex.len().min(7)],
            path.display()
        )
    };
    let top = main_top(here).with_context(failed)?;

    if path.join(".git").is_file() {
        match checkout(ctx, &path, &hex) {
            Ok(()) => return Ok(path),
            // A real checkout failure in a healthy worktree.
            Err(e) if is_registered(ctx, &top, &path).with_context(failed)? => {
                if format!("{e:#}").contains("index.lock") {
                    return Err(e).with_context(|| index_lock_message(&path));
                }
                return Err(e).with_context(failed);
            }
            Err(e) => {
                tracing::warn!(error = %format!("{e:#}"), "measuring worktree is no longer registered; recreating it");
                remove_dir(&path)?;
            }
        }
    } else if path.exists() {
        tracing::warn!(path = %path.display(), "removing a leftover directory that is not a worktree");
        remove_dir(&path)?;
    }
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).with_context(failed)?;
    }
    add(ctx, &top, &path, &hex).with_context(failed)?;
    Ok(path)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::Repo;
    use crate::test_support::TestRepo;
    use devctl_platform::fake::FakePlatform;

    fn oid(hex: &str) -> ObjectId {
        ObjectId::from_hex(hex.as_bytes()).unwrap()
    }

    fn head_of(t: &TestRepo, dir: &std::path::Path) -> String {
        t.git_in(dir, &["rev-parse", "HEAD"])
    }

    #[test]
    fn creates_then_reuses_keeping_untracked_files() {
        let t = TestRepo::new();
        t.write("f", "1\n");
        let a = t.commit("a");
        t.write("f", "2\n");
        let b = t.commit("b");
        let ctx = Ctx::open(t.path(), Box::new(FakePlatform::new())).unwrap();

        let path = prepare(&ctx, "feature/x", oid(&a)).unwrap();
        assert_eq!(path, ctx.dirs.measure("feature/x"));
        assert_eq!(head_of(&t, &path), a);
        assert_eq!(std::fs::read_to_string(path.join("f")).unwrap(), "1\n");
        std::fs::write(path.join("build-cache"), "x").unwrap();
        std::fs::write(path.join("f"), "dirty\n").unwrap();

        let again = prepare(&ctx, "feature/x", oid(&b)).unwrap();
        assert_eq!(again, path);
        assert_eq!(head_of(&t, &path), b);
        assert_eq!(std::fs::read_to_string(path.join("f")).unwrap(), "2\n");
        assert!(path.join("build-cache").exists(), "untracked output kept");
        // The developer's checkout is untouched.
        assert_eq!(ctx.repo.current_branch().unwrap().as_deref(), Some("dev"));
    }

    #[test]
    fn recovers_from_a_deleted_worktree() {
        let t = TestRepo::new();
        let a = t.commit("a");
        let ctx = Ctx::open(t.path(), Box::new(FakePlatform::new())).unwrap();
        let path = prepare(&ctx, "dev", oid(&a)).unwrap();
        std::fs::remove_dir_all(&path).unwrap();
        // The registration is now stale; `worktree add` alone would refuse.
        let path = prepare(&ctx, "dev", oid(&a)).unwrap();
        assert_eq!(head_of(&t, &path), a);
    }

    #[test]
    fn recovers_from_a_pruned_registration() {
        let t = TestRepo::new();
        let a = t.commit("a");
        let ctx = Ctx::open(t.path(), Box::new(FakePlatform::new())).unwrap();
        let path = prepare(&ctx, "dev", oid(&a)).unwrap();
        std::fs::write(path.join("cache"), "x").unwrap();
        // Admin dir gone (e.g. `git worktree prune` while the dir was
        // unreadable): the directory's `.git` file now points nowhere.
        let admin = t.path().join(".git").join("worktrees");
        std::fs::remove_dir_all(&admin).unwrap();
        let path = prepare(&ctx, "dev", oid(&a)).unwrap();
        assert_eq!(head_of(&t, &path), a);
    }

    #[test]
    fn failing_post_checkout_hook_does_not_run() {
        let t = TestRepo::new();
        t.write("f", "1\n");
        let a = t.commit("a");
        t.write("f", "2\n");
        let b = t.commit("b");
        let hook = t.path().join(".git").join("hooks").join("post-checkout");
        std::fs::create_dir_all(hook.parent().unwrap()).unwrap();
        std::fs::write(&hook, "#!/bin/sh\nexit 1\n").unwrap();
        devctl_platform::current().make_executable(&hook).unwrap();
        let ctx = Ctx::open(t.path(), Box::new(FakePlatform::new())).unwrap();
        let path = prepare(&ctx, "dev", oid(&a)).unwrap(); // worktree add
        prepare(&ctx, "dev", oid(&b)).unwrap(); // checkout
        assert_eq!(head_of(&t, &path), b);
    }

    #[test]
    fn stale_index_lock_is_named_in_the_error() {
        let t = TestRepo::new();
        t.write("f", "1\n");
        let a = t.commit("a");
        t.write("f", "2\n");
        let b = t.commit("b");
        let ctx = Ctx::open(t.path(), Box::new(FakePlatform::new())).unwrap();
        let path = prepare(&ctx, "dev", oid(&a)).unwrap();
        let lock = t.git_in(
            &path,
            &[
                "rev-parse",
                "--path-format=absolute",
                "--git-path",
                "index.lock",
            ],
        );
        std::fs::write(&lock, "").unwrap();
        let err = format!("{:#}", prepare(&ctx, "dev", oid(&b)).unwrap_err());
        assert!(err.contains(&lock), "{err}");
        assert!(
            err.contains("delete it if no git process is running"),
            "{err}"
        );
        assert!(Path::new(&lock).exists(), "not deleted automatically");
    }

    #[test]
    fn worktree_is_registered_under_its_slug() {
        let t = TestRepo::new();
        let a = t.commit("a");
        let ctx = Ctx::open(t.path(), Box::new(FakePlatform::new())).unwrap();
        prepare(&ctx, "feature/x", oid(&a)).unwrap();
        let list = t.git(&["worktree", "list", "--porcelain"]);
        assert!(
            list.lines()
                .any(|l| l.starts_with("worktree ") && l.ends_with("feature-x")),
            "{list}"
        );
    }
}
