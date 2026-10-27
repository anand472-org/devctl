//! The `just` recipe contract (spec section 5): run `coverage` (then `test`
//! if it wrote no `JUnit`) in a measuring worktree, collect and normalize the
//! LCOV and `JUnit` outputs, and classify the result into a record [`Status`].

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
}

/// How one recipe run ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum RecipeExit {
    Exited { success: bool, stderr_tail: String },
    Cancelled,
    TimedOut,
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
/// How long to wait for the stderr reader after the recipe ends. A
/// background process the recipe left behind can hold the pipe open forever.
const READER_JOIN: Duration = Duration::from_secs(2);

/// Whether the worktree root itself holds a justfile (`justfile` in any case,
/// or `.justfile`). Without one, `just` would search parent directories and
/// could find the main repository's justfile.
fn has_justfile(root: &Path) -> anyhow::Result<bool> {
    let unreadable = || format!("cannot read worktree directory {}", root.display());
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

/// A `just` command in `root` with repository-selecting git variables
/// removed: devctl may run inside a git hook, and recipes must see the
/// measuring worktree, not the hook's repository.
fn just_command(just: &JustCommand, root: &Path) -> Command {
    let mut cmd = just.command();
    for var in crate::git::REPO_ENV {
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
            return Ok(Waited::Exited(status.success()));
        }
        let now = Instant::now();
        let elapsed = now.duration_since(start);
        let outcome = if elapsed >= run.timeout {
            Some(Waited::TimedOut)
        } else if now >= next_poll {
            next_poll = now + CANCEL_POLL;
            (run.cancel)().then_some(Waited::Cancelled)
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
    let waited = waited?;
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
            RecipeExit::TimedOut
        }
    })
}

/// Files in `root/dir` with extension `ext` (any case), sorted by name.
fn outputs(root: &Path, dir: &str, ext: &str) -> anyhow::Result<Vec<PathBuf>> {
    let dir = root.join(dir);
    let entries = match std::fs::read_dir(&dir) {
        Ok(e) => e,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e).with_context(|| format!("cannot read {}", dir.display())),
    };
    let mut files = Vec::new();
    for entry in entries {
        let path = entry
            .with_context(|| format!("cannot read {}", dir.display()))?
            .path();
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
        let merged =
            Report::merge(&reports).map_paths(|sf| relativize(platform, root, &prefix, sf));
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

/// An error status ending with `msg`, after as much of the recipes' stderr
/// as fits in the kept tail.
fn error_with_stderr(msg: &str, stderr_tail: &str) -> Status {
    let tail = stderr_tail.trim();
    if tail.is_empty() {
        Status::error(msg)
    } else {
        Status::error(&format!("{tail}\n{msg}"))
    }
}

/// The record status for a contract run: no LCOV or no `JUnit` output is an
/// error; a failed or errored test case, or a non-zero exit, is
/// `TestsFailed`; otherwise `Ok`. A `JUnit` run with no test cases is not by
/// itself an error.
#[must_use]
pub fn classify(
    exit_ok: bool,
    stderr_tail: &str,
    report: Option<&Report>,
    tests: Option<&TestRun>,
) -> Status {
    let Some(tests) = tests else {
        return error_with_stderr(
            "no JUnit output in test-results/*.xml; make `just test` write JUnit XML there",
            stderr_tail,
        );
    };
    if report.is_none() {
        return error_with_stderr(
            "no LCOV output in coverage/*.info; make `just coverage` write LCOV there",
            stderr_tail,
        );
    }
    let counts = tests.counts();
    if !exit_ok || counts.failed > 0 || counts.errored > 0 {
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
        "recipe `{name}` exceeded record.timeout_minutes ({minutes}); its process tree was killed"
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
        Err(e) => Err(e).with_context(|| format!("cannot remove stale {}", path.display())),
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
                    "missing recipe `{name}`; run `devctl init`"
                )));
            }
        }
        Err(e) => return Ok(done_error(&format!("{e:#}"))),
    }

    let mut exit_ok = true;
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
                    exit_ok = false;
                    let _ = writeln!(stderr, "`just {name}` failed:\n{}", stderr_tail.trim_end());
                }
            }
            RecipeExit::Cancelled => return Ok(ContractResult::Cancelled),
            RecipeExit::TimedOut => return Ok(done_error(&timeout_message(name, run.timeout))),
        }
    }

    Ok(match collect(run.root, run.platform) {
        Ok((report, tests, raw)) => ContractResult::Done {
            status: classify(exit_ok, &stderr, report.as_ref(), tests.as_ref()),
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
            let run = RecipeRun {
                platform: self.platform.as_ref(),
                just: &self.just,
                root: self.root(),
                commit: "abc123",
                timeout,
                cancel,
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
        assert_eq!(classify(true, "", Some(&report), Some(&pass)), Status::Ok);
        assert_eq!(
            classify(false, "boom", Some(&report), Some(&pass)),
            Status::TestsFailed
        );
        let e = classify(false, "boom", Some(&report), None);
        assert!(error_text(&e).contains("no JUnit output in test-results/*.xml"));
        assert!(error_text(&e).contains("boom"), "stderr tail kept: {e:?}");
        let e = classify(true, "", None, Some(&pass));
        assert!(error_text(&e).contains("no LCOV output in coverage/*.info"));
        assert_eq!(
            classify(true, "", Some(&report), Some(&fail)),
            Status::TestsFailed
        );
    }

    #[test]
    fn classify_empty_junit_is_not_an_error() {
        let report = devctl_lcov::parse(LCOV).unwrap();
        let empty = devctl_junit::parse("<testsuites/>").unwrap();
        assert_eq!(classify(true, "", Some(&report), Some(&empty)), Status::Ok);
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
            "coverage:\n    echo compile-error-xyz >&2\n    exit 2\n\ntest:\n    echo test-broke >&2\n    exit 3\n",
        );
        let r = f.run();
        let text = error_text(status_of(&r));
        assert!(text.contains("compile-error-xyz"), "{text}");
        assert!(text.contains("test-broke"), "{text}");
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
        }
    }

    #[test]
    fn missing_recipe_message() {
        let f = Fixture::new("coverage:\n    true\n");
        assert_eq!(missing_recipes(&f.just, f.root()).unwrap(), vec!["test"]);
        let text = error_text(status_of(&f.run())).to_string();
        assert!(
            text.contains("missing recipe `test`; run `devctl init`"),
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
        };
        let r = run_contract(&run).unwrap();
        assert!(error_text(status_of(&r)).contains("missing recipe `coverage`; run `devctl init`"));
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
    const HANGING: &str = "coverage:\n    sh -c 'while true; do echo x >> beat; sleep 0.1; done' &\n    sleep 30\n\ntest:\n    true\n";

    fn assert_tree_dead(root: &Path) {
        let beat = root.join("beat");
        let len = || std::fs::metadata(&beat).map_or(0, |m| m.len());
        let before = len();
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
            "recipe `coverage` exceeded record.timeout_minutes (10); its process tree was killed"
        );
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
            for field in event.metadata().fields() {
                self.0
                    .lock()
                    .unwrap()
                    .push(format!("field:{}", field.name()));
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
}
