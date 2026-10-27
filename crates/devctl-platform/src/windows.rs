//! Windows implementation. Uses std's `CommandExt::creation_flags` and `taskkill`, so it needs
//! no Win32 bindings.

use std::io;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};

use crate::{JustCommand, JustError, Lock, Platform, detect_just, normalize_windows};

const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub struct WindowsPlatform;

impl Platform for WindowsPlatform {
    fn spawn_detached(&self, mut cmd: Command) -> io::Result<u32> {
        cmd.stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            // Not DETACHED_PROCESS: Windows ignores CREATE_NO_WINDOW when both are given, and
            // a console-less worker would make every console child it starts (just, git) open
            // a visible window. CREATE_NO_WINDOW gives it a hidden console they inherit.
            .creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
        Ok(cmd.spawn()?.id())
    }

    fn spawn_group(&self, mut cmd: Command) -> io::Result<Child> {
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
        cmd.spawn()
    }

    /// Repeats `taskkill /T /F` (up to 3 passes) because descendants started while a pass walks
    /// the tree can be missed. "Not found" (exit code 128) is success, like ESRCH on Unix.
    /// Without a Job Object (needs unsafe) a process that re-parents itself away from the tree
    /// before `taskkill` sees it cannot be reached.
    fn kill_tree(&self, pid: u32) -> io::Result<()> {
        const NOT_FOUND: i32 = 128;
        for _ in 0..3 {
            let out = Command::new("taskkill")
                .args(["/T", "/F", "/PID"])
                .arg(pid.to_string())
                .creation_flags(CREATE_NO_WINDOW)
                .stdin(Stdio::null())
                .output()?;
            match out.status.code() {
                Some(NOT_FOUND) => return Ok(()),
                Some(0) => {}
                _ => {
                    return Err(io::Error::other(format!(
                        "taskkill /T /F /PID {pid} failed ({}): {}",
                        out.status,
                        String::from_utf8_lossy(&out.stderr).trim()
                    )));
                }
            }
        }
        Ok(())
    }

    fn lock(&self, path: &Path) -> io::Result<Lock> {
        Lock::acquire(path)
    }

    fn try_lock(&self, path: &Path) -> io::Result<Option<Lock>> {
        Lock::try_acquire(path)
    }

    fn open_browser(&self, url: &str) -> io::Result<()> {
        // Not through `cmd`: it would interpret `&`, `|`, `^` and `%` in the URL.
        let status = Command::new("rundll32")
            .arg("url.dll,FileProtocolHandler")
            .arg(url)
            .creation_flags(CREATE_NO_WINDOW)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?;
        if status.success() {
            Ok(())
        } else {
            Err(io::Error::other(format!("rundll32 exited with {status}")))
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
        let abs = std::fs::canonicalize(p)
            .or_else(|_| std::path::absolute(p))
            .unwrap_or_else(|_| p.to_path_buf());
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
