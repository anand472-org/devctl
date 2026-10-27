# Phase 1: devctl design

*Date: 27 Oct 2026. Sources: `Dev_Tool_Dashboard_Plan.md` and `Dev_Metrics_Plan.xlsx`. This is the umbrella spec for phase 1: it fixes the architecture and shared contracts, and designs milestone M1 in full. Milestones M2–M4 get their own short specs when they start.*

## 1. Goal

`devctl` is a cross-platform CLI (Windows, Linux, macOS) that developers use every day. It records code coverage and test results for each commit on the developer's machine, blocks pushes that break the team's rules, and runs the team's git workflow. It must be useful on its own, because phases 2–3 depend on it staying installed.

Workbook metrics delivered in phase 1:

| Metric | Priority | How |
|---|---|---|
| Local coverage trend and per-commit change | High | Background recording per commit; `devctl status` (M1) and the dashboard (M3) |
| Local coverage gate | High | `pre-push` blocks when new-code coverage is below the threshold (M1) |

## 2. Decisions

Decisions from the design discussion. Where they differ from the plan, the plan is superseded.

| Area | Decision | Differs from plan |
|---|---|---|
| Purity | All dependencies are pure Rust (`just check-pure`); no C compiled into the binary | Yes: plan allowed SQLite and zstd |
| Store | `redb` at `.git/devctl/store.redb`; raw reports compressed with `ruzstd` | Yes: plan had SQLite `coverage.db` + zstd |
| TLS | No rustls in phase 1; decided later | Deferred |
| Background work | No daemon. Hooks start short-lived detached workers | — |
| Recording | In the background after each commit; the newest commit on a branch wins | — |
| Where recording runs | A hidden measuring worktree per branch, `.git/devctl/measure/<branch>` | New |
| Branch worktrees | Every non-base branch has its own worktree at `.worktrees/<branch>` (gitignored), no exceptions | New |
| History | `dev` and `master` (configurable) are linear and never rewritten. Other branches may be rebased and force-pushed | Replaces "history survives rebases" |
| New code | Lines added since the branch split from its target branch; on `dev`/`master`, the commits being pushed | — |
| Gate | Blocks on: non-compliant branch name, rewrite of or merge commit into a protected branch, any failed test, new-code coverage below threshold, record error | Adds failed tests and branch names |
| Threshold | 95% new-code line coverage by default | Workbook target was 80% |
| Hooks | `pre-commit`, `post-commit` and `pre-push` | `pre-commit` added: size check |
| Telemetry | OpenTelemetry logs, traces and metrics built in from M1; export off by default | New |
| Dev tools | mise (`mise.toml` + `mise.lock`); Rust via rustup | New |

**Consequence for this repo:** PRs into `dev` and `master` must land without merge commits. Set GitHub's merge setting to "Rebase and merge" (or squash), or finish branches with `devctl feature finish`, which fast-forwards.

## 3. Architecture

No daemon. Every `devctl` process is short-lived:

```
git commit ──► pre-commit hook ──► devctl hook pre-commit ──► size rule ──► allow / block
git commit ──► post-commit hook ──► devctl hook post-commit ──► queue job, spawn detached worker ──► exit
                                                                       │
                                     devctl worker ◄───────────────────┘
                                       ├─ checkout commit in .git/devctl/measure/<branch>
                                       ├─ just coverage [, just test]
                                       ├─ parse LCOV + JUnit
                                       └─ write record ──► .git/devctl/store.redb

git push ──► pre-push hook ──► devctl hook pre-push ──► gate rules (waits for or runs the record) ──► allow / block
```

### 3.1 Workspace layout

