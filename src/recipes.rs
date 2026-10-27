//! The `just` recipe contract (spec section 5): run `coverage` (then `test`
//! if it wrote no `JUnit`) in a measuring worktree, collect and normalize the
//! LCOV and `JUnit` outputs, and classify the result into a record [`Status`].
//!
//! Required recipes are detected with `just --summary`, which omits private
//! recipes (`[private]` or a leading `_`); a private `coverage` or `test`
//! is reported as missing.

use std::fmt::Write as _;
use std::io::Read;
use std::path::{Component, Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex, PoisonError, mpsc};
use std::time::{Duration, Instant};

use anyhow::{Context, anyhow, bail};
use devctl_junit::TestRun;
use devctl_lcov::Report;
use devctl_platform::{JustCommand, Platform};
use devctl_store::{RawReports, Status};

use crate::interrupt::Interrupt;

/// One recipe-contract run in a measuring worktree.
pub struct RecipeRun<'a> {
    pub platform: &'a dyn Platform,
    pub just: &'a JustCommand,
    /// The measuring worktree root; recipes run here.
    pub root: &'a Path,
    /// Commit hex, passed to recipes as `DEVCTL_COMMIT`.
    pub commit: &'a str,
    /// Per-recipe limit (`record.timeout_minutes`).
    pub timeout: Duration,
    /// Polled every second; `true` kills the running recipe.
    pub cancel: &'a dyn Fn() -> bool,
    /// Ctrl+C: polled with `cancel`, and told while a recipe runs.
    pub interrupt: &'a Interrupt,
}

/// How one recipe run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecipeExit {
    Exited {
        success: bool,
        stderr_tail: String,
    },
    Cancelled,
    /// Killed after `RecipeRun::timeout`; `stderr_tail` shows what it was
    /// doing.
    TimedOut {
        stderr_tail: String,
    },
}

/// What [`run_contract`] produced.
#[derive(Debug, Clone, PartialEq)]
pub enum ContractResult {
    Done {
        status: Status,
        report: Option<Report>,
        tests: Option<TestRun>,
        raw: RawReports,
    },
    Cancelled,
}

/// Recipes every project must define, in the order they run.
const REQUIRED: [&str; 2] = ["coverage", "test"];
/// LCOV output, relative to the worktree root.
const COVERAGE_DIR: &str = "coverage";
/// `JUnit` output, relative to the worktree root.
const TEST_RESULTS_DIR: &str = "test-results";
/// How much of a recipe's stderr is kept.
const STDERR_TAIL_BYTES: usize = 4096;
/// How often `cancel` is polled.
const CANCEL_POLL: Duration = Duration::from_secs(1);
/// How often the child is checked for exit and the timeout.
const WAIT_TICK: Duration = Duration::from_millis(50);
/// How long to wait for the stderr reader after the recipe ends. Leftover
/// processes are killed with the tree, but one that escaped it (e.g. via
/// `setsid`) could hold the pipe open forever.
const READER_JOIN: Duration = Duration::from_secs(2);

/// Whether the worktree root itself holds a justfile (`justfile` in any case,
/// or `.justfile`). Without one, `just` would search parent directories and
/// could find the main repository's justfile.
fn has_justfile(root: &Path) -> anyhow::Result<bool> {
    let unreadable = || {
        format!(
            "cannot read worktree directory {}; check its permissions, or delete it and run `devctl record`",
            root.display()
        )
    };
    for entry in std::fs::read_dir(root).with_context(unreadable)? {
        let name = entry
            .with_context(unreadable)?
            .file_name()
            .to_string_lossy()
            .to_ascii_lowercase();
        if name == "justfile" || name == ".justfile" {
            return Ok(true);
        }
    }
    Ok(false)
}

/// `just` settings from the environment that would change which justfile,
/// directory, shell or dotenv file is used, or stop recipes from running.
const JUST_ENV: [&str; 14] = [
    "JUST_JUSTFILE",
    "JUST_JUSTFILE_NAME",
    "JUST_WORKING_DIRECTORY",
    "JUST_CEILING",
    "JUST_SHELL",
    "JUST_DOTENV_COMMAND",
    "JUST_DOTENV_FILENAME",
    "JUST_DOTENV_PATH",
    "JUST_NO_DOTENV",
    "JUST_DRY_RUN",
    "JUST_NO_DEPS",
    "JUST_ALLOW_MISSING",
    "JUST_GROUP",
    "JUST_QUIET",
];

/// A `just` command in `root` with repository-selecting git variables
/// removed (devctl may run inside a git hook, and recipes must see the
/// measuring worktree, not the hook's repository) and with the developer's
/// `just` overrides ([`JUST_ENV`]) removed.
fn just_command(just: &JustCommand, root: &Path) -> Command {
    let mut cmd = just.command();
    for var in crate::git::REPO_ENV.into_iter().chain(JUST_ENV) {
        cmd.env_remove(var);
    }
    cmd.current_dir(root).stdin(Stdio::null());
    cmd
}

fn summary_command(just: &JustCommand, root: &Path) -> Command {
    let mut cmd = just_command(just, root);
    cmd.arg("--summary");
    cmd
}

fn recipe_command(run: &RecipeRun, name: &str) -> Command {
    let mut cmd = just_command(run.just, run.root);
    cmd.arg(name)
        .env("DEVCTL", "1")
        .env("DEVCTL_COMMIT", run.commit)
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    cmd
}

