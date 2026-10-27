# M1: Record and Gate Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Ship milestone M1 of the phase 1 spec: `devctl` records coverage and test results per commit in the background, blocks non-compliant commits and pushes, and shows status — on Windows, Linux and macOS, x64 and arm64 — with the project's dev tools installed by mise.

**Architecture:** A cargo workspace: four pure library crates (`devctl-platform`, `devctl-lcov`, `devctl-junit`, `devctl-store`) and the `devctl` binary at the root, whose modules (`config`, `telemetry`, `git`, `recipes`, `record`, `status`, `gate`, `commit_size`, `hooks`) are built bottom-up. No daemon: git hooks call `devctl hook <name>`, which either decides in the foreground (pre-commit, pre-push) or queues a job and spawns a detached `devctl worker` (post-commit).

**Tech Stack:** mise ≥ 2026.8.16 for dev tools (`just`, gitoxide CLI), rustup for Rust, Rust 1.99.1 (edition 2024), clap 4, gix 0.90, redb 4.4, postcard 1.1, ruzstd 0.9, serde 1, toml 1.1, regex 1.13, quick-xml 0.42, thiserror 2, rustix 1 (Unix process groups), tracing 0.1 + tracing-subscriber 0.3 + tracing-appender 0.2, opentelemetry/opentelemetry_sdk/opentelemetry-otlp/opentelemetry-appender-tracing 0.33.1, tracing-opentelemetry 0.34, tempfile 3, assert_cmd 2.

**Spec:** `docs/superpowers/specs/2026-10-27-phase1-devctl-design.md` (sections 2–9; section 7 is M1). Plus two user-requested additions not yet in the spec: a `pre-commit` size check (Task 15 adds it to the spec) and moving dev tools from cargo aliases to mise (Task 1).

**Branch:** implement on `feature/m1-record-gate`, branched from `feature/phase1-spec` (or from `dev` once the spec PR has merged). PRs go to `dev`.

## Global Constraints

