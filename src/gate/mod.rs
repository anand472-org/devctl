//! The push gate (spec section 4.2): the rules `pre-push` applies to each
//! pushed ref, `devctl gate` and `devctl hook pre-push`.

pub mod coverage;

use std::collections::BTreeMap;
use std::fmt::{self, Write as _};
use std::time::{Duration, Instant};

use anyhow::{Context, Result, anyhow, bail};
use devctl_store::{RawReports, Record, Status, Store};

use crate::ctx::Ctx;
use crate::git::{ObjectId, Repo};
use crate::record::{queue, worker};
use coverage::{compress_lines, new_code, passes};

/// The first seven hex digits of `id`.
fn short(id: ObjectId) -> String {
    let mut hex = id.to_string();
    hex.truncate(7);
    hex
}

/// One line of `pre-push` input. `None` stands for git's all-zero ID: a new
/// remote ref (`remote`) or a deletion (`local`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RefUpdate {
    pub local_ref: String,
    pub local: Option<ObjectId>,
    pub remote_ref: String,
    pub remote: Option<ObjectId>,
}

/// Parses git's `pre-push` stdin: `<local ref> <local id> <remote ref>
/// <remote id>` per line, LF or CRLF; blank lines are ignored.
///
/// # Errors
/// On a line without exactly four fields or with an invalid object ID.
pub fn parse_pre_push(stdin: &str) -> Result<Vec<RefUpdate>> {
    const NEXT: &str = "git writes this input; run `git push` instead of calling the hook by hand";
    let mut updates = Vec::new();
    // `lines` also strips a trailing `\r`.
    for (i, line) in stdin.lines().enumerate() {
        let n = i + 1;
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let fields: Vec<&str> = line.split_whitespace().collect();
        let [local_ref, local, remote_ref, remote] = fields[..] else {
            bail!(
                "cannot parse pre-push input line {n} {line:?}: expected `<local ref> <local id> \
                 <remote ref> <remote id>`; {NEXT}"
            );
        };
        let id = |hex: &str| -> Result<Option<ObjectId>> {
            let id = ObjectId::from_hex(hex.as_bytes()).map_err(|e| {
                anyhow!("pre-push input line {n}: {hex:?} is not an object ID ({e}); {NEXT}")
            })?;
            Ok((!id.is_null()).then_some(id))
        };
        updates.push(RefUpdate {
            local_ref: local_ref.to_string(),
            local: id(local)?,
            remote_ref: remote_ref.to_string(),
            remote: id(remote)?,
        });
    }
    Ok(updates)
}

/// A gate rule, in evaluation order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Rule {
    BranchName,
    NoDelete,
    NoRewrite,
    Linear,
    Record,
    Tests,
    NewCodeCoverage,
}

impl Rule {
    /// Every rule, in order.
    pub const ALL: [Rule; 7] = [
        Rule::BranchName,
        Rule::NoDelete,
        Rule::NoRewrite,
        Rule::Linear,
        Rule::Record,
        Rule::Tests,
        Rule::NewCodeCoverage,
    ];

    fn name(self) -> &'static str {
        match self {
            Rule::BranchName => "branch-name",
            Rule::NoDelete => "no-delete",
            Rule::NoRewrite => "no-rewrite",
            Rule::Linear => "linear",
            Rule::Record => "record",
            Rule::Tests => "tests",
            Rule::NewCodeCoverage => "new-code-coverage",
        }
    }
}

impl fmt::Display for Rule {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.name())
    }
}

/// How a rule ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    Pass,
    Skip,
    /// The push is blocked; `next` is what to do about it.
    Block {
        next: String,
    },
}

/// One rule's result for one ref.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RuleResult {
    pub rule: Rule,
    pub outcome: Outcome,
    pub detail: String,
    /// For a coverage block: path to the uncovered new lines.
    pub uncovered: BTreeMap<String, Vec<u32>>,
}

