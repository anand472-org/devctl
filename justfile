default:
    @just --list

build:
    cargo build

run *args:
    cargo run -- {{args}}

test *filter:
    cargo test --locked --workspace {{filter}}

lint:
    cargo fmt --check
    cargo clippy --locked --workspace --all-targets -- -D warnings

fmt:
    cargo fmt

# Fails if any dependency, on any target platform, is a -sys crate or needs a C
# build step. linux-raw-sys and windows-sys are allowed: they are pure-Rust
# syscall/API declarations that compile no C.
check-pure:
    #!/usr/bin/env bash
    set -euo pipefail
    if cargo tree --locked --workspace --target all -e normal,build,dev --prefix none \
        | grep -E -- '-sys |^cc |^cmake|^bindgen' \
        | grep -vE '^(linux-raw-sys|windows-sys) '; then
        echo "error: non-pure-Rust dependencies found (listed above)" >&2
        exit 1
    fi
    echo "ok: all dependencies are pure Rust"

# Fails if platform-specific cfg (target_os, unix, windows) appears outside
# crates/devctl-platform/.
check-cfg:
    #!/usr/bin/env bash
    set -euo pipefail
    if grep -rnE 'cfg!?\(.*(target_os|unix|windows)' --include='*.rs' \
        --exclude-dir=devctl-platform --exclude-dir=target \
        --exclude-dir=.superpowers --exclude-dir=.worktrees --exclude-dir=.git .; then
        echo "error: platform cfg found outside crates/devctl-platform/ (listed above)" >&2
        exit 1
    fi
    echo "ok: no platform cfg outside crates/devctl-platform"

# Everything CI runs.
ci: lint test check-pure check-cfg

# Run `just ci` in each environment of test-envs.toml (default: every enabled one):
# podman container, SSH host (the UTM Windows VM), or this machine. Not part of `ci`:
# CI runners have no podman or UTM.
test-local *envs:
    #!/bin/sh
    set -u
    . scripts/test-local/config.sh
    all=$(env_all_names) || exit 1
    envs="{{envs}}"
    [ -n "$envs" ] || envs=$(env_names) || exit 1
    if [ -z "$envs" ]; then
        echo "no test environments enabled in test-envs.toml" >&2
        exit 1
    fi
    for e in $envs; do
        if ! printf '%s\n' "$all" | grep -qxF -- "$e"; then
            echo "error: unknown environment '$e'; defined: $(echo $all)" >&2
            exit 1
        fi
    done
    failed=0
    for e in $envs; do
        kind=$(env_get "$e" kind) || { echo "test-envs.toml: envs.$e.kind is required" >&2; echo "FAIL $e"; failed=1; continue; }
        case "$kind" in
            container|ssh|local) ;;
            *) echo "test-envs.toml: envs.$e.kind must be container, ssh or local" >&2; echo "FAIL $e"; failed=1; continue ;;
        esac
        if sh scripts/test-local/"$kind".sh "$e"; then
            echo "PASS $e"
        else
            echo "FAIL $e"
            failed=1
        fi
    done
    exit "$failed"