| Crate | Purpose | Main interface |
|---|---|---|
| `devctl-platform` | Everything that differs by OS. The only crate allowed to use `cfg(target_os)` | `trait Platform`: `spawn_detached(cmd)`, `lock(path) -> Lock`, `kill_tree(pid)`, `open_browser(url)`, `just() -> Result<JustCommand>`, `data_dir()`, `normalize_path(p)`. Implementations `Windows`, `Linux`, `MacOs`; `FakePlatform` for tests |
| `devctl-lcov` | Parse and merge LCOV | `parse(&str) -> Result<Report>`, `Report::merge(&[Report]) -> Report`. A `Report` maps each file to line hits, functions and branches |
| `devctl-junit` | Parse JUnit XML | `parse(&str) -> Result<TestRun>`; suites and cases with name, status (passed, failed, error, skipped) and duration |
| `devctl-store` | Per-repo storage | `trait Store`: `put_record`, `record_for(commit)`, `record_for_tree(tree)`, `records_on(branch)`, `latest_ok(branch)`, `prune(policy)`. Implemented by `RedbStore` |
| `devctl` (binary) | CLI and everything else | Modules below |

Modules in the `devctl` binary:

| Module | Responsibility |
|---|---|
| `cli` | clap commands (section 7) |
| `git` | `trait Repo` built on gix: commits, trees, diffs, merge-base, ancestry. Falls back to the `git` CLI for push, fetch, rebase and `worktree add/remove`, which gix can't do in pure Rust |
| `config` | Loads and validates `.devctl.toml` |
| `recipes` | Runs the `just` contract in a given folder and collects outputs |
| `record` | Job queue, worker loop, measuring worktrees, cancellation |
| `gate` | Pre-push rules |
| `workflow` | Branch `start`/`finish` and `sync` (M2) |
| `hooks` | Installs hook scripts |
| `dashboard` | axum on `127.0.0.1` (M3) |
| `telemetry` | `tracing` subscriber and OpenTelemetry setup |
| `events` | Phase 2–3 event model, off by default (M4) |

### 3.2 Files on disk

| Path | Committed | Contents |
|---|---|---|
| `.devctl.toml` | yes | Policy settings |
| `justfile` | yes | Recipes (the recipe contract) |
| `.worktrees/<branch>` | no (gitignored) | Developer worktree per non-base branch |
| `.git/devctl/store.redb` | no | Records |
| `.git/devctl/queue/<branch>` | no | Newest commit waiting to be recorded |
| `.git/devctl/locks/<branch>.lock` | no | Worker lock per branch |
| `.git/devctl/measure/<branch>` | no | Measuring worktree per branch |
| `.git/devctl/logs/` | no | Rotated log files |

Branch names in paths replace `/` with `-` (`feature/x` → `feature-x`).

`.git/devctl/` means the repo's common git directory (`git rev-parse --git-common-dir`), so every worktree of a repo shares one store, queue and set of measuring worktrees. Inside a linked worktree `.git` is a file, not this directory.

## 4. Data flow

### 4.1 Commit (`post-commit`, background)

1. `devctl hook post-commit` writes the new commit ID to `.git/devctl/queue/<branch>`, overwriting any older job.
2. It starts `devctl worker --branch <branch>` detached, passing its trace context in `TRACEPARENT`, and returns within milliseconds. It never fails the commit.
3. The worker takes `.git/devctl/locks/<branch>.lock`. If another worker holds it, the new worker exits; the running one will pick up the job.
4. The worker reads the job. If a record exists for the same file tree (for example after a message-only amend), it copies that record for the new commit and skips the run.
5. Otherwise it creates the measuring worktree on first use (`git worktree add --detach`), checks out the commit there, and runs the recipes (section 5).
6. Every second during the run it re-reads the job file. If a newer commit is queued, it kills the recipe's process tree and starts again with the newer commit.
7. It parses outputs, computes the summary, writes the record, applies retention, and loops until the queue for its branch is empty.

### 4.2 Push (`pre-push`, foreground)

git passes one line per ref: local ref, local commit, remote ref, remote commit. For each ref:

1. **Branch name:** the target branch must match `branches.pattern` or be in `branches.protected`.
2. **Protected branches:**
   - Deleting a protected branch is blocked.
   - The remote commit must be an ancestor of the local commit (no rewrite).
   - The pushed range (remote commit..local commit) must contain no merge commits (linear).
