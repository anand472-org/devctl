# CLAUDE.md

This file provides guidance to Claude Code (claude.ai/code) when working with code in this repository.

Prefer using the LSP tool over text-grep for code navigation, finding references, and validating type structures.

## Status

The repository has no application code yet — only git/CI scaffolding. Add build, lint, and test commands and an architecture overview here once code exists.

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

`.github/workflows/` contains the Claude Code GitHub Actions, both authenticated with the `ANTHROPIC_API_KEY` repo secret:

- `claude-code-review.yml` — runs the `code-review` plugin on every PR.
- `claude.yml` — responds to `@claude` mentions in issues and PR comments.
