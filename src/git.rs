//! Repository access. Reads go through gix ([`GixRepo`]); [`git`] runs the git
//! CLI for the few operations gix does not cover (worktree add/remove/prune,
//! checkout in a measuring worktree, and later push/fetch/rebase).

use std::collections::{BTreeMap, BTreeSet};
use std::path::{Path, PathBuf};

use std::process::{Command, Stdio};

use anyhow::{Context, Result, anyhow, bail};
pub use gix::ObjectId;
use gix::bstr::ByteSlice;
use gix::diff::blob::{Algorithm, Diff, InternedInput};

/// One staged file: lines added and removed between `HEAD` and the index.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileChange {
    /// Repo-relative, `/`-separated; the new path for a rename.
    pub path: String,
    pub added: u32,
    pub removed: u32,
    /// Either side has a NUL byte in its first 8000 bytes; counts are 0.
    pub binary: bool,
}

/// Path (`/`-separated, repo-relative) to the 1-based new-side line numbers
/// added there.
pub type AddedLines = BTreeMap<String, BTreeSet<u32>>;

/// Lowercase hex of an object ID's bytes.
#[must_use]
pub fn hex(id: &[u8]) -> String {
    use std::fmt::Write as _;
    id.iter()
        .fold(String::with_capacity(id.len() * 2), |mut s, b| {
            let _ = write!(s, "{b:02x}");
            s
        })
}

/// The first seven hex digits of an object ID, as git abbreviates it.
#[must_use]
pub fn short_hex(id: &[u8]) -> String {
    let mut s = hex(&id[..id.len().min(4)]);
    s.truncate(7);
    s
}

/// Everything devctl reads from git.
pub trait Repo {
    /// The shared git directory (`git rev-parse --git-common-dir`), absolute.
    fn common_dir(&self) -> &Path;
    /// The top level of this worktree, absolute; `None` for a bare repository.
    fn workdir(&self) -> Option<&Path>;
    /// The checked-out branch (short name); `None` when `HEAD` is detached.
    ///
    /// # Errors
    /// If `HEAD` cannot be read.
    fn current_branch(&self) -> Result<Option<String>>;
    /// The object `rev` names (any rev-parse spec).
    ///
    /// # Errors
    /// If `rev` does not name exactly one object.
    fn resolve(&self, rev: &str) -> Result<ObjectId>;
    /// The tree of `commit`.
    ///
    /// # Errors
    /// If `commit` is not a commit in this repository.
    fn tree_of(&self, commit: ObjectId) -> Result<ObjectId>;
    /// Whether `ancestor` is reachable from `descendant` (a commit is its own
    /// ancestor).
    ///
    /// # Errors
    /// If the commit graph cannot be walked.
    fn is_ancestor(&self, ancestor: ObjectId, descendant: ObjectId) -> Result<bool>;
    /// The best common ancestor of `a` and `b`, if any.
    ///
    /// # Errors
    /// If the commit graph cannot be walked.
    fn merge_base(&self, a: ObjectId, b: ObjectId) -> Result<Option<ObjectId>>;
    /// Merge commits reachable from `tip` but not from `base`.
    ///
    /// # Errors
    /// If the commit graph cannot be walked.
    fn merges_in_range(&self, base: Option<ObjectId>, tip: ObjectId) -> Result<Vec<ObjectId>>;
    /// Lines added from `base`'s tree (`None` = the empty tree) to `tip`'s
    /// tree. Renames are tracked (only lines changed in the renamed file count);
    /// binary files, symlinks and submodules are skipped; files with no added
    /// line are absent.
    ///
    /// # Errors
    /// If a commit, tree or blob cannot be read.
    fn added_lines(&self, base: Option<ObjectId>, tip: ObjectId) -> Result<AddedLines>;
    /// Files that differ between the `HEAD` tree (empty if unborn) and the
    /// index. A rename is one entry under its new path.
    ///
    /// # Errors
    /// If `HEAD`, the index or a blob cannot be read.
    fn staged_changes(&self) -> Result<Vec<FileChange>>;
    /// Whether a merge is in progress in this worktree (`MERGE_HEAD` exists).
    fn is_merging(&self) -> bool;
    /// A config value (`section.key` or `section.sub.key`), as git resolves it.
    ///
    /// # Errors
    /// Reserved for configuration that cannot be read.
    fn config_string(&self, key: &str) -> Result<Option<String>>;
    /// The object `name` (full or short ref name) points to, peeled; `None` if
    /// no such ref exists.
    ///
    /// # Errors
    /// If the ref exists but cannot be read or peeled.
    fn ref_target(&self, name: &str) -> Result<Option<ObjectId>>;
    /// The repository name for telemetry: the last path segment of `origin`'s
    /// URL without `.git`. `None` without an origin.
    fn remote_url_name(&self) -> Option<String>;
}

/// [`Repo`] backed by gix.
pub struct GixRepo {
    repo: gix::ThreadSafeRepository,
    common_dir: PathBuf,
    workdir: Option<PathBuf>,
}

