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
# On macOS, /usr/bin/git is an xcrun shim. Started from an x86_64 test binary
# under Rosetta it runs as x86_64 and fails to load the arm64-only libxcrun, so
# extra-target runs put the real (native) git first on PATH.
if [ -n "$extra" ] && command -v xcrun >/dev/null 2>&1 && real_git=$(xcrun --find git 2>/dev/null); then
    PATH="$(dirname "$real_git"):$PATH"
fi
for target in $extra; do
    rustup target add "$target"
    cargo test --locked --workspace --target "$target"
done