/// How the record rule waits for a record being made elsewhere.
pub struct Waiting<'a> {
    /// How often to look for the record.
    pub poll: Duration,
    /// How often to report progress.
    pub progress_every: Duration,
    /// The longest the whole record rule may take before it blocks.
    pub limit: Duration,
    /// Receives each progress line (`waiting for record of ...`).
    pub progress: &'a dyn Fn(&str),
}

/// Applies every rule to `update`, pushed to `remote` (the remote's name, or
/// its URL when pushing to a URL).
///
/// # Errors
/// If git, the store or the record queue cannot be read; the gate fails
/// closed, so callers must treat an error as a block.
pub fn evaluate(ctx: &Ctx, remote: &str, update: &RefUpdate) -> Result<Vec<RuleResult>> {
    let progress = |line: &str| eprintln!("{line}");
    let minutes = u64::from(ctx.config.record.timeout_minutes);
    let waiting = Waiting {
        poll: Duration::from_secs(1),
        progress_every: Duration::from_secs(5),
        limit: Duration::from_secs(minutes * 60) + WAIT_MARGIN,
        progress: &progress,
    };
    evaluate_with(ctx, remote, update, &waiting)
}

/// Added to `record.timeout_minutes` to bound the record rule: time for
/// checking out the commit and storing the record.
const WAIT_MARGIN: Duration = Duration::from_secs(60);

/// How many times the record rule runs the worker in the foreground before
/// giving up (a newer commit queued meanwhile can supersede the first run).
const FOREGROUND_RUNS: u32 = 2;

fn result(rule: Rule, outcome: Outcome, detail: impl Into<String>) -> RuleResult {
    RuleResult {
        rule,
        outcome,
        detail: detail.into(),
        uncovered: BTreeMap::new(),
    }
}

fn pass(rule: Rule, detail: impl Into<String>) -> RuleResult {
    result(rule, Outcome::Pass, detail)
}

fn skip(rule: Rule, detail: impl Into<String>) -> RuleResult {
    result(rule, Outcome::Skip, detail)
}

fn block(rule: Rule, detail: impl Into<String>, next: impl Into<String>) -> RuleResult {
    result(rule, Outcome::Block { next: next.into() }, detail)
}

fn is_block(r: &RuleResult) -> bool {
    matches!(r.outcome, Outcome::Block { .. })
}

/// Runs one rule in a `gate.rule` span and counts its outcome.
fn checked(rule: Rule, f: impl FnOnce() -> Result<RuleResult>) -> Result<RuleResult> {
    let span = tracing::info_span!("gate.rule", rule = rule.name());
    span.in_scope(|| {
        let r = f()?;
        let outcome = match r.outcome {
            Outcome::Pass => "pass",
            Outcome::Skip => "skip",
            Outcome::Block { .. } => "block",
        };
        tracing::info!(
            monotonic_counter.devctl.gate.result = 1_u64,
            rule = rule.name(),
            outcome,
            "gate rule"
        );
        Ok(r)
    })
}

/// Appends a `Skip` with `detail` for every rule not yet in `out`.
fn skip_rest(out: &mut Vec<RuleResult>, detail: &str) -> Result<()> {
    for rule in Rule::ALL.into_iter().skip(out.len()) {
        out.push(checked(rule, || Ok(skip(rule, detail)))?);
    }
    Ok(())
}