3. **Record:** the gate needs a finished record for the local commit. If one is queued or running, it waits for it and shows progress. If none exists, it queues one and runs the worker in the foreground.
4. **Record error:** a record with status `error` blocks the push.
5. **Tests:** any failed or errored test blocks the push. Skipped tests don't.
6. **New-code coverage:**
   - Base: for a push to a protected branch, the remote commit. Otherwise, the merge-base of the local commit and the target branch given by `branches.targets` for the branch's prefix.
   - New lines: lines added in the diff from base to local commit (added lines only, including those in renamed files; binary files ignored).
   - Coverable new lines: new lines that have a `DA` entry in the record's LCOV.
   - Coverage = hit coverable new lines ÷ coverable new lines. With no coverable new lines, the rule passes.
   - Below `gate.new_code_coverage`, the push is blocked.
7. **Output:** one line per rule with its result. For a coverage failure, it lists each file with uncovered new lines and their line numbers.

A blocked push exits non-zero with the reason and the next step. `git push --no-verify` bypasses the gate; phase 2 detects that.

### 4.3 Workflow (M2)

- `devctl feature start x`: creates `feature/x` from `dev`, its worktree at `.worktrees/feature-x`, and its measuring worktree. The same applies to `bugfix`, `release`, `hotfix` and `support`, from their target branches.
- `devctl feature finish x`: requires a passing gate on the branch tip. It rebases the branch onto `dev` if needed, fast-forwards `dev`, pushes `dev`, then removes both worktrees and deletes the branch locally and on the remote. Pushing before deleting means an open PR shows as merged.
- `devctl sync`: fetches. On a protected branch it fast-forwards only. On other branches it rebases unpushed commits onto the upstream branch.

## 5. Recipe contract

Recipes always run from the root of the measuring worktree. Before each run `devctl` deletes `coverage/` and `test-results/`.

| Recipe | Required | Must produce |
|---|---|---|
| `coverage` | yes | LCOV files in `coverage/*.info` (several are merged) |
| `test` | yes | JUnit XML files in `test-results/*.xml` |
| `lint` | no | Not used in phase 1 |

Rules:

- If `coverage` already wrote JUnit XML, `just test` is skipped so tests run once.
- Exit codes: non-zero with JUnit output means tests failed (status `tests_failed`). Non-zero without JUnit output, or no LCOV output, means status `error`.
- A missing required recipe stops with: "missing recipe `coverage`; run `devctl init`". `devctl` never runs commands of its own instead.
- Minimum `just` version: 1.59.0, checked by `Platform::just()`.
- LCOV `SF` paths are made relative to the worktree root and use `/` separators. Files outside the worktree are dropped.
- Recipes get `DEVCTL=1` and `DEVCTL_COMMIT=<commit>` in their environment.

## 6. Configuration: `.devctl.toml`

Policy only; no commands. Defaults shown:

```toml
[branches]
protected = ["master", "dev"]
pattern   = "^(feature|bugfix|release|hotfix|support)/[a-z0-9][a-z0-9._-]*$"
targets   = { feature = "dev", bugfix = "dev", release = "master", hotfix = "master", support = "master" }

[gate]
new_code_coverage = 95        # percent, 0–100

[commit]
max_files = 10                # files changed, at most
max_lines = 500               # added + removed, must be under

[record]
timeout_minutes = 30

[retention]
records_per_branch = 200      # oldest records pruned
raw_reports_days   = 90       # summaries kept; raw LCOV/JUnit dropped after this

[telemetry]
endpoint      = ""            # OTLP/HTTP endpoint; empty = export off
include_paths = false
log_level     = "info"
```

A missing file means all defaults. An invalid file stops every command with the key and the reason.

## 7. M1 in full: record and gate

### 7.1 Commands

