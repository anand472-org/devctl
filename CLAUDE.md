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
just ci                # lint + test + check-pure, same as CI
```

## Architecture

`devctl` is a clap-derive CLI:

- `src/cli.rs` — the `Cli` parser and `Command` subcommand enum. Add a subcommand by adding a variant here.
- `src/lib.rs` — `run(&Cli)` dispatches each `Command` variant to its handler; errors propagate as `anyhow::Result`.
- `src/main.rs` — thin entry point: parses args, calls `run`, prints `error: …` and exits non-zero on failure.
- `tests/cli.rs` — integration tests that run the built binary through `assert_cmd`.

Git access goes through the `gix` crate (gitoxide), not `git2`/libgit2 or shelling out to `git`. It is on default features, which cover local repository operations only. gix's network transports all pull in C-backed code (curl/OpenSSL, or `ring` under rustls), which fails `just check-pure`, so remote operations need a deliberate exception to the purity rule.

## Purity constraints

The crate is kept "max-pure":

- `unsafe_code` is `forbid` in `[lints.rust]`.
- Dependencies must be pure Rust: no `-sys` crates, `cc`/`cmake`/`bindgen` build steps, or system libraries. Check with `just check-pure`, which covers every target platform (`--target all`), so Linux- and Windows-only dependencies are caught on macOS too. `linux-raw-sys` and `windows-sys` are allowlisted in the recipe because they are pure-Rust declarations that compile no C. Allowlist another crate only after checking it has no C build step.

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
