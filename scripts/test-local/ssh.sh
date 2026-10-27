#!/bin/sh
# ssh.sh <env>: run `just ci` on the working tree on an SSH host (Windows; the remote
# shell is PowerShell 7 or Windows PowerShell 5.1, so remote commands avoid PS7-only syntax),
# starting its UTM VM first when `vm` is set.
set -eu
cd "$(dirname "$0")/../.."
. scripts/test-local/config.sh
name=$1
env_require "$name" host workdir
host=$(env_get "$name" host)
workdir=$(env_get "$name" workdir)
vm=$(env_get "$name" vm "")

not_reachable() {
    echo "SSH host $host not reachable; see docs/dev-setup.md#local-windows-vm" >&2
    exit 1
}
probe() { ssh -o BatchMode=yes -o ConnectTimeout=5 "$host" exit >/dev/null 2>&1; }

if ! probe; then
    # An alias missing from ~/.ssh/config resolves to itself.
    if [ "$(ssh -G "$host" 2>/dev/null | awk '/^hostname /{print $2}')" = "$host" ]; then
        not_reachable
    fi
    [ -n "$vm" ] || not_reachable
    if command -v utmctl >/dev/null 2>&1; then
        utmctl=utmctl
    elif [ -x /Applications/UTM.app/Contents/MacOS/utmctl ]; then
        utmctl=/Applications/UTM.app/Contents/MacOS/utmctl
    else
        not_reachable
    fi
    "$utmctl" status "$vm" >/dev/null 2>&1 || not_reachable
    if [ "$("$utmctl" status "$vm")" != started ]; then
        "$utmctl" start "$vm"
    fi
    waited=0
    until probe; do
        waited=$((waited + 5))
        [ "$waited" -lt 120 ] || not_reachable
        sleep 5
    done
fi

make_tree_tar
ssh "$host" "\$ErrorActionPreference='Stop'; if (Test-Path '$workdir') { Get-ChildItem -Force '$workdir' | Remove-Item -Recurse -Force } else { New-Item -ItemType Directory '$workdir' | Out-Null }"
ssh "$host" "tar -xf - -C '$workdir'" < "$TREE_TAR"
# Native commands only: no Stop preference (5.1 turns native stderr into errors); exit codes via if.
ssh "$host" "Set-Location '$workdir'; if (-not \$?) { exit 1 }; rustup toolchain install; if (\$LASTEXITCODE -ne 0) { exit \$LASTEXITCODE }; mise trust; if (\$LASTEXITCODE -ne 0) { exit \$LASTEXITCODE }; mise install; if (\$LASTEXITCODE -ne 0) { exit \$LASTEXITCODE }; just ci; exit \$LASTEXITCODE"