/// [`evaluate`] with explicit waiting behaviour.
///
/// # Errors
/// As [`evaluate`].
pub fn evaluate_with(
    ctx: &Ctx,
    remote: &str,
    update: &RefUpdate,
    waiting: &Waiting,
) -> Result<Vec<RuleResult>> {
    let mut out = Vec::with_capacity(Rule::ALL.len());
    let Some(branch) = update.remote_ref.strip_prefix("refs/heads/") else {
        skip_rest(&mut out, "not a branch")?;
        return Ok(out);
    };
    let protected = ctx.config.is_protected(branch);

    let Some(local) = update.local else {
        out.push(checked(Rule::BranchName, || {
            Ok(if protected {
                pass(Rule::BranchName, format!("`{branch}` is protected"))
            } else {
                skip(Rule::BranchName, "deleting a branch")
            })
        })?);
        out.push(checked(Rule::NoDelete, || {
            Ok(if protected {
                block(
                    Rule::NoDelete,
                    format!("deletes the protected branch `{branch}`"),
                    format!("protected branches cannot be deleted; keep `{branch}`"),
                )
            } else {
                pass(Rule::NoDelete, "not protected")
            })
        })?);
        skip_rest(&mut out, "deleting a branch")?;
        return Ok(out);
    };

    let name = checked(Rule::BranchName, || Ok(branch_name(ctx, branch, protected)))?;
    let stop = is_block(&name);
    out.push(name);
    if stop {
        skip_rest(&mut out, "not checked: the branch name is blocked")?;
        return Ok(out);
    }

    // `None` for a new remote branch; a remote commit this repository does
    // not have cannot be an ancestor of `local`.
    let remote_id = update.remote;
    let remote_known = remote_id.is_none_or(|r| ctx.repo.tree_of(r).is_ok());
    if protected {
        out.push(checked(Rule::NoDelete, || {
            Ok(pass(Rule::NoDelete, "not a deletion"))
        })?);
        out.push(checked(Rule::NoRewrite, || {
            no_rewrite(ctx, remote, branch, local, remote_id, remote_known)
        })?);
        out.push(checked(Rule::Linear, || {
            linear(ctx, local, remote_id, remote_known)
        })?);
    } else {
        for rule in [Rule::NoDelete, Rule::NoRewrite, Rule::Linear] {
            out.push(checked(rule, || Ok(skip(rule, "not protected")))?);
        }
    }

    // Records live on the pushed local branch (its measuring worktree).
    let record_branch = update
        .local_ref
        .strip_prefix("refs/heads/")
        .unwrap_or(branch);
    let mut record = None;
    out.push(checked(Rule::Record, || {
        let (r, got) = record_rule(ctx, record_branch, local, waiting)?;
        record = got;
        Ok(r)
    })?);
    let Some((record, raw)) = record else {
        skip_rest(&mut out, "no usable record")?;
        return Ok(out);
    };
    out.push(checked(Rule::Tests, || Ok(tests_rule(&record)))?);
    out.push(checked(Rule::NewCodeCoverage, || {
        if protected && !remote_known {
            // No base to diff from; no-rewrite already blocks.
            return Ok(skip(
                Rule::NewCodeCoverage,
                "the remote commit is not in this repository",
            ));
        }
        coverage_rule(ctx, remote, branch, protected, local, remote_id, &raw)
    })?);
    Ok(out)
}

fn branch_name(ctx: &Ctx, branch: &str, protected: bool) -> RuleResult {
    let pattern = &ctx.config.branches.pattern;
    if protected {
        pass(Rule::BranchName, format!("`{branch}` is protected"))
    } else if pattern.is_match(branch) {
        pass(
            Rule::BranchName,
            format!("`{branch}` matches branches.pattern"),
        )
    } else {
        block(
            Rule::BranchName,
            format!(
                "`{branch}` does not match branches.pattern `{}` and is not protected",
                pattern.as_str()
            ),
            "push to a branch whose name matches branches.pattern in .devctl.toml \
             (for example `git branch -m feature/<name>`)",
        )
    }
}

fn no_rewrite(
    ctx: &Ctx,
    remote: &str,
    branch: &str,
    local: ObjectId,
    remote_id: Option<ObjectId>,
    remote_known: bool,
) -> Result<RuleResult> {
    let next = format!("pull and rebase onto `{remote}/{branch}`, then push without --force");
    Ok(match remote_id {
        None => pass(Rule::NoRewrite, "new branch"),
        Some(r) if !remote_known => block(
            Rule::NoRewrite,
            format!(
                "`{remote}/{branch}` is at {}, which is not in this repository",
                short(r)
            ),
            next,
        ),
        Some(r) if ctx.repo.is_ancestor(r, local)? => {
            pass(Rule::NoRewrite, format!("fast-forward from {}", short(r)))
        }
        Some(r) => block(
            Rule::NoRewrite,
            format!(
                "{} on `{remote}/{branch}` is not an ancestor of {}",
                short(r),
                short(local)
            ),
            next,
        ),
    })
}

