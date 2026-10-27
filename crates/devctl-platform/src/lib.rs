//! The only crate in the workspace allowed to contain OS-specific code.

use std::fs::{File, OpenOptions, TryLockError};
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};

mod normalize;
pub use normalize::{normalize_unix, normalize_windows};

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

#[cfg(any(feature = "fake", test))]
pub mod fake;

/// Oldest `just` devctl supports.
pub const MIN_JUST: (u32, u32, u32) = (1, 59, 0);

/// Everything devctl needs from the operating system.
pub trait Platform: Send + Sync {
    /// Starts `cmd` detached from this process (no console, own process group, null stdio).
    /// Returns its pid.
    ///
    /// # Errors
    /// If the process cannot be spawned.
    fn spawn_detached(&self, cmd: Command) -> io::Result<u32>;

    /// Spawns `cmd` as the leader of a new process group so `kill_tree` can kill everything it
    /// starts.
    ///
    /// # Errors
    /// If the process cannot be spawned.
    fn spawn_group(&self, cmd: Command) -> io::Result<Child>;

    /// Kills the process `pid` (a group leader from `spawn_group`/`spawn_detached`) and all of
    /// its descendants. A process that is already gone is not an error on Unix.
    ///
    /// # Errors
    /// If the signal or `taskkill` fails.
    fn kill_tree(&self, pid: u32) -> io::Result<()>;

    /// Takes an exclusive lock on `path` (created if missing), blocking until it is free.
    ///
    /// # Errors
    /// If the file cannot be created or locked.
    fn lock(&self, path: &Path) -> io::Result<Lock>;

    /// Like `lock`, but returns `None` if another holder has the lock.
    ///
    /// # Errors
    /// If the file cannot be created or locked for a reason other than contention.
    fn try_lock(&self, path: &Path) -> io::Result<Option<Lock>>;

    /// Opens `url` in the default browser.
    ///
    /// # Errors
    /// If the opener cannot be run or reports failure.
    fn open_browser(&self, url: &str) -> io::Result<()>;

    /// Locates `just` on `PATH` and checks it is at least [`MIN_JUST`].
    ///
    /// # Errors
    /// [`JustError`] if it is missing, too old, or its version cannot be read.
    fn just(&self) -> Result<JustCommand, JustError>;

    /// Per-user directory for devctl's own data, if the environment defines one.
    fn data_dir(&self) -> Option<PathBuf>;

    /// Absolute, `/`-separated, no `\\?\` prefix; drive letter upper-cased on Windows.
    fn normalize_path(&self, p: &Path) -> String;

    /// Marks `path` executable (mode `0o755` on Unix, a no-op on Windows).
    ///
    /// # Errors
    /// If the permissions cannot be changed.
    fn make_executable(&self, path: &Path) -> io::Result<()>;
}

/// An exclusive file lock, released when dropped.
#[derive(Debug)]
pub struct Lock {
    file: File,
}

impl Lock {
    fn open(path: &Path) -> io::Result<File> {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        OpenOptions::new()
            .read(true)
            .write(true)
            .create(true)
            .truncate(false)
            .open(path)
    }

    pub(crate) fn acquire(path: &Path) -> io::Result<Self> {
        let file = Self::open(path)?;
        file.lock()?;
        Ok(Self { file })
    }

    pub(crate) fn try_acquire(path: &Path) -> io::Result<Option<Self>> {
        let file = Self::open(path)?;
        match file.try_lock() {
            Ok(()) => Ok(Some(Self { file })),
            Err(TryLockError::WouldBlock) => Ok(None),
            Err(TryLockError::Error(e)) => Err(e),
        }
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        let _ = self.file.unlock();
    }
}

/// A `just` executable that satisfies [`MIN_JUST`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JustCommand {
    pub program: PathBuf,
    pub version: (u32, u32, u32),
}

impl JustCommand {
    /// A fresh `Command` for this `just`.
    #[must_use]
    pub fn command(&self) -> Command {
        Command::new(&self.program)
    }

    /// The version as `major.minor.patch`.
    #[must_use]
    pub fn version_string(&self) -> String {
        let (a, b, c) = self.version;
        format!("{a}.{b}.{c}")
    }
}

