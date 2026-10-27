default:
    @just --list

build:
    cargo build

run *args:
    cargo run -- {{args}}

test *filter:
    cargo test --locked {{filter}}

lint:
    cargo fmt --check
    cargo clippy --locked --all-targets -- -D warnings

fmt:
    cargo fmt

# Fails if any dependency is a -sys crate or needs a C build step.
check-pure:
    #!/usr/bin/env bash
    set -euo pipefail
    if cargo tree --locked -e normal,build,dev --prefix none | grep -E -- '-sys |^cc |^cmake|^bindgen'; then
        echo "error: non-pure-Rust dependencies found (listed above)" >&2
        exit 1
    fi
    echo "ok: all dependencies are pure Rust"

# Everything CI runs.
ci: lint test check-pure
