# Dev setup

Dev tools (`just`, the gitoxide CLI) come from [mise](https://mise.jdx.dev), pinned in `mise.toml` and `mise.lock`. Rust comes from rustup, using the toolchain pinned in `rust-toolchain.toml`. Supported platforms: Windows, macOS and Linux, each on x64 and arm64.

## macOS

1. Install mise: `brew install mise` (or `curl https://mise.run | sh`).
2. Install rustup from <https://rustup.rs>.
3. Install the Xcode command line tools for `git`: `xcode-select --install`.
4. Activate mise in your shell (see Shells below), then from the repo root run `mise install` and `just ci`.

## Linux

1. Install mise: `curl https://mise.run | sh`.
2. Install rustup from <https://rustup.rs>.
3. Install `git` from your distro.
4. Activate mise in your shell (see Shells below), then from the repo root run `mise install` and `just ci`.

## Windows (x64 and arm64)

1. Install the tools: `winget install Git.Git Microsoft.PowerShell jdx.mise Rustlang.Rustup`.
2. Install Visual Studio Build Tools with the C++ workload (the MSVC linker Rust needs, `Microsoft.VisualStudio.Workload.VCTools`).
3. Add `C:\Program Files\Git\bin` and `C:\Program Files\Git\usr\bin` to the user `Path`, so `just` finds `sh` and `bash` from pwsh, and recipes (including `#!/usr/bin/env bash` shebang recipes) find coreutils such as `env`, `mkdir` and `cp`.
4. Run `git config --global core.longpaths true`.
5. Add `%LOCALAPPDATA%\mise\shims` to the user `Path`, so IDEs find the tools.
6. Activate mise in your shell (see Shells below), then from the repo root run `mise install` and `just ci`.

## Shells

- bash: `eval "$(mise activate bash)"`; zsh: `eval "$(mise activate zsh)"`.
- fish: `mise activate fish | source`.
- pwsh, in `$PROFILE`: `mise activate pwsh | Out-String | Invoke-Expression`.
- Git Bash, in `~/.bashrc`: `export PATH="$(cygpath "$LOCALAPPDATA")/mise/shims:$PATH"`. Don't use `mise activate bash` here: under Git Bash it writes Windows-style paths into `PATH`.

## Local test environments

`just test-local` runs `just ci` on the working tree (including uncommitted and untracked, non-ignored files) in each enabled environment of `test-envs.toml` (restricted TOML, documented in `scripts/test-local/config.sh`; with no file there is one `local` environment). This repo defines three: a podman Ubuntu 24.04 arm64 container, a UTM Windows 11 ARM VM over SSH, and this Mac (arm64, plus the tests again as `x86_64-apple-darwin` under Rosetta). It prints `PASS` or `FAIL` per environment and exits non-zero if any failed. Run one at a time with `just test-local linux`, `just test-local windows` or `just test-local macos`. Environment kinds are `container` (podman), `ssh` (a host alias, optionally with a UTM VM to start) and `local`.

The Linux container needs podman (`brew install podman`, `podman machine init`); the script starts the machine if it is stopped. Named volumes keep the Rust toolchain, mise tools and `target/` between runs, so later runs are incremental.

### Local Windows VM

One-time setup, by hand:

1. `brew install --cask utm`.
2. In UTM, create a VM named `devctl-win` from the Windows 11 ARM64 ISO (Microsoft's download page; activation isn't needed): 4 CPUs, 8 GB RAM, 80 GB disk, shared network. Create a local user (any name).
3. In the VM, run `scripts/test-local/windows-vm-setup.ps1` as Administrator, then reboot. It is idempotent. It checks each tool by the file or command it needs, not by `winget list`: sshd needs the machine-wide PowerShell at `C:\Program Files\PowerShell\7\pwsh.exe`, so a Microsoft Store copy (`where pwsh` pointing into `WindowsApps`) does not count, and the script installs the MSI with `--scope machine --force` next to it.
4. On the Mac, add an `~/.ssh/config` entry `Host devctl-win` with the VM's IP, your Windows user name (quoted if it contains spaces, e.g. `User "Jane Doe"`) and an `IdentityFile`, and append the Mac's public key (one line) to `C:\ProgramData\ssh\administrators_authorized_keys` in the VM. The first Windows user is an administrator, so sshd reads that file, not `~\.ssh\authorized_keys`; the setup script creates it and fixes its permissions.

Done when `ssh devctl-win "just --version"` prints `just 1.59.0`. If `utmctl` or the `devctl-win` host is missing, `just test-local windows` fails with "SSH host devctl-win not reachable".