- Supported platforms: `windows-x64`, `windows-arm64`, `macos-x64`, `macos-arm64`, `linux-x64`, `linux-arm64`. CI runs `just ci` on all six from Task 1 on (`windows-latest`, `windows-11-arm`, `macos-15-intel`, `macos-latest`, `ubuntu-latest`, `ubuntu-24.04-arm`; the repo is public, so arm64 runners are free).
- Dev tools come from `mise.toml` (`min_version = "2026.8.16"`), with checksums in `mise.lock` for all six platforms. Rust comes from rustup via `rust-toolchain.toml`, not mise. No `.cargo/config.toml` install aliases or `.tools/` after Task 1.
- Every `justfile` recipe runs under POSIX `sh` (Git for Windows provides `sh` and `bash` on Windows). Text files are checked out with LF everywhere (`.gitattributes`), and every parser (LCOV, JUnit, pre-push stdin, queue files) also accepts CRLF, because repos devctl measures may use `core.autocrlf=true`. Files devctl writes (hook scripts, queue files) always use LF.
- Local testing: `just test-local` runs `just ci` in three places — a podman Ubuntu 24.04 arm64 container, a UTM Windows 11 ARM VM over SSH, and this Mac (macOS arm64, plus the tests again as `x86_64-apple-darwin` under Rosetta). Tasks 4, 11, 12, 16 and 17 (platform, recipes, record, hooks, end-to-end) must pass `just test-local` before their commit step.
- Paths: never build paths by string concatenation with `/` or `\`; use `Path::join`. `git worktree` and `git checkout` calls pass `-c core.longpaths=true` so deep build output in measuring worktrees works on Windows.
- All dependencies pure Rust; `just check-pure` must pass with `--target all`. Allowlist only `linux-raw-sys`, `windows-sys`, and (from Task 9) `js-sys`, `web-sys` — wasm-bindgen declarations pulled in for wasm targets by the OTLP HTTP client; they compile no C.
- `unsafe_code = "forbid"`; clippy pedantic with `-D warnings`. All crates use `[workspace.lints]`.
- `cfg(target_os = …)`, `cfg(unix)`, `cfg(windows)` and `cfg!(…)` equivalents appear only under `crates/devctl-platform/` (enforced by `just check-cfg` from Task 3).
- Git reads go through gix. The `git` CLI is used only for `worktree add/remove/prune`, `checkout` inside a measuring worktree, and (M2) push/fetch/rebase. Tests may use the `git` CLI freely to build fixture repos.
- `.git/devctl/` means `<git common dir>/devctl`. Branch names in paths replace `/` with `-` (`feature/x` → `feature-x`).
- `.devctl.toml` defaults exactly as spec section 6, plus `[commit] max_files = 10`, `max_lines = 500` (Task 15).
- Minimum `just` version 1.59.0. Recipes get `DEVCTL=1` and `DEVCTL_COMMIT=<commit hex>`.
- Store access: open, one transaction, close; retry while busy with exponential backoff, total wait ≤ 5 s.
- Telemetry export is off by default, plain HTTP only, never blocks git; flush timeout 2 s for `pre-push`, 5 s otherwise.
- Every user-facing error names what failed, why, and the next command to run.
- `post-commit` never exits non-zero. `pre-commit` and `pre-push` fail closed.
- Commit messages: imperative subject in the repo's existing style, ending with `Co-Authored-By: Claude Opus 5.5 <noreply@anthropic.com>`.

## Review Focus

1. **Detached HEAD and unborn branches.** `post-commit` on a detached HEAD logs and exits 0 without queueing; `record` on a detached HEAD fails with "HEAD is detached; check out a branch, then run `devctl record`"; `pre-commit` on the first commit of a repo compares the index against the empty tree. Tests: Tasks 10, 13, 14.
2. **Pushes that aren't ordinary branch updates.** Tags and other non-`refs/heads/` refs get `skip` for every rule; a new remote branch (remote commit all zeros) takes its base from the merge-base with its target branch (non-protected) or the empty tree (protected); a branch whose prefix has no `branches.targets` entry blocks with "add `<prefix>` to branches.targets in .devctl.toml". Tests: Task 14.
3. **Raw reports pruned.** When the gate finds a record whose raw LCOV was dropped by `retention.raw_reports_days`, it re-records the commit instead of passing or blocking. Test: Task 14.
4. **Dead or stuck workers.** A worker killed mid-run releases its OS lock; a gate waiting on it sees the lock free with no record and runs the worker in the foreground instead of waiting forever. A job queued just as a worker exits is still picked up. Tests: Tasks 10, 12.
5. **Merges and large mechanical commits at pre-commit.** During a merge (`MERGE_HEAD` exists) the size check is skipped; otherwise binary files count toward `max_files` with 0 lines, and a rename counts as one file. Test: Task 15.

---

## File Structure

```
mise.toml, mise.lock               dev tools, all six platforms
.gitattributes                     LF everywhere
docs/dev-setup.md                  per-OS and per-shell setup, local test VMs
scripts/test-local/{linux,windows,macos}.sh   one runner per environment
scripts/test-local/linux.Containerfile        Ubuntu 24.04 test image
scripts/test-local/windows-vm-setup.ps1       one-time Windows VM provisioning
.github/workflows/ci.yml           six-platform matrix
Cargo.toml                         workspace + devctl binary package
justfile                           + check-cfg; --workspace everywhere
crates/devctl-platform/src/{lib.rs, unix.rs, windows.rs, fake.rs, normalize.rs}
crates/devctl-lcov/src/lib.rs      + tests/fixtures/*.info, tests/parse.rs
crates/devctl-junit/src/lib.rs     + tests/fixtures/*.xml, tests/parse.rs
crates/devctl-store/src/{lib.rs, record.rs, redb_store.rs, schema.rs}
src/cli.rs                         clap commands
src/lib.rs                         run(): dispatch
src/main.rs                        unchanged
src/config.rs                      .devctl.toml
src/telemetry.rs                   tracing + OTel
src/git.rs                         trait Repo, GixRepo, git CLI helper
src/ctx.rs                         Ctx, DevctlDirs, slug()
src/recipes.rs                     just contract
src/record/{mod.rs, queue.rs, measure.rs, worker.rs}
src/status.rs                      status + shared summary formatting
src/gate/{mod.rs, coverage.rs}     pre-push rules
src/commit_size.rs                 pre-commit rule
src/hooks.rs                       hooks install + hook dispatch
src/test_support.rs                #[cfg(test)] TestRepo helper (git CLI)
tests/cli.rs                       existing
tests/support/mod.rs               TestRepo + devctl() for integration tests
tests/e2e.rs                       real commit/push through hooks
tests/fixtures/e2e/                sample repo justfile + prepared LCOV/JUnit
```

---

### Task 1: Dev tools via mise on every platform

**Files:**
- Create: `mise.toml`, `mise.lock`, `.gitattributes`, `docs/dev-setup.md`
- Delete: `.cargo/config.toml` (it only holds the `install-just` / `install-gix` aliases)
- Modify: `.gitignore` (drop `/.tools`), `.github/workflows/ci.yml`, `CLAUDE.md`

**Interfaces:**
- Produces: after `mise install`, `just` 1.59.0 and the gitoxide 0.61.0 max-pure CLI (`gix`, `ein`) on PATH on all six platforms; a CI `check` job that runs `just ci` on all six runners. Later tasks only add steps to that job.

`mise.toml` (asset names verified against the v0.61.0 release):

```toml
min_version = "2026.8.16"   # fixes Windows `activate --shims` PATH output

[settings]
lockfile = true

[tools]
just = "1.59.0"             # aqua:casey/just

[tools."github:GitoxideLabs/gitoxide"]
version = "0.61.0"

[tools."github:GitoxideLabs/gitoxide".platforms]
windows-x64   = { asset_pattern = "gitoxide-max-pure-v*-x86_64-pc-windows-msvc.zip" }
windows-arm64 = { asset_pattern = "gitoxide-max-pure-v*-aarch64-pc-windows-msvc.zip" }
macos-x64     = { asset_pattern = "gitoxide-max-pure-v*-x86_64-apple-darwin.tar.gz" }
macos-arm64   = { asset_pattern = "gitoxide-max-pure-v*-aarch64-apple-darwin.tar.gz" }
linux-x64     = { asset_pattern = "gitoxide-max-pure-v*-x86_64-unknown-linux-musl.tar.gz" }
linux-arm64   = { asset_pattern = "gitoxide-max-pure-v*-aarch64-unknown-linux-musl.tar.gz" }
```

- [ ] **Step 1:** Add `.gitattributes` with `* text=auto eol=lf`, run `git add --renormalize .`, and confirm `git status` shows no content changes beyond line endings (expected: none, the repo is already LF).
- [ ] **Step 2:** Add `mise.toml`, run `mise trust && mise install`. Expected: `just --version` prints `just 1.59.0`; `gix --version` exits 0.
- [ ] **Step 3:** Run `mise lock --platform windows-x64,windows-arm64,macos-x64,macos-arm64,linux-x64,linux-arm64`. Expected: `mise.lock` has a checksum entry for each tool on each of the six platforms (12 entries); fail the step if any is missing.
- [ ] **Step 4:** Delete `.cargo/config.toml`, remove `/.tools` from `.gitignore`, `rm -rf .tools`, then run `just ci` locally. Expected: passes using the mise-provided `just`.
- [ ] **Step 5: CI.** Replace the restore/install/save/PATH tool steps with `jdx/mise-action@v3` (`version: 2026.8.16`, `install: true`, `cache: true`; env `MISE_TRUSTED_CONFIG_PATHS: ${{ github.workspace }}`). Turn `check` into a matrix over the six runners in Global Constraints with `fail-fast: false` and `defaults.run.shell: bash`; keep `rustup toolchain install` and `Swatinem/rust-cache@v2`; add a step `git config --global user.name ci && git config --global user.email ci@example.invalid` for the git-based tests in later tasks.
- [ ] **Step 6:** `docs/dev-setup.md`, one section per OS, each ending with `mise install` and `just ci`:
  - **macOS:** `brew install mise` (or `curl https://mise.run | sh`), rustup from rustup.rs, Xcode command line tools for `git`.
  - **Linux:** `curl https://mise.run | sh`, rustup, `git` from the distro.
  - **Windows (x64 and arm64):** `winget install Git.Git Microsoft.PowerShell jdx.mise Rustlang.Rustup`; Visual Studio Build Tools with the C++ workload (the MSVC linker Rust needs, `Microsoft.VisualStudio.Workload.VCTools`); add `C:\Program Files\Git\bin` to the user `Path` so `just` finds `sh` and `bash` from pwsh; `git config --global core.longpaths true`; add `%LOCALAPPDATA%\mise\shims` to the user `Path` for IDEs.
  - **Shells:** bash/zsh `eval "$(mise activate bash)"` / `zsh`; fish `mise activate fish | source`; pwsh `$PROFILE`: `mise activate pwsh | Out-String | Invoke-Expression`; Git Bash `~/.bashrc`: `export PATH="$(cygpath "$LOCALAPPDATA")/mise/shims:$PATH"` (not `mise activate bash`, which writes Windows-style paths into `PATH` under Git Bash).
- [ ] **Step 7:** `CLAUDE.md` Commands: replace the cargo-alias / `.tools/bin` paragraph with: dev tools are pinned in `mise.toml`; run `mise install`; to add a tool, add it to `mise.toml` for all six platforms and re-run the Step 3 `mise lock` command; Rust stays on rustup; link `docs/dev-setup.md`. Update the CI section (six-platform matrix, mise-action instead of the `.tools` cache). In the spec, add a section 2 Decisions row `| Dev tools | mise (`mise.toml` + `mise.lock`); Rust via rustup | New |` and change section 7.4's first line to "On Windows, Linux and macOS, each on x64 and arm64, in CI:".
- [ ] **Step 8:** Push the branch; confirm all six `check` jobs pass. If one fails on a platform-specific problem in existing code, fix it in this task.
- [ ] **Step 9:** Commit: `Install dev tools with mise and run CI on six platforms`.

---

### Task 2: Local test environments (Linux container, Windows VM, Mac)

**Files:**
- Create: `scripts/test-local/{linux.sh, windows.sh, macos.sh, linux.Containerfile, windows-vm-setup.ps1}`
- Modify: `justfile` (`test-local` recipe), `docs/dev-setup.md`, `CLAUDE.md`

**Interfaces:**
- Consumes: `mise.toml`, `rust-toolchain.toml` from Task 1.
- Produces: `just test-local [linux] [windows] [macos]` (default: all three). It prints `PASS`/`FAIL` per environment, then exits non-zero if any failed. Each script runs `just ci` on the working tree as it is now, including uncommitted and untracked files that aren't ignored.

Machine facts this task relies on (checked 2026-10-27): Apple M1 Pro, macOS 27.2, podman 6.1.4 at `/opt/podman/bin`, machine `podman-machine-default` (libkrun, aarch64, 5 CPUs, 9.3 GiB, 46 GiB disk, no `/dev/kvm`). Windows can't run under podman here, so it runs in UTM.

- [ ] **Step 1 (human, once): set up the Windows VM.** `brew install --cask utm`; create a VM named `devctl-win11` from the Windows 11 ARM64 ISO (Microsoft's download page; activation isn't needed for testing): 4 CPUs, 8 GB RAM, 80 GB disk, shared network. In the VM, run `scripts/test-local/windows-vm-setup.ps1` as Administrator. On the Mac, add an `~/.ssh/config` entry `Host devctl-win` pointing at the VM's IP with user `dev` and a key. Done when `ssh devctl-win "just --version"` prints `just 1.59.0`.
- [ ] **Step 2: Write `windows-vm-setup.ps1`.** It must be idempotent. It enables the OpenSSH server (`Add-WindowsCapability -Online -Name OpenSSH.Server~~~~0.0.1.0`, service set to automatic), sets the SSH `DefaultShell` registry value to pwsh, and installs `Git.Git`, `Microsoft.PowerShell`, `jdx.mise`, `Rustlang.Rustup` and VS Build Tools with `Microsoft.VisualStudio.Workload.VCTools` (including the ARM64 tools) via winget. It also adds `C:\Program Files\Git\bin` and `%LOCALAPPDATA%\mise\shims` to the machine `Path`, runs `git config --system core.longpaths true`, creates `C:\devctl-work`, and sets `CARGO_TARGET_DIR=C:\devctl-target` machine-wide.
- [ ] **Step 3: Write `linux.Containerfile`.** It uses `docker.io/library/ubuntu:24.04` and installs `git`, `curl`, `ca-certificates`, `build-essential` (the linker Rust needs, not a crate dependency) and `xz-utils`. It installs rustup with `--default-toolchain none` and mise `v2026.8.16` via `curl https://mise.run | MISE_VERSION=v2026.8.16 sh`. `PATH` includes `~/.cargo/bin`, `~/.local/bin` and `~/.local/share/mise/shims`. It sets git `user.name`/`user.email` and creates a non-root user `dev`, so tests that check file permissions behave as they do on a developer machine.
- [ ] **Step 4: Write the three runners.** Each one streams the tree as `git ls-files -co --exclude-standard -z | tar --null -T - -cf -` into the environment, unpacks it into a clean directory and runs `rustup toolchain install && mise trust && mise install && just ci` there.
  - `linux.sh`: `podman build -t devctl-test-linux -f scripts/test-local/linux.Containerfile scripts/test-local`, then `podman run --rm -i --platform linux/arm64` with named volumes `devctl-linux-rustup`, `devctl-linux-cargo`, `devctl-linux-mise` and `devctl-linux-target` (mounted at `CARGO_TARGET_DIR`) so later runs are incremental. If `podman machine list` doesn't show `Currently running`, it runs `podman machine start` first.
  - `windows.sh`: starts the VM with `utmctl start devctl-win11` if `utmctl status devctl-win11` isn't `started`, and waits up to 120 s for `ssh devctl-win exit`. It then runs `Remove-Item -Recurse -Force C:\devctl-work\*`, pipes the tar stream into `ssh devctl-win "tar -xf - -C C:/devctl-work"` (Windows' built-in bsdtar) and runs the commands in `C:\devctl-work`. If `utmctl` or the `devctl-win` host is missing, it fails with: "Windows VM not set up; see docs/dev-setup.md#local-windows-vm".
  - `macos.sh`: runs `just ci` in place, then `rustup target add x86_64-apple-darwin` and `cargo test --locked --workspace --target x86_64-apple-darwin`, which runs through Rosetta.
- [ ] **Step 5:** Add the recipe `test-local *envs="linux windows macos"`. It runs each script in turn without stopping at the first failure, prints `PASS <env>` or `FAIL <env>`, and exits 1 if any failed. It is not part of `ci`, because CI runners have no podman or UTM.
- [ ] **Step 6: Verify.** Run `just test-local`. Expected: `PASS linux`, `PASS windows`, `PASS macos`. Run `just test-local linux` twice; the second run should rebuild noticeably faster (incremental target volume). Break one test temporarily: all three report `FAIL` and the exit code is 1. Then revert.
- [ ] **Step 7:** In `docs/dev-setup.md`, add a "Local test environments" section with Step 1's one-time setup (anchor `local-windows-vm`) and how to run one environment at a time. Add `just test-local` to the Commands list in `CLAUDE.md`.
- [ ] **Step 8:** Commit: `Add local test runs for Linux, Windows and macOS`.

---

### Task 3: Workspace split and CI guards

**Files:**
- Modify: `Cargo.toml`, `justfile`, `CLAUDE.md`
- Create: `crates/devctl-{platform,lcov,junit,store}/Cargo.toml`, each with an empty `src/lib.rs`

**Interfaces:**
- Produces: workspace members `crates/*`; `[workspace.lints]` (move the current `[lints]` there; root and every crate set `lints.workspace = true`); `[workspace.dependencies]` for shared versions; just recipe `check-cfg`.

- [ ] **Step 1:** Convert `Cargo.toml` to a workspace with the root package as a member, add the four crates (each `edition = "2024"`, `rust-version = "1.99"`, `publish = false`) and add them as path dependencies of `devctl`.
- [ ] **Step 2:** Update `justfile`: `test` → `cargo test --locked --workspace {{filter}}`; `lint` → clippy with `--workspace --all-targets`; `check-pure` → `cargo tree --workspace …`; add `check-cfg` and include it in `ci: lint test check-pure check-cfg`. `check-cfg` must fail if `grep -rnE 'cfg!?\((.*target_os|unix|windows)' --include='*.rs'` finds a match outside `crates/devctl-platform/`, printing the matches.
- [ ] **Step 3:** Verify the guard works: temporarily add `#[cfg(unix)] fn _x() {}` to `src/lib.rs`, run `just check-cfg`, expect failure listing that line; remove it.
- [ ] **Step 4:** Run `just ci`. Expected: all pass, `ok: all dependencies are pure Rust`.
- [ ] **Step 5:** Update `CLAUDE.md` Architecture: list the four crates and their purpose (one line each), `check-cfg` in Commands, and replace "not … shelling out to `git`" with the CLI exception from Global Constraints.
- [ ] **Step 6:** Commit: `Split into a cargo workspace and add the cfg guard`.

---

### Task 4: `devctl-platform`

**Files:**
- Create: `crates/devctl-platform/src/{lib.rs, unix.rs, windows.rs, normalize.rs, fake.rs}`
- Test: unit tests in each file

**Interfaces:**
- Produces (all in `devctl_platform`):

```rust
pub trait Platform: Send + Sync {
    /// Starts `cmd` detached from this process (no console, own process group, null stdio). Returns its pid.
    fn spawn_detached(&self, cmd: Command) -> io::Result<u32>;
    /// Spawns `cmd` as the leader of a new process group so `kill_tree` can kill everything it starts.
    fn spawn_group(&self, cmd: Command) -> io::Result<Child>;
    fn kill_tree(&self, pid: u32) -> io::Result<()>;
    fn lock(&self, path: &Path) -> io::Result<Lock>;               // blocks
    fn try_lock(&self, path: &Path) -> io::Result<Option<Lock>>;   // None if held elsewhere
    fn open_browser(&self, url: &str) -> io::Result<()>;
    fn just(&self) -> Result<JustCommand, JustError>;
    fn data_dir(&self) -> Option<PathBuf>;
    /// Absolute, `/`-separated, no `\\?\` prefix; drive letter upper-cased on Windows.
    fn normalize_path(&self, p: &Path) -> String;
    fn make_executable(&self, path: &Path) -> io::Result<()>;
}
pub struct Lock { /* File; unlocked on drop */ }
pub struct JustCommand { pub program: PathBuf, pub version: (u32, u32, u32) }
impl JustCommand { pub fn command(&self) -> Command; pub fn version_string(&self) -> String; }
#[derive(Debug, thiserror::Error)]
pub enum JustError { Missing, TooOld { found: String }, Unreadable { output: String } }
pub const MIN_JUST: (u32, u32, u32) = (1, 59, 0);
pub fn current() -> Box<dyn Platform>;
pub fn parse_just_version(stdout: &str) -> Option<(u32, u32, u32)>;
pub fn normalize_windows(p: &str) -> String;   // pure, compiled on all OSes
pub fn normalize_unix(p: &str) -> String;
#[cfg(feature = "fake")] pub mod fake { pub struct FakePlatform; }
```

- [ ] **Step 1: Write failing tests** (in `lib.rs` / `normalize.rs`):

```rust
#[test] fn parses_just_version() {
    assert_eq!(parse_just_version("just 1.59.0\n"), Some((1, 59, 0)));
    assert_eq!(parse_just_version("just 1.60.2-dev"), Some((1, 60, 2)));
    assert_eq!(parse_just_version("nope"), None);
}
#[test] fn normalizes_windows_paths() {
    assert_eq!(normalize_windows(r"\\?\c:\work\repo\src\a.rs"), "C:/work/repo/src/a.rs");
    assert_eq!(normalize_windows(r"c:\work\.\repo\..\repo\b.rs"), "C:/work/repo/b.rs");
}
#[test] fn normalizes_unix_paths() {
    assert_eq!(normalize_unix("/work/./repo/../repo/a.rs"), "/work/repo/a.rs");
}
#[test] fn try_lock_is_exclusive() {
    let dir = tempfile::tempdir().unwrap(); let p = dir.path().join("x.lock");
    let held = current().try_lock(&p).unwrap().expect("first lock");
    assert!(current().try_lock(&p).unwrap().is_none());
    drop(held);
    assert!(current().try_lock(&p).unwrap().is_some());
}
#[test] fn kill_tree_kills_grandchildren() {
    // spawn_group a `sh -c 'sleep 30 & echo $! ; wait'` (Windows: `cmd /c start /b timeout 30 & …` —
    // use `ping -n 30 127.0.0.1` as the grandchild), read the grandchild pid, kill_tree(leader),
    // then assert the grandchild is gone within 2 s (Unix: rustix::process::test_kill; Windows: `tasklist /FI "PID eq n"`).
}
```

- [ ] **Step 2:** `just test -p devctl-platform`; expect compile failures.
- [ ] **Step 3: Implement.** `lib.rs` selects `unix::UnixPlatform` (`linux`/`macos` share it) or `windows::WindowsPlatform` in `current()`. Approaches:
  - locks: `std::fs::File::lock` / `try_lock` (stable std; no extra crate), file created if missing, parent dirs created.
  - Unix: `CommandExt::process_group(0)` for both spawns; `kill_tree` = `rustix::process::kill_process_group(pgid, Signal::KILL)`; `open_browser` = `open` (macOS) / `xdg-open` (Linux); `data_dir` = `~/Library/Application Support/devctl` or `$XDG_DATA_HOME/devctl` falling back to `~/.local/share/devctl`; `make_executable` sets mode `0o755`.
  - Windows: `creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW)` for `spawn_detached`, `CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW` for `spawn_group`; `kill_tree` = `taskkill /T /F /PID <pid>`; `open_browser` = `cmd /c start "" <url>`; `data_dir` = `%LOCALAPPDATA%\devctl`; `make_executable` is a no-op.
  - `just()`: run `just --version`; `NotFound` → `Missing`; unparsable → `Unreadable`; below `MIN_JUST` → `TooOld`. `Display` messages: "`just` not found on PATH; install just 1.59.0 or newer (https://just.systems)" and "`just` {found} is too old; devctl needs 1.59.0 or newer".
  - `fake` feature: `FakePlatform` delegates process and lock calls to `current()` but records every `spawn_detached` command (program + args + env) in a `Mutex<Vec<…>>` **without** spawning; `just()` returns a value set via `FakePlatform::with_just(Result<JustCommand, JustError>)`, defaulting to the real one.
- [ ] **Step 4:** `just test -p devctl-platform` passes; `just check-cfg` passes.
- [ ] **Step 5:** Commit: `Add devctl-platform with Unix and Windows implementations`.

---

### Task 5: `devctl-lcov`

**Files:**
- Create: `crates/devctl-lcov/src/lib.rs`, `crates/devctl-lcov/tests/parse.rs`, `crates/devctl-lcov/tests/fixtures/{cargo-llvm-cov,vitest,jest,pytest-cov,jacoco,go,coverlet,windows-paths,no-end-of-record,empty}.info`

**Interfaces:**

```rust
pub struct Report { pub files: BTreeMap<String, FileCoverage> }        // key = SF path as written
pub struct FileCoverage {
    pub lines: BTreeMap<u32, u64>,                                        // DA line -> hits
    pub functions: BTreeMap<String, Function>,                            // FN/FNDA
    pub branches: BTreeMap<(u32, String, String), Option<u64>>,           // BRDA line,block,branch -> taken ("-" = None)
}
pub struct Function { pub line: u32, pub hits: u64 }
pub struct LineCounts { pub coverable: u32, pub covered: u32 }
#[derive(Debug, thiserror::Error)] #[error("line {line}: {reason}")]
pub struct ParseError { pub line: usize, pub reason: String }
pub fn parse(input: &str) -> Result<Report, ParseError>;
impl Report {
    pub fn merge(reports: &[Report]) -> Report;          // sums hits per file/line/function/branch
    pub fn to_lcov(&self) -> String;                     // round-trips through parse
    pub fn line_counts(&self) -> (LineCounts, BTreeMap<String, LineCounts>);
    pub fn map_paths(self, f: impl FnMut(&str) -> Option<String>) -> Report;   // None drops the file; collisions merge
}
```

- [ ] **Step 1: Fixtures.** Hand-write each fixture to match its tool's real output shape (they can't be generated here): cargo-llvm-cov (absolute SF, FN with mangled names, BRDA), vitest/jest (relative SF, `TN:`, FNF/FNH/LF/LH), pytest-cov (no FN, DA only), jacoco via `jacoco-to-lcov` style, go via `gcov2lcov` style, coverlet (`BRDA` with `-`), `windows-paths` (`SF:C:\work\repo\src\a.cs`), `no-end-of-record` (last record unterminated), `empty` (0 bytes). Include a `DA:line,hits,checksum` line somewhere.
- [ ] **Step 2: Failing tests** in `tests/parse.rs`, one per fixture, asserting exact `line_counts()` totals and one specific `lines[&n]` value per fixture, plus:

```rust
#[test] fn empty_file_is_empty_report() { assert!(parse("").unwrap().files.is_empty()); }
#[test] fn missing_end_of_record_keeps_last_file() { assert_eq!(parse(fixture("no-end-of-record")).unwrap().files.len(), 2); }
#[test] fn merge_sums_hits() {
    let a = parse("SF:a.rs\nDA:1,1\nDA:2,0\nend_of_record\n").unwrap();
    let b = parse("SF:a.rs\nDA:2,3\nDA:3,0\nend_of_record\n").unwrap();
    let m = Report::merge(&[a, b]);
    assert_eq!(m.files["a.rs"].lines, BTreeMap::from([(1, 1), (2, 3), (3, 0)]));
    assert_eq!(m.line_counts().0, LineCounts { coverable: 3, covered: 2 });
}
#[test] fn round_trips() { let r = parse(fixture("coverlet")).unwrap(); assert_eq!(parse(&r.to_lcov()).unwrap(), r); }
#[test] fn crlf_line_endings_parse() { assert_eq!(parse("SF:a.rs\r\nDA:1,2\r\nend_of_record\r\n").unwrap().files["a.rs"].lines[&1], 2); }
#[test] fn bad_da_reports_line_number() { assert_eq!(parse("SF:a\nDA:x,1\n").unwrap_err().line, 2); }
#[test] fn map_paths_drops_and_merges() { /* None drops; two keys mapped to one merge */ }
```

- [ ] **Step 3:** Run, expect failures. **Step 4:** Implement (line-based parser; unknown record types ignored; `DA` lines before any `SF` are an error). **Step 5:** `just test -p devctl-lcov` passes.
- [ ] **Step 6:** Commit: `Add devctl-lcov parser and merge`.

---

### Task 6: `devctl-junit`

**Files:**
- Create: `crates/devctl-junit/src/lib.rs`, `crates/devctl-junit/tests/parse.rs`, `crates/devctl-junit/tests/fixtures/{nextest,vitest,jest-junit,pytest,surefire,go-junit-report,dotnet-junit-logger,single-suite-root,nested-suites}.xml`

**Interfaces:**

```rust
pub enum CaseStatus { Passed, Failed, Error, Skipped }
pub struct Case { pub name: String, pub classname: Option<String>, pub status: CaseStatus, pub duration: Duration }
pub struct Suite { pub name: String, pub cases: Vec<Case> }
pub struct TestRun { pub suites: Vec<Suite> }
#[derive(Default, PartialEq, Debug)]
pub struct TestCounts { pub passed: u32, pub failed: u32, pub errored: u32, pub skipped: u32, pub duration: Duration }
pub fn parse(xml: &str) -> Result<TestRun, ParseError>;   // ParseError wraps quick-xml errors with byte position
impl TestRun { pub fn merge(runs: Vec<TestRun>) -> TestRun; pub fn counts(&self) -> TestCounts; }
```

- [ ] **Step 1:** Hand-write fixtures in each tool's real shape (root `<testsuites>` vs root `<testsuite>`, nested suites, `<skipped/>`, `<failure>` with CDATA, `<error>`, `time="1,234.5"` from surefire in some locales, missing `time`, XML entities in names).
- [ ] **Step 2: Failing tests**: one per fixture asserting exact `counts()` (all four counters) and one case name; plus:

```rust
#[test] fn duration_with_thousands_separator() { /* time="1,234.5" -> 1234.5 s */ }
#[test] fn missing_time_is_zero() { … }
#[test] fn suite_duration_is_sum_of_cases() { /* counts().duration sums case times, not suite time attrs */ }
#[test] fn crlf_xml_parses() { /* a fixture's text with \n replaced by \r\n -> same counts() */ }
#[test] fn malformed_xml_errors() { assert!(parse("<testsuite><testcase>").is_err()); }
```

- [ ] **Step 3–5:** Run (fail), implement with `quick_xml::Reader` event loop (a case's status is the first of `failure`/`error`/`skipped` child, else `Passed`; nested `testsuite`s flatten into `suites`), run (pass).
- [ ] **Step 6:** Commit: `Add devctl-junit parser`.

---

### Task 7: `devctl-store`

**Files:**
- Create: `crates/devctl-store/src/{lib.rs, record.rs, redb_store.rs, schema.rs}`, `crates/devctl-store/tests/store.rs`

**Interfaces:**

```rust
pub type RecordId = u64;
#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct Record {
    pub commit: Vec<u8>, pub tree: Vec<u8>, pub branch: String,
    pub started_at_ms: i64, pub finished_at_ms: i64,
    pub status: Status,
    pub coverage: CoverageSummary,
    pub tests: TestSummary,
    pub devctl_version: String, pub just_version: String,
}
pub enum Status { Ok, TestsFailed, Error { stderr_tail: String } }
impl Status { pub fn error(message: &str) -> Status; }        // keeps the last 4096 bytes, on a char boundary
pub struct LineCounts { pub coverable: u32, pub covered: u32 }
pub struct CoverageSummary { pub total: LineCounts, pub files: BTreeMap<String, LineCounts> }
pub struct TestSummary { pub passed: u32, pub failed: u32, pub errored: u32, pub skipped: u32, pub duration_ms: u64 }
pub struct RawReports { pub lcov: String, pub junit: Vec<String> }
pub struct RetentionPolicy { pub records_per_branch: usize, pub raw_reports_days: u32 }
pub struct PruneStats { pub records_removed: usize, pub raw_removed: usize }

