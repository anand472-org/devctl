//! The `pre-commit` size rule: a commit may change at most `max_files` files
//! and fewer than `max_lines` lines (added + removed).

use std::fmt::Write as _;

use anyhow::{Context, Result};

use crate::config::CommitLimits;
use crate::ctx::Ctx;
use crate::git::{FileChange, Repo as _};

/// How many of the largest files a block lists.
const LISTED: usize = 5;

/// What a staged change touches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SizeCheck {
    pub files: u32,
    /// Added plus removed lines over all files.
    pub lines: u32,
    /// Up to 5 files, largest (added + removed) first, ties by path.
    pub largest: Vec<FileChange>,
}

/// Totals `changes` and picks the largest files.
#[must_use]
pub fn measure(changes: &[FileChange]) -> SizeCheck {
    let size = |c: &FileChange| u64::from(c.added) + u64::from(c.removed);
    let files = u32::try_from(changes.len()).unwrap_or(u32::MAX);
    let lines = changes.iter().fold(0u32, |acc, c| {
        acc.saturating_add(c.added).saturating_add(c.removed)
    });
    let mut largest = changes.to_vec();
    largest.sort_by(|a, b| size(b).cmp(&size(a)).then_with(|| a.path.cmp(&b.path)));
    largest.truncate(LISTED);
    SizeCheck {
        files,
        lines,
        largest,
    }
}

/// The violated limits, worded for the first line of a block; empty when the
/// commit is allowed.
#[must_use]
pub fn violations(check: &SizeCheck, limits: &CommitLimits) -> Vec<String> {
    let mut out = Vec::new();
    if check.files > limits.max_files {
        out.push(format!(
            "{} files (limit {})",
            check.files, limits.max_files
        ));
    }
    if check.lines >= limits.max_lines {
        out.push(format!(
            "{} changed lines (must be under {})",
            check.lines, limits.max_lines
        ));
    }
    out
}

/// The text printed for a blocked commit.
fn render(check: &SizeCheck, violated: &[String]) -> String {
    let mut text = format!("BLOCK commit-size: {}\n", violated.join(", "));
    for c in &check.largest {
        if c.binary {
            let _ = writeln!(text, "      {}: binary", c.path);
        } else {
            let _ = writeln!(text, "      {}: +{} -{}", c.path, c.added, c.removed);
        }
    }
    text.push_str(
        "commit blocked: split this into smaller commits (`git commit --no-verify` bypasses)\n",
    );
    text
}

/// Checks the index git is about to commit (`GIT_INDEX_FILE` is honoured).
/// Prints nothing when the commit is allowed, the block text to stderr
/// otherwise. Returns `false` to block. Skipped while a merge is in progress.
///
/// # Errors
/// If the staged changes cannot be read.
pub fn run_pre_commit(ctx: &Ctx) -> Result<bool> {
    if ctx.repo.is_merging() {
        tracing::info!("merge in progress; skipping the commit size check");
        return Ok(true);
    }
    let changes = ctx
        .repo
        .staged_changes()
        .context("cannot read the staged changes")?;
    let check = measure(&changes);
    let violated = violations(&check, &ctx.config.commit);
    if violated.is_empty() {
        tracing::info!(files = check.files, lines = check.lines, "commit size ok");
        return Ok(true);
    }
    eprint!("{}", render(&check, &violated));
    Ok(false)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fc(path: &str, added: u32, removed: u32) -> FileChange {
        FileChange {
            path: path.into(),
            added,
            removed,
            binary: false,
        }
    }

    fn limits() -> CommitLimits {
        CommitLimits {
            max_files: 10,
            max_lines: 500,
        }
    }

    #[test]
    fn ten_files_allowed_eleven_blocked() {
        let ten: Vec<_> = (0..10).map(|i| fc(&format!("f{i}"), 1, 0)).collect();
        assert_eq!(violations(&measure(&ten), &limits()), Vec::<String>::new());
        let eleven: Vec<_> = (0..11).map(|i| fc(&format!("f{i}"), 1, 0)).collect();
        assert_eq!(
            violations(&measure(&eleven), &limits()),
            ["11 files (limit 10)"]
        );
    }

    #[test]
    fn gross_change_counts_add_plus_remove() {
        let ok = measure(&[fc("a", 300, 199)]);
        assert_eq!(ok.lines, 499);
        assert_eq!(violations(&ok, &limits()), Vec::<String>::new());
        let bad = measure(&[fc("a", 300, 200)]);
        assert_eq!(
            violations(&bad, &limits()),
            ["500 changed lines (must be under 500)"]
        );
    }

    #[test]
    fn both_limits_listed() {
        let many: Vec<_> = (0..11).map(|i| fc(&format!("f{i}"), 100, 0)).collect();
        assert_eq!(violations(&measure(&many), &limits()).len(), 2);
    }

    #[test]
    fn binary_counts_as_file_with_zero_lines() {
        let c = measure(&[FileChange {
            path: "b.bin".into(),
            added: 0,
            removed: 0,
            binary: true,
        }]);
        assert_eq!((c.files, c.lines), (1, 0));
    }

    #[test]
    fn largest_is_top_five_by_size_then_path() {
        let changes = vec![
            fc("e", 1, 0),
            fc("d", 5, 5),
            fc("c", 10, 0),
            fc("b", 10, 0),
            fc("a", 2, 0),
            fc("f", 3, 0),
            fc("g", 20, 0),
        ];
        let paths: Vec<_> = measure(&changes)
            .largest
            .into_iter()
            .map(|c| c.path)
            .collect();
        assert_eq!(paths, ["g", "b", "c", "d", "f"]);
    }
}