#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum JustError {
    #[error("`just` not found on PATH; install just 1.59.0 or newer (https://just.systems)")]
    Missing,
    #[error("`just` {found} is too old; devctl needs 1.59.0 or newer")]
    TooOld { found: String },
    #[error(
        "could not read the version from `just --version` (got: {output}); \
         install just 1.59.0 or newer (https://just.systems)"
    )]
    Unreadable { output: String },
}

/// Parses `just 1.59.0` (optionally with a `-dev`/`+build` suffix) into `(major, minor, patch)`.
#[must_use]
pub fn parse_just_version(stdout: &str) -> Option<(u32, u32, u32)> {
    let token = stdout
        .trim()
        .strip_prefix("just")?
        .split_whitespace()
        .next()?;
    let core = token.split(['-', '+']).next()?;
    let mut parts = core.split('.');
    let version = (
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
        parts.next()?.parse().ok()?,
    );
    parts.next().is_none().then_some(version)
}

/// Runs `just --version` from `PATH`.
pub(crate) fn detect_just() -> Result<JustCommand, JustError> {
    let out = match Command::new("just").arg("--version").output() {
        Ok(o) => o,
        Err(e) if e.kind() == io::ErrorKind::NotFound => return Err(JustError::Missing),
        Err(e) => {
            return Err(JustError::Unreadable {
                output: e.to_string(),
            });
        }
    };
    let stdout = String::from_utf8_lossy(&out.stdout);
    let Some(version) = parse_just_version(&stdout) else {
        let stderr = String::from_utf8_lossy(&out.stderr);
        let shown = if stdout.trim().is_empty() {
            stderr.trim()
        } else {
            stdout.trim()
        };
        return Err(JustError::Unreadable {
            output: shown.to_owned(),
        });
    };
    if version < MIN_JUST {
        let (a, b, c) = version;
        return Err(JustError::TooOld {
            found: format!("{a}.{b}.{c}"),
        });
    }
    Ok(JustCommand {
        program: PathBuf::from("just"),
        version,
    })
}

/// The platform this binary was built for.
#[must_use]
pub fn current() -> Box<dyn Platform> {
    #[cfg(unix)]
    {
        Box::new(unix::UnixPlatform)
    }
    #[cfg(windows)]
    {
        Box::new(windows::WindowsPlatform)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_just_version() {
        assert_eq!(parse_just_version("just 1.59.0\n"), Some((1, 59, 0)));
        assert_eq!(parse_just_version("just 1.60.2-dev"), Some((1, 60, 2)));
        assert_eq!(parse_just_version("nope"), None);
    }

    #[test]
    fn normalizes_windows_paths() {
        assert_eq!(
            normalize_windows(r"\\?\c:\work\repo\src\a.rs"),
            "C:/work/repo/src/a.rs"
        );
        assert_eq!(
            normalize_windows(r"c:\work\.\repo\..\repo\b.rs"),
            "C:/work/repo/b.rs"
        );
    }

    #[test]
    fn normalizes_unix_paths() {
        assert_eq!(
            normalize_unix("/work/./repo/../repo/a.rs"),
            "/work/repo/a.rs"
        );
    }

    #[test]
    fn try_lock_is_exclusive() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("x.lock");
        let held = current().try_lock(&p).unwrap().expect("first lock");
        assert!(current().try_lock(&p).unwrap().is_none());
        drop(held);
        assert!(current().try_lock(&p).unwrap().is_some());
    }

    #[test]
    fn lock_creates_parent_dirs() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a").join("b").join("x.lock");
        assert!(current().lock(&p).is_ok());
    }

    #[test]
    fn just_error_messages() {
        assert_eq!(
            JustError::Missing.to_string(),
            "`just` not found on PATH; install just 1.59.0 or newer (https://just.systems)"
        );
        assert_eq!(
            JustError::TooOld {
                found: "1.2.3".into()
            }
            .to_string(),
            "`just` 1.2.3 is too old; devctl needs 1.59.0 or newer"
        );
    }
}
