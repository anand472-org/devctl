//! `devctl hook <name>`: what the git hook scripts call. Installing them comes
//! with `devctl hooks install`.

use std::io::Read as _;

use anyhow::{Context, Result, anyhow, bail};

use crate::cli::HookName;
use crate::commit_size;
use crate::ctx::Ctx;
use crate::gate;
use crate::git::Repo as _;
use crate::record::{queue, worker};
use crate::{Process, in_session};

/// Runs hook `name` with git's arguments `args`.
///
/// # Errors
/// For `pre-push`: when the push is blocked or the gate cannot be evaluated
/// (it fails closed). Hooks not in this build always fail.
pub fn run_hook(name: HookName, args: &[String]) -> Result<()> {
    match name {
        HookName::PrePush => {
            let process = Process::Hook(name.as_str().to_string());
            let blocked = in_session(
                &process,
                || tracing::info_span!("hook", name = name.as_str()),
                |ctx| {
                    crate::interrupt::install();
                    let mut stdin = String::new();
                    std::io::stdin()
                        .read_to_string(&mut stdin)
                        .context("cannot read the refs git passed on stdin")?;
                    pre_push(ctx, args, &stdin, &mut |text| {
                        print!("{text}");
                        let _ = std::io::Write::flush(&mut std::io::stdout());
                    })
                },
            )
            .context("the pre-push gate could not run, so the push is blocked")?;
            if blocked {
                bail!("push blocked by devctl; fix what is listed above, then push again");
            }
            Ok(())
        }
        HookName::PreCommit => {
            let process = Process::Hook(name.as_str().to_string());
            let allowed = in_session(
                &process,
                || tracing::info_span!("hook", name = name.as_str()),
                commit_size::run_pre_commit,
            )
            .context("the pre-commit size check could not run, so the commit is blocked")?;
            if !allowed {
                bail!("commit blocked by devctl; split it as listed above, then commit again");
            }
            Ok(())
        }
        HookName::PostCommit => {
            // Never fails a commit: `in_session` logs any error (to the log
            // file once the session is open); without a session there is
            // nowhere to log, so say it on stderr (the hook script discards
            // it).
            let process = Process::Hook(name.as_str().to_string());
            let outcome = std::panic::catch_unwind(|| {
                in_session(
                    &process,
                    || tracing::info_span!("hook", name = name.as_str()),
                    post_commit,
                )
            });
            match outcome {
                Ok(Ok(())) => {}
                Ok(Err(e)) => eprintln!("devctl post-commit: {e:#}"),
                Err(_) => {
                    eprintln!("devctl post-commit: internal error (panic); commit unaffected");
                }
            }
            Ok(())
        }
    }
}

/// The three hooks `devctl hooks install` writes.
pub const HOOKS: [&str; 3] = ["pre-commit", "post-commit", "pre-push"];

/// The text of hook `name`'s script (LF line endings on every platform).
/// `post-commit` detaches the standard streams: the background worker it
/// starts would otherwise inherit, and hold open, the pipes of the git or IDE
/// process that ran the commit (on Windows a handle held by the worker keeps
/// them from seeing the end of the output).
#[must_use]
pub fn script(name: &str) -> String {
    if name == "post-commit" {
        format!("#!/bin/sh\nexec devctl hook {name} \"$@\" </dev/null >/dev/null 2>&1\n")
    } else {
        format!("#!/bin/sh\nexec devctl hook {name} \"$@\"\n")
    }
}

/// What to add by hand to an existing hook `name` at `path`: the command
/// line of [`script`] without `exec` (which would end the hook there), and
/// for `pre-push` a reminder that git's ref list arrives on stdin.
fn add_line(path: &std::path::Path, name: &str) -> String {
    let script = script(name);
    let command = script
        .lines()
        .nth(1)
        .and_then(|l| l.strip_prefix("exec "))
        .unwrap_or_default();
    let mut text = format!("add this line to {}: {command}", path.display());
    if name == "pre-push" {
        text.push_str("\n  (devctl must be the first command in the hook to read stdin)");
    }
    text
}