fn linear(
    ctx: &Ctx,
    local: ObjectId,
    remote_id: Option<ObjectId>,
    remote_known: bool,
) -> Result<RuleResult> {
    if !remote_known {
        return Ok(skip(
            Rule::Linear,
            "the remote commit is not in this repository",
        ));
    }
    let merges = ctx.repo.merges_in_range(remote_id, local)?;
    Ok(if merges.is_empty() {
        pass(Rule::Linear, "no merge commits")
    } else {
        let ids: Vec<String> = merges.into_iter().map(short).collect();
        block(
            Rule::Linear,
            format!("merge commits {}", ids.join(", ")),
            "rebase to drop the merge commits",
        )
    })
}

/// A record for the gate: an `error` one (no raw reports needed), or an
/// `ok`/`tests_failed` one with its raw reports.
type Usable = (Record, RawReports);

fn store_error(ctx: &Ctx) -> String {
    format!(
        "cannot read the record store {}; delete it and run `devctl record` again",
        ctx.dirs.store().display()
    )
}

/// The newest record of `commit`, unless it is `ok`/`tests_failed` with its
/// raw reports pruned (then it has to be recorded again).
fn usable_record(ctx: &Ctx, commit: ObjectId) -> Result<Option<Usable>> {
    if !ctx.dirs.store().exists() {
        return Ok(None);
    }
    let store = ctx.store();
    let Some((id, record)) = store
        .record_for(commit.as_bytes())
        .with_context(|| store_error(ctx))?
    else {
        return Ok(None);
    };
    if let Status::Error { .. } = record.status {
        return Ok(Some((record, RawReports::default())));
    }
    Ok(store
        .raw(id)
        .with_context(|| store_error(ctx))?
        .map(|raw| (record, raw)))
}

/// What waiting for a record ended with.
enum Got {
    Record(Box<Usable>),
    /// No record; detail and next step for the block.
    Nothing(String, String),
}

/// Finds or makes the record of `local` (spec section 4.2, step 3): waits
/// while a worker holds `branch`'s lock, then records in the foreground.
fn obtain_record(ctx: &Ctx, branch: &str, local: ObjectId, waiting: &Waiting) -> Result<Got> {
    let id = short(local);
    let start = Instant::now();
    let mut reported: Option<Duration> = None;
    let mut runs = 0;
    loop {
        if let Some(found) = usable_record(ctx, local)? {
            return Ok(Got::Record(Box::new(found)));
        }
        let elapsed = start.elapsed();
        if elapsed >= waiting.limit {
            return Ok(Got::Nothing(
                format!("no record of {id} after {}s", elapsed.as_secs()),
                format!(
                    "check the worker with `devctl status`, or run `devctl record --commit {id}`; \
                     then push again"
                ),
            ));
        }
        if worker::worker_running(ctx, branch)? {
            if reported.is_none_or(|at| elapsed.saturating_sub(at) >= waiting.progress_every) {
                (waiting.progress)(&format!(
                    "waiting for record of {id} ({}s)",
                    elapsed.as_secs()
                ));
                reported = Some(elapsed);
            }
            std::thread::sleep(waiting.poll.min(waiting.limit.saturating_sub(elapsed)));
            continue;
        }
        if runs == FOREGROUND_RUNS {
            return Ok(Got::Nothing(
                format!("recording {id} produced no record"),
                format!(
                    "run `devctl record --commit {id}` and fix what it reports, then push again"
                ),
            ));
        }
        runs += 1;
        (waiting.progress)(&format!("recording {id} on {branch}"));
        if queue::peek(ctx, branch)? != Some(local) {
            queue::enqueue(ctx, branch, local)?;
        }
        // Returns at once if a worker took the lock meanwhile; the loop then
        // waits for it.
        worker::run_worker(ctx, branch)?;
    }
}

