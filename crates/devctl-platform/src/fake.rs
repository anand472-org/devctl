//! A recording [`Platform`] for tests of crates that spawn background work.

use std::ffi::OsString;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::sync::Mutex;
use std::sync::atomic::{AtomicU32, Ordering};

use crate::{JustCommand, JustError, Lock, Platform, current};

/// One command passed to [`FakePlatform::spawn_detached`].
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SpawnRecord {
    pub program: OsString,
    pub args: Vec<OsString>,
    pub current_dir: Option<PathBuf>,
    /// Environment changes as set on the command (`None` means removed).
    pub envs: Vec<(OsString, Option<OsString>)>,
}

/// Delegates process and lock calls to the real platform, except `spawn_detached`, which only
/// records the command, and `just`, which can be overridden with [`FakePlatform::with_just`].
pub struct FakePlatform {
    real: Box<dyn Platform>,
    spawned: Mutex<Vec<SpawnRecord>>,
    just: Option<Result<JustCommand, JustError>>,
    next_pid: AtomicU32,
}

impl Default for FakePlatform {
    fn default() -> Self {
        Self::new()
    }
}

impl FakePlatform {
    #[must_use]
    pub fn new() -> Self {
        Self {
            real: current(),
            spawned: Mutex::new(Vec::new()),
            just: None,
            next_pid: AtomicU32::new(40_000),
        }
    }

    /// Makes `just()` return `result` instead of probing the real `just`.
    #[must_use]
    pub fn with_just(mut self, result: Result<JustCommand, JustError>) -> Self {
        self.just = Some(result);
        self
    }

    /// Every command passed to `spawn_detached`, in order.
    ///
    /// # Panics
    /// If the internal mutex was poisoned by a panic in another thread.
    #[must_use]
    pub fn spawned(&self) -> Vec<SpawnRecord> {
        self.spawned.lock().unwrap().clone()
    }
}

impl Platform for FakePlatform {
    fn spawn_detached(&self, cmd: Command) -> io::Result<u32> {
        let record = SpawnRecord {
            program: cmd.get_program().to_owned(),
            args: cmd.get_args().map(ToOwned::to_owned).collect(),
            current_dir: cmd.get_current_dir().map(Path::to_path_buf),
            envs: cmd
                .get_envs()
                .map(|(k, v)| (k.to_owned(), v.map(ToOwned::to_owned)))
                .collect(),
        };
        self.spawned.lock().unwrap().push(record);
        Ok(self.next_pid.fetch_add(1, Ordering::Relaxed))
    }

    fn spawn_group(&self, cmd: Command) -> io::Result<Child> {
        self.real.spawn_group(cmd)
    }

    fn kill_tree(&self, pid: u32) -> io::Result<()> {
        self.real.kill_tree(pid)
    }

    fn lock(&self, path: &Path) -> io::Result<Lock> {
        self.real.lock(path)
    }

    fn try_lock(&self, path: &Path) -> io::Result<Option<Lock>> {
        self.real.try_lock(path)
    }

    fn open_browser(&self, url: &str) -> io::Result<()> {
        self.real.open_browser(url)
    }

    fn just(&self) -> Result<JustCommand, JustError> {
        match &self.just {
            Some(r) => r.clone(),
            None => self.real.just(),
        }
    }

    fn data_dir(&self) -> Option<PathBuf> {
        self.real.data_dir()
    }

    fn normalize_path(&self, p: &Path) -> String {
        self.real.normalize_path(p)
    }

    fn make_executable(&self, path: &Path) -> io::Result<()> {
        self.real.make_executable(path)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn records_spawn_detached_without_spawning() {
        let fake = FakePlatform::new();
        let mut cmd = Command::new("/definitely/not/a/program");
        cmd.current_dir("/tmp/wd")
            .arg("--flag")
            .env("DEVCTL_X", "1")
            .env_remove("DEVCTL_Y");
        let pid = fake.spawn_detached(cmd).unwrap();
        let rec = fake.spawned();
        assert_eq!(rec.len(), 1);
        assert_eq!(rec[0].program, "/definitely/not/a/program");
        assert_eq!(rec[0].current_dir, Some(PathBuf::from("/tmp/wd")));
        assert_eq!(rec[0].args, vec![OsString::from("--flag")]);
        assert!(rec[0].envs.contains(&("DEVCTL_X".into(), Some("1".into()))));
        assert!(rec[0].envs.contains(&("DEVCTL_Y".into(), None)));
        assert_ne!(pid, fake.spawn_detached(Command::new("x")).unwrap());
    }

    #[test]
    fn with_just_overrides() {
        let fake = FakePlatform::new().with_just(Err(JustError::Missing));
        assert!(matches!(fake.just(), Err(JustError::Missing)));
    }
}
