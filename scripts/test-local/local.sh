#!/bin/sh
# local.sh <env>: run `just ci` in place, then the tests again for each extra target.
set -eu
cd "$(dirname "$0")/../.."
. scripts/test-local/config.sh
name=$1
env_require "$name" kind
extra=$(env_get "$name" extra_targets "")

PATH="$HOME/.cargo/bin:$HOME/.local/bin:$HOME/.local/share/mise/shims:$PATH"
rustup toolchain install
mise trust
mise install
just ci
for target in $extra; do
    rustup target add "$target"
    cargo test --locked --workspace --target "$target"
done