/// Writes the three hook scripts into `<common dir>/hooks`. Writes nothing
/// when `core.hooksPath` is set or any hook exists with other content; the
/// error then lists the line to add to each affected hook.
///
/// # Errors
/// On a hooks path, a foreign hook, or a file that cannot be read or written.
pub fn install(ctx: &Ctx) -> Result<()> {
    if let Some(path) = ctx.repo.config_string("core.hooksPath")? {
        let dir = std::path::PathBuf::from(&path);
        let lines: Vec<String> = HOOKS.iter().map(|n| add_line(&dir.join(n), n)).collect();
        bail!(
            "core.hooksPath is set to `{path}`, so devctl wrote no hook; do this by hand:\n{}",
            lines.join("\n")
        );
    }
    let dir = ctx.repo.common_dir().join("hooks");
    if dir.exists() && !dir.is_dir() {
        bail!(
            "`{}` is not a directory; remove it, then run `devctl hooks install`",
            dir.display()
        );
    }
    let mut conflicts = Vec::new();
    // Hooks to (re)write: missing, or identical but for CRLF line endings.
    let mut to_write = Vec::new();
    for name in HOOKS {
        let path = dir.join(name);
        match std::fs::read(&path) {
            Ok(existing) => {
                let text = String::from_utf8_lossy(&existing).replace("\r\n", "\n");
                if text != script(name) {
                    conflicts.push(add_line(&path, name));
                } else if existing != script(name).as_bytes() {
                    to_write.push(name);
                }
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => to_write.push(name),
            Err(e) => {
                return Err(e).with_context(|| {
                    format!("cannot read {}; check its permissions", path.display())
                });
            }
        }
    }
    if !conflicts.is_empty() {
        bail!(
            "a hook already exists, so devctl wrote no hook; do this by hand:\n{}",
            conflicts.join("\n")
        );
    }
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("cannot create {}; check its permissions", dir.display()))?;
    for name in HOOKS {
        let path = dir.join(name);
        let wrote = to_write.contains(&name);
        if wrote {
            std::fs::write(&path, script(name)).with_context(|| {
                format!("cannot write {}; check its permissions", path.display())
            })?;
        }
        // Also for hooks already in place: repairs a lost executable bit.
        ctx.platform
            .make_executable(&path)
            .with_context(|| format!("cannot make {} executable", path.display()))?;
        if wrote {
            println!("installed {}", path.display());
        }
    }
    if to_write.is_empty() {
        println!("devctl hooks are already installed in {}", dir.display());
    }
    Ok(())
}

/// Queues the commit just made on the current branch and starts the
/// background worker. Returns at once; a detached HEAD queues nothing. The
/// caller logs and swallows any error, so a commit never fails because of it.
///
/// # Errors
/// If HEAD cannot be resolved, the queue cannot be written or the worker
/// cannot be started.
pub fn post_commit(ctx: &Ctx) -> Result<()> {
    let Some(branch) = ctx.repo.current_branch()? else {
        tracing::info!("detached HEAD; nothing to record");
        return Ok(());
    };
    let head = ctx.repo.resolve("HEAD")?;
    queue::enqueue(ctx, &branch, head)?;
    worker::spawn_worker(ctx, &branch)
}

