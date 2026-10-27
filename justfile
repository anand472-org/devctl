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

# Fails if any dependency, on any target platform, is a -sys crate or needs a C
# build step. linux-raw-sys and windows-sys are allowed: they are pure-Rust
# syscall/API declarations that compile no C.
check-pure:
    #!/usr/bin/env bash
    set -euo pipefail
    if cargo tree --locked --target all -e normal,build,dev --prefix none \
        | grep -E -- '-sys |^cc |^cmake|^bindgen' \
        | grep -vE '^(linux-raw-sys|windows-sys) '; then
        echo "error: non-pure-Rust dependencies found (listed above)" >&2
        exit 1
    fi
    echo "ok: all dependencies are pure Rust"

# Everything CI runs.
ci: lint test check-pure
