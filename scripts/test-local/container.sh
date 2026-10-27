#!/bin/sh
# container.sh <env>: run `just ci` on the working tree in a podman container.
set -eu
cd "$(dirname "$0")/../.."
. scripts/test-local/config.sh
name=$1
env_require "$name" containerfile image platform
containerfile=$(env_get "$name" containerfile)
image=$(env_get "$name" image)
platform=$(env_get "$name" platform)

# Volume prefix: the main repo's directory name (stable across git worktrees).
common=$(git rev-parse --path-format=absolute --git-common-dir 2>/dev/null || true)
if [ -n "$common" ]; then
    project=$(basename "$(dirname "$common")")
else
    project=$(basename "$PWD")
fi
project=$(printf '%s' "$project" | tr 'A-Z' 'a-z' | tr -c 'a-z0-9_.\n-' '-')
vol="$project-$name"

PATH="/opt/podman/bin:$PATH"
if ! podman machine list | grep -q 'Currently running'; then
    podman machine start
fi

podman build -t "$image" -f "$containerfile" "$(dirname "$containerfile")"

tree_tar | podman run --rm -i --platform "$platform" \
    -v "$vol-rustup:/home/dev/.rustup" \
    -v "$vol-cargo:/home/dev/.cargo" \
    -v "$vol-mise:/home/dev/.local/share/mise" \
    -v "$vol-target:/home/dev/target" \
    "$image" \
    sh -euc "rm -rf /home/dev/work/* && tar -xf - -C /home/dev/work && cd /home/dev/work && $TEST_CMD"