impl GixRepo {
    /// Finds the repository containing `cwd` (searching upwards). `GIT_DIR`
    /// does not override `cwd`, but once the repository is found gix still
    /// applies the repository-local overrides `GIT_WORK_TREE`, `GIT_INDEX_FILE`
    /// (set by git for hooks during `git commit -a`) and `GIT_NAMESPACE`.
    ///
    /// # Errors
    /// If there is no repository at or above `cwd` ("not inside a git
    /// repository"), or one was found but cannot be opened (ownership,
    /// permissions, corrupt config), with the underlying error.
    pub fn discover(cwd: &Path) -> Result<GixRepo> {
        use gix::discover::upwards::Error as Discover;

        let repo = gix::ThreadSafeRepository::discover(cwd).map_err(|e| {
            match e.downcast_any_ref::<Discover>() {
                Some(
                    Discover::NoGitRepository { .. }
                    | Discover::NoGitRepositoryWithinCeiling { .. }
                    | Discover::NoGitRepositoryWithinFs { .. },
                ) => anyhow!(
                    "{} is not inside a git repository ({e}); run devctl from a git \
                     checkout, or create one with `git init`",
                    cwd.display()
                ),
                Some(Discover::NoTrustedGitRepository { candidate, .. }) => anyhow!(
                    "the git repository at {} is owned by another user, so devctl will not \
                     use it ({e}); run devctl as its owner, or mark it safe with \
                     `git config --global --add safe.directory <path>`",
                    candidate.display()
                ),
                _ => anyhow!(
                    "cannot open the git repository containing {}: {e}; check that \
                     `git status` works there and fix what it reports",
                    cwd.display()
                ),
            }
        })?;
        let local = repo.to_thread_local();
        let base = local.current_dir().to_path_buf();
        let common_dir = absolute(local.common_dir(), &base);
        let workdir = local.workdir().map(|w| absolute(w, &base));
        Ok(GixRepo {
            repo,
            common_dir,
            workdir,
        })
    }

    fn local(&self) -> gix::Repository {
        self.repo.to_thread_local()
    }
}

/// `p` made absolute against `base`, symlinks resolved where possible (gix's
/// `realpath` never adds a `\\?\` prefix on Windows).
fn absolute(p: &Path, base: &Path) -> PathBuf {
    let joined = if p.is_absolute() {
        p.to_path_buf()
    } else {
        base.join(p)
    };
    gix::path::realpath(&joined).unwrap_or(joined)
}

/// git's binary heuristic: a NUL byte in the first 8000 bytes.
fn is_binary(data: &[u8]) -> bool {
    data.iter().take(8000).any(|&b| b == 0)
}

/// The contents of blob `id`; the empty blob need not exist in the odb.
fn blob(repo: &gix::Repository, id: &gix::oid) -> Result<Vec<u8>> {
    if id.is_empty_blob() {
        return Ok(Vec::new());
    }
    let mut obj = repo
        .find_blob(id)
        .with_context(|| format!("cannot read blob {id}"))?;
    Ok(obj.take_data())
}

/// Line diff of `old` -> `new` as `git diff` computes it by default (Myers,
/// then git's slider heuristics). Histogram is not used: imara's histogram
/// disagrees with git's on repetitive input (untouched lines reported as new),
/// while Myers plus line postprocessing matches `git diff` and `--numstat`. A
/// line is a run of bytes ending in `\n` (CR stays part of the line), plus a
/// final unterminated run.
fn line_diff(old: &[u8], new: &[u8]) -> Diff {
    let input = InternedInput::new(old, new);
    let mut diff = Diff::compute(Algorithm::Myers, &input);
    diff.postprocess_lines(&input);
    diff
}

fn path_string(p: &gix::bstr::BStr) -> String {
    p.to_str_lossy().into_owned()
}

impl Repo for GixRepo {
    fn common_dir(&self) -> &Path {
        &self.common_dir
    }

    fn workdir(&self) -> Option<&Path> {
        self.workdir.as_deref()
    }

    fn current_branch(&self) -> Result<Option<String>> {
        let name = self
            .local()
            .head_name()
            .context("cannot read HEAD")?
            .and_then(|n| {
                n.as_bstr()
                    .strip_prefix(b"refs/heads/")
                    .map(|b| path_string(b.as_bstr()))
            });
        Ok(name)
    }

    fn resolve(&self, rev: &str) -> Result<ObjectId> {
        let repo = self.local();
        let id = repo
            .rev_parse_single(rev)
            .with_context(|| format!("cannot resolve '{rev}' to a git object"))?
            .object()
            .with_context(|| format!("cannot read the object '{rev}' names"))?
            .peel_to_commit()
            .with_context(|| format!("'{rev}' does not name a commit"))?
            .id;
        Ok(id)
    }