fn record_rule(
    ctx: &Ctx,
    branch: &str,
    local: ObjectId,
    waiting: &Waiting,
) -> Result<(RuleResult, Option<Usable>)> {
    Ok(match obtain_record(ctx, branch, local, waiting)? {
        Got::Nothing(detail, next) => (block(Rule::Record, detail, next), None),
        Got::Record(found) => {
            let (record, raw) = *found;
            match &record.status {
                Status::Error { stderr_tail } => {
                    let first = stderr_tail
                        .lines()
                        .map(str::trim)
                        .find(|l| !l.is_empty())
                        .unwrap_or("(no output)");
                    (
                        block(
                            Rule::Record,
                            first,
                            "fix the recipe, then run `devctl record`",
                        ),
                        None,
                    )
                }
                Status::Ok | Status::TestsFailed => {
                    let status = if record.status == Status::Ok {
                        "ok"
                    } else {
                        "tests failed"
                    };
                    (
                        pass(Rule::Record, format!("{} {status}", short(local))),
                        Some((record, raw)),
                    )
                }
            }
        }
    })
}

fn tests_rule(record: &Record) -> RuleResult {
    let t = &record.tests;
    if t.failed + t.errored > 0 {
        block(
            Rule::Tests,
            format!("{} failed, {} errored", t.failed, t.errored),
            "fix the failing tests, then commit and push again",
        )
    } else if record.status == Status::TestsFailed {
        // The recipe exited non-zero although no test failed.
        block(
            Rule::Tests,
            "the coverage recipe exited non-zero with no failed test",
            "make `just coverage` exit 0 when the tests pass, then run `devctl record`",
        )
    } else {
        pass(
            Rule::Tests,
            format!("{} passed, {} skipped", t.passed, t.skipped),
        )
    }
}

/// Whether `remote` is a remote's name rather than a URL or path (git passes
/// the URL when pushing to one directly).
fn is_remote_name(remote: &str) -> bool {
    !remote.is_empty() && !remote.contains([':', '\\']) && !remote.starts_with(['/', '.', '~'])
}

/// The commit of `target`: its remote-tracking ref on `remote`, else the local
/// branch.
fn target_commit(ctx: &Ctx, remote: &str, target: &str) -> Result<Option<ObjectId>> {
    if is_remote_name(remote)
        && let Some(id) = ctx
            .repo
            .ref_target(&format!("refs/remotes/{remote}/{target}"))?
    {
        return Ok(Some(id));
    }
    ctx.repo.ref_target(&format!("refs/heads/{target}"))
}

fn coverage_rule(
    ctx: &Ctx,
    remote: &str,
    branch: &str,
    protected: bool,
    local: ObjectId,
    remote_id: Option<ObjectId>,
    raw: &RawReports,
) -> Result<RuleResult> {
    const RULE: Rule = Rule::NewCodeCoverage;
    let base = if protected {
        // The remote commit; the empty tree for a new protected branch.
        remote_id
    } else {
        let prefix = branch.split_once('/').map_or(branch, |(p, _)| p);
        let Some(target) = ctx.config.target_for(branch) else {
            return Ok(block(
                RULE,
                format!("no target branch for prefix `{prefix}`"),
                format!("add `{prefix}` to branches.targets in .devctl.toml"),
            ));
        };
        let Some(target_id) = target_commit(ctx, remote, target)? else {
            return Ok(block(
                RULE,
                format!("target branch `{target}` not found"),
                format!("run `git fetch {remote} {target}`, then push again"),
            ));
        };
        ctx.repo.merge_base(local, target_id)?
    };
    let added = ctx.repo.added_lines(base, local)?;
    let id = short(local);
    let report = devctl_lcov::parse(&raw.lcov).map_err(|e| {
        anyhow!(
            "cannot parse the LCOV stored with the record of {id}: {e}; run \
             `devctl record --commit {id}`"
        )
    })?;
    let n = new_code(&added, &report);
    let threshold = ctx.config.gate.new_code_coverage;
    let detail = if n.coverable == 0 {
        "no coverable new lines".to_string()
    } else {
        format!(
            "{}/{} new lines covered ({:.1}%, threshold {threshold}%)",
            n.covered,
            n.coverable,
            f64::from(n.covered) * 100.0 / f64::from(n.coverable)
        )
    };
    Ok(if passes(&n, threshold) {
        pass(RULE, detail)
    } else {
        RuleResult {
            uncovered: n.uncovered,
            ..block(
                RULE,
                detail,
                "add tests for the uncovered lines listed above, then commit and push again",
            )
        }
    })
}

