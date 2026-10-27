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
3. Add `C:\Program Files\Git\bin` to the user `Path`, so `just` finds `sh` and `bash` from pwsh.
4. Run `git config --global core.longpaths true`.
5. Add `%LOCALAPPDATA%\mise\shims` to the user `Path`, so IDEs find the tools.
6. Activate mise in your shell (see Shells below), then from the repo root run `mise install` and `just ci`.

## Shells

- bash: `eval "$(mise activate bash)"`; zsh: `eval "$(mise activate zsh)"`.
- fish: `mise activate fish | source`.
- pwsh, in `$PROFILE`: `mise activate pwsh | Out-String | Invoke-Expression`.
- Git Bash, in `~/.bashrc`: `export PATH="$(cygpath "$LOCALAPPDATA")/mise/shims:$PATH"`. Don't use `mise activate bash` here: under Git Bash it writes Windows-style paths into `PATH`.