    fn tree_of(&self, commit: ObjectId) -> Result<ObjectId> {
        let repo = self.local();
        let tree = repo
            .find_commit(commit)
            .with_context(|| format!("{commit} is not a commit in this repository"))?
            .tree_id()
            .with_context(|| format!("cannot read the tree of commit {commit}"))?
            .detach();
        Ok(tree)
    }

    fn is_ancestor(&self, ancestor: ObjectId, descendant: ObjectId) -> Result<bool> {
        // If `ancestor` is reachable from `descendant` it is the single best
        // common ancestor of the two.
        Ok(ancestor == descendant || self.merge_base(ancestor, descendant)? == Some(ancestor))
    }

    fn merge_base(&self, a: ObjectId, b: ObjectId) -> Result<Option<ObjectId>> {
        let base = self
            .local()
            .merge_base(a, b)
            .with_context(|| format!("cannot compute the merge base of {a} and {b}"))?
            .map(gix::Id::detach);
        Ok(base)
    }

    fn merges_in_range(&self, base: Option<ObjectId>, tip: ObjectId) -> Result<Vec<ObjectId>> {
        let repo = self.local();
        let walk = repo.rev_walk([tip]).with_hidden(base);
        let mut merges = Vec::new();
        for info in walk
            .all()
            .with_context(|| format!("cannot walk the history of {tip}"))?
        {
            let info = info.with_context(|| format!("cannot walk the history of {tip}"))?;
            if info.parent_ids.len() > 1 {
                merges.push(info.id);
            }
        }
        Ok(merges)
    }

    fn added_lines(&self, base: Option<ObjectId>, tip: ObjectId) -> Result<AddedLines> {
        use gix::object::tree::diff::ChangeDetached as Change;

        let repo = self.local();
        let tree = |c: ObjectId| -> Result<gix::Tree<'_>> {
            repo.find_commit(c)
                .with_context(|| format!("{c} is not a commit in this repository"))?
                .tree()
                .with_context(|| format!("cannot read the tree of commit {c}"))
        };
        let new_tree = tree(tip)?;
        let old_tree = base.map(tree).transpose()?;
        let opts =
            gix::diff::Options::default().with_rewrites(Some(gix::diff::Rewrites::default()));
        let changes = repo
            .diff_tree_to_tree(old_tree.as_ref(), &new_tree, opts)
            .with_context(|| {
                format!(
                    "cannot diff {} against {tip}",
                    base.map_or_else(|| "the empty tree".to_string(), |b| b.to_string())
                )
            })?;

