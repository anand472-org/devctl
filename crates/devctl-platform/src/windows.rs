//! Windows implementation. Uses std's `CommandExt::creation_flags` and `taskkill`, so it needs
//! no Win32 bindings.

use std::io;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use crate::{JustCommand, JustError, Lock, Platform, detect_just, normalize_windows};

const DETACHED_PROCESS: u32 = 0x0000_0008;
const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub struct WindowsPlatform;

impl Platform for WindowsPlatform {
    fn spawn_detached(&self, mut cmd: Command) -> io::Result<u32> {
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
        Ok(cmd.spawn()?.id())
    }

    fn spawn_group(&self, mut cmd: Command) -> io::Result<Child> {
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
        cmd.spawn()
    }

    fn kill_tree(&self, pid: u32) -> io::Result<()> {
        let out = Command::new("taskkill")
            .args(["/T", "/F", "/PID"])
            .arg(pid.to_string())
            .creation_flags(CREATE_NO_WINDOW)
            .stdin(Stdio::null())
            .output()?;
        if out.status.success() {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "taskkill /T /F /PID {pid} failed: {}",
                String::from_utf8_lossy(&out.stderr).trim()
            )))
        }
    }

    fn lock(&self, path: &Path) -> io::Result<Lock> {
        Lock::acquire(path)
    }

    fn try_lock(&self, path: &Path) -> io::Result<Option<Lock>> {
        Lock::try_acquire(path)
    }

    fn open_browser(&self, url: &str) -> io::Result<()> {
        let status = Command::new("cmd")
            .args(["/c", "start", ""])
            .arg(url)
            .creation_flags(CREATE_NO_WINDOW)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        if status.success() {
            Ok(())
        } else {
            Err(io::Error::other(format!(
                "cmd /c start exited with {status}"
            )))
        }
    }

    fn just(&self) -> Result<JustCommand, JustError> {
        detect_just()
    }

    fn data_dir(&self) -> Option<PathBuf> {
        std::env::var_os("LOCALAPPDATA")
            .filter(|v| !v.is_empty())
            .map(|v| PathBuf::from(v).join("devctl"))
    }

    fn normalize_path(&self, p: &Path) -> String {
        let abs = std::path::absolute(p).unwrap_or_else(|_| p.to_path_buf());
        normalize_windows(&abs.to_string_lossy())
    }

    fn make_executable(&self, _path: &Path) -> io::Result<()> {
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader};
    use std::time::{Duration, Instant};

    fn is_alive(pid: u32) -> bool {
        Command::new("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
            .creation_flags(CREATE_NO_WINDOW)
            .output()
            .is_ok_and(|o| String::from_utf8_lossy(&o.stdout).contains(&format!("\"{pid}\"")))
    }

    struct Cleanup(u32);
    impl Drop for Cleanup {
        fn drop(&mut self) {
            let _ = WindowsPlatform.kill_tree(self.0);
        }
    }

    #[test]
    fn kill_tree_kills_grandchildren() {
        // powershell starts `ping -n 30` as its child, prints the child's pid, then waits on it.
        let script = "$p = Start-Process ping -ArgumentList '-n','30','127.0.0.1' -PassThru \
                      -WindowStyle Hidden; $p.Id; Wait-Process -Id $p.Id";
        let mut cmd = Command::new("powershell");
        cmd.args(["-NoProfile", "-NonInteractive", "-Command", script])
            .stdout(Stdio::piped());
        let mut leader = WindowsPlatform.spawn_group(cmd).unwrap();
        let _cleanup = Cleanup(leader.id());

        let mut line = String::new();
        BufReader::new(leader.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let grandchild: u32 = line.trim().parse().expect("grandchild pid");
        assert!(is_alive(grandchild), "grandchild should be running");

        WindowsPlatform.kill_tree(leader.id()).unwrap();
        let _ = leader.wait();

        let deadline = Instant::now() + Duration::from_secs(2);
        while is_alive(grandchild) {
            assert!(
                Instant::now() < deadline,
                "grandchild {grandchild} survived kill_tree"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}