/// Required recipes absent from the worktree's justfile, in contract order.
/// A worktree without a justfile is missing all of them.
///
/// # Errors
/// If `just --summary` cannot run or fails (e.g. the justfile does not parse).
pub fn missing_recipes(just: &JustCommand, root: &Path) -> anyhow::Result<Vec<&'static str>> {
    if !has_justfile(root)? {
        return Ok(REQUIRED.to_vec());
    }
    let out = summary_command(just, root).output().with_context(|| {
        format!(
            "cannot run `{} --summary` in {}; check that just is installed",
            just.program.display(),
            root.display()
        )
    })?;
    if !out.status.success() {
        bail!(
            "`just --summary` failed in {}: {}; fix the justfile and commit again",
            root.display(),
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    let summary = String::from_utf8_lossy(&out.stdout);
    let names: Vec<&str> = summary.split_whitespace().collect();
    Ok(REQUIRED
        .into_iter()
        .filter(|r| !names.contains(r))
        .collect())
}

/// Keeps the last [`STDERR_TAIL_BYTES`] of a pipe, read on its own thread.
struct StderrTail {
    buf: Arc<Mutex<Vec<u8>>>,
    done: mpsc::Receiver<()>,
}

impl StderrTail {
    fn spawn(mut pipe: impl Read + Send + 'static) -> StderrTail {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let (tx, done) = mpsc::channel();
        let shared = Arc::clone(&buf);
        std::thread::spawn(move || {
            let mut chunk = [0u8; 8192];
            loop {
                match pipe.read(&mut chunk) {
                    Ok(0) => break,
                    Ok(n) => {
                        let mut b = shared.lock().unwrap_or_else(PoisonError::into_inner);
                        b.extend_from_slice(&chunk[..n]);
                        if b.len() > STDERR_TAIL_BYTES {
                            let excess = b.len() - STDERR_TAIL_BYTES;
                            b.drain(..excess);
                        }
                    }
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => {}
                    Err(_) => break,
                }
            }
            let _ = tx.send(());
        });
        StderrTail { buf, done }
    }

    /// Waits up to [`READER_JOIN`] for EOF, then returns what was read. If a
    /// leftover process still holds the pipe, the reader thread is left to
    /// end on its own when that process exits; its buffer stays bounded.
    fn finish(self) -> String {
        let _ = self.done.recv_timeout(READER_JOIN);
        let bytes = self
            .buf
            .lock()
            .unwrap_or_else(PoisonError::into_inner)
            .clone();
        // Skip a UTF-8 sequence cut by the window.
        let start = bytes
            .iter()
            .take(3)
            .take_while(|&&b| b & 0xC0 == 0x80)
            .count();
        String::from_utf8_lossy(&bytes[start..]).into_owned()
    }
}

/// How waiting on a recipe ended.
enum Waited {
    Exited(bool),
    Cancelled,
    TimedOut,
}

fn wait_for(run: &RecipeRun, child: &mut Child) -> anyhow::Result<Waited> {
    let start = Instant::now();
    let mut next_poll = start;
    loop {
        if let Some(status) = child.try_wait().context("cannot wait for recipe")? {
            // Kill whatever the recipe left running, so it cannot hold the
            // stderr pipe or files in the worktree. Safe after reaping: on
            // Unix the group id cannot be reused while members live, and on
            // Windows the open `child` handle prevents pid reuse.
            let _ = run.platform.kill_tree(child.id());
            return Ok(Waited::Exited(status.success()));
        }
        let now = Instant::now();
        let elapsed = now.duration_since(start);
        let outcome = if elapsed >= run.timeout {
            Some(Waited::TimedOut)
        } else if now >= next_poll {
            next_poll = now + CANCEL_POLL;
            (run.interrupt.is_set() || (run.cancel)()).then_some(Waited::Cancelled)
        } else {
            None
        };
        if let Some(outcome) = outcome {
            kill(run.platform, child);
            return Ok(outcome);
        }
        let left = run.timeout.saturating_sub(elapsed);
        std::thread::sleep(WAIT_TICK.min(left).max(Duration::from_millis(1)));
    }
}

/// Kills the recipe's process tree and reaps the leader.
fn kill(platform: &dyn Platform, child: &mut Child) {
    if let Err(e) = platform.kill_tree(child.id()) {
        tracing::warn!(
            error = %e,
            pid = child.id(),
            "kill_tree failed; killing the recipe process only"
        );
        let _ = child.kill();
    }
    let _ = child.wait();
}

/// Runs `just <name>` in the worktree with `DEVCTL=1` and
/// `DEVCTL_COMMIT=<commit>`, stdout discarded and the last 4096 bytes of
/// stderr kept. Cancellation (polled every second) and the timeout kill the
/// whole process tree.
///
/// # Errors
/// If the recipe cannot be spawned or waited on.
pub fn run_recipe(run: &RecipeRun, name: &str) -> anyhow::Result<RecipeExit> {
    let span = tracing::info_span!("recipe.run", recipe = name);
    let _entered = span.enter();
    let start = Instant::now();
    let running = run.interrupt.recipe_running();
    let mut child = run
        .platform
        .spawn_group(recipe_command(run, name))
        .with_context(|| {
            format!(
                "cannot run `just {name}` in {}; check that just is installed",
                run.root.display()
            )
        })?;
    let tail = child.stderr.take().map(StderrTail::spawn);
    let waited = wait_for(run, &mut child);
    if waited.is_err() {
        kill(run.platform, &mut child);
    }
    let stderr_tail = tail.map(StderrTail::finish).unwrap_or_default();
    // Dropped before the flag is read again: see `Interrupt::signal`.
    drop(running);
    let waited = match waited? {
        // Ctrl+C as the recipe ended: the handler left the exit to us.
        Waited::Exited(_) if run.interrupt.is_set() => Waited::Cancelled,
        w => w,
    };
    tracing::info!(
        histogram.devctl.recipe.duration = start.elapsed().as_secs_f64(),
        recipe = name
    );
    Ok(match waited {
        Waited::Exited(success) => {
            tracing::info!(success, "recipe exited");
            RecipeExit::Exited {
                success,
                stderr_tail,
            }
        }
        Waited::Cancelled => {
            tracing::info!("recipe cancelled");
            RecipeExit::Cancelled
        }
        Waited::TimedOut => {
            tracing::warn!("recipe timed out");
            RecipeExit::TimedOut { stderr_tail }
        }
    })
}

fn unreadable_dir(dir: &Path) -> String {
    format!(
        "cannot read {}; check permissions on the measuring worktree, or delete it and run `devctl record`",
        dir.display()
    )
}

/// Files in `root/dir` with extension `ext` (any case), sorted by name.
fn outputs(root: &Path, dir: &str, ext: &str) -> anyhow::Result<Vec<PathBuf>> {
    let dir = root.join(dir);
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| unreadable_dir(&dir)),
    };
    let mut files = Vec::new();
    for entry in entries {
        let path = entry.with_context(|| unreadable_dir(&dir))?.path();
        let matches = path
            .extension()
            .is_some_and(|e| e.to_string_lossy().eq_ignore_ascii_case(ext));
        if matches && path.is_file() {
            files.push(path);
        }
    }
    files.sort();
    Ok(files)
}