| Command | Behaviour |
|---|---|
| `devctl record [--commit <rev>]` | Records a commit (default `HEAD`) in the foreground in its branch's measuring worktree and prints the summary |
| `devctl status [--branch <b>]` | Last record per branch: commit, status, line coverage, change vs the previous record, test counts, age; whether a worker is running |
| `devctl gate [--target <branch>]` | Runs the pre-push rules for the current branch tip without pushing |
| `devctl hooks install` | Writes `pre-commit`, `post-commit` and `pre-push` scripts that call `devctl hook <name>`. If a hook already exists or `core.hooksPath` is set, it stops and prints the line to add to the existing hook |
| `devctl hook <name>` | Hidden; called by hooks |
| `devctl worker --branch <b>` | Hidden; the background worker |
| `devctl version` | Existing |

### 7.2 Store schema

Values are serialized with serde + `postcard`.

| Table | Key | Value |
|---|---|---|
| `meta` | `"schema"` | schema version (u32) |
| `records` | record ID (u64, increasing) | `Record` |
| `by_commit` | commit ID bytes | latest record ID |
| `by_tree` | tree ID bytes | latest `ok` or `tests_failed` record ID |
| `by_branch` | (branch, finished_at) | record ID |
| `raw` | record ID | ruzstd-compressed LCOV and JUnit |

`Record`: commit ID, tree ID, branch, started_at, finished_at, status (`ok`, `tests_failed`, `error` with the last 4 KB of stderr), total and per-file coverable/covered lines, test counts (passed, failed, errored, skipped) and duration, `devctl` and `just` versions.

- Commit and tree IDs are variable-length bytes, so SHA-1 and SHA-256 repos both work.
- Only line coverage (`DA`) is used for the gate and trend; function and branch data stay in the raw LCOV.
- A newer `devctl` migrates older schemas forward. An older `devctl` refuses a newer schema and says to upgrade.
- **Access:** redb allows one process to open the file at a time. Every process opens the store, runs one transaction and closes it, retrying with exponential backoff (up to 5 s) while it's busy.

### 7.3 Error handling

- `post-commit` never fails. Problems go to the logs and to an `error` record, shown by `devctl status`.
- `pre-commit` fails closed: an unreadable index or an invalid `.devctl.toml` blocks the commit with the reason. It is skipped while a merge is in progress.
- `pre-push` fails closed: a record error, missing `just`, a missing recipe or an unreadable store blocks the push with a specific message and fix.
- A recipe that exceeds `record.timeout_minutes` has its process tree killed (`Platform::kill_tree`) and is stored as `error`.
- A corrupt store is reported with its path. It's a local cache: deleting it and recording again is the fix.
- Every message names what failed, why, and the next command to run.

### 7.4 M1 exit criteria

On Windows, Linux and macOS, each on x64 and arm64, in CI:

- A commit is recorded in the background and shown by `devctl status`.
- `pre-commit` blocks a commit of more than 10 files or 500 or more changed lines (added + removed); `git commit --no-verify` bypasses it.
- `pre-push` blocks each of: a bad branch name, a force push to `dev`, a merge commit pushed to `dev`, a failed test, new-code coverage below 95%, and a record error. A compliant push passes.
- A message-only amend reuses the previous record. A second commit during a run cancels the first.
- `cfg(target_os)` appears only in `devctl-platform`.
- `just ci` passes, including `check-pure`.

## 8. Telemetry

- **Instrumentation:** all code uses `tracing`. Spans: hook, worker run, recipe run, parse, store transaction, gate rule, git command. Notable steps are events.
- **Local logs (always on):** `.git/devctl/logs/<process>.log`, rotated.
- **Export (off by default):** OTLP over HTTP with protobuf, enabled by `telemetry.endpoint` or the standard `OTEL_EXPORTER_OTLP_ENDPOINT`. `OTEL_SDK_DISABLED` and `OTEL_SERVICE_NAME` are respected.
  - Logs via `opentelemetry-appender-tracing`; traces via `tracing-opentelemetry`.
  - Metrics: `devctl.record.duration` (histogram, by status), `devctl.recipe.duration` (histogram, by recipe), `devctl.gate.result` (counter, by rule and outcome), `devctl.store.busy_retries` (counter), `devctl.worker.cancelled` (counter).