pub trait Store {
    fn put_record(&self, record: &Record, raw: Option<&RawReports>) -> Result<RecordId, StoreError>;
    fn record_for(&self, commit: &[u8]) -> Result<Option<(RecordId, Record)>, StoreError>;
    fn record_for_tree(&self, tree: &[u8]) -> Result<Option<(RecordId, Record)>, StoreError>;
    fn records_on(&self, branch: &str, limit: usize) -> Result<Vec<(RecordId, Record)>, StoreError>; // newest first
    fn latest_ok(&self, branch: &str) -> Result<Option<(RecordId, Record)>, StoreError>;
    fn branches(&self) -> Result<Vec<String>, StoreError>;
    fn raw(&self, id: RecordId) -> Result<Option<RawReports>, StoreError>;
    fn prune(&self, policy: &RetentionPolicy, now_ms: i64) -> Result<PruneStats, StoreError>;
}
pub struct RedbStore { /* path */ }
impl RedbStore { pub fn new(path: PathBuf) -> RedbStore; pub fn with_busy_budget(self, d: Duration) -> RedbStore; }
pub const SCHEMA_VERSION: u32 = 1;
#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("store at {path} is busy (another devctl process held it for 5 s); try again")] Busy { path: PathBuf },
    #[error("store at {path} is corrupt ({reason}); delete it and run `devctl record`")] Corrupt { path: PathBuf, reason: String },
    #[error("store at {path} has schema {found}, this devctl supports {supported}; upgrade devctl")] NewerSchema { path: PathBuf, found: u32, supported: u32 },
    #[error("store at {path}: {source}")] Io { path: PathBuf, source: std::io::Error },
}
```

Tables exactly as spec 7.2: `meta` (`&str` → `u32`), `records` (`u64` → postcard bytes), `by_commit` (`&[u8]` → `u64`), `by_tree` (`&[u8]` → `u64`, written only for `Ok`/`TestsFailed`), `by_branch` (`(&str, i64)` → `u64`), `raw` (`u64` → ruzstd-compressed postcard of `RawReports`, `CompressionLevel::Fastest`).

- [ ] **Step 1: Failing tests** in `tests/store.rs` (each on a `tempfile` dir):

```rust
#[test] fn put_then_lookup_by_commit_tree_branch() { … }
#[test] fn by_tree_ignores_error_records() { /* put Error record for tree T -> record_for_tree(T) is None */ }
#[test] fn record_for_returns_latest() { /* two records same commit -> later id */ }
#[test] fn records_on_is_newest_first_and_limited() { … }
#[test] fn raw_round_trips_compressed() { /* 1 MB repetitive LCOV; file size on disk < 200 KB */ }
#[test] fn prune_keeps_n_per_branch_and_drops_old_raw() {
    // 205 records on "dev", records_per_branch=200 -> 5 removed (oldest), indexes cleaned;
    // a record finished 91 days before now_ms keeps its summary but raw() is None.
}
#[test] fn newer_schema_is_refused() { /* write meta.schema = 2 directly with redb -> NewerSchema */ }
#[test] fn garbage_file_is_corrupt() { /* write b"not redb" to path -> Corrupt with path */ }
#[test] fn concurrent_processes_all_succeed() {
    // spawn 8 threads, each calling put_record 20 times on its own RedbStore::new(same path);
    // (redb's per-process exclusivity applies across handles too) -> 160 records, none lost.
}
#[test] fn busy_budget_is_honoured() { /* hold Database open in this test; with_busy_budget(200ms) -> Busy */ }
```

- [ ] **Step 2:** Run; expect failures.
- [ ] **Step 3: Implement.** Each trait method: open (`Database::create`), one transaction, drop. On `DatabaseError::DatabaseAlreadyOpen` (or the lock-held I/O error) sleep 10 ms doubling up to the busy budget (default 5 s), emitting `tracing::debug!(monotonic_counter.devctl.store.busy_retries = 1_u64)` per retry. `schema.rs`: on open, missing `meta.schema` → write `SCHEMA_VERSION`; lower → run `migrate(from)` (no steps yet; a `match` with only the identity arm); higher → `NewerSchema`. Wrap every operation in a `tracing::info_span!("store.txn", op = …)`.
- [ ] **Step 4:** `just test -p devctl-store` passes; `just check-pure` passes.
- [ ] **Step 5:** Commit: `Add redb-backed devctl-store`.

---

### Task 8: `config`

**Files:**
- Create: `src/config.rs`; Modify: `src/lib.rs` (`pub mod config;`), `Cargo.toml` (`serde`, `toml`, `regex`)

**Interfaces:**

```rust
pub struct Config { pub branches: Branches, pub gate: Gate, pub commit: CommitLimits, pub record: RecordCfg, pub retention: Retention, pub telemetry: TelemetryCfg }
pub struct Branches { pub protected: Vec<String>, pub pattern: Regex, pub targets: BTreeMap<String, String> }
pub struct Gate { pub new_code_coverage: u8 }
pub struct CommitLimits { pub max_files: u32, pub max_lines: u32 }    // files <= max_files; added+removed < max_lines
pub struct RecordCfg { pub timeout_minutes: u32 }
pub struct Retention { pub records_per_branch: usize, pub raw_reports_days: u32 }
pub struct TelemetryCfg { pub endpoint: String, pub include_paths: bool, pub log_level: String }
impl Default for Config;                                               // spec section 6 values; commit 10 / 500
impl Config {
    pub fn load(worktree_root: &Path) -> anyhow::Result<Config>;      // missing file = defaults
    pub fn parse(text: &str) -> anyhow::Result<Config>;
    pub fn target_for(&self, branch: &str) -> Option<&str>;            // prefix before first '/' looked up in targets
    pub fn is_protected(&self, branch: &str) -> bool;
}
```

- [ ] **Step 1: Failing tests** (unit, in `config.rs`):

```rust
#[test] fn empty_is_defaults() { let c = Config::parse("").unwrap(); assert_eq!(c.gate.new_code_coverage, 95); assert_eq!(c.commit.max_files, 10); assert_eq!(c.commit.max_lines, 500); assert_eq!(c.branches.protected, ["master", "dev"]); }
#[test] fn partial_section_keeps_other_defaults() { let c = Config::parse("[gate]\nnew_code_coverage = 80").unwrap(); assert_eq!(c.record.timeout_minutes, 30); }
#[test] fn threshold_over_100_names_key() { assert_eq!(Config::parse("[gate]\nnew_code_coverage = 120").unwrap_err().to_string(), "invalid .devctl.toml: gate.new_code_coverage: must be 0–100, got 120"); }
#[test] fn bad_regex_names_key() { assert!(Config::parse("[branches]\npattern = \"(\"").unwrap_err().to_string().starts_with("invalid .devctl.toml: branches.pattern:")); }
#[test] fn unknown_key_is_rejected() { assert!(Config::parse("[gate]\nfoo = 1").unwrap_err().to_string().contains("foo")); }
#[test] fn target_for_uses_prefix() { let c = Config::default(); assert_eq!(c.target_for("feature/x"), Some("dev")); assert_eq!(c.target_for("hotfix/y"), Some("master")); assert_eq!(c.target_for("wip/z"), None); }
#[test] fn telemetry_log_level_validated() { assert!(Config::parse("[telemetry]\nlog_level = \"loud\"").is_err()); }
```

- [ ] **Step 2–4:** Run (fail); implement with `#[serde(default, deny_unknown_fields)]` raw structs then a validating conversion (also: `timeout_minutes ≥ 1`, `max_files ≥ 1`, `max_lines ≥ 1`); run (pass).
- [ ] **Step 5:** Commit: `Load and validate .devctl.toml`.

