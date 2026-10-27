//! The per-branch job queue (spec section 4.1): `.git/devctl/queue/<branch>`
//! holds the newest commit waiting to be recorded, as hex plus `\n`. Writes
//! and the compare-and-delete in [`complete`] happen under
//! [`DevctlDirs::queue_lock`](crate::ctx::DevctlDirs::queue_lock); reads do
//! not lock, so writes go through a temporary file and a rename.

use std::io::ErrorKind;
use std::path::Path;

use anyhow::{Context, Result};
use devctl_platform::Lock;

use crate::ctx::{Ctx, slug};
use crate::git::ObjectId;

/// What a queue file holds.
enum Job {
    Empty,
    Commit(ObjectId),
    /// Not a commit ID; the raw text, for the log.
    Garbage(String),
}

fn read(path: &Path) -> Result<Job> {
    let text = match std::fs::read(path) {
        Ok(bytes) => String::from_utf8_lossy(&bytes).into_owned(),
        Err(e) if e.kind() == ErrorKind::NotFound => return Ok(Job::Empty),
        Err(e) => {
            return Err(e).with_context(|| {
                format!(
                    "cannot read the record queue {}; check its permissions, or delete it and run `devctl record`",
                    path.display()
                )
            });
        }
    };
    Ok(match ObjectId::from_hex(text.trim().as_bytes()) {
        Ok(id) => Job::Commit(id),
        Err(_) => Job::Garbage(text),
    })
}

fn lock(ctx: &Ctx, branch: &str) -> Result<Lock> {
    let path = ctx.dirs.queue_lock(branch);
    ctx.platform.lock(&path).with_context(|| {
        format!(
            "cannot lock {}; check the permissions of {}",
            path.display(),
            ctx.dirs.root.display()
        )
    })
}

fn remove(path: &Path) -> Result<()> {
    match std::fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| {
            format!(
                "cannot remove the record queue {}; check its permissions",
                path.display()
            )
        }),
    }
}

/// Queues `commit` on `branch`, replacing any older job.
///
/// # Errors
/// If the queue lock or file cannot be written.
pub fn enqueue(ctx: &Ctx, branch: &str, commit: ObjectId) -> Result<()> {
    let path = ctx.dirs.queue(branch);
    let dir = path.parent().unwrap_or(&ctx.dirs.root);
    let _held = lock(ctx, branch)?;
    let unwritable = || {
        format!(
            "cannot write the record queue {}; check the permissions of {}",
            path.display(),
            dir.display()
        )
    };
    std::fs::create_dir_all(dir).with_context(unwritable)?;
    // Readers do not lock, so they must never see a half-written file. Git
    // ref names cannot start with `.`, so this name never collides.
    let tmp = dir.join(format!(".{}.tmp", slug(branch)));
    std::fs::write(&tmp, format!("{commit}\n")).with_context(unwritable)?;
    std::fs::rename(&tmp, &path).with_context(unwritable)?;
    tracing::debug!(branch, %commit, "queued");
    Ok(())
}

/// The commit queued on `branch`, if any. An unparsable queue file is logged
/// and removed.
///
/// # Errors
/// If the queue file exists but cannot be read.
pub fn peek(ctx: &Ctx, branch: &str) -> Result<Option<ObjectId>> {
    let path = ctx.dirs.queue(branch);
    match read(&path)? {
        Job::Empty => Ok(None),
        Job::Commit(id) => Ok(Some(id)),
        Job::Garbage(_) => {
            // Re-check under the lock, so a job queued meanwhile survives.
            let _held = lock(ctx, branch)?;
            match read(&path)? {
                Job::Empty => Ok(None),
                Job::Commit(id) => Ok(Some(id)),
                Job::Garbage(text) => {
                    tracing::warn!(
                        branch,
                        contents = %text.chars().take(100).collect::<String>(),
                        "removing an unparsable record queue file"
                    );
                    remove(&path)?;
                    Ok(None)
                }
            }
        }
    }
}

/// Removes the job for `branch` if it is still `commit`; returns whether it
/// did.
///
/// # Errors
/// If the queue lock cannot be taken or the file cannot be read or removed.
pub fn complete(ctx: &Ctx, branch: &str, commit: ObjectId) -> Result<bool> {
    let path = ctx.dirs.queue(branch);
    let _held = lock(ctx, branch)?;
    match read(&path)? {
        Job::Commit(id) if id == commit => {
            remove(&path)?;
            Ok(true)
        }
        _ => Ok(false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::test_support::TestRepo;
    use devctl_platform::fake::FakePlatform;

    fn setup() -> (TestRepo, Ctx, ObjectId, ObjectId) {
        let t = TestRepo::new();
        let a = t.commit("a");
        let b = t.commit("b");
        let ctx = Ctx::open(t.path(), Box::new(FakePlatform::new())).unwrap();
        (
            t,
            ctx,
            ObjectId::from_hex(a.as_bytes()).unwrap(),
            ObjectId::from_hex(b.as_bytes()).unwrap(),
        )
    }

    #[test]
    fn enqueue_overwrites_and_peek_reads() {
        let (_t, ctx, a, b) = setup();
        assert_eq!(peek(&ctx, "feature/x").unwrap(), None);
        enqueue(&ctx, "feature/x", a).unwrap();
        assert_eq!(peek(&ctx, "feature/x").unwrap(), Some(a));
        enqueue(&ctx, "feature/x", b).unwrap();
        assert_eq!(peek(&ctx, "feature/x").unwrap(), Some(b));
        let text = std::fs::read_to_string(ctx.dirs.queue("feature/x")).unwrap();
        assert_eq!(text, format!("{b}\n"), "LF-terminated hex");
        assert_eq!(peek(&ctx, "dev").unwrap(), None, "per-branch");
    }

    #[test]
    fn complete_deletes_only_the_same_commit() {
        let (_t, ctx, a, b) = setup();
        enqueue(&ctx, "dev", b).unwrap();
        assert!(!complete(&ctx, "dev", a).unwrap());
        assert_eq!(peek(&ctx, "dev").unwrap(), Some(b));
        assert!(complete(&ctx, "dev", b).unwrap());
        assert_eq!(peek(&ctx, "dev").unwrap(), None);
        assert!(!complete(&ctx, "dev", b).unwrap(), "nothing queued");
    }

    #[test]
    fn peek_tolerates_crlf_and_whitespace() {
        let (_t, ctx, a, _) = setup();
        let path = ctx.dirs.queue("dev");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, format!("  {a}\r\n\r\n")).unwrap();
        assert_eq!(peek(&ctx, "dev").unwrap(), Some(a));
    }

    #[test]
    fn unparsable_queue_file_is_removed() {
        let (_t, ctx, _, _) = setup();
        let path = ctx.dirs.queue("dev");
        std::fs::create_dir_all(path.parent().unwrap()).unwrap();
        std::fs::write(&path, "not a commit\n").unwrap();
        assert_eq!(peek(&ctx, "dev").unwrap(), None);
        assert!(!path.exists());
    }
}
