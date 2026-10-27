//! Linux and macOS implementation.

use std::ffi::OsString;
use std::io;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use rustix::process::{Pid, Signal, kill_process_group};

use crate::{JustCommand, JustError, Lock, Platform, detect_just, normalize_unix};

pub struct UnixPlatform;

fn data_dir_from(
    macos: bool,
    home: Option<OsString>,
    xdg_data_home: Option<OsString>,
) -> Option<PathBuf> {
    let home = home.filter(|h| !h.is_empty()).map(PathBuf::from);
    if macos {
        return home.map(|h| h.join("Library").join("Application Support").join("devctl"));
    }
    let xdg = xdg_data_home.map(PathBuf::from).filter(|p| p.is_absolute());
    match xdg {
        Some(x) => Some(x.join("devctl")),
        None => home.map(|h| h.join(".local").join("share").join("devctl")),
    }
}

impl Platform for UnixPlatform {
    fn spawn_detached(&self, mut cmd: Command) -> io::Result<u32> {
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .process_group(0);
        // process_group(0) is enough: no controlling-terminal signals reach a new group.
        // The child is intentionally not waited on; it outlives this process.
        Ok(cmd.spawn()?.id())
    }

    fn spawn_group(&self, mut cmd: Command) -> io::Result<Child> {
        cmd.process_group(0);
        cmd.spawn()
    }

    fn kill_tree(&self, pid: u32) -> io::Result<()> {
        let group = i32::try_from(pid)
            .ok()
            .and_then(Pid::from_raw)
            .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid process id"))?;
        match kill_process_group(group, Signal::KILL) {
            Ok(()) | Err(rustix::io::Errno::SRCH) => Ok(()),
            Err(e) => Err(e.into()),
        }
    }

    fn lock(&self, path: &Path) -> io::Result<Lock> {
        Lock::acquire(path)
    }

    fn try_lock(&self, path: &Path) -> io::Result<Option<Lock>> {
        Lock::try_acquire(path)
    }

    fn open_browser(&self, url: &str) -> io::Result<()> {
        let opener = if cfg!(target_os = "macos") {
            "open"
        } else {
            "xdg-open"
        };
        let status = Command::new(opener)
            .arg(url)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        if status.success() {
            Ok(())
        } else {
            Err(io::Error::other(format!("{opener} exited with {status}")))
        }
    }

    fn just(&self) -> Result<JustCommand, JustError> {
        detect_just()
    }

    fn data_dir(&self) -> Option<PathBuf> {
        data_dir_from(
            cfg!(target_os = "macos"),
            std::env::var_os("HOME"),
            std::env::var_os("XDG_DATA_HOME"),
        )
    }

    fn normalize_path(&self, p: &Path) -> String {
        let abs = std::fs::canonicalize(p)
            .or_else(|_| std::path::absolute(p))
            .unwrap_or_else(|_| p.to_path_buf());
        normalize_unix(&abs.to_string_lossy())
    }

    fn make_executable(&self, path: &Path) -> io::Result<()> {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::time::{Duration, Instant};

    #[test]
    fn data_dir_layouts() {
        let h = || Some(OsString::from("/home/u"));
        assert_eq!(
            data_dir_from(true, h(), None),
            Some(PathBuf::from("/home/u/Library/Application Support/devctl"))
        );
        assert_eq!(
            data_dir_from(false, h(), Some("/x/data".into())),
            Some(PathBuf::from("/x/data/devctl"))
        );
        assert_eq!(
            data_dir_from(false, h(), Some("relative".into())),
            Some(PathBuf::from("/home/u/.local/share/devctl"))
        );
        assert_eq!(data_dir_from(false, None, None), None);
    }

    #[test]
    fn normalize_path_resolves_symlinks() {
        let dir = tempfile::tempdir().unwrap();
        let canon = std::fs::canonicalize(dir.path()).unwrap();
        assert_eq!(
            UnixPlatform.normalize_path(dir.path()),
            UnixPlatform.normalize_path(&canon)
        );
        assert_eq!(
            UnixPlatform.normalize_path(dir.path()),
            canon.to_string_lossy()
        );
        let missing = dir.path().join("no").join("..").join("file.txt");
        assert_eq!(
            UnixPlatform.normalize_path(&missing),
            format!("{}/file.txt", dir.path().to_string_lossy())
        );
    }

    #[test]
    fn make_executable_sets_0755() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f.sh");
        std::fs::write(&p, "#!/bin/sh\n").unwrap();
        UnixPlatform.make_executable(&p).unwrap();
        let mode = std::fs::metadata(&p).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755);
    }

    fn is_alive(pid: u32) -> bool {
        let Some(p) = i32::try_from(pid).ok().and_then(Pid::from_raw) else {
            return false;
        };
        if rustix::process::test_kill_process(p).is_err() {
            return false;
        }
        // A killed process that nobody has reaped yet is a zombie: dead for our purposes.
        match std::fs::read_to_string(format!("/proc/{pid}/stat")) {
            Ok(stat) => !stat.contains(") Z"),
            Err(_) => true,
        }
    }

    struct Cleanup(u32);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = UnixPlatform.kill_tree(self.0);
        }
    }

    #[test]
    fn kill_tree_kills_grandchildren() {
        let mut cmd = Command::new("sh");
        cmd.args(["-c", "sleep 30 & echo $!; wait"])
            .stdout(Stdio::piped());
        let mut leader = UnixPlatform.spawn_group(cmd).unwrap();
        let _cleanup = Cleanup(leader.id());

        let mut line = String::new();
        BufReader::new(leader.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let grandchild: u32 = line.trim().parse().expect("grandchild pid");
        assert!(is_alive(grandchild), "grandchild should be running");

        UnixPlatform.kill_tree(leader.id()).unwrap();
        let _ = leader.wait();

        let deadline = Instant::now() + Duration::from_secs(2);
        while is_alive(grandchild) {
            assert!(
                Instant::now() < deadline,
                "grandchild {grandchild} survived kill_tree"
            );
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}