---

### Task 9: `telemetry`

**Files:**
- Create: `src/telemetry.rs`, `tests/telemetry_otlp.rs`; Modify: `Cargo.toml`, `justfile` (allowlist `js-sys`, `web-sys` with a comment saying why)

Dependencies (verified pure with TLS off):

```toml
opentelemetry = "0.33.1"
opentelemetry_sdk = { version = "0.33.1", default-features = false, features = ["trace", "logs", "metrics"] }
opentelemetry-otlp = { version = "0.33.1", default-features = false, features = ["http-proto", "reqwest-blocking-client", "trace", "logs", "metrics"] }
opentelemetry-appender-tracing = "0.33.1"
tracing-opentelemetry = { version = "0.34.0", features = ["metrics"] }
tracing-appender = "0.2.5"
tracing-subscriber = { version = "0.3.23", features = ["env-filter"] }
# dev: opentelemetry_sdk feature "testing" for in-memory exporters
```

**Interfaces:**

```rust
pub const PATH_TARGET: &str = "devctl::paths";   // events carrying file paths use this target
pub struct Settings<'a> { pub process: &'a str, pub logs_dir: Option<&'a Path>, pub cfg: &'a TelemetryCfg, pub flush_timeout: Duration, pub repo: Option<&'a str> }
pub struct Guard;                                  // Drop: flush + shutdown on a thread, abandoned after flush_timeout with a logged warning
pub fn init(s: &Settings) -> Guard;               // never fails: problems become a warning in the log
pub fn init_with(s: &Settings, exporters: Exporters) -> Guard;   // tests: in-memory exporters
pub fn traceparent() -> Option<String>;            // W3C header for the current span, None when export is off
pub fn adopt_traceparent(span: &tracing::Span);    // reads TRACEPARENT from env
pub fn export_endpoint(cfg: &TelemetryCfg) -> Option<String>;   // cfg.endpoint, else OTEL_EXPORTER_OTLP_ENDPOINT; None if empty or OTEL_SDK_DISABLED=true
```

