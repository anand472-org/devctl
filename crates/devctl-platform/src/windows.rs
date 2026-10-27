//! Windows implementation. Process trees are held in Job Objects through the `win32job` crate,
//! which wraps the Win32 calls in a safe API; everything else uses std and stock commands
//! (`taskkill`, `rundll32`).

use std::collections::HashMap;
use std::io;
use std::os::windows::io::AsRawHandle;
use std::os::windows::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{LazyLock, Mutex, PoisonError};

use win32job::{ExtendedLimitInfo, Job};

use crate::{
    JustCommand, JustError, Lock, Platform, detect_just, detect_just_at, find_just_on_path,
    normalize_windows,
};

const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
const CREATE_NO_WINDOW: u32 = 0x0800_0000;

pub struct WindowsPlatform;

/// The Job Object of each live `spawn_group` child, by pid. Each job has
/// `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` and this map holds its only handle, so removing and
/// dropping an entry terminates every process in the job. Process-wide rather than per
/// `WindowsPlatform` because `current()` hands out a fresh value on each call.
static JOBS: LazyLock<Mutex<HashMap<u32, Job>>> = LazyLock::new(Mutex::default);

fn jobs() -> std::sync::MutexGuard<'static, HashMap<u32, Job>> {
    JOBS.lock().unwrap_or_else(PoisonError::into_inner)
}

/// Puts `child` into a new kill-on-close Job Object. Processes it starts from then on are
/// members too, whether or not their parent is still alive.
fn job_for(child: &Child) -> Result<Job, win32job::JobError> {
    let job = Job::create_with_limit_info(ExtendedLimitInfo::new().limit_kill_on_job_close())?;
    job.assign_process(child.as_raw_handle() as isize)?;
    Ok(job)
}

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

    /// Also puts the child in a Job Object (see [`JOBS`]), so `kill_tree` reaches descendants
    /// whose parent has exited: Windows does not re-parent orphans, so `taskkill /T` cannot
    /// find them. The child is assigned right after it starts; anything it spawns before that
    /// is outside the job, which is harmless for `just` (it parses the justfile before
    /// starting a shell). If the job cannot be set up the child still runs, and `kill_tree`
    /// falls back to `taskkill`.
    fn spawn_group(&self, mut cmd: Command) -> io::Result<Child> {
        cmd.creation_flags(CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW);
        let child = cmd.spawn()?;
        if let Ok(job) = job_for(&child) {
            // A stale entry for a reused pid belongs to a reaped child; dropping it kills
            // whatever that child left behind.
            jobs().insert(child.id(), job);
        }
        Ok(child)
    }

    /// For a `spawn_group` child, closes its Job Object, which terminates every process in the
    /// job (the leader and all descendants, orphaned or not) and releases the job. Otherwise
    /// repeats `taskkill /T /F` (up to 3 passes) because descendants started while a pass walks
    /// the tree can be missed. "Not found" (exit code 128) is success, like ESRCH on Unix.
    fn kill_tree(&self, pid: u32) -> io::Result<()> {
        const NOT_FOUND: i32 = 128;
        let job = jobs().remove(&pid);
        if let Some(job) = job {
            drop(job);
            return Ok(());
        }
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

    /// `just` from `PATH`; when only a `just.cmd`/`just.bat` shim is there
    /// (which `Command::new("just")` does not find), that shim.
    fn just(&self) -> Result<JustCommand, JustError> {
        match detect_just() {
            Err(JustError::Missing) => {
                let path = std::env::var_os("PATH").unwrap_or_default();
                let pathext = std::env::var("PATHEXT").ok();
                match find_just_on_path(&path, pathext.as_deref(), &Path::is_file) {
                    Some(shim) => detect_just_at(shim),
                    None => Err(JustError::Missing),
                }
            }
            other => other,
        }
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

    #[test]
    fn kill_tree_kills_orphaned_grandchildren() {
        // powershell starts `ping -n 30`, prints its pid and exits, orphaning it: `taskkill /T`
        // can no longer find it from the leader, but the job still holds it.
        let script = "$p = Start-Process ping -ArgumentList '-n','30','127.0.0.1' -PassThru \
                      -WindowStyle Hidden; $p.Id";
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
        assert!(leader.wait().unwrap().success());
        assert!(
            is_alive(grandchild),
            "orphaned grandchild should be running"
        );

        WindowsPlatform.kill_tree(leader.id()).unwrap();

        let deadline = Instant::now() + Duration::from_secs(2);
        while is_alive(grandchild) {
            assert!(
                Instant::now() < deadline,
                "orphaned grandchild {grandchild} survived kill_tree"
            );
            std::thread::sleep(Duration::from_millis(50));
        }
    }
}