        let mut out = AddedLines::new();
        for change in changes {
            let (location, mode, id, old) = match change {
                Change::Deletion { .. } => continue,
                Change::Addition {
                    location,
                    entry_mode,
                    id,
                    ..
                } => (location, entry_mode, id, None),
                Change::Modification {
                    location,
                    previous_entry_mode,
                    previous_id,
                    entry_mode,
                    id,
                } => (
                    location,
                    entry_mode,
                    id,
                    Some((previous_entry_mode, previous_id)),
                ),
                Change::Rewrite {
                    location,
                    source_entry_mode,
                    source_id,
                    entry_mode,
                    id,
                    ..
                } => (
                    location,
                    entry_mode,
                    id,
                    Some((source_entry_mode, source_id)),
                ),
            };
            // Only regular and executable files: not trees, symlinks or submodules.
            if !mode.is_blob() {
                continue;
            }
            let new = blob(&repo, &id)?;
            let old = match old {
                Some((m, old_id)) if m.is_blob() => blob(&repo, &old_id)?,
                _ => Vec::new(),
            };
            if is_binary(&new) || is_binary(&old) {
                continue;
            }
            let diff = line_diff(&old, &new);
            let lines: BTreeSet<u32> = diff.hunks().flat_map(|h| h.after).map(|l| l + 1).collect();
            if !lines.is_empty() {
                out.insert(path_string(location.as_ref()), lines);
            }
        }
        Ok(out)
    }

    fn staged_changes(&self) -> Result<Vec<FileChange>> {
        use gix::diff::index::ChangeRef;

        struct Staged {
            path: String,
            old: Option<(gix::index::entry::Mode, ObjectId)>,
            new: Option<(gix::index::entry::Mode, ObjectId)>,
        }

        let repo = self.local();
        let head_tree = repo
            .head_tree_id_or_empty()
            .context("cannot read the HEAD tree")?
            .detach();
        let index = repo
            .index_or_empty()
            .with_context(|| format!("cannot read the index {}", repo.index_path().display()))?;
        let mut staged = Vec::new();
        repo.tree_index_status(
            &head_tree,
            &index,
            None,
            gix::status::tree_index::TrackRenames::Given(gix::diff::Rewrites::default()),
            |change, _, _| {
                staged.push(match change {
                    ChangeRef::Addition {
                        location,
                        entry_mode,
                        id,
                        ..
                    } => Staged {
                        path: path_string(&location),
                        old: None,
                        new: Some((entry_mode, id.into_owned())),
                    },
                    ChangeRef::Deletion {
                        location,
                        entry_mode,
                        id,
                        ..
                    } => Staged {
                        path: path_string(&location),
                        old: Some((entry_mode, id.into_owned())),
                        new: None,
                    },
                    ChangeRef::Modification {
                        location,
                        previous_entry_mode,
                        previous_id,
                        entry_mode,
                        id,
                        ..
                    } => Staged {
                        path: path_string(&location),
                        old: Some((previous_entry_mode, previous_id.into_owned())),
                        new: Some((entry_mode, id.into_owned())),
                    },
                    ChangeRef::Rewrite {
                        source_entry_mode,
                        source_id,
                        location,
                        entry_mode,
                        id,
                        ..
                    } => Staged {
                        path: path_string(&location),
                        old: Some((source_entry_mode, source_id.into_owned())),
                        new: Some((entry_mode, id.into_owned())),
                    },
                });
                Ok(std::ops::ControlFlow::Continue(()))
            },
        )
        .context("cannot compare the index with HEAD")?;

        let read = |side: Option<(gix::index::entry::Mode, ObjectId)>| -> Result<Vec<u8>> {
            match side {
                Some((mode, id)) if !mode.is_submodule() => blob(&repo, &id),
                _ => Ok(Vec::new()),
            }
        };
        let mut out = Vec::with_capacity(staged.len());
        for s in staged {
            let old = read(s.old)?;
            let new = read(s.new)?;
            let binary = is_binary(&old) || is_binary(&new);
            let (added, removed) = if binary {
                (0, 0)
            } else {
                let diff = line_diff(&old, &new);
                (diff.count_additions(), diff.count_removals())
            };
            out.push(FileChange {
                path: s.path,
                added,
                removed,
                binary,
            });
        }
        Ok(out)
    }

    fn is_merging(&self) -> bool {
        self.repo.git_dir().join("MERGE_HEAD").is_file()
    }

    fn config_string(&self, key: &str) -> Result<Option<String>> {
        Ok(self
            .local()
            .config_snapshot()
            .string(key)
            .map(|v| path_string(v.as_ref())))
    }

    fn ref_target(&self, name: &str) -> Result<Option<ObjectId>> {
        let repo = self.local();
        let Some(mut reference) = repo
            .try_find_reference(name)
            .with_context(|| format!("cannot read ref {name}"))?
        else {
            return Ok(None);
        };
        let id = reference
            .peel_to_id()
            .with_context(|| format!("cannot resolve ref {name}"))?
            .detach();
        Ok(Some(id))
    }

    fn remote_url_name(&self) -> Option<String> {
        let url = self.config_string("remote.origin.url").ok().flatten()?;
        repo_name_from_url(&url)
    }
}

/// The repository name in a remote URL: its last path segment without `.git`.
/// Handles `scheme://[user@]host/path`, scp-like `[user@]host:path` (including
/// ssh host aliases such as `github:org/repo.git`) and local paths. Never
/// returns user info or a host.
#[must_use]
pub fn repo_name_from_url(url: &str) -> Option<String> {
    let url = url.trim();
    let path = if let Some((_, rest)) = url.split_once("://") {
        // Drop the authority (user info, host, port).
        rest.split_once('/').map_or("", |(_, p)| p)
    } else if is_local_path(url) {
        url
    } else if let Some((_, p)) = url.split_once(':') {
        p
    } else {
        url
    };
    // The last segment after any `/`, `\`, `:` or `@`, so neither user info
    // (`u:pw@host:repo.git`) nor a host can come through.
    path.split(['/', '\\', ':', '@'])
        .rev()
        .map(|seg| seg.strip_suffix(".git").unwrap_or(seg))
        .find(|seg| !seg.is_empty())
        .map(str::to_string)
}

/// Absolute or relative filesystem path rather than a URL: starts with `/`,
/// `\` or `.`, or a Windows drive (`C:\`, `C:/`).
fn is_local_path(url: &str) -> bool {
    let b = url.as_bytes();
    url.starts_with(['/', '\\', '.'])
        || (b.len() >= 3
            && b[0].is_ascii_alphabetic()
            && b[1] == b':'
            && matches!(b[2], b'/' | b'\\'))
}

/// Variables that would make git act on another repository, worktree or index
/// than the one in `dir` (git sets some of them for hooks). Removed by [`git`];
/// child processes that run git (recipes) should drop them too. Kept identical
/// to the list in the test helpers.
pub const REPO_ENV: [&str; 8] = [
    "GIT_DIR",
    "GIT_WORK_TREE",
    "GIT_INDEX_FILE",
    "GIT_COMMON_DIR",
    "GIT_OBJECT_DIRECTORY",
    "GIT_ALTERNATE_OBJECT_DIRECTORIES",
    "GIT_NAMESPACE",
    "GIT_CEILING_DIRECTORIES",
];