fn read_output(path: &Path) -> anyhow::Result<String> {
    let bytes = std::fs::read(path).with_context(|| {
        format!(
            "cannot read {}; check the recipe that writes it",
            path.display()
        )
    })?;
    String::from_utf8(bytes).map_err(|_| {
        anyhow!(
            "{} is not valid UTF-8; fix the recipe that writes it",
            path.display()
        )
    })
}

/// Rewrites an `SF` path to a worktree-relative `/` path, or `None` when it
/// lies outside the worktree. `root_prefix` is the normalized root plus `/`.
fn relativize(platform: &dyn Platform, root: &Path, root_prefix: &str, sf: &str) -> Option<String> {
    let given = Path::new(sf);
    let abs = if given.is_absolute() {
        given.to_path_buf()
    } else {
        root.join(given)
    };
    let normalized = platform.normalize_path(&abs);
    if let Some(rel) = normalized.strip_prefix(root_prefix) {
        return (!rel.is_empty()).then(|| rel.to_string());
    }
    // A file that does not exist cannot be canonicalized, so its spelling
    // may differ from the canonical root (e.g. macOS /var vs /private/var).
    // Accept it when it is lexically under the root as given.
    if abs.exists() {
        return None;
    }
    let rest = abs.strip_prefix(root).ok()?;
    let parts: Option<Vec<String>> = rest
        .components()
        .map(|c| match c {
            Component::Normal(s) => Some(s.to_string_lossy().into_owned()),
            _ => None,
        })
        .collect();
    parts.filter(|p| !p.is_empty()).map(|p| p.join("/"))
}

/// Parses `coverage/*.info` (merged, paths made worktree-relative, files
/// outside the worktree dropped) and `test-results/*.xml` (merged). Either is
/// `None` when no file exists. `raw` holds the normalized merged LCOV and
/// each `JUnit` file as written.
///
/// # Errors
/// If a result file cannot be read, is not UTF-8, or does not parse; the
/// message names the file.
pub fn collect(
    root: &Path,
    platform: &dyn Platform,
) -> anyhow::Result<(Option<Report>, Option<TestRun>, RawReports)> {
    let _span = tracing::info_span!("recipe.collect").entered();
    let mut raw = RawReports::default();

    let lcov_files = outputs(root, COVERAGE_DIR, "info")?;
    let report = if lcov_files.is_empty() {
        None
    } else {
        let mut reports = Vec::with_capacity(lcov_files.len());
        for path in &lcov_files {
            let text = read_output(path)?;
            reports.push(devctl_lcov::parse(&text).with_context(|| {
                format!(
                    "cannot parse LCOV file {}; fix the `coverage` recipe",
                    path.display()
                )
            })?);
        }
        let mut prefix = platform.normalize_path(root);
        if !prefix.ends_with('/') {
            prefix.push('/');
        }
        let mut dropped = 0_usize;
        let mut examples = Vec::new();
        let merged = Report::merge(&reports).map_paths(|sf| {
            let rel = relativize(platform, root, &prefix, sf);
            if rel.is_none() {
                dropped += 1;
                if examples.len() < 2 {
                    examples.push(sf.to_string());
                }
            }
            rel
        });
        if dropped > 0 {
            tracing::warn!(
                target: crate::telemetry::PATH_TARGET,
                dropped,
                examples = ?examples,
                "dropped LCOV files outside the worktree"
            );
            tracing::warn!(dropped, "dropped LCOV files outside the worktree");
        }
        raw.lcov = merged.to_lcov();
        Some(merged)
    };

    let junit_files = outputs(root, TEST_RESULTS_DIR, "xml")?;
    let tests = if junit_files.is_empty() {
        None
    } else {
        let mut runs = Vec::with_capacity(junit_files.len());
        for path in &junit_files {
            let text = read_output(path)?;
            runs.push(devctl_junit::parse(&text).with_context(|| {
                format!(
                    "cannot parse JUnit file {}; fix the recipe that writes it",
                    path.display()
                )
            })?);
            raw.junit.push(text);
        }
        Some(TestRun::merge(runs))
    };
    Ok((report, tests, raw))
}

/// Heads each recipe's stderr in an error status: `` `just <name>` failed: ``
/// or `` `just <name>` was killed: ``.
fn output_header(name: &str, killed: bool) -> String {
    let how = if killed { "was killed" } else { "failed" };
    format!("`just {name}` {how}:")
}

/// Stands in for a header cut off by the stderr limit.
const CUT_HEADER: &str = "(earlier recipe output cut)";

fn is_output_header(line: &str) -> bool {
    line == CUT_HEADER
        || (line.starts_with("`just ")
            && (line.ends_with("` failed:") || line.ends_with("` was killed:")))
}

/// `just`'s own report of a failing recipe (`error: recipe `x` failed on line
/// 3 with exit code 2`), which says nothing the message does not.
fn is_just_failure_line(line: &str) -> bool {
    let lower = line.to_ascii_lowercase();
    lower.starts_with("error: recipe `") && lower.contains("` failed")
}

/// An error status ending with the one-line `msg`, after as much of the
/// recipes' stderr (`stderr`, headed by [`output_header`]s) as fits in the
/// kept tail. Output cut at the front starts with [`CUT_HEADER`], so
/// [`error_detail`] can still tell it from devctl's message.
fn error_with_stderr(msg: &str, stderr: &str) -> Status {
    let tail = stderr.trim();
    if tail.is_empty() {
        return Status::error(msg);
    }
    let budget = STDERR_TAIL_BYTES.saturating_sub(msg.len() + CUT_HEADER.len() + 2);
    if tail.len() <= budget {
        return Status::error(&format!("{tail}\n{msg}"));
    }
    let mut start = tail.len() - budget;
    while !tail.is_char_boundary(start) {
        start += 1;
    }
    Status::error(&format!("{CUT_HEADER}\n{}\n{msg}", &tail[start..]))
}