- **Transport:** plain HTTP only in phase 1, for a collector on localhost or a trusted network. HTTPS comes with the rustls decision. The OpenTelemetry crates must pass `just check-pure` with TLS off; any that don't are flagged before being added.
- **One trace per commit:** the hook passes `TRACEPARENT` to the worker.
- **Flush:** every process flushes on exit with a hard timeout (2 s for `pre-push`). An unreachable collector never blocks git; data is dropped with a logged warning.
- **Privacy:** exports include repo name, branch, commit ID, durations and outcomes. Never source code or file contents; file paths only with `include_paths = true`; never developer identity.
- Telemetry is for operating `devctl`. The phase 2–3 events are a separate model.

## 9. Testing

- **Parsers:** fixtures from each tool `init` targets (cargo-llvm-cov, vitest/jest, pytest-cov, JaCoCo, `go test` + converter, coverlet); merging several `.info` files; edge cases (empty file, missing `end_of_record`, Windows paths).
- **Store:** temporary folders; several processes at once; schema migration; retention.
- **Git rules and gate:** integration tests on throwaway repos for every gate rule, new-code coverage against the split point, no coverable lines, amend reuse, and cancellation.
- **End to end:** a sample repo whose `justfile` recipes write prepared LCOV and JUnit files (no language toolchains needed), driven through real `git commit` and `git push` to a local bare remote with the hooks installed.
- **Platform:** `FakePlatform` in unit tests; real implementations exercised in CI.
- **Telemetry:** in-memory exporter checks spans and metrics; one end-to-end test exports to a local OTLP receiver.
- **CI:** Ubuntu, macOS and Windows. `just ci` adds a check that fails if `cfg(target_os)` appears outside `devctl-platform`.

## 10. Milestones

| # | Milestone | Contents | Done when |
|---|---|---|---|
| M1 | Record and gate | Workspace split; `Platform` for 3 OSes; LCOV and JUnit parsers; redb store; recipe runner; queue, worker and measuring worktrees; `record`, `status`, `gate`, `hooks install`; `.devctl.toml`; telemetry; 3-OS CI | Section 7.4 |
| M2 | Workflow and `init` | `start`/`finish` for each branch type with `.worktrees/`, measuring worktrees created at `start`; `sync`; `init` templates for Rust, Node, Python, Java, Go, .NET with `windows-shell` set to PowerShell; `init` writes `.devctl.toml`, adds `.worktrees/` to `.gitignore`, installs hooks, and never overwrites files; `devctl test` running the `just` contract in configurable environments (`[test.envs]` in `.devctl.toml`: container, SSH host, local), replacing this repo's `just test-local` scripts | A feature goes from start to finish with no plain `git` commands, and its PR shows as merged |
| M3 | Dashboard | axum on `127.0.0.1`, random port, embedded assets; coverage trend per commit, file and line drill-down, test results; `devctl dashboard --export <dir>` | A branch's history and a file's covered lines are browsable with no network access |
| M4 | Release and events | cargo-dist installers that install `just`; signed binaries for x64 and arm64 on 3 OSes; phase 2–3 event model, off by default | The plan's phase 1 exit criteria are met |

## 11. Out of scope for phase 1

Commit-message rules; `lint` results; sending heartbeats or uploads (modelled only); rustls and HTTPS; gix network transport; a store repair tool; a `pre-rebase` hook.

## 12. Risks

- **Disk use:** each active branch has two worktrees plus build output. Mitigated by removing both at `finish`; `devctl status` shows measuring worktree sizes.
- **First record per branch is slow** (cold build in a new measuring worktree). Accepted; later records are incremental.
- **redb single-process access** could make processes wait. Mitigated by one short transaction per open and bounded retries; `devctl.store.busy_retries` shows whether it's a problem.
- **OpenTelemetry crates and purity:** if the OTLP exporter can't be built pure-Rust without TLS, export moves to a later milestone and only local logs ship in M1.
- **Merge-commit habit:** PRs merged with merge commits would be blocked by the linear-history rule. Mitigated by the GitHub merge setting and `devctl feature finish`.