/// Runs `git <args>` in `dir` and returns its stdout. Never prints; stdin is
/// closed. Repository-selecting environment variables (`GIT_DIR`,
/// `GIT_INDEX_FILE`, ...) are removed so `dir` decides the repository.
///
/// # Errors
/// If git cannot be started or exits non-zero; the message names the args and
/// includes git's stderr.
pub fn git(dir: &Path, args: &[&str]) -> Result<String> {
    let mut cmd = Command::new("git");
    for var in REPO_ENV {
        cmd.env_remove(var);
    }
    let out = cmd
        .current_dir(dir)
        .args(args)
        .env("GIT_TERMINAL_PROMPT", "0")
        .stdin(Stdio::null())
        .output()
        .with_context(|| {
            format!(
                "cannot run `git {}` in {}; check that git is installed and on PATH",
                args.join(" "),
                dir.display()
            )
        })?;
    if !out.status.success() {
        bail!(
            "`git {}` failed in {} ({}): {}",
            args.join(" "),
            dir.display(),
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        );
    }
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ctx::slug;
    use crate::test_support::TestRepo;

    #[test]
    fn hex_and_short_hex() {
        let id = [0x1a, 0x2b, 0x3c, 0x4d, 0x5e];
        assert_eq!(hex(&id), "1a2b3c4d5e");
        assert_eq!(short_hex(&id), "1a2b3c4");
        assert_eq!(short_hex(&[0xab]), "ab");
        let oid = ObjectId::from_hex(b"0123456789abcdef0123456789abcdef01234567").unwrap();
        assert_eq!(short_hex(oid.as_bytes()), "0123456");
    }

    fn oid(hex: &str) -> ObjectId {
        ObjectId::from_hex(hex.as_bytes()).unwrap()
    }

    fn open(t: &TestRepo) -> GixRepo {
        GixRepo::discover(t.path()).unwrap()
    }

    fn lines(path: &str, ns: &[u32]) -> AddedLines {
        AddedLines::from([(path.to_string(), ns.iter().copied().collect())])
    }

    #[test]
    fn added_lines_counts_only_additions() {
        let t = TestRepo::new();
        t.write("f.txt", "a\nb\nc\n");
        let base = t.commit("base");
        t.write("f.txt", "a\nX\nc\nd\n");
        let tip = t.commit("tip");
        let r = open(&t);
        assert_eq!(
            r.added_lines(Some(oid(&base)), oid(&tip)).unwrap(),
            lines("f.txt", &[2, 4])
        );
    }

    #[test]
    fn added_lines_follows_renames() {
        let t = TestRepo::new();
        t.write("a.rs", "1\n2\n3\n4\n");
        let base = t.commit("base");
        t.git(&["mv", "a.rs", "b.rs"]);
        t.write("b.rs", "1\n2\n3\n4\n5\n");
        let tip = t.commit("rename");
        let r = open(&t);
        assert_eq!(
            r.added_lines(Some(oid(&base)), oid(&tip)).unwrap(),
            lines("b.rs", &[5])
        );
    }

    #[test]
    fn added_lines_skips_binary() {
        let t = TestRepo::new();
        t.write("keep.txt", "x\n");
        let base = t.commit("base");
        t.write("bin.dat", b"abc\0def\nmore\n");
        t.write("keep.txt", "x\ny\n");
        let tip = t.commit("tip");
        let r = open(&t);
        assert_eq!(
            r.added_lines(Some(oid(&base)), oid(&tip)).unwrap(),
            lines("keep.txt", &[2])
        );
    }

    #[test]
    fn added_lines_from_empty_tree() {
        let t = TestRepo::new();
        t.write("a.txt", "1\n2\n");
        t.write("dir/b.txt", "1\n2\n3");
        let tip = t.commit("first");
        let r = open(&t);
        let mut want = lines("a.txt", &[1, 2]);
        want.insert("dir/b.txt".into(), BTreeSet::from([1, 2, 3]));
        assert_eq!(r.added_lines(None, oid(&tip)).unwrap(), want);
    }

    #[test]
    fn added_lines_numbers_crlf_lines_like_lf() {
        let t = TestRepo::new();
        t.write("w.txt", "a\r\nb\r\n");
        let base = t.commit("base");
        t.write("w.txt", "a\r\nNEW\r\nb\r\nend\r\n");
        let tip = t.commit("tip");
        let r = open(&t);
        assert_eq!(
            r.added_lines(Some(oid(&base)), oid(&tip)).unwrap(),
            lines("w.txt", &[2, 4])
        );
    }

    /// New-side line numbers from `git diff -U0` (default algorithm) hunk
    /// headers.
    fn git_cli_added(t: &TestRepo, base: &str, tip: &str, path: &str) -> BTreeSet<u32> {
        let out = t.git(&[
            "-c",
            "diff.algorithm=myers",
            "diff",
            "-U0",
            base,
            tip,
            "--",
            path,
        ]);
        let mut set = BTreeSet::new();
        for line in out.lines().filter(|l| l.starts_with("@@ ")) {
            let new = line.split(' ').nth(2).unwrap().trim_start_matches('+');
            let (start, len) = new.split_once(',').map_or((new, "1"), |(s, l)| (s, l));
            let (start, len): (u32, u32) = (start.parse().unwrap(), len.parse().unwrap());
            set.extend(start..start + len);
        }
        set
    }

    #[test]
    fn added_lines_match_git_cli_on_ambiguous_edits() {
        let t = TestRepo::new();
        let before = "fn a() {\n    1;\n}\n\nfn b() {\n    2;\n}\n";
        let after = "fn a() {\n    1;\n}\n\nfn new() {\n    9;\n}\n\nfn b() {\n    2;\n}\n";
        t.write("s.rs", before);
        t.write("d.txt", "x\ny\nx\ny\nx\n");
        let (braces_before, braces_after) = repetitive_braces();
        t.write("braces.rs", &braces_before);
        let (moved_before, moved_after) = moved_block();
        t.write("moved.rs", &moved_before);
        let base = t.commit("base");
        t.write("s.rs", after);
        t.write("d.txt", "x\ny\nx\ny\nx\ny\nx\nz\n");
        t.write("braces.rs", &braces_after);
        t.write("moved.rs", &moved_after);
        let tip = t.commit("tip");
        let got = open(&t).added_lines(Some(oid(&base)), oid(&tip)).unwrap();
        for path in ["s.rs", "d.txt", "braces.rs", "moved.rs"] {
            assert_eq!(
                got.get(path).cloned().unwrap_or_default(),
                git_cli_added(&t, &base, &tip, path),
                "{path}"
            );
        }
    }

    /// Many `}` and blank lines, edited in the middle.
    fn repetitive_braces() -> (String, String) {
        let block =
            |name: &str| format!("fn {name}() {{\n    if x {{\n        y();\n    }}\n}}\n\n");
        let before: String = (0..12).map(|i| block(&format!("f{i}"))).collect();
        let mut after = String::new();
        for i in 0..12 {
            if i == 5 {
                after.push_str("fn extra() {\n    {\n    }\n}\n\n}\n\n");
                continue;
            }
            after.push_str(&block(&format!("f{i}")));
            if i == 7 {
                after.push_str("}\n\n\n}\n");
            }
        }
        (before, after)
    }

    /// A block of lines moved further down the same file.
    fn moved_block() -> (String, String) {
        let lines: Vec<String> = (0..30).map(|i| format!("line {}\n", i % 7)).collect();
        let before: String = lines.concat();
        let mut after: Vec<String> = lines.clone();
        let block: Vec<String> = after.drain(3..9).collect();
        let at = after.len() - 4;
        after.splice(at..at, block);
        (before, after.concat())
    }

    #[test]
    fn staged_counts_match_git_numstat_on_repetitive_edits() {
        let t = TestRepo::new();
        let (braces_before, braces_after) = repetitive_braces();
        let (moved_before, moved_after) = moved_block();
        t.write("braces.rs", &braces_before);
        t.write("moved.rs", &moved_before);
        t.commit("base");
        t.write("braces.rs", &braces_after);
        t.write("moved.rs", &moved_after);
        t.git(&["add", "-A"]);
        let numstat = t.git(&[
            "-c",
            "diff.algorithm=myers",
            "diff",
            "--cached",
            "--numstat",
        ]);
        let mut want: Vec<FileChange> = numstat
            .lines()
            .map(|l| {
                let mut f = l.split('\t');
                let added = f.next().unwrap().parse().unwrap();
                let removed = f.next().unwrap().parse().unwrap();
                FileChange {
                    path: f.next().unwrap().to_string(),
                    added,
                    removed,
                    binary: false,
                }
            })
            .collect();
        want.sort_by(|a, b| a.path.cmp(&b.path));
        let mut got = open(&t).staged_changes().unwrap();
        got.sort_by(|a, b| a.path.cmp(&b.path));
        assert_eq!(got, want);
    }

    #[test]
    fn merges_in_range_finds_merge_commit() {
        let t = TestRepo::new();
        t.write("f", "1\n");
        let base = t.commit("base");
        t.checkout_new("feature/x");
        t.write("g", "1\n");
        t.commit("on feature");
        t.git(&["checkout", "-q", "dev"]);
        t.write("h", "1\n");
        t.commit("on dev");
        t.merge_no_ff("feature/x");
        let merge = t.git(&["rev-parse", "HEAD"]);
        let r = open(&t);
        let tip = r.resolve("HEAD").unwrap();
        assert_eq!(
            r.merges_in_range(Some(oid(&base)), tip).unwrap(),
            vec![oid(&merge)]
        );
        assert_eq!(r.merges_in_range(None, tip).unwrap(), vec![oid(&merge)]);
        assert_eq!(
            r.merges_in_range(Some(tip), tip).unwrap(),
            Vec::<ObjectId>::new()
        );
    }

    #[test]
    fn merge_base_and_ancestry() {
        let t = TestRepo::new();
        t.write("f", "1\n");
        let base = oid(&t.commit("base"));
        t.checkout_new("feature/x");
        t.write("g", "1\n");
        let feat = oid(&t.commit("feat"));
        t.git(&["checkout", "-q", "dev"]);
        t.write("h", "1\n");
        let dev = oid(&t.commit("dev"));
        let r = open(&t);
        assert_eq!(r.merge_base(feat, dev).unwrap(), Some(base));
        assert!(r.is_ancestor(base, feat).unwrap());
        assert!(r.is_ancestor(feat, feat).unwrap());
        assert!(!r.is_ancestor(feat, dev).unwrap());
        assert!(!r.is_ancestor(feat, base).unwrap());

        t.git(&["checkout", "-q", "--orphan", "lonely"]);
        t.write("z", "1\n");
        let lonely = oid(&t.commit("lonely"));
        assert_eq!(r.merge_base(lonely, dev).unwrap(), None);
        assert!(!r.is_ancestor(lonely, dev).unwrap());
    }

    #[test]
    fn resolve_tree_of_and_ref_target() {
        let t = TestRepo::new();
        t.write("f", "1\n");
        let c = t.commit("c");
        let tree = t.git(&["rev-parse", "HEAD^{tree}"]);
        let r = open(&t);
        assert_eq!(r.resolve("HEAD").unwrap(), oid(&c));
        assert_eq!(r.tree_of(oid(&c)).unwrap(), oid(&tree));
        assert_eq!(r.ref_target("refs/heads/dev").unwrap(), Some(oid(&c)));
        assert_eq!(r.ref_target("refs/heads/nope").unwrap(), None);
        assert!(r.resolve("nope").is_err());
        assert_eq!(r.current_branch().unwrap().as_deref(), Some("dev"));
    }

    #[test]
    fn resolve_peels_annotated_tags_to_their_commit() {
        let t = TestRepo::new();
        t.write("f", "1\n");
        let c = t.commit("c");
        t.git(&["tag", "-a", "v1", "-m", "release"]);
        let tag_object = t.git(&["rev-parse", "v1"]);
        assert_ne!(tag_object, c);
        let r = open(&t);
        assert_eq!(r.resolve("v1").unwrap(), oid(&c));
        assert_eq!(r.resolve("refs/tags/v1").unwrap(), oid(&c));
        let err = r.resolve("HEAD^{tree}").err().unwrap();
        assert!(
            format!("{err:#}").contains("does not name a commit"),
            "{err:#}"
        );
    }

    #[test]
    fn staged_changes_counts_add_and_remove() {
        let t = TestRepo::new();
        t.write("f.txt", "a\nb\nc\nd\n");
        t.commit("base");
        t.write("f.txt", "a\nX\nd\ne\nf\n");
        t.git(&["add", "f.txt"]);
        let r = open(&t);
        assert_eq!(
            r.staged_changes().unwrap(),
            vec![FileChange {
                path: "f.txt".into(),
                added: 3,
                removed: 2,
                binary: false
            }]
        );
    }

    #[test]
    fn staged_changes_reports_rename_once_and_binary() {
        let t = TestRepo::new();
        t.write("old.txt", "1\n2\n3\n4\n5\n");
        t.write("gone.txt", "x\ny\n");
        t.commit("base");
        t.git(&["mv", "old.txt", "new.txt"]);
        t.git(&["rm", "-q", "gone.txt"]);
        t.write("img.bin", b"\0\x01\x02");
        t.git(&["add", "img.bin"]);
        let r = open(&t);
        let mut got = r.staged_changes().unwrap();
        got.sort_by(|a, b| a.path.cmp(&b.path));
        assert_eq!(
            got,
            vec![
                FileChange {
                    path: "gone.txt".into(),
                    added: 0,
                    removed: 2,
                    binary: false
                },
                FileChange {
                    path: "img.bin".into(),
                    added: 0,
                    removed: 0,
                    binary: true
                },
                FileChange {
                    path: "new.txt".into(),
                    added: 0,
                    removed: 0,
                    binary: false
                },
            ]
        );
    }

    #[test]
    fn staged_changes_on_unborn_branch() {
        let t = TestRepo::new();
        t.write("a.txt", "1\n");
        t.write("b/c.txt", "1\n2\n");
        t.git(&["add", "-A"]);
        let r = open(&t);
        let mut got = r.staged_changes().unwrap();
        got.sort_by(|a, b| a.path.cmp(&b.path));
        assert_eq!(
            got,
            vec![
                FileChange {
                    path: "a.txt".into(),
                    added: 1,
                    removed: 0,
                    binary: false
                },
                FileChange {
                    path: "b/c.txt".into(),
                    added: 2,
                    removed: 0,
                    binary: false
                },
            ]
        );
        assert_eq!(r.current_branch().unwrap().as_deref(), Some("dev"));
    }

    #[test]
    fn is_merging_while_merge_conflicts() {
        let t = TestRepo::new();
        t.write("f", "base\n");
        t.commit("base");
        t.checkout_new("feature/x");
        t.write("f", "feature\n");
        t.commit("feat");
        t.git(&["checkout", "-q", "dev"]);
        t.write("f", "dev\n");
        t.commit("dev");
        let r = open(&t);
        assert!(!r.is_merging());
        let out = t
            .command(t.path())
            .args(["merge", "-q", "feature/x"])
            .output()
            .unwrap();
        assert!(!out.status.success(), "merge should conflict");
        assert!(r.is_merging());
    }

    #[test]
    fn detached_head_has_no_branch() {
        let t = TestRepo::new();
        t.write("f", "1\n");
        t.commit("one");
        t.git(&["checkout", "-q", "--detach"]);
        assert_eq!(open(&t).current_branch().unwrap(), None);
    }

    #[test]
    fn common_dir_is_shared_from_linked_worktree() {
        let t = TestRepo::new();
        t.write("f", "1\n");
        t.commit("one");
        let wt_parent = tempfile::tempdir().unwrap();
        let wt = wt_parent.path().join("wt");
        t.git(&[
            "worktree",
            "add",
            "-q",
            "-b",
            "feature/wt",
            wt.to_str().unwrap(),
        ]);
        let main = open(&t);
        let linked = GixRepo::discover(&wt).unwrap();
        let real = |p: &Path| gix::path::realpath(p).unwrap();
        assert_eq!(real(linked.common_dir()), real(&t.path().join(".git")));
        assert_eq!(real(main.common_dir()), real(&t.path().join(".git")));
        assert_eq!(real(linked.workdir().unwrap()), real(&wt));
        assert_eq!(
            linked.current_branch().unwrap().as_deref(),
            Some("feature/wt")
        );
        assert!(linked.common_dir().is_absolute());
    }

    #[test]
    fn discover_outside_a_repo_fails_with_next_step() {
        let dir = tempfile::tempdir().unwrap();
        let err = GixRepo::discover(dir.path()).err().unwrap();
        let msg = format!("{err:#}");
        assert!(msg.contains("not inside a git repository"), "{msg}");
        assert!(msg.contains("git init"), "{msg}");
    }

    #[test]
    fn discover_reports_an_unopenable_repo_as_such() {
        let t = TestRepo::new();
        std::fs::write(t.path().join(".git").join("config"), "[core\nbroken").unwrap();
        let err = GixRepo::discover(t.path()).err().unwrap();
        let msg = format!("{err:#}");
        assert!(!msg.contains("not inside a git repository"), "{msg}");
        assert!(msg.contains("cannot open the git repository"), "{msg}");
    }

    #[test]
    fn config_string_and_remote_url_name() {
        let t = TestRepo::new();
        let r = open(&t);
        assert_eq!(r.remote_url_name(), None);
        assert_eq!(r.config_string("remote.origin.url").unwrap(), None);
        t.git(&["remote", "add", "origin", "github:anand472-org/devctl.git"]);
        let r = open(&t);
        assert_eq!(
            r.config_string("remote.origin.url").unwrap().as_deref(),
            Some("github:anand472-org/devctl.git")
        );
        assert_eq!(r.remote_url_name().as_deref(), Some("devctl"));
    }

    #[test]
    fn repo_name_from_url_forms() {
        for (url, want) in [
            ("git@github.com:org/repo.git", Some("repo")),
            ("git@host:repo.git", Some("repo")),
            ("https://github.com/org/repo.git", Some("repo")),
            ("https://user:secret@github.com/org/repo", Some("repo")),
            ("https://github.com/org/repo.git/", Some("repo")),
            ("ssh://git@host:2222/org/repo.git", Some("repo")),
            ("github:anand472-org/devctl.git", Some("devctl")),
            ("file:///srv/git/proj.git", Some("proj")),
            ("/srv/git/proj/.git", Some("proj")),
            ("C:\\repos\\proj.git", Some("proj")),
            ("u:pw@host:repo.git", Some("repo")),
            ("u:pw@host:org/repo.git", Some("repo")),
            ("https://user@host", None),
            ("https://host/", None),
            ("", None),
        ] {
            assert_eq!(repo_name_from_url(url).as_deref(), want, "{url}");
        }
    }

    #[test]
    fn git_cli_returns_stdout_and_reports_failures() {
        let t = TestRepo::new();
        t.write("f", "1\n");
        let c = t.commit("one");
        assert_eq!(git(t.path(), &["rev-parse", "HEAD"]).unwrap().trim(), c);
        let err = git(t.path(), &["rev-parse", "--verify", "no-such-ref"])
            .err()
            .unwrap();
        let msg = format!("{err:#}");
        assert!(msg.contains("rev-parse --verify no-such-ref"), "{msg}");
        assert!(msg.contains("fatal"), "{msg}");
    }

    #[test]
    fn slug_replaces_slashes() {
        assert_eq!(slug("feature/x"), "feature-x");
        assert_eq!(slug("dev"), "dev");
    }
}