Metric names, emitted elsewhere via tracing-opentelemetry's `MetricsLayer` field prefixes: `histogram.devctl.record.duration` (s, field `status`), `histogram.devctl.recipe.duration` (s, field `recipe`), `monotonic_counter.devctl.gate.result` (fields `rule`, `outcome`), `monotonic_counter.devctl.store.busy_retries`, `monotonic_counter.devctl.worker.cancelled`.

- [ ] **Step 1: Failing tests** (unit, in `telemetry.rs`):

```rust
#[test] fn endpoint_resolution() { /* cfg wins over env; empty both -> None; OTEL_SDK_DISABLED=true -> None */ }
#[test] fn spans_and_metrics_reach_exporter() {
    // init_with in-memory exporters; inside info_span!("gate.rule", rule = "tests") emit
    // info!(monotonic_counter.devctl.gate.result = 1_u64, rule = "tests", outcome = "pass"); drop guard;
    // assert one span named "gate.rule" and a sum metric "devctl.gate.result" == 1 with both attributes.
}
#[test] fn service_name_from_env() { /* OTEL_SERVICE_NAME=x -> resource service.name = "x"; default "devctl" */ }
#[test] fn paths_not_exported_unless_enabled() {
    // event with target PATH_TARGET -> absent from in-memory logs when include_paths=false, present when true
}
#[test] fn traceparent_round_trips() { /* traceparent() inside a span, adopt_traceparent on a new span -> same trace id */ }
#[test] fn local_log_file_written() { /* logs_dir set -> a file starting with "<process>." exists after drop */ }
```

And `tests/telemetry_otlp.rs`: start a `std::net::TcpListener` on `127.0.0.1:0` that accepts and replies `200` to each request while recording its path; run `devctl version` with `OTEL_EXPORTER_OTLP_ENDPOINT=http://127.0.0.1:<port>`; assert a request to `/v1/traces` arrived. Then point at a closed port and assert `devctl version` still exits 0 in under 3 s.

- [ ] **Step 2:** Run; expect failures.
- [ ] **Step 3: Implement.** Subscriber = `EnvFilter` from `cfg.log_level` + file layer (`tracing_appender::rolling::Builder` daily, prefix `process`, suffix `log`, `max_log_files(7)`) when `logs_dir` is set; when export is on, add `OpenTelemetryLayer` (traces), `OpenTelemetryTracingBridge` (logs) and `MetricsLayer`, each with a per-layer filter excluding `PATH_TARGET` unless `include_paths`. Resource: `service.name`, `service.version`, `vcs.repository.name` = `repo`. Never record developer identity. Wiring into commands happens in Task 10.
- [ ] **Step 4:** `just test telemetry`, then `just check-pure` (with the new allowlist entries). Expected: pass.
- [ ] **Step 5:** Commit: `Add tracing and OpenTelemetry export`.

---

### Task 10: `git` and `ctx`

**Files:**
- Create: `src/git.rs`, `src/ctx.rs`, `src/test_support.rs`, `tests/support/mod.rs`; Modify: `src/lib.rs`

**Interfaces:**