/// The gate's report for one ref: a line per rule, uncovered lines under a
/// coverage block and, when blocked, the next step.
#[must_use]
pub fn render(update: &RefUpdate, results: &[RuleResult]) -> String {
    let mut s = String::new();
    for r in results {
        let outcome = match r.outcome {
            Outcome::Pass => "pass",
            Outcome::Skip => "skip",
            Outcome::Block { .. } => "BLOCK",
        };
        let _ = writeln!(s, "{outcome:<5} {}: {}", r.rule, r.detail);
        for (path, lines) in &r.uncovered {
            let _ = writeln!(s, "      {path}: {}", compress_lines(lines));
        }
    }
    let first_block = results.iter().find_map(|r| match &r.outcome {
        Outcome::Block { next } => Some(next),
        _ => None,
    });
    if let Some(next) = first_block {
        let branch = update
            .remote_ref
            .strip_prefix("refs/heads/")
            .unwrap_or(&update.remote_ref);
        let _ = writeln!(s, "push blocked for {branch}: {next}");
    }
    s
}

/// Whether any rule blocks.
#[must_use]
pub fn blocked(results: &[RuleResult]) -> bool {
    results.iter().any(is_block)
}

/// What the gate printed and decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateReport {
    /// The rendered rule lines of every ref.
    pub text: String,
    pub blocked: bool,
}

