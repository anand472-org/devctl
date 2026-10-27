pub mod cli;
pub mod commit_size;
pub mod config;
pub mod ctx;
pub mod gate;
pub mod git;
pub mod hooks;
pub mod recipes;
pub mod record;
pub mod status;
pub mod telemetry;

#[cfg(test)]
pub(crate) mod test_support;

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use devctl_store::Status;

use cli::{Cli, Command};
use ctx::Ctx;
use git::Repo;

/// Which devctl process is running; names the log file and decides the
/// telemetry flush timeout.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Process {
    /// A command run by a developer.
    Cli,
    /// The background recorder.
    Worker,
    /// A git hook, by hook name (`pre-push`, `post-commit`, ...).
    Hook(String),
}

impl Process {
    /// `cli`, `worker` or `hook-<name>`.
    #[must_use]
    pub fn name(&self) -> String {
        match self {
            Process::Cli => "cli".to_string(),
            Process::Worker => "worker".to_string(),
            Process::Hook(h) => format!("hook-{h}"),
        }
    }

    /// 2 s for `pre-push` (the developer is waiting on git), else 5 s.
    #[must_use]
    pub fn flush_timeout(&self) -> Duration {
        match self {
            Process::Hook(h) if h == "pre-push" => Duration::from_secs(2),
            _ => Duration::from_secs(5),
        }
    }
}

/// An open repository context with telemetry running. Telemetry is flushed
/// when this is dropped, so keep it alive for the whole command.
pub struct Session {
    pub ctx: Ctx,
    // Declared last: dropped after `ctx`, flushing everything recorded.
    _telemetry: telemetry::Guard,
}

/// Opens the repository containing `cwd` and starts telemetry for `process`:
/// logs in `<common dir>/devctl/logs`, export settings from `.devctl.toml`,
/// and the repository name from `origin`.
///
/// # Errors
/// If [`Ctx::open`] fails (not in a repository, bad `.devctl.toml`, ...).
pub fn open_session(cwd: &Path, process: &Process) -> Result<Session> {
    let ctx = Ctx::open(cwd, devctl_platform::current())?;
    let logs = ctx.dirs.logs();
    // tracing-appender prints "Error reading the log directory" to stderr when
    // pruning old files in a directory that does not exist yet. A failure here
    // surfaces as the logger's own warning.
    let _ = std::fs::create_dir_all(&logs);
    let repo = ctx.repo.remote_url_name();
    let name = process.name();
    let guard = telemetry::init(&telemetry::Settings {
        process: &name,
        logs_dir: Some(&logs),
        cfg: &ctx.config.telemetry,
        flush_timeout: process.flush_timeout(),
        repo: repo.as_deref(),
    });
    Ok(Session {
        ctx,
        _telemetry: guard,
    })
}

/// Runs `body` for a repository-bound command: opens a [`Session`] in the
/// current directory, enters the span made by `span` (created once the
/// subscriber is installed) and logs a failure with `tracing::error!` before
/// telemetry is flushed. The error is still returned for `main` to print.
fn in_session<T>(
    process: &Process,
    span: impl FnOnce() -> tracing::Span,
    body: impl FnOnce(&Ctx) -> Result<T>,
) -> Result<T> {
    let cwd = std::env::current_dir().context("cannot read the current directory")?;
    let session = open_session(&cwd, process)?;
    let span = span();
    span.in_scope(|| {
        let result = body(&session.ctx);
        if let Err(e) = &result {
            tracing::error!(error = %format!("{e:#}"), "command failed");
        }
        result
    })
}

/// Dispatches a parsed command line to its subcommand handler. `version` runs
/// without a repository or telemetry; every other command opens a
/// [`Session`] first.
///
/// # Errors
///
/// Returns an error if the subcommand fails, including `record` producing an
/// `error` record.
pub fn run(cli: &Cli) -> Result<()> {
    match &cli.command {
        Command::Version => {
            println!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Command::Record { commit } => in_session(
            &Process::Cli,
            || tracing::info_span!("devctl.cli", command = "record"),
            |ctx| {
                let out = record::run_record(ctx, commit.as_deref())?;
                println!("{}", out.text);
                if let Status::Error { .. } = out.status {
                    bail!(
                        "the record ended with status error (shown above); fix the cause, \
                         then run `devctl record` again"
                    );
                }
                Ok(())
            },
        ),
        Command::Status { branch } => in_session(
            &Process::Cli,
            || tracing::info_span!("devctl.cli", command = "status"),
            |ctx| {
                println!("{}", status::run_status(ctx, branch.as_deref())?);
                Ok(())
            },
        ),
        Command::Worker { branch } => in_session(
            &Process::Worker,
            || {
                let span = tracing::info_span!("worker.run", branch = %branch);
                telemetry::adopt_traceparent(&span);
                span
            },
            |ctx| record::worker::run_worker(ctx, branch),
        ),
        Command::Gate { target } => {
            let report = in_session(
                &Process::Cli,
                || tracing::info_span!("devctl.cli", command = "gate"),
                |ctx| {
                    let report = gate::run_gate(ctx, target.as_deref())?;
                    print!("{}", report.text);
                    Ok(report)
                },
            )?;
            if report.blocked {
                bail!(
                    "push blocked by devctl; fix what is listed above, then run `devctl gate` again"
                );
            }
            Ok(())
        }
        Command::Hook { name, args } => hooks::run_hook(*name, args),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn process_names_and_flush_timeouts() {
        assert_eq!(Process::Cli.name(), "cli");
        assert_eq!(Process::Worker.name(), "worker");
        assert_eq!(Process::Hook("pre-push".into()).name(), "hook-pre-push");
        assert_eq!(
            Process::Hook("pre-push".into()).flush_timeout(),
            Duration::from_secs(2)
        );
        assert_eq!(
            Process::Hook("post-commit".into()).flush_timeout(),
            Duration::from_secs(5)
        );
        assert_eq!(Process::Cli.flush_timeout(), Duration::from_secs(5));
        assert_eq!(Process::Worker.flush_timeout(), Duration::from_secs(5));
    }

    #[test]
    fn open_session_outside_a_repo_errors() {
        let dir = tempfile::tempdir().unwrap();
        let err = open_session(dir.path(), &Process::Cli).err().unwrap();
        assert!(
            format!("{err:#}").contains("not inside a git repository"),
            "{err:#}"
        );
    }
}