```rust
// git.rs
pub struct FileChange { pub path: String, pub added: u32, pub removed: u32, pub binary: bool }
pub type AddedLines = BTreeMap<String, BTreeSet<u32>>;   // path ('/'-separated, repo-relative) -> new line numbers
pub trait Repo {
    fn common_dir(&self) -> &Path;
    fn workdir(&self) -> Option<&Path>;
    fn current_branch(&self) -> anyhow::Result<Option<String>>;     // None = detached
    fn resolve(&self, rev: &str) -> anyhow::Result<ObjectId>;
    fn tree_of(&self, commit: ObjectId) -> anyhow::Result<ObjectId>;
    fn is_ancestor(&self, ancestor: ObjectId, descendant: ObjectId) -> anyhow::Result<bool>;
    fn merge_base(&self, a: ObjectId, b: ObjectId) -> anyhow::Result<Option<ObjectId>>;
    fn merges_in_range(&self, base: Option<ObjectId>, tip: ObjectId) -> anyhow::Result<Vec<ObjectId>>;  // reachable from tip, not from base
    fn added_lines(&self, base: Option<ObjectId>, tip: ObjectId) -> anyhow::Result<AddedLines>;          // None = empty tree; renames tracked; binary skipped
    fn staged_changes(&self) -> anyhow::Result<Vec<FileChange>>;   // HEAD tree (or empty) vs index; renames as one entry
    fn is_merging(&self) -> bool;                                  // MERGE_HEAD exists
    fn config_string(&self, key: &str) -> anyhow::Result<Option<String>>;
    fn ref_target(&self, name: &str) -> anyhow::Result<Option<ObjectId>>;
    fn remote_url_name(&self) -> Option<String>;                   // repo name for telemetry (last path segment of origin url, sans .git)
}
pub struct GixRepo { /* gix::ThreadSafeRepository */ }
impl GixRepo { pub fn discover(cwd: &Path) -> anyhow::Result<GixRepo>; }
pub fn git(dir: &Path, args: &[&str]) -> anyhow::Result<String>;   // runs the git CLI; error includes args and stderr

// ctx.rs
pub fn slug(branch: &str) -> String;
pub struct DevctlDirs { pub root: PathBuf }    // <common dir>/devctl
impl DevctlDirs { pub fn store(&self) -> PathBuf; pub fn queue(&self, b: &str) -> PathBuf; pub fn lock(&self, b: &str) -> PathBuf; pub fn queue_lock(&self, b: &str) -> PathBuf; pub fn measure(&self, b: &str) -> PathBuf; pub fn logs(&self) -> PathBuf; }
pub struct Ctx { pub repo: GixRepo, pub platform: Box<dyn Platform>, pub config: Config, pub dirs: DevctlDirs }
impl Ctx {
    pub fn open(cwd: &Path, platform: Box<dyn Platform>) -> anyhow::Result<Ctx>;   // config from the worktree top level
    pub fn store(&self) -> RedbStore;
}
```

`queue_lock(b)` is `locks/<slug>.queue.lock`; the others follow spec 3.2.

- [ ] **Step 1:** `TestRepo` helper (both copies): `TestRepo::new()` → tempdir with `git init -b dev`, identity via `-c user.name=t -c user.email=t@t` on each commit, methods `write(path, contents)`, `commit(msg) -> String` (hex), `checkout_new(branch)`, `merge_no_ff(branch)`, `git(args) -> String`, `path()`.
- [ ] **Step 2: Failing tests** (unit, `git.rs`):

```rust
#[test] fn added_lines_counts_only_additions() { /* base "a\nb\nc\n" -> tip "a\nX\nc\nd\n" => {"f.txt": {2, 4}} */ }
#[test] fn added_lines_follows_renames() { /* git mv a.rs b.rs + add line 5 -> {"b.rs": {5}} */ }
#[test] fn added_lines_skips_binary() { /* file with NUL byte -> absent */ }
#[test] fn added_lines_from_empty_tree() { /* base None -> every line of every file */ }
#[test] fn merges_in_range_finds_merge_commit() { … }
#[test] fn merge_base_and_ancestry() { … }
#[test] fn staged_changes_counts_add_and_remove() { /* "a\nb\nc\nd\n" -> "a\nX\nd\ne\nf\n", staged -> added 3, removed 2 */ }
#[test] fn staged_changes_on_unborn_branch() { /* first commit, 2 staged files -> both listed */ }
#[test] fn detached_head_has_no_branch() { … }
#[test] fn common_dir_is_shared_from_linked_worktree() { /* git worktree add; GixRepo::discover(wt).common_dir() == main .git */ }
#[test] fn slug_replaces_slashes() { assert_eq!(slug("feature/x"), "feature-x"); }
```

- [ ] **Step 3–4:** Run (fail), implement, and in `run` call `telemetry::init` after `Ctx::open` with process = `cli`, `worker`, or `hook-<name>`, `logs_dir = dirs.logs()`, `repo = remote_url_name()` and flush timeout 2 s for `hook pre-push`, else 5 s (`version` runs without telemetry). Tree diffs via `gix` `diff_tree_to_tree` with rewrite tracking; line diffs via `gix::diff::blob` (imara-diff) histogram algorithm; staged changes via `Repository::tree_index_status`; binary = NUL in the first 8000 bytes, matching git. Run (pass).
- [ ] **Step 5:** Commit: `Add gix-backed repository access`.

---

### Task 11: `recipes`

**Files:**
- Create: `src/recipes.rs`; Modify: `src/lib.rs`

**Interfaces:**
- Consumes: `Platform::{spawn_group, kill_tree, normalize_path}`, `JustCommand`, `devctl_lcov`, `devctl_junit`, `Status`, `RawReports`.
- Produces:

```rust
pub struct RecipeRun<'a> {
    pub platform: &'a dyn Platform, pub just: &'a JustCommand, pub root: &'a Path,
    pub commit: &'a str, pub timeout: Duration, pub cancel: &'a dyn Fn() -> bool,   // polled every 1 s
}
pub enum RecipeExit { Exited { success: bool, stderr_tail: String }, Cancelled, TimedOut }
pub enum ContractResult {
    Done { status: Status, report: Option<Report>, tests: Option<TestRun>, raw: RawReports },
    Cancelled,
}
pub fn missing_recipes(just: &JustCommand, root: &Path) -> anyhow::Result<Vec<&'static str>>;   // via `just --summary`
pub fn run_recipe(run: &RecipeRun, name: &str) -> anyhow::Result<RecipeExit>;
pub fn collect(root: &Path, platform: &dyn Platform) -> anyhow::Result<(Option<Report>, Option<TestRun>, RawReports)>;
pub fn classify(exit_ok: bool, stderr_tail: &str, report: Option<&Report>, tests: Option<&TestRun>) -> Status;
pub fn run_contract(run: &RecipeRun) -> anyhow::Result<ContractResult>;
```

Contract (spec section 5): delete `coverage/` and `test-results/`; missing recipe → `Status::error("missing recipe `coverage`; run `devctl init`")`; run `coverage`; if `test-results/*.xml` absent run `test`; `collect` parses all `coverage/*.info` (merged) and `test-results/*.xml` (merged); LCOV paths rewritten by `map_paths` to worktree-relative `/` paths using `normalize_path` (relative `SF` resolved against root first; outside root → dropped); `raw.lcov` is the normalized merged `to_lcov()`. Timeout → `Status::error("recipe `<name>` exceeded record.timeout_minutes (<n>); its process tree was killed")`. `classify`: no LCOV → error "no LCOV output in coverage/*.info"; no JUnit → error "no JUnit output in test-results/*.xml"; any failed/errored case or non-zero exit → `TestsFailed`; else `Ok`. Each recipe run is a `recipe.run` span and emits `histogram.devctl.recipe.duration`.

- [ ] **Step 1: Failing tests** (unit, tempdir roots with a hand-written `justfile`; tests need `just` ≥ 1.59 on PATH):

```rust
#[test] fn classify_table() {
    // (exit_ok, lcov, junit counts) -> status, one assert per spec row:
    // (true, some, 0 failed) Ok; (false, some, some) TestsFailed; (false, some, None) Error;
    // (true, None, some) Error; (true, some, 1 failed) TestsFailed
}
#[test] fn skips_test_recipe_when_coverage_wrote_junit() { /* test recipe writes marker file; assert absent */ }
#[test] fn runs_test_recipe_otherwise() { … }
#[test] fn passes_devctl_env() { /* recipe: echo $DEVCTL $DEVCTL_COMMIT > env.txt -> "1 abc123" */ }
#[test] fn missing_recipe_message() { /* justfile without `test` -> Error containing "missing recipe `test`; run `devctl init`" */ }
#[test] fn lcov_paths_relativized_and_outside_dropped() { /* SF:<root>/src/a.rs and SF:/elsewhere/b.rs -> only "src/a.rs" */ }
#[test] fn cancel_kills_recipe_tree() { /* recipe `sleep 30 & sleep 30`; cancel returns true after 1 s -> Cancelled in < 4 s */ }
#[test] fn timeout_is_error() { /* timeout 1 s, recipe sleeps 30 -> Done{status: Error(..exceeded..)} */ }
#[test] fn stale_outputs_removed_before_run() { /* pre-existing coverage/old.info is not parsed */ }
```

- [ ] **Step 2–4:** Run (fail), implement (stderr piped to a reader thread keeping the last 4096 bytes; stdout to null), run (pass).
- [ ] **Step 5:** Commit: `Run the just recipe contract`.

---

### Task 12: `record` — queue, measuring worktrees, worker, `record` and `worker` commands

**Files:**
- Create: `src/record/{mod.rs, queue.rs, measure.rs, worker.rs}`; Modify: `src/cli.rs`, `src/lib.rs`

**Interfaces:**
- Consumes: `Ctx`, `Repo`, `recipes::run_contract`, `Store`, `Platform::{lock, try_lock, spawn_detached}`, `telemetry::{traceparent, adopt_traceparent}`.
- Produces:

```rust
// queue.rs — all writes and the compare-and-delete happen under queue_lock(branch)
pub fn enqueue(ctx: &Ctx, branch: &str, commit: ObjectId) -> anyhow::Result<()>;   // overwrite
pub fn peek(ctx: &Ctx, branch: &str) -> anyhow::Result<Option<ObjectId>>;
pub fn complete(ctx: &Ctx, branch: &str, commit: ObjectId) -> anyhow::Result<bool>; // deletes only if still == commit
// measure.rs
pub fn prepare(ctx: &Ctx, branch: &str, commit: ObjectId) -> anyhow::Result<PathBuf>;
// worker.rs
pub enum Outcome { Recorded(RecordId, Record), Reused(RecordId, Record), Superseded }
pub fn record_commit(ctx: &Ctx, branch: &str, commit: ObjectId, cancel: &dyn Fn() -> bool) -> anyhow::Result<Outcome>;
pub fn run_worker(ctx: &Ctx, branch: &str) -> anyhow::Result<()>;   // returns at once if the branch lock is held
pub fn spawn_worker(ctx: &Ctx, branch: &str) -> anyhow::Result<()>; // current_exe() worker --branch b, detached, TRACEPARENT set
pub fn worker_running(ctx: &Ctx, branch: &str) -> anyhow::Result<bool>;   // try_lock fails
// cli.rs
Command::Record { #[arg(long)] commit: Option<String> }
#[command(hide = true)] Command::Worker { #[arg(long)] branch: String }
```

Algorithms:
- `prepare`: if `measure(branch)` isn't a registered worktree, `git worktree prune`, then `git -c core.longpaths=true worktree add --detach <path> <commit>`; else `git -C <path> -c core.longpaths=true checkout --detach --force <commit>`. Untracked build output is kept (incremental builds).
- `record_commit`: if `record_for_tree(tree)` exists, copy it (new commit, branch, timestamps; same raw) → `Reused`. Else `prepare`, `run_contract`; `Cancelled` → `Superseded` and emit `monotonic_counter.devctl.worker.cancelled`. Otherwise build the `Record` (versions from `CARGO_PKG_VERSION` and `JustCommand::version_string`), `put_record`, `prune(retention, now)`, emit `histogram.devctl.record.duration` with `status`. Any error inside (missing `just`, git failure) becomes an `Error` record so `status` shows it.
- `run_worker`: `loop { let Some(lock) = try_lock(lock(branch)) else { return }; drain(ctx, branch, &lock); drop(lock); if peek().is_none() { return } }` — the re-check after release covers a hook that queued (and whose spawned worker exited) while this worker held the lock. `pub fn drain(ctx, branch, _held: &Lock) -> anyhow::Result<()>`: loop `peek` → none: break; `record_commit` with `cancel = || peek() != Some(commit)`; `complete`.
- `record` command: branch = `current_branch()` or fail "HEAD is detached; check out a branch, then run `devctl record`"; `enqueue` then `run_worker` in the foreground (waiting on the lock with `lock` if a background worker holds it, then checking `record_for(commit)`), print the summary line from Task 13's `format_summary`, exit non-zero only for `Error`.

- [ ] **Step 1: Failing tests** (unit with `TestRepo` + a fixture `justfile` whose `coverage` recipe copies committed `fixture/cov.info` and `fixture/junit.xml` into place, `test` recipe a no-op):

```rust
#[test] fn records_head() { /* record_commit -> Recorded, status Ok, coverage totals from fixture */ }
#[test] fn amend_message_only_reuses() { /* commit, record, `git commit --amend -m new`, record -> Reused, same coverage, raw() present */ }
#[test] fn newer_job_supersedes() { /* slow recipe (sleep 5); enqueue A, start run_worker on a thread, after 1 s enqueue B -> store has B, no record for A, < 10 s */ }
#[test] fn second_worker_exits_when_locked() { … }
#[test] fn job_queued_during_exit_is_picked_up() { /* hold the lock, drain(A), enqueue(B), drop the lock, then the run_worker re-check path records B */ }
#[test] fn killed_worker_releases_lock() { /* spawn `devctl worker` child on slow recipe, kill it, worker_running() == false */ }
#[test] fn missing_just_becomes_error_record() { /* FakePlatform::with_just(Err(Missing)) -> Error record mentioning just.systems */ }
#[test] fn spawn_worker_passes_branch_and_traceparent() { /* FakePlatform records: args ["worker","--branch","feature/x"] */ }
#[test] fn record_on_detached_head_fails() { … }
```

- [ ] **Step 2–4:** Run (fail), implement, run (pass). Then `just run record` in a scratch repo to see the summary.
- [ ] **Step 5:** Commit: `Record commits in measuring worktrees with a background worker`.

---

### Task 13: `status`

**Files:**
- Create: `src/status.rs`; Modify: `src/cli.rs`, `src/lib.rs`

**Interfaces:**

```rust
pub fn format_summary(record: &Record, previous: Option<&Record>, now_ms: i64) -> String;
pub fn run_status(ctx: &Ctx, branch: Option<&str>) -> anyhow::Result<String>;
Command::Status { #[arg(long)] branch: Option<String> }
```

Line format (one per branch, branches sorted, current branch first):
`<branch>  <short commit 7>  <ok|tests failed|error>  <cov 1 dp>% (<+/-delta 1 dp> | new)  tests <p> passed, <f> failed, <e> errored, <s> skipped  <age>  [worker running]  measure <size>`. `previous` = the next-newest record on the branch. Age: `<n>s|m|h|d ago`. Size: recursive size of `measure(branch)`, `B/KB/MB/GB` one decimal. `Error` records add a second line `    error: <first line of stderr_tail>`. No records: `no records yet; run \`devctl record\``.

- [ ] **Step 1: Failing tests:** `format_summary` exact strings for ok-with-previous (`87.3% (+1.2)`), first record (`(new)`), error second line, coverage of 0 coverable lines shown as `n/a`; `run_status` lists two branches and shows `[worker running]` while a lock is held.
- [ ] **Step 2–4:** Run (fail), implement, run (pass).
- [ ] **Step 5:** Commit: `Show per-branch record status`.

---

### Task 14: `gate` — pre-push rules, `gate` command, `hook pre-push`

**Files:**
- Create: `src/gate/{mod.rs, coverage.rs}`, `src/hooks.rs` (dispatch only; install comes in Task 16); Modify: `src/cli.rs`, `src/lib.rs`

**Interfaces:**

```rust
pub struct RefUpdate { pub local_ref: String, pub local: Option<ObjectId>, pub remote_ref: String, pub remote: Option<ObjectId> } // None = all-zero id
pub fn parse_pre_push(stdin: &str) -> anyhow::Result<Vec<RefUpdate>>;
pub enum Rule { BranchName, NoDelete, NoRewrite, Linear, Record, Tests, NewCodeCoverage }   // Display: branch-name, no-delete, no-rewrite, linear, record, tests, new-code-coverage
pub enum Outcome { Pass, Skip, Block { next: String } }
pub struct RuleResult { pub rule: Rule, pub outcome: Outcome, pub detail: String, pub uncovered: BTreeMap<String, Vec<u32>> }
pub fn evaluate(ctx: &Ctx, remote: &str, update: &RefUpdate) -> anyhow::Result<Vec<RuleResult>>;
pub fn render(update: &RefUpdate, results: &[RuleResult]) -> String;
pub fn blocked(results: &[RuleResult]) -> bool;
// coverage.rs
pub struct NewCode { pub coverable: u32, pub covered: u32, pub uncovered: BTreeMap<String, Vec<u32>> }
pub fn new_code(added: &AddedLines, report: &Report) -> NewCode;
pub fn passes(n: &NewCode, threshold: u8) -> bool;     // coverable == 0 || covered*100 >= threshold*coverable (integers)
pub fn compress_lines(lines: &[u32]) -> String;        // [3,4,5,9] -> "3-5, 9"
Command::Gate { #[arg(long)] target: Option<String> }
#[command(hide = true)] Command::Hook { name: HookName }      // ValueEnum: post-commit, pre-commit, pre-push (this task wires pre-push)
```

Rules per ref, in order (spec 4.2), stopping at the first block of rules 1–2 and continuing otherwise:
1. Non-`refs/heads/` remote ref → every rule `Skip` ("not a branch").
2. `branch-name`: remote branch matches `pattern` or `is_protected`.
3. Protected: `no-delete` (local None → block); `no-rewrite` (remote Some and not ancestor of local → block, next "pull and rebase onto `<remote>/<branch>`, then push without --force"); `linear` (`merges_in_range(remote, local)` non-empty → block listing short ids, next "rebase to drop the merge commits"). New remote branch: `no-rewrite` passes; `linear` checks all of `local`'s history.
4. `record`: `record_for(local)`; if absent or its `raw` is gone: if `worker_running` or `peek` == local, print `waiting for record of <short> (<elapsed>s)` every 5 s to stderr, polling every 1 s, until a record appears or the lock frees; then if still absent `enqueue` + `run_worker` in the foreground. `Error` → block, detail = first stderr line, next "fix the recipe, then run `devctl record`".
5. `tests`: failed + errored > 0 → block, detail `"<f> failed, <e> errored"`.
6. `new-code-coverage`: base = remote (protected; None → empty tree) or `merge_base(local, ref_target("refs/remotes/<remote>/<target>") or ref_target("refs/heads/<target>"))`; missing target mapping → block with "add `<prefix>` to branches.targets in .devctl.toml". Parse raw LCOV, `new_code`, `passes`.

`render` format, one line per rule: `{outcome:<5} {rule}: {detail}` with outcome `pass`/`skip`/`BLOCK`; for coverage blocks, then one line per file `      <path>: <compress_lines>`; when blocked, a final line `push blocked for <branch>: <next>`. Each rule runs in a `gate.rule` span and emits `monotonic_counter.devctl.gate.result`.

`devctl gate`: synthesizes one `RefUpdate` (local = HEAD, remote_ref = `refs/heads/<target or current branch>`, remote = `refs/remotes/origin/<that>` if present) and exits non-zero when blocked. `devctl hook pre-push <remote> <url>`: reads stdin, evaluates every ref, prints, exits non-zero when any is blocked or on any error (fail closed, telemetry flush 2 s).