/// The `pre-push` gate: `args` are git's (`<remote name or URL> <URL>`),
/// `stdin` its ref lines. Each ref's report goes to `emit` as soon as it is
/// ready, headed by `<remote ref>:` when several refs are pushed, so the
/// refs before one that fails to evaluate are still shown. Returns whether
/// any ref is blocked.
///
/// # Errors
/// On missing arguments, unparsable input or any failure evaluating a rule.
pub fn pre_push(
    ctx: &Ctx,
    args: &[String],
    stdin: &str,
    emit: &mut dyn FnMut(&str),
) -> Result<bool> {
    let name = args.first().ok_or_else(|| {
        anyhow!(
            "pre-push was called without the remote name; git passes it, so run \
             `git push` instead of calling the hook directly"
        )
    })?;
    let remote = gate::Remote::from_hook_args(name, args.get(1).map(String::as_str));
    let updates = gate::parse_pre_push(stdin)?;
    let mut blocked = false;
    for update in &updates {
        if updates.len() > 1 {
            emit(&format!("{}:\n", update.remote_ref));
        }
        let results = gate::evaluate(ctx, &remote, update)?;
        blocked |= gate::blocked(&results);
        emit(&gate::render(update, &results));
    }
    Ok(blocked)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::record::worker::tests::SharedFake;
    use crate::test_support::TestRepo;
    use devctl_platform::fake::FakePlatform;
    use std::ffi::OsString;
    use std::sync::Arc;

    fn ctx_for(t: &TestRepo) -> Ctx {
        Ctx::open(t.path(), Box::new(FakePlatform::new())).unwrap()
    }

    fn hooks_dir(t: &TestRepo) -> std::path::PathBuf {
        t.file(".git/hooks")
    }

    #[test]
    fn scripts_are_lf_and_exact() {
        assert_eq!(
            script("pre-commit"),
            "#!/bin/sh\nexec devctl hook pre-commit \"$@\"\n"
        );
        assert_eq!(
            script("pre-push"),
            "#!/bin/sh\nexec devctl hook pre-push \"$@\"\n"
        );
        assert_eq!(
            script("post-commit"),
            "#!/bin/sh\nexec devctl hook post-commit \"$@\" </dev/null >/dev/null 2>&1\n"
        );
        assert!(HOOKS.iter().all(|n| !script(n).contains('\r')));
    }

    #[test]
    fn installs_three_executable_hooks() {
        let t = TestRepo::new();
        install(&ctx_for(&t)).unwrap();
        for name in HOOKS {
            let path = hooks_dir(&t).join(name);
            assert_eq!(std::fs::read_to_string(&path).unwrap(), script(name));
        }
    }

    #[test]
    fn install_creates_a_missing_hooks_dir() {
        let t = TestRepo::new();
        let _ = std::fs::remove_dir_all(hooks_dir(&t));
        install(&ctx_for(&t)).unwrap();
        assert!(hooks_dir(&t).join("pre-push").is_file());
    }

    #[test]
    fn install_is_idempotent() {
        let t = TestRepo::new();
        install(&ctx_for(&t)).unwrap();
        install(&ctx_for(&t)).unwrap();
        for name in HOOKS {
            let text = std::fs::read_to_string(hooks_dir(&t).join(name)).unwrap();
            assert_eq!(text, script(name));
        }
    }

    #[test]
    fn existing_foreign_hook_stops_with_line() {
        let t = TestRepo::new();
        std::fs::create_dir_all(hooks_dir(&t)).unwrap();
        std::fs::write(hooks_dir(&t).join("pre-push"), "#!/bin/sh\necho mine\n").unwrap();
        let err = format!("{:#}", install(&ctx_for(&t)).unwrap_err());
        assert!(err.contains("devctl hook pre-push \"$@\""), "{err}");
        assert!(err.contains("add this line to"), "{err}");
        assert!(!err.contains("devctl hook pre-commit"), "{err}");
        assert!(
            !hooks_dir(&t).join("post-commit").exists(),
            "all or nothing"
        );
        assert!(!hooks_dir(&t).join("pre-commit").exists(), "all or nothing");
        assert_eq!(
            std::fs::read_to_string(hooks_dir(&t).join("pre-push")).unwrap(),
            "#!/bin/sh\necho mine\n"
        );
    }

    #[test]
    fn hand_install_lines_match_the_scripts() {
        let p = std::path::Path::new("h");
        assert_eq!(
            add_line(&p.join("post-commit"), "post-commit"),
            format!(
                "add this line to {}: devctl hook post-commit \"$@\" </dev/null >/dev/null 2>&1",
                p.join("post-commit").display()
            )
        );
        assert_eq!(
            add_line(&p.join("pre-push"), "pre-push"),
            format!(
                "add this line to {}: devctl hook pre-push \"$@\"\n  (devctl must be the first \
                 command in the hook to read stdin)",
                p.join("pre-push").display()
            )
        );
        assert_eq!(
            add_line(&p.join("pre-commit"), "pre-commit"),
            format!(
                "add this line to {}: devctl hook pre-commit \"$@\"",
                p.join("pre-commit").display()
            )
        );
    }

    #[test]
    fn hooks_path_set_stops() {
        let t = TestRepo::new();
        t.git(&["config", "core.hooksPath", "tools/hooks"]);
        let err = format!("{:#}", install(&ctx_for(&t)).unwrap_err());
        for name in HOOKS {
            assert!(
                err.contains(&format!(
                    "{}: devctl hook {name} \"$@\"",
                    std::path::Path::new("tools/hooks").join(name).display()
                )),
                "{err}"
            );
        }
        assert!(!hooks_dir(&t).join("pre-push").exists());
    }

    #[test]
    fn post_commit_queues_and_spawns() {
        let t = TestRepo::new();
        t.commit("one");
        let fake = Arc::new(FakePlatform::new());
        let ctx = Ctx::open(t.path(), Box::new(SharedFake(fake.clone()))).unwrap();
        post_commit(&ctx).unwrap();
        let head = ctx.repo.resolve("HEAD").unwrap();
        let queued = std::fs::read_to_string(ctx.dirs.queue("dev")).unwrap();
        assert_eq!(queued, format!("{head}\n"));
        let spawned = fake.spawned();
        assert_eq!(spawned.len(), 1);
        assert_eq!(
            spawned[0].args,
            ["worker", "--branch", "dev"].map(OsString::from)
        );
    }

    #[test]
    fn post_commit_detached_is_a_noop() {
        let t = TestRepo::new();
        let sha = t.commit("one");
        t.git(&["checkout", "-q", "--detach", &sha]);
        let fake = Arc::new(FakePlatform::new());
        let ctx = Ctx::open(t.path(), Box::new(SharedFake(fake.clone()))).unwrap();
        post_commit(&ctx).unwrap();
        assert!(!ctx.dirs.queue("dev").exists());
        assert_eq!(fake.spawned().len(), 0);
    }
}