/// `devctl gate [--target <branch>]`: the rules for pushing `HEAD` to
/// `target` (default: the current branch) on `origin`.
///
/// # Errors
/// On a detached `HEAD` without `target`, or as [`evaluate`].
pub fn run_gate(ctx: &Ctx, target: Option<&str>) -> Result<GateReport> {
    const REMOTE: &str = "origin";
    let current = ctx.repo.current_branch()?;
    let target = match (target, current.as_deref()) {
        (Some(t), _) => t.to_string(),
        (None, Some(c)) => c.to_string(),
        (None, None) => {
            bail!("HEAD is detached; check out a branch, or run `devctl gate --target <branch>`")
        }
    };
    let local = ctx.repo.resolve("HEAD")?;
    let update = RefUpdate {
        local_ref: current.map_or_else(|| "HEAD".to_string(), |c| format!("refs/heads/{c}")),
        local: Some(local),
        remote_ref: format!("refs/heads/{target}"),
        remote: ctx
            .repo
            .ref_target(&format!("refs/remotes/{REMOTE}/{target}"))?,
    };
    let results = evaluate(ctx, REMOTE, &update)?;
    Ok(GateReport {
        text: render(&update, &results),
        blocked: blocked(&results),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::Repo;
    use crate::record::queue;
    use crate::record::worker::tests::{fixture_repo, runs};
    use devctl_platform::fake::FakePlatform;
    use devctl_store::Store;
    use std::cell::RefCell;

    const A: &str = "1111111111111111111111111111111111111111";
    const B: &str = "2222222222222222222222222222222222222222";
    const ZERO: &str = "0000000000000000000000000000000000000000";

    fn oid(hex: &str) -> ObjectId {
        ObjectId::from_hex(hex.as_bytes()).unwrap()
    }

    #[test]
    fn parses_pre_push_lines() {
        let input = format!(
            "refs/heads/feature/x {A} refs/heads/feature/x {B}\r\n\
             \r\n\
             (delete) {ZERO} refs/heads/x {B}\n\
             refs/tags/v1 {A} refs/tags/v1 {ZERO}\n"
        );
        let got = parse_pre_push(&input).unwrap();
        assert_eq!(
            got,
            vec![
                RefUpdate {
                    local_ref: "refs/heads/feature/x".into(),
                    local: Some(oid(A)),
                    remote_ref: "refs/heads/feature/x".into(),
                    remote: Some(oid(B)),
                },
                RefUpdate {
                    local_ref: "(delete)".into(),
                    local: None,
                    remote_ref: "refs/heads/x".into(),
                    remote: Some(oid(B)),
                },
                RefUpdate {
                    local_ref: "refs/tags/v1".into(),
                    local: Some(oid(A)),
                    remote_ref: "refs/tags/v1".into(),
                    remote: None,
                },
            ]
        );
        assert_eq!(parse_pre_push("").unwrap(), Vec::<RefUpdate>::new());
    }

    #[test]
    fn parse_pre_push_rejects_malformed_lines() {
        let err = parse_pre_push(&format!("refs/heads/x {A} refs/heads/x\n")).unwrap_err();
        assert!(format!("{err:#}").contains("line 1"), "{err:#}");
        let err = parse_pre_push(&format!("refs/heads/x nothex refs/heads/x {A}\n")).unwrap_err();
        assert!(format!("{err:#}").contains("nothex"), "{err:#}");
    }

    fn result(rule: Rule, outcome: Outcome, detail: &str) -> RuleResult {
        RuleResult {
            rule,
            outcome,
            detail: detail.into(),
            uncovered: BTreeMap::new(),
        }
    }

    #[test]
    fn renders_one_line_per_rule_and_the_next_step() {
        let update = RefUpdate {
            local_ref: "refs/heads/feature/x".into(),
            local: Some(oid(A)),
            remote_ref: "refs/heads/feature/x".into(),
            remote: None,
        };
        let mut cov = result(
            Rule::NewCodeCoverage,
            Outcome::Block {
                next: "add tests".into(),
            },
            "1/4 new lines covered",
        );
        cov.uncovered.insert("src/a.rs".into(), vec![4, 5, 6, 9]);
        cov.uncovered.insert("src/b.rs".into(), vec![1]);
        let results = vec![
            result(Rule::BranchName, Outcome::Pass, "ok"),
            result(Rule::NoDelete, Outcome::Skip, "not protected"),
            cov,
        ];
        assert!(blocked(&results));
        assert_eq!(
            render(&update, &results),
            "pass  branch-name: ok\n\
             skip  no-delete: not protected\n\
             BLOCK new-code-coverage: 1/4 new lines covered\n      \
             src/a.rs: 4-6, 9\n      \
             src/b.rs: 1\n\
             push blocked for feature/x: add tests\n"
        );
        assert!(!blocked(&results[..2]));
        assert_eq!(render(&update, &results[..1]), "pass  branch-name: ok\n");
    }

    fn ctx_with(t: &crate::test_support::TestRepo) -> Ctx {
        Ctx::open(t.path(), Box::new(FakePlatform::new())).unwrap()
    }

    fn quick(progress: &dyn Fn(&str)) -> Waiting<'_> {
        Waiting {
            poll: Duration::from_millis(20),
            progress_every: Duration::from_millis(100),
            limit: Duration::from_secs(60),
            progress,
        }
    }

    fn outcome_of(results: &[RuleResult], rule: Rule) -> &RuleResult {
        results.iter().find(|r| r.rule == rule).unwrap()
    }

    #[test]
    fn tag_push_skips_every_rule() {
        let t = fixture_repo(false);
        let head = t.commit("one");
        let ctx = ctx_with(&t);
        let update = RefUpdate {
            local_ref: "refs/tags/v1".into(),
            local: Some(oid(&head)),
            remote_ref: "refs/tags/v1".into(),
            remote: None,
        };
        let results = evaluate_with(&ctx, "origin", &update, &quick(&|_| {})).unwrap();
        assert_eq!(results.len(), Rule::ALL.len());
        assert!(results.iter().all(|r| r.outcome == Outcome::Skip));
        assert_eq!(results[0].detail, "not a branch");
        assert_eq!(runs(&ctx, "dev"), 0, "nothing recorded");
    }

    #[test]
    fn unknown_remote_commit_blocks_rewrite_and_skips_history_rules() {
        let t = fixture_repo(false);
        let head = t.commit("one");
        let ctx = ctx_with(&t);
        // Someone else pushed `A` to dev; this clone has not fetched it.
        let update = RefUpdate {
            local_ref: "refs/heads/dev".into(),
            local: Some(oid(&head)),
            remote_ref: "refs/heads/dev".into(),
            remote: Some(oid(A)),
        };
        let results = evaluate_with(&ctx, "origin", &update, &quick(&|_| {})).unwrap();
        let rewrite = outcome_of(&results, Rule::NoRewrite);
        assert!(
            matches!(rewrite.outcome, Outcome::Block { .. }),
            "{rewrite:?}"
        );
        assert!(
            rewrite.detail.contains("not in this repository"),
            "{rewrite:?}"
        );
        assert_eq!(outcome_of(&results, Rule::Linear).outcome, Outcome::Skip);
        assert_eq!(
            outcome_of(&results, Rule::NewCodeCoverage).outcome,
            Outcome::Skip
        );
    }

    #[test]
    fn waits_for_a_held_lock_then_records_in_the_foreground() {
        let t = fixture_repo(false);
        t.commit("base");
        t.checkout_new("feature/x");
        let head = t.commit("one");
        let ctx = ctx_with(&t);
        let head_id = ctx.repo.resolve("HEAD").unwrap();
        // A stale job, and a "worker" holding the lock that dies without
        // recording.
        queue::enqueue(&ctx, "feature/x", head_id).unwrap();
        let lock = crate::record::worker::try_lock(&ctx, "feature/x")
            .unwrap()
            .unwrap();
        let release = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(300));
            drop(lock);
        });
        let lines = RefCell::new(Vec::<String>::new());
        let progress = |l: &str| lines.borrow_mut().push(l.to_string());
        // The target `dev` exists locally only.
        let update = RefUpdate {
            local_ref: "refs/heads/feature/x".into(),
            local: Some(head_id),
            remote_ref: "refs/heads/feature/x".into(),
            remote: None,
        };
        let results = evaluate_with(&ctx, "origin", &update, &quick(&progress)).unwrap();
        release.join().unwrap();
        let lines = lines.into_inner();
        assert!(
            lines[0].starts_with(&format!("waiting for record of {} (0s)", &head[..7])),
            "{lines:?}"
        );
        assert!(lines.len() >= 2, "progress repeats: {lines:?}");
        assert_eq!(outcome_of(&results, Rule::Record).outcome, Outcome::Pass);
        assert_eq!(runs(&ctx, "feature/x"), 1, "recorded in the foreground");
        assert_eq!(queue::peek(&ctx, "feature/x").unwrap(), None);
        assert!(
            ctx.store()
                .record_for(head_id.as_bytes())
                .unwrap()
                .is_some()
        );
    }

    #[test]
    fn record_wait_is_bounded() {
        let t = fixture_repo(false);
        t.commit("base");
        t.checkout_new("feature/x");
        t.commit("one");
        let ctx = ctx_with(&t);
        let head_id = ctx.repo.resolve("HEAD").unwrap();
        let _held = crate::record::worker::try_lock(&ctx, "feature/x")
            .unwrap()
            .unwrap();
        let waiting = Waiting {
            limit: Duration::from_millis(200),
            ..quick(&|_| {})
        };
        let update = RefUpdate {
            local_ref: "refs/heads/feature/x".into(),
            local: Some(head_id),
            remote_ref: "refs/heads/feature/x".into(),
            remote: None,
        };
        let results = evaluate_with(&ctx, "origin", &update, &waiting).unwrap();
        let record = outcome_of(&results, Rule::Record);
        let Outcome::Block { next } = &record.outcome else {
            panic!("{record:?}");
        };
        assert!(next.contains("devctl record"), "{next}");
        assert_eq!(outcome_of(&results, Rule::Tests).outcome, Outcome::Skip);
    }
}