- [ ] **Step 1: Failing tests.** Unit (`coverage.rs`):

```rust
#[test] fn new_code_only_counts_da_lines() { /* added {a.rs:{1,2,3}}, DA 1 hit, DA 2 zero, no DA 3 -> coverable 2, covered 1, uncovered {a.rs:[2]} */ }
#[test] fn no_coverable_lines_passes() { assert!(passes(&NewCode{coverable:0,covered:0,..}, 95)); }
#[test] fn threshold_boundary() { /* 19/20 at 95 passes; 18/20 blocks; 100 needs all */ }
#[test] fn compresses_ranges() { assert_eq!(compress_lines(&[3,4,5,9]), "3-5, 9"); }
#[test] fn parses_pre_push_lines() { /* incl. CRLF line endings, "(delete) 0000… refs/heads/x <sha>" and a tag */ }
```

Integration (`TestRepo` with a local bare remote, `devctl hook pre-push origin <url>` fed via stdin, fixture justfile from Task 12), one test per case, each asserting exit status and the rule line:

```rust
#[test] fn bad_branch_name_blocks() {}          // wip/x -> "BLOCK branch-name"
#[test] fn force_push_to_dev_blocks() {}        // remote not ancestor -> "BLOCK no-rewrite"
#[test] fn merge_commit_to_dev_blocks() {}      // "BLOCK linear"
#[test] fn deleting_dev_blocks() {}
#[test] fn failed_test_blocks() {}              // junit with <failure>
#[test] fn low_new_code_coverage_blocks() {}    // lists "src/a.rs: 4-6"
#[test] fn coverage_measured_from_split_point() {}  // uncovered lines on dev before the split don't count
#[test] fn record_error_blocks() {}             // recipe exits 1 without junit
#[test] fn compliant_push_passes() {}
#[test] fn tag_push_is_skipped() {}
#[test] fn unknown_prefix_blocks_with_fix() {}  // pattern widened to allow wip/, no target
#[test] fn pruned_raw_is_rerecorded() {}        // prune with raw_reports_days=0 then gate -> passes after a fresh record
#[test] fn waits_then_runs_when_worker_dies() {} // stale job + no lock holder -> foreground record, passes
#[test] fn gate_command_matches_hook() {}
```

- [ ] **Step 2–4:** Run (fail), implement, run (pass).
- [ ] **Step 5:** Commit: `Gate pushes on branch, history, test and new-code coverage rules`.

---

### Task 15: Pre-commit size check

**Files:**
- Create: `src/commit_size.rs`; Modify: `src/hooks.rs` (wire `pre-commit`), `docs/superpowers/specs/2026-10-27-phase1-devctl-design.md`

**Interfaces:**

```rust
pub struct SizeCheck { pub files: u32, pub lines: u32, pub largest: Vec<FileChange> }   // largest: top 5 by added+removed
pub fn measure(changes: &[FileChange]) -> SizeCheck;
pub fn violations(check: &SizeCheck, limits: &CommitLimits) -> Vec<String>;   // empty = allowed
pub fn run_pre_commit(ctx: &Ctx) -> anyhow::Result<bool>;                     // prints; false = block
```

Rule: allowed when `files <= max_files` **and** `added + removed < max_lines`. Skipped (allowed, logged) while `is_merging()`. Block output:

```
BLOCK commit-size: 14 files (limit 10), 812 changed lines (must be under 500)
      src/big.rs: +400 -120
      …
commit blocked: split this into smaller commits (`git commit --no-verify` bypasses)
```

Only the violated limit(s) appear in the first line.

- [ ] **Step 1: Failing tests** (unit on `measure`/`violations`, integration through `devctl hook pre-commit` in a `TestRepo`):

```rust
#[test] fn ten_files_allowed_eleven_blocked() {}
#[test] fn gross_change_counts_add_plus_remove() { /* +300 -199 = 499 allowed; +300 -200 = 500 blocked */ }
#[test] fn binary_counts_as_file_with_zero_lines() {}
#[test] fn rename_counts_once() {}
#[test] fn first_commit_compared_to_empty_tree() {}
#[test] fn merge_in_progress_is_skipped() {}
#[test] fn limits_come_from_config() { /* [commit] max_files = 2 -> 3 files blocked */ }
```

- [ ] **Step 2–4:** Run (fail), implement, run (pass).
- [ ] **Step 5: Update the spec:** section 2 Hooks row → "`pre-commit`, `post-commit` and `pre-push`"; section 3 diagram gets `git commit ──► pre-commit hook ──► devctl hook pre-commit ──► size rule ──► allow / block`; section 6 adds `[commit]` with `max_files = 10  # files changed, at most` and `max_lines = 500  # added + removed, must be under`; section 7.1 `hooks install` writes three hooks; section 7.3 adds "`pre-commit` fails closed"; section 7.4 adds "`pre-commit` blocks a commit of more than 10 files or 500 or more changed lines"; section 11 still excludes commit-*message* rules.
- [ ] **Step 6:** Commit: `Block commits over 10 files or 500 changed lines`.

---

### Task 16: `hooks install` and `hook post-commit`

**Files:**
- Modify: `src/hooks.rs`, `src/cli.rs`

**Interfaces:**

```rust
pub const HOOKS: [&str; 3] = ["pre-commit", "post-commit", "pre-push"];
pub fn script(name: &str) -> String;     // "#!/bin/sh\nexec devctl hook <name> \"$@\"\n"
pub fn install(ctx: &Ctx) -> anyhow::Result<()>;
pub fn post_commit(ctx: &Ctx) -> anyhow::Result<()>;    // caller swallows the error after logging
Command::Hooks { #[command(subcommand)] action: HooksAction }   // HooksAction::Install
```

`install`: hooks dir = `<common dir>/hooks`. If `core.hooksPath` is set, or any of the three files exists and isn't byte-identical to `script(name)`, write nothing and fail with, per affected hook, `add this line to <path>: devctl hook <name> "$@"` (for hooksPath, `<path>` is under that dir). Otherwise write all three and `make_executable`. Re-running when already installed succeeds.

`post_commit`: branch = `current_branch()`; detached → `info!` and return; `enqueue(HEAD)`; `spawn_worker`. `run` dispatches `Hook { PostCommit }` so that any error is logged and the process still exits 0.

- [ ] **Step 1: Failing tests:**

```rust
#[test] fn installs_three_executable_hooks() {}
#[test] fn install_is_idempotent() {}
#[test] fn existing_foreign_hook_stops_with_line() { /* pre-push exists with other content -> error contains `devctl hook pre-push "$@"`; post-commit not written */ }
#[test] fn hooks_path_set_stops() {}
#[test] fn post_commit_queues_and_spawns() { /* FakePlatform: queue file has HEAD hex; one spawn with --branch dev */ }
#[test] fn post_commit_detached_is_noop_and_exits_zero() {}
#[test] fn post_commit_never_fails() { /* corrupt store + unwritable queue dir -> exit 0, message in log file */ }
```

- [ ] **Step 2–4:** Run (fail), implement, run (pass).
- [ ] **Step 5:** Commit: `Install hooks and queue records after each commit`.

---

### Task 17: End-to-end tests and docs

**Files:**
- Create: `tests/e2e.rs`, `tests/fixtures/e2e/{justfile, .devctl.toml, src/lib.txt, cases/*}`
- Modify: `.github/workflows/ci.yml`, `CLAUDE.md`

The sample repo's `justfile` (default `sh` shell, which Git for Windows provides): `coverage` copies `fixture/cov.info` → `coverage/` and `fixture/junit.xml` → `test-results/`, sleeps if `fixture/slow` exists, then `exit $(cat fixture/exit-code)`; `test` is a no-op. Each test commits the fixture files that produce its case. `devctl` is put on `PATH` for the hooks by prepending the directory of `assert_cmd::cargo::cargo_bin("devctl")`.

- [ ] **Step 1: Failing tests** (`tests/e2e.rs`; real `git commit`/`git push` to a local bare remote, hooks installed with `devctl hooks install`), covering spec 7.4 and the pre-commit addition:

```rust
#[test] fn commit_is_recorded_in_background() { /* git commit; poll `devctl status` ≤ 60 s for "<short>  ok" */ }
#[test] fn push_blocks_bad_branch_name() {}
#[test] fn push_blocks_force_push_to_dev() {}
#[test] fn push_blocks_merge_commit_to_dev() {}
#[test] fn push_blocks_failed_test() {}
#[test] fn push_blocks_low_new_code_coverage() {}
#[test] fn push_blocks_record_error() {}
#[test] fn compliant_push_passes() {}
#[test] fn amend_reuses_record() {}
#[test] fn second_commit_cancels_first() { /* slow fixture; two commits; final status shows the second; log contains "cancelled" */ }
#[test] fn oversized_commit_is_blocked() {}
```

- [ ] **Step 2:** `just test e2e`; expect failures only where behaviour is missing, then fix any integration gaps the earlier tasks left (each fix in the module that owns it, with its unit test).
- [ ] **Step 3:** CI needs no change (Task 1's six-platform matrix already runs `just ci`). Confirm the e2e tests run, not skip, on all six jobs.
- [ ] **Step 4:** Update `CLAUDE.md`: modules list from this plan's File Structure, hooks and `.git/devctl/` layout, how to run e2e tests (needs `git` and `just` on PATH).
- [ ] **Step 5:** `just ci` locally passes; push the branch and confirm all six CI jobs pass.
- [ ] **Step 6:** Commit: `Add end-to-end tests`.
