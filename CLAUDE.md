# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

Prefer using the LSP tool over text-grep for code navigation, finding references, and validating type structures.

## Commands

Rust toolchain is pinned in `rust-toolchain.toml`. On this machine `cargo` lives in `~/.cargo/bin`, which is not on the default shell PATH — prefix commands with `PATH="$HOME/.cargo/bin:$PATH"`.

Dev tools (`just`, and the gitoxide CLI `gix`/`ein` built with its `max-pure` feature) are pinned in `mise.toml`, with checksums for all six platforms in `mise.lock`. Run `mise install` from the repo root. To add a tool, add it to `mise.toml` for all six platforms and re-run `mise lock --platform windows-x64,windows-arm64,macos-x64,macos-arm64,linux-x64,linux-arm64`. Rust stays on rustup. Per-OS setup is in `docs/dev-setup.md`.

Recipes live in the `justfile`:

```sh
just build
just run <args>        # e.g. just run version
just test              # unit + integration tests
just test <name>       # single test by name filter
just lint              # fmt --check + clippy -D warnings (clippy::pedantic is on)
just fmt
just check-pure        # fail on non-pure-Rust dependencies
just check-cfg         # fail on target_os/unix/windows cfg outside crates/devctl-platform
just ci                # lint + test + check-pure + check-cfg, same as CI
just test-local [env...]  # just ci in each env of test-envs.toml: podman container, UTM Windows VM, this Mac (default: all enabled)
```

## Architecture

`devctl` is a cargo workspace: the root `devctl` package (a clap-derive CLI) plus library crates under `crates/`, all sharing `[workspace.lints]` and `[workspace.dependencies]`:

- `crates/devctl-platform` — the only place `cfg(target_os = …)`, `cfg(unix)`, `cfg(windows)` (and `cfg!` forms) may appear; enforced by `just check-cfg`.
- `crates/devctl-lcov` — LCOV coverage report parsing.
- `crates/devctl-junit` — JUnit XML test report parsing.
- `crates/devctl-store` — the record store (`trait Store`, `RedbStore` on redb; postcard values, ruzstd-compressed raw reports, retention).

The `devctl` package (commands: `record`, `status`, `gate`, `hooks install`, `version`; hidden `hook <name>` and `worker --branch <b>`):

- `src/cli.rs` — the `Cli` parser and `Command` subcommand enum. Add a subcommand by adding a variant here.
- `src/lib.rs` — `run(&Cli)` dispatches each `Command` variant to its handler; repository-bound commands open a session (`Ctx` + telemetry) first. Errors propagate as `anyhow::Result`.
- `src/main.rs` — thin entry point: parses args, calls `run`, prints `error: …` and exits non-zero on failure.
- `src/config.rs` — loads and validates `.devctl.toml` (policy only; spec section 6 defaults).
- `src/telemetry.rs` — `tracing` subscriber: rotated local logs always, OTLP/HTTP export when configured.
- `src/git.rs` — `trait Repo` / `GixRepo` (reads via gix) and the `git` CLI helper.
- `src/ctx.rs` — `Ctx` (repo, platform, config, dirs), `DevctlDirs` (paths under `.git/devctl/`), `slug()`.
- `src/recipes.rs` — the `just` recipe contract: runs `coverage`/`test`, collects LCOV and JUnit, classifies the status.
- `src/record/` — `queue.rs` (per-branch job file), `measure.rs` (measuring worktrees), `worker.rs` (lock, drain loop, cancellation, tree reuse, spawning the detached worker), `mod.rs` (`devctl record`).
- `src/status.rs` — `devctl status` and the shared one-line record summary.
- `src/gate/` — the pre-push rules (`mod.rs`) and new-code coverage (`coverage.rs`); also `devctl gate`.
- `src/commit_size.rs` — the pre-commit size rule.
- `src/hooks.rs` — `devctl hooks install` and `devctl hook <name>` dispatch.
- `src/test_support.rs` — `#[cfg(test)]` `TestRepo` helper; `tests/support/mod.rs` is its copy for integration tests.
- `tests/*.rs` — integration tests that run the built binary through `assert_cmd`; `tests/e2e.rs` is the end-to-end suite (below).

### Hooks and `.git/devctl/`

