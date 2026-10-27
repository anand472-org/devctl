# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

Prefer using the LSP tool over text-grep for code navigation, finding references, and validating type structures.

## Commands

Rust toolchain is pinned in `rust-toolchain.toml`. On this machine `cargo` lives in `~/.cargo/bin`, which is not on the default shell PATH — prefix commands with `PATH="$HOME/.cargo/bin:$PATH"`.

```sh
cargo build
cargo run -- <args>                          # e.g. cargo run -- version
cargo test                                   # unit + integration tests
cargo test <name>                            # single test by name filter
cargo clippy --all-targets -- -D warnings    # lint (clippy::pedantic is on)
cargo fmt --check
```

## Architecture

`devctl` is a clap-derive CLI:

- `src/cli.rs` — the `Cli` parser and `Command` subcommand enum. Add a subcommand by adding a variant here.
- `src/lib.rs` — `run(&Cli)` dispatches each `Command` variant to its handler; errors propagate as `anyhow::Result`.
- `src/main.rs` — thin entry point: parses args, calls `run`, prints `error: …` and exits non-zero on failure.
- `tests/cli.rs` — integration tests that run the built binary through `assert_cmd`.

## Purity constraints

The crate is kept "max-pure":

- `unsafe_code` is `forbid` in `[lints.rust]`.
- Dependencies must be pure Rust: no `-sys` crates, `cc`/`cmake`/`bindgen` build steps, or system libraries (for example, use `rustls`, not OpenSSL). Check with `cargo tree -e normal,build,dev --prefix none | grep -E -- '-sys |^cc |^cmake|^bindgen'`, which should print nothing.

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

`.github/workflows/ci.yml` runs fmt, clippy, tests and the pure-Rust dependency check on PRs and on pushes to `dev` and `master`.

The Claude Code GitHub Actions are both authenticated with the `ANTHROPIC_API_KEY` repo secret:

- `claude-code-review.yml` — runs the `code-review` plugin on every PR.
- `claude.yml` — responds to `@claude` mentions in issues and PR comments.