/// One line describing an error record, for `devctl status` and the gate.
/// When the record holds recipe output: the last non-blank line the recipes
/// printed (not `just`'s own "recipe failed" line), a dash, and devctl's
/// message (the last line), e.g. `compile error: x — `just coverage` exited
/// non-zero ...`. Otherwise the record is devctl's own error, whose first
/// line says what failed.
#[must_use]
pub fn error_detail(stderr_tail: &str) -> String {
    let lines: Vec<&str> = stderr_tail
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect();
    let Some((&msg, before)) = lines.split_last() else {
        return "(no output)".to_string();
    };
    if !before.iter().any(|l| is_output_header(l)) {
        return lines[0].to_string();
    }
    match before
        .iter()
        .rev()
        .find(|l| !is_output_header(l) && !is_just_failure_line(l))
    {
        Some(own) => format!("{own} \u{2014} {msg}"),
        None => msg.to_string(),
    }
}

/// The error for missing `what` output (`JUnit XML in test-results/*.xml`):
/// after a failing recipe, point at its error rather than at the contract.
fn missing_output(failed: &[&str], what: &str, fix: &str) -> String {
    match failed.last() {
        Some(name) => format!(
            "`just {name}` exited non-zero and wrote no {what}; fix the error above, \
             then run `devctl record`"
        ),
        None => fix.to_string(),
    }
}

/// The record status for a contract run, given the recipes that exited
/// non-zero (`failed`, in run order): no LCOV or no `JUnit` output is an
/// error; a failed or errored test case, or a non-zero exit, is
/// `TestsFailed`; otherwise `Ok`. A `JUnit` run with no test cases is not by
/// itself an error.
#[must_use]
pub fn classify(
    failed: &[&str],
    stderr_tail: &str,
    report: Option<&Report>,
    tests: Option<&TestRun>,
) -> Status {
    let Some(tests) = tests else {
        let msg = missing_output(
            failed,
            "JUnit XML",
            "no JUnit output in test-results/*.xml; make `just test` write JUnit XML there",
        );
        return error_with_stderr(&msg, stderr_tail);
    };
    if report.is_none() {
        let coverage_failed: Vec<&str> = failed
            .iter()
            .copied()
            .filter(|n| *n == "coverage")
            .collect();
        let msg = missing_output(
            &coverage_failed,
            "LCOV",
            "no LCOV output in coverage/*.info; make `just coverage` write LCOV there",
        );
        return error_with_stderr(&msg, stderr_tail);
    }
    let counts = tests.counts();
    if !failed.is_empty() || counts.failed > 0 || counts.errored > 0 {
        Status::TestsFailed
    } else {
        Status::Ok
    }
}

/// The timeout error; whole minutes, or two decimals for sub-minute limits.
fn timeout_message(name: &str, timeout: Duration) -> String {
    let secs = timeout.as_secs();
    let minutes = if secs.is_multiple_of(60) && timeout.subsec_nanos() == 0 {
        (secs / 60).to_string()
    } else {
        format!("{:.2}", timeout.as_secs_f64() / 60.0)
    };
    format!(
        "recipe `{name}` exceeded record.timeout_minutes ({minutes}); its process tree was killed. \
         Raise record.timeout_minutes in .devctl.toml or make `just {name}` faster"
    )
}

fn done_error(message: &str) -> ContractResult {
    ContractResult::Done {
        status: Status::error(message),
        report: None,
        tests: None,
        raw: RawReports::default(),
    }
}

fn clear_dir(root: &Path, dir: &str) -> anyhow::Result<()> {
    let path = root.join(dir);
    match std::fs::remove_dir_all(&path) {
        Ok(()) => Ok(()),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(e) => Err(e).with_context(|| {
            format!(
                "cannot remove stale {}; check permissions on the measuring worktree, or delete it and run `devctl record`",
                path.display()
            )
        }),
    }
}