`devctl hooks install` writes three `#!/bin/sh` scripts into `<common dir>/hooks` that `exec devctl hook <name>` (so `devctl` must be on the PATH git runs hooks with). It stops, printing the line to add, if a hook already exists with other content or `core.hooksPath` is set.

- `pre-commit` — blocks a commit of more than `commit.max_files` files or `commit.max_lines` or more changed lines; `git commit --no-verify` bypasses it.
- `post-commit` — queues the commit and starts a detached `devctl worker`; never fails the commit.
- `pre-push` — the gate (branch name, no delete/rewrite/merge commits on protected branches, record, tests, new-code coverage); waits for or runs the record; fails closed.

Everything else lives in `.git/devctl/` (the common git dir, shared by all worktrees; branch `/` becomes `-` in names): `store.redb` (records), `queue/<branch>` (newest commit to record), `locks/<branch>.lock` (worker lock) and `<branch>.queue.lock`, `measure/<branch>` (measuring worktree), `logs/<process>.<date>.log` (rotated logs; the first place to look when a background record misbehaves).

### End-to-end tests

`tests/e2e.rs` drives a sample repo (`tests/fixtures/e2e/`: a POSIX-`sh` `justfile` that copies the prepared LCOV/JUnit of `fixture/` into place, plus `cases/*`) through real `git commit` and `git push` to a local bare remote with the hooks installed, covering the M1 exit criteria. They run as part of `just test` (filter with `just test --test e2e`) and need `git` and `just` (≥ 1.59.0) on PATH; the tests prepend the built `devctl` binary's directory to PATH for the hooks. Background records are awaited by polling with deadlines, and each test waits for its workers to go idle before its temp dirs are removed. Log lines are flushed by a background thread when a process exits, so assert on logs by polling (`wait_log`), not right after `devctl status` shows a record.

Git access goes through the `gix` crate (gitoxide), not `git2`/libgit2. The `git` CLI is used only for `worktree add/remove/prune`, `checkout` inside a measuring worktree, and (M2) push/fetch/rebase; tests may use it freely to build fixture repos. It is on default features, which cover local repository operations only. gix's network transports all pull in C-backed code (curl/OpenSSL, or `ring` under rustls), which fails `just check-pure`, so remote operations need a deliberate exception to the purity rule.

## Purity constraints

The crate is kept "max-pure":

- `unsafe_code` is `forbid` in `[workspace.lints.rust]`, inherited by every crate.
- Dependencies must be pure Rust: no `-sys` crates, `cc`/`cmake`/`bindgen` build steps, or system libraries. Check with `just check-pure`, which covers every target platform (`--target all`), so Linux- and Windows-only dependencies are caught on macOS too. `linux-raw-sys`, `windows-sys`, `js-sys` and `web-sys` are allowlisted in the recipe because they are pure-Rust declarations that compile no C (`js-sys`/`web-sys` come in for wasm targets through the OTLP HTTP client). Allowlist another crate only after checking it has no C build step.

## Branching (git-flow)

Uses git-flow-next (`git flow`) with the classic preset, configured in `.git/config` (not committed):

- `master` — production; `dev` — integration branch and the GitHub default branch.
- `feature/*`, `bugfix/*` branch from `dev`; `release/*` branch from `dev` and finish into `master` (tagged); `hotfix/*` branch from `master` (tagged).
- Open PRs against `dev`.

## Remote

`origin` is `github:anand472-org/devctl.git`, where `github` is a host alias in `~/.ssh/config` (github.com, key `~/.ssh/id_anand472`). Git operations go over SSH; they don't need the token below.

## GitHub API access

`gh` is not logged in on this machine. For GitHub API calls (PRs, runs, repo settings), use the gitignored `.env` at the repo root, which holds `GITHUB_TOKEN` for the `anand472-dev` account. Never print it or commit it.

## CI

`.github/workflows/ci.yml` runs `just ci` on PRs and on pushes to `dev` and `master`, as a matrix over six runners (Windows, macOS and Linux, each x64 and arm64). `jdx/mise-action` installs and caches the dev tools from `mise.toml`/`mise.lock`.

The Claude Code GitHub Actions are both authenticated with the `ANTHROPIC_API_KEY` repo secret:

- `claude-code-review.yml` — runs the `code-review` plugin on every PR.
- `claude.yml` — responds to `@claude` mentions in issues and PR comments.
