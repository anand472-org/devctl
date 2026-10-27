pub mod cli;
pub mod config;
pub mod ctx;
pub mod git;
pub mod telemetry;

#[cfg(test)]
pub(crate) mod test_support;

use std::path::Path;
use std::time::Duration;

use anyhow::{Context, Result};

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

/// Dispatches a parsed command line to its subcommand handler. `version` runs
/// without a repository or telemetry; every other command opens a
/// [`Session`] first.
///
/// # Errors
///
/// Returns an error if the subcommand fails.
pub fn run(cli: &Cli) -> Result<()> {
    match cli.command {
        Command::Version => {
            println!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
            Ok(())
        }
        Command::Probe => {
            let cwd = std::env::current_dir().context("cannot read the current directory")?;
            let session = open_session(&cwd, &Process::Cli)?;
            let span = tracing::info_span!("devctl.cli", command = "probe");
            let _entered = span.enter();
            tracing::info!(
                branch = session.ctx.repo.current_branch()?.as_deref(),
                "probe"
            );
            Ok(())
        }
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