/// Runs the whole contract (spec section 5): clear `coverage/` and
/// `test-results/`, check the required recipes, run `coverage`, run `test`
/// unless `coverage` already wrote `JUnit`, then collect and classify.
///
/// Problems with the project (missing recipes, a broken justfile, a timeout,
/// unreadable outputs) become an `Error` status rather than an `Err`.
///
/// # Errors
/// If the output directories cannot be cleared or `just` cannot be spawned.
pub fn run_contract(run: &RecipeRun) -> anyhow::Result<ContractResult> {
    clear_dir(run.root, COVERAGE_DIR)?;
    clear_dir(run.root, TEST_RESULTS_DIR)?;
    match missing_recipes(run.just, run.root) {
        Ok(missing) => {
            if let Some(name) = missing.first() {
                return Ok(done_error(&format!(
                    "missing recipe `{name}`; add it to the justfile (`devctl init` will generate one in M2)"
                )));
            }
        }
        Err(e) => return Ok(done_error(&format!("{e:#}"))),
    }

    let mut failed = Vec::new();
    let mut stderr = String::new();
    for name in REQUIRED {
        if name == "test" && !outputs(run.root, TEST_RESULTS_DIR, "xml")?.is_empty() {
            tracing::info!("coverage wrote JUnit; skipping `just test`");
            continue;
        }
        match run_recipe(run, name)? {
            RecipeExit::Exited {
                success,
                stderr_tail,
            } => {
                if !success {
                    failed.push(name);
                    let _ = writeln!(
                        stderr,
                        "{}\n{}",
                        output_header(name, false),
                        stderr_tail.trim_end()
                    );
                }
            }
            RecipeExit::Cancelled => return Ok(ContractResult::Cancelled),
            RecipeExit::TimedOut { stderr_tail } => {
                let _ = write!(
                    stderr,
                    "{}\n{}",
                    output_header(name, true),
                    stderr_tail.trim_end()
                );
                return Ok(ContractResult::Done {
                    status: error_with_stderr(&timeout_message(name, run.timeout), &stderr),
                    report: None,
                    tests: None,
                    raw: RawReports::default(),
                });
            }
        }
    }

    Ok(match collect(run.root, run.platform) {
        Ok((report, tests, raw)) => ContractResult::Done {
            status: classify(&failed, &stderr, report.as_ref(), tests.as_ref()),
            report,
            tests,
            raw,
        },
        Err(e) => done_error(&format!("{e:#}")),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};
    use std::time::Instant;

    const LCOV: &str = "SF:src/a.rs\nDA:1,1\nDA:2,0\nend_of_record\n";
    const JUNIT_PASS: &str =
        r#"<testsuites><testsuite name="s"><testcase name="a"/></testsuite></testsuites>"#;
    const JUNIT_FAIL: &str = r#"<testsuites><testsuite name="s"><testcase name="a"><failure/></testcase></testsuite></testsuites>"#;

    struct Fixture {
        dir: tempfile::TempDir,
        platform: Box<dyn Platform>,
        just: JustCommand,
    }

    impl Fixture {
        fn new(justfile: &str) -> Fixture {
            let dir = tempfile::tempdir().unwrap();
            std::fs::write(dir.path().join("justfile"), justfile).unwrap();
            let platform = devctl_platform::current();
            let just = platform.just().expect("just >= 1.59.0 on PATH");
            Fixture {
                dir,
                platform,
                just,
            }
        }

        fn root(&self) -> &Path {
            self.dir.path()
        }

        fn write(&self, rel: &[&str], contents: &str) {
            let path = rel.iter().fold(self.root().to_path_buf(), |p, c| p.join(c));
            std::fs::create_dir_all(path.parent().unwrap()).unwrap();
            std::fs::write(path, contents).unwrap();
        }

        fn run_with(&self, timeout: Duration, cancel: &dyn Fn() -> bool) -> ContractResult {
            self.run_interruptible(timeout, cancel, &Interrupt::new())
        }

        fn run_interruptible(
            &self,
            timeout: Duration,
            cancel: &dyn Fn() -> bool,
            interrupt: &Interrupt,
        ) -> ContractResult {
            let run = RecipeRun {
                platform: self.platform.as_ref(),
                just: &self.just,
                root: self.root(),
                commit: "abc123",
                timeout,
                cancel,
                interrupt,
            };
            run_contract(&run).unwrap()
        }

        fn run(&self) -> ContractResult {
            self.run_with(Duration::from_secs(60), &|| false)
        }
    }

    fn status_of(r: &ContractResult) -> &Status {
        match r {
            ContractResult::Done { status, .. } => status,
            ContractResult::Cancelled => panic!("unexpected Cancelled"),
        }
    }

    fn error_text(s: &Status) -> &str {
        match s {
            Status::Error { stderr_tail } => stderr_tail,
            other => panic!("expected Error, got {other:?}"),
        }
    }

    /// A justfile whose `coverage` writes LCOV (and optionally `JUnit`), and
    /// whose `test` writes `JUnit` plus a marker file.
    fn contract_justfile(coverage_junit: bool) -> String {
        let junit = if coverage_junit {
            format!(
                "\n    mkdir -p test-results\n    printf '%s' '{JUNIT_PASS}' > test-results/c.xml"
            )
        } else {
            String::new()
        };
        format!(
            "coverage:\n    mkdir -p coverage\n    printf 'SF:src/a.rs\\nDA:1,1\\nend_of_record\\n' > coverage/a.info{junit}\n\n\
             test:\n    touch test-ran\n    mkdir -p test-results\n    printf '%s' '{JUNIT_PASS}' > test-results/t.xml\n"
        )
    }

    #[test]
    fn classify_table() {
        let report = devctl_lcov::parse(LCOV).unwrap();
        let pass = devctl_junit::parse(JUNIT_PASS).unwrap();
        let fail = devctl_junit::parse(JUNIT_FAIL).unwrap();
        assert_eq!(classify(&[], "", Some(&report), Some(&pass)), Status::Ok);
        assert_eq!(
            classify(&["coverage"], "boom", Some(&report), Some(&pass)),
            Status::TestsFailed
        );
        let e = classify(&[], "", Some(&report), None);
        assert_eq!(
            error_text(&e),
            "no JUnit output in test-results/*.xml; make `just test` write JUnit XML there"
        );
        let e = classify(&["coverage"], "boom", Some(&report), None);
        assert_eq!(
            error_text(&e),
            "boom\n`just coverage` exited non-zero and wrote no JUnit XML; fix the error above, \
             then run `devctl record`"
        );
        let e = classify(&[], "", None, Some(&pass));
        assert!(error_text(&e).contains("no LCOV output in coverage/*.info"));
        let e = classify(&["coverage"], "boom", None, Some(&pass));
        assert!(
            error_text(&e).ends_with(
                "`just coverage` exited non-zero and wrote no LCOV; fix the error above, \
                 then run `devctl record`"
            ),
            "{e:?}"
        );
        assert_eq!(
            classify(&[], "", Some(&report), Some(&fail)),
            Status::TestsFailed
        );
    }

    #[test]
    fn error_detail_pairs_the_recipe_line_with_the_message() {
        let msg = "`just coverage` exited non-zero and wrote no JUnit XML; fix the error above, \
                   then run `devctl record`";
        let tail = format!(
            "`just coverage` failed:\ncompile error: x\n\nerror: recipe `coverage` failed on line 3 \
             with exit code 2\n{msg}"
        );
        assert_eq!(
            error_detail(&tail),
            format!("compile error: x \u{2014} {msg}")
        );
        // A recipe that printed nothing: the message alone.
        assert_eq!(
            error_detail(&format!("`just coverage` failed:\n\n{msg}")),
            msg
        );
        // Two failing recipes: the last line any of them printed.
        assert_eq!(
            error_detail(&format!(
                "`just coverage` failed:\nA\n`just test` failed:\n\n{msg}"
            )),
            format!("A \u{2014} {msg}")
        );
        // devctl's own error without recipe output: its first line.
        assert_eq!(
            error_detail("missing recipe `test`; add it to the justfile"),
            "missing recipe `test`; add it to the justfile"
        );
        assert_eq!(
            error_detail("`just --summary` failed in /w: error: bad\n  |\n1 | x; fix it"),
            "`just --summary` failed in /w: error: bad"
        );
        assert_eq!(error_detail(" \n"), "(no output)");
    }

    #[test]
    fn long_recipe_output_keeps_its_header_and_the_message() {
        let msg = "the message";
        let stderr = format!(
            "`just coverage` failed:\n{}\nlast words",
            "x".repeat(10_000)
        );
        let Status::Error { stderr_tail } = error_with_stderr(msg, &stderr) else {
            unreachable!()
        };
        assert!(
            stderr_tail.len() <= STDERR_TAIL_BYTES,
            "{}",
            stderr_tail.len()
        );
        assert!(stderr_tail.ends_with("last words\nthe message"));
        assert_eq!(
            error_detail(&stderr_tail),
            "last words \u{2014} the message"
        );
    }

    #[test]
    fn classify_empty_junit_is_not_an_error() {
        let report = devctl_lcov::parse(LCOV).unwrap();
        let empty = devctl_junit::parse("<testsuites/>").unwrap();
        assert_eq!(classify(&[], "", Some(&report), Some(&empty)), Status::Ok);
    }

    #[test]
    fn skips_test_recipe_when_coverage_wrote_junit() {
        let f = Fixture::new(&contract_justfile(true));
        let r = f.run();
        assert_eq!(status_of(&r), &Status::Ok);
        assert!(!f.root().join("test-ran").exists(), "test recipe ran");
    }

    #[test]
    fn runs_test_recipe_otherwise() {
        let f = Fixture::new(&contract_justfile(false));
        let r = f.run();
        assert_eq!(status_of(&r), &Status::Ok, "{r:?}");
        assert!(
            f.root().join("test-ran").exists(),
            "test recipe did not run"
        );
        let ContractResult::Done {
            report, tests, raw, ..
        } = r
        else {
            unreachable!()
        };
        assert_eq!(
            report.unwrap().files.keys().collect::<Vec<_>>(),
            vec!["src/a.rs"]
        );
        assert_eq!(tests.unwrap().counts().passed, 1);
        assert!(raw.lcov.contains("SF:src/a.rs"), "{}", raw.lcov);
        assert_eq!(raw.junit, vec![JUNIT_PASS.to_string()]);
    }

    #[test]
    fn failing_tests_recipe_is_tests_failed() {
        let f = Fixture::new(&format!(
            "coverage:\n    mkdir -p coverage\n    printf 'SF:a.rs\\nDA:1,1\\nend_of_record\\n' > coverage/a.info\n\n\
             test:\n    mkdir -p test-results\n    printf '%s' '{JUNIT_FAIL}' > test-results/t.xml\n    echo failing >&2\n    exit 1\n"
        ));
        assert_eq!(status_of(&f.run()), &Status::TestsFailed);
    }

    #[test]
    fn failing_recipe_without_junit_is_error_with_stderr() {
        let f = Fixture::new(
            "coverage:\n    echo compile-error-xyz >&2\n    exit 2\n\ntest:\n    echo test-broke >&2\n    @exit 3\n",
        );
        let r = f.run();
        let text = error_text(status_of(&r));
        assert!(text.contains("compile-error-xyz"), "{text}");
        assert!(text.contains("test-broke"), "{text}");
        assert!(
            text.ends_with(
                "`just test` exited non-zero and wrote no JUnit XML; fix the error above, \
                 then run `devctl record`"
            ),
            "{text}"
        );
        assert_eq!(
            error_detail(text),
            "test-broke \u{2014} `just test` exited non-zero and wrote no JUnit XML; fix the \
             error above, then run `devctl record`"
        );
    }

    #[test]
    fn coverage_failing_after_writing_lcov_names_its_error() {
        // The review's repro: LCOV written, an error printed, exit 2, no JUnit;
        // `test` exits 0 without writing JUnit either.
        let f = Fixture::new(
            "coverage:\n    mkdir -p coverage\n    printf 'SF:src/a.rs\\nDA:1,1\\nend_of_record\\n' > coverage/a.info\n    @echo 'compile error: x' >&2\n    @exit 2\n\ntest:\n    true\n",
        );
        let r = f.run();
        let text = error_text(status_of(&r));
        assert_eq!(
            error_detail(text),
            "compile error: x \u{2014} `just coverage` exited non-zero and wrote no JUnit XML; \
             fix the error above, then run `devctl record`"
        );
    }

    #[test]
    fn passes_devctl_env() {
        let f = Fixture::new(
            "coverage:\n    echo $DEVCTL $DEVCTL_COMMIT > env.txt\n\ntest:\n    true\n",
        );
        let _ = f.run();
        let env = std::fs::read_to_string(f.root().join("env.txt")).unwrap();
        assert_eq!(env.trim(), "1 abc123");
    }

    #[test]
    fn recipe_commands_drop_repo_env() {
        let f = Fixture::new("coverage:\n    true\n");
        let run = RecipeRun {
            platform: f.platform.as_ref(),
            just: &f.just,
            root: f.root(),
            commit: "abc123",
            timeout: Duration::from_secs(60),
            cancel: &|| false,
            interrupt: &Interrupt::new(),
        };
        for cmd in [
            recipe_command(&run, "coverage"),
            summary_command(&f.just, f.root()),
        ] {
            let envs: Vec<_> = cmd.get_envs().collect();
            for var in crate::git::REPO_ENV {
                assert!(
                    envs.iter()
                        .any(|(k, v)| *k == std::ffi::OsStr::new(var) && v.is_none()),
                    "{var} not removed: {envs:?}"
                );
            }
            for var in [
                "JUST_JUSTFILE",
                "JUST_JUSTFILE_NAME",
                "JUST_WORKING_DIRECTORY",
                "JUST_CEILING",
                "JUST_SHELL",
                "JUST_DOTENV_COMMAND",
                "JUST_DOTENV_FILENAME",
                "JUST_DOTENV_PATH",
                "JUST_NO_DOTENV",
                "JUST_DRY_RUN",
            ] {
                assert!(
                    envs.iter()
                        .any(|(k, v)| *k == std::ffi::OsStr::new(var) && v.is_none()),
                    "{var} not removed: {envs:?}"
                );
            }
        }
    }

    #[test]
    fn missing_recipe_message() {
        let f = Fixture::new("coverage:\n    true\n");
        assert_eq!(missing_recipes(&f.just, f.root()).unwrap(), vec!["test"]);
        let text = error_text(status_of(&f.run())).to_string();
        assert!(
            text.contains("missing recipe `test`; add it to the justfile (`devctl init` will generate one in M2)"),
            "{text}"
        );
    }

    #[test]
    fn missing_justfile_is_missing_coverage() {
        let f = Fixture::new("");
        std::fs::remove_file(f.root().join("justfile")).unwrap();
        // A justfile in a parent directory must not be picked up.
        let inner = f.root().join("wt");
        std::fs::create_dir(&inner).unwrap();
        std::fs::write(f.root().join("justfile"), contract_justfile(true)).unwrap();
        assert_eq!(
            missing_recipes(&f.just, &inner).unwrap(),
            vec!["coverage", "test"]
        );
        let run = RecipeRun {
            platform: f.platform.as_ref(),
            just: &f.just,
            root: &inner,
            commit: "abc123",
            timeout: Duration::from_secs(60),
            cancel: &|| false,
            interrupt: &Interrupt::new(),
        };
        let r = run_contract(&run).unwrap();
        assert!(error_text(status_of(&r)).contains("missing recipe `coverage`; add it to the justfile (`devctl init` will generate one in M2)"));
    }

    #[test]
    fn lcov_paths_relativized_and_outside_dropped() {
        let f = Fixture::new("");
        f.write(&["src", "a.rs"], "");
        let root = f.root().to_string_lossy().replace('\\', "/");
        f.write(
            &["coverage", "x.info"],
            &format!(
                "SF:{root}/src/a.rs\nDA:1,1\nend_of_record\nSF:/elsewhere/b.rs\nDA:1,1\nend_of_record\n\
                 SF:src/c.rs\nDA:1,0\nend_of_record\nSF:../outside.rs\nDA:1,1\nend_of_record\n"
            ),
        );
        let (report, _, raw) = collect(f.root(), f.platform.as_ref()).unwrap();
        let report = report.unwrap();
        assert_eq!(
            report.files.keys().collect::<Vec<_>>(),
            vec!["src/a.rs", "src/c.rs"]
        );
        assert_eq!(raw.lcov, report.to_lcov());
    }

    #[test]
    fn collect_merges_files_and_reports_bad_ones() {
        let f = Fixture::new("");
        f.write(&["coverage", "a.info"], "SF:a.rs\nDA:1,1\nend_of_record\n");
        f.write(&["coverage", "b.info"], "SF:a.rs\nDA:1,2\nend_of_record\n");
        f.write(&["test-results", "a.xml"], JUNIT_PASS);
        f.write(&["test-results", "b.xml"], JUNIT_FAIL);
        let (report, tests, raw) = collect(f.root(), f.platform.as_ref()).unwrap();
        assert_eq!(report.unwrap().files["a.rs"].lines[&1], 3);
        let counts = tests.unwrap().counts();
        assert_eq!((counts.passed, counts.failed), (1, 1));
        assert_eq!(raw.junit.len(), 2);

        std::fs::write(f.root().join("coverage").join("c.info"), [0xff, 0xfe]).unwrap();
        let err = format!("{:#}", collect(f.root(), f.platform.as_ref()).unwrap_err());
        assert!(err.contains("c.info"), "{err}");
    }

    #[test]
    fn unreadable_result_is_error_status() {
        let f = Fixture::new(
            "coverage:\n    mkdir -p coverage test-results\n    printf 'SF:a.rs\\nDA:1,1\\nend_of_record\\n' > coverage/a.info\n    printf '<testsuites><bad' > test-results/broken.xml\n\ntest:\n    true\n",
        );
        let text = error_text(status_of(&f.run())).to_string();
        assert!(text.contains("broken.xml"), "{text}");
    }

    #[test]
    fn stale_outputs_removed_before_run() {
        let f = Fixture::new(&contract_justfile(true));
        f.write(
            &["coverage", "old.info"],
            "SF:old.rs\nDA:1,1\nend_of_record\n",
        );
        f.write(&["test-results", "old.xml"], JUNIT_FAIL);
        let r = f.run();
        assert_eq!(status_of(&r), &Status::Ok);
        let ContractResult::Done { report, .. } = r else {
            unreachable!()
        };
        assert!(!report.unwrap().files.contains_key("old.rs"));
    }

    /// The recipe starts a background loop that appends to `beat` and then
    /// blocks; after the run returns, `beat` must stop growing.
    const HANGING: &str = "coverage:\n    sh -c 'while true; do echo x >> beat; sleep 0.1; done' &\n    echo waiting-on-something >&2\n    sleep 30\n\ntest:\n    true\n";

    fn assert_tree_dead(root: &Path) {
        let beat = root.join("beat");
        let len = || std::fs::metadata(&beat).map_or(0, |m| m.len());
        let before = len();
        assert!(before > 0, "heartbeat never started");
        std::thread::sleep(Duration::from_millis(500));
        assert_eq!(before, len(), "background process still running");
    }

    #[test]
    fn cancel_kills_recipe_tree() {
        let f = Fixture::new(HANGING);
        let start = Instant::now();
        let cancel = move || start.elapsed() >= Duration::from_secs(1);
        let r = f.run_with(Duration::from_secs(60), &cancel);
        assert_eq!(r, ContractResult::Cancelled);
        assert!(
            start.elapsed() < Duration::from_secs(4),
            "{:?}",
            start.elapsed()
        );
        assert_tree_dead(f.root());
    }

    #[test]
    fn interrupt_kills_recipe_tree() {
        let f = Fixture::new(HANGING);
        let interrupt = Interrupt::new();
        let start = Instant::now();
        let r = std::thread::scope(|s| {
            s.spawn(|| {
                std::thread::sleep(Duration::from_secs(1));
                // A recipe is running, so the handler leaves it to the loop.
                assert_eq!(interrupt.signal(), crate::interrupt::OnSignal::Cancel);
            });
            f.run_interruptible(Duration::from_secs(60), &|| false, &interrupt)
        });
        assert_eq!(r, ContractResult::Cancelled);
        assert!(
            start.elapsed() < Duration::from_secs(4),
            "{:?}",
            start.elapsed()
        );
        assert_tree_dead(f.root());
        // The recipe guard is released.
        assert_eq!(interrupt.signal(), crate::interrupt::OnSignal::Exit);
    }

    #[test]
    fn interrupt_before_a_recipe_cancels_it() {
        let f = Fixture::new(&contract_justfile(true));
        let interrupt = Interrupt::new();
        let r = f.run_interruptible(Duration::from_secs(60), &|| false, &interrupt);
        assert!(matches!(r, ContractResult::Done { .. }));
        interrupt.signal();
        let r = f.run_interruptible(Duration::from_secs(60), &|| false, &interrupt);
        assert_eq!(r, ContractResult::Cancelled);
    }

    #[test]
    fn timeout_is_error() {
        let f = Fixture::new(HANGING);
        let start = Instant::now();
        let r = f.run_with(Duration::from_secs(1), &|| false);
        let text = error_text(status_of(&r)).to_string();
        assert!(
            text.contains("recipe `coverage` exceeded record.timeout_minutes"),
            "{text}"
        );
        assert!(text.contains("its process tree was killed"), "{text}");
        assert!(
            text.contains(
                "Raise record.timeout_minutes in .devctl.toml or make `just coverage` faster"
            ),
            "{text}"
        );
        assert!(
            text.contains("waiting-on-something"),
            "stderr tail kept: {text}"
        );
        assert!(
            start.elapsed() < Duration::from_secs(4),
            "{:?}",
            start.elapsed()
        );
        assert_tree_dead(f.root());
    }

    #[test]
    fn timeout_message_uses_minutes() {
        assert_eq!(
            timeout_message("coverage", Duration::from_secs(600)),
            "recipe `coverage` exceeded record.timeout_minutes (10); its process tree was killed. \
             Raise record.timeout_minutes in .devctl.toml or make `just coverage` faster"
        );
    }

    #[test]
    fn leftover_background_processes_killed_after_exit() {
        let f = Fixture::new(
            "coverage:\n    sh -c 'while true; do echo x >> beat; sleep 0.1; done' &\n    sleep 30 &\n    sleep 0.3\n",
        );
        let run = RecipeRun {
            platform: f.platform.as_ref(),
            just: &f.just,
            root: f.root(),
            commit: "abc123",
            timeout: Duration::from_secs(60),
            cancel: &|| false,
            interrupt: &Interrupt::new(),
        };
        let start = Instant::now();
        let exit = run_recipe(&run, "coverage").unwrap();
        assert!(
            matches!(exit, RecipeExit::Exited { success: true, .. }),
            "{exit:?}"
        );
        assert!(
            start.elapsed() < Duration::from_millis(1500),
            "{:?}",
            start.elapsed()
        );
        assert_tree_dead(f.root());
    }

    #[test]
    fn stderr_tail_keeps_last_4096_bytes() {
        let f = Fixture::new(
            "coverage:\n    i=0; while [ $i -lt 2000 ]; do echo line-$i >&2; i=$((i+1)); done; echo LAST-LINE >&2\n",
        );
        let run = RecipeRun {
            platform: f.platform.as_ref(),
            just: &f.just,
            root: f.root(),
            commit: "abc123",
            timeout: Duration::from_secs(60),
            cancel: &|| false,
            interrupt: &Interrupt::new(),
        };
        let RecipeExit::Exited {
            success,
            stderr_tail,
        } = run_recipe(&run, "coverage").unwrap()
        else {
            panic!("expected Exited")
        };
        assert!(success);
        assert!(stderr_tail.len() <= 4096, "{}", stderr_tail.len());
        assert!(
            stderr_tail.trim_end().ends_with("LAST-LINE"),
            "{stderr_tail}"
        );
    }

    /// Records span names and the field names of events.
    #[derive(Clone, Default)]
    struct Capture(Arc<Mutex<Vec<String>>>);

    impl<S: tracing::Subscriber> tracing_subscriber::Layer<S> for Capture {
        fn on_new_span(
            &self,
            attrs: &tracing::span::Attributes<'_>,
            _id: &tracing::span::Id,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            self.0
                .lock()
                .unwrap()
                .push(format!("span:{}", attrs.metadata().name()));
        }

        fn on_event(
            &self,
            event: &tracing::Event<'_>,
            _ctx: tracing_subscriber::layer::Context<'_, S>,
        ) {
            let mut seen = self.0.lock().unwrap();
            seen.push(format!("target:{}", event.metadata().target()));
            for field in event.metadata().fields() {
                seen.push(format!("field:{}", field.name()));
            }
        }
    }

    #[test]
    fn recipe_run_span_and_duration_metric() {
        use tracing_subscriber::layer::SubscriberExt;
        let f = Fixture::new("coverage:\n    true\n");
        // tracing-core quirk: while exactly one dispatcher is registered, a
        // callsite first hit on another thread (other tests run run_recipe
        // in parallel, with no subscriber) is registered against that
        // thread's no-op default and cached as "never". A second live
        // dispatcher makes registration consult every registered dispatcher.
        let _second = tracing::Dispatch::new(tracing_subscriber::registry());
        let cap = Capture::default();
        let subscriber = tracing_subscriber::registry().with(cap.clone());
        tracing::subscriber::with_default(subscriber, || {
            let run = RecipeRun {
                platform: f.platform.as_ref(),
                just: &f.just,
                root: f.root(),
                commit: "abc123",
                timeout: Duration::from_secs(60),
                cancel: &|| false,
                interrupt: &Interrupt::new(),
            };
            run_recipe(&run, "coverage").unwrap();
        });
        let seen = cap.0.lock().unwrap().clone();
        assert!(seen.contains(&"span:recipe.run".to_string()), "{seen:?}");
        assert!(
            seen.contains(&"field:histogram.devctl.recipe.duration".to_string()),
            "{seen:?}"
        );
        assert!(seen.contains(&"field:recipe".to_string()), "{seen:?}");
    }

    #[test]
    fn dropped_lcov_paths_are_warned() {
        use tracing_subscriber::layer::SubscriberExt;
        let f = Fixture::new("");
        f.write(
            &["coverage", "x.info"],
            "SF:/elsewhere/b.rs\nDA:1,1\nend_of_record\nSF:a.rs\nDA:1,1\nend_of_record\n",
        );
        // See recipe_run_span_and_duration_metric.
        let _second = tracing::Dispatch::new(tracing_subscriber::registry());
        let cap = Capture::default();
        let subscriber = tracing_subscriber::registry().with(cap.clone());
        tracing::subscriber::with_default(subscriber, || {
            collect(f.root(), f.platform.as_ref()).unwrap();
        });
        let seen = cap.0.lock().unwrap().clone();
        assert!(
            seen.contains(&format!("target:{}", crate::telemetry::PATH_TARGET)),
            "{seen:?}"
        );
        assert!(seen.contains(&"field:examples".to_string()), "{seen:?}");
        assert!(seen.contains(&"field:dropped".to_string()), "{seen:?}");
    }
}
