#Requires -RunAsAdministrator
# One-time, idempotent setup of the Windows 11 VM used by `just test-local`.
# Run in an elevated PowerShell inside the VM. Safe to re-run.
#
# Order matters: SSH access is set up first, so a later package failure still leaves
# the VM reachable. All paths are built with Join-Path/variables, so profile paths
# with spaces (C:\Users\Some Name\...) are fine.
$ErrorActionPreference = 'Stop'
$script:RebootNeeded = $false

function Add-MachinePath([string]$Dir) {
    $current = [Environment]::GetEnvironmentVariable('Path', 'Machine')
    $parts = @($current -split ';' | Where-Object { $_ })
    if ($parts -notcontains $Dir) {
        [Environment]::SetEnvironmentVariable('Path', (($parts + $Dir) -join ';'), 'Machine')
    }
}

# winget does not refresh this session's PATH; do it so freshly installed tools are found.
function Update-SessionPath {
    $env:Path = ([Environment]::GetEnvironmentVariable('Path', 'Machine'),
        [Environment]::GetEnvironmentVariable('Path', 'User')) -join ';'
}

# Always use the winget source: a broken msstore source must not make the id ambiguous.
$WingetCommon = @('--source', 'winget', '--accept-source-agreements', '--disable-interactivity')

function Install-Winget([string]$Id, [string[]]$Extra = @()) {
    winget list --id $Id --exact @WingetCommon *> $null
    if ($LASTEXITCODE -eq 0) {
        Write-Host "$Id already installed"
        return
    }
    winget install --id $Id --exact --silent --accept-package-agreements @WingetCommon @Extra
    # -1978335189 = no applicable update / already installed; 3010 = success, reboot required
    if ($LASTEXITCODE -eq 3010) { $script:RebootNeeded = $true }
    elseif ($LASTEXITCODE -ne 0 -and $LASTEXITCODE -ne -1978335189) {
        throw "winget install $Id failed with exit code $LASTEXITCODE"
    }
}

# --- SSH access (before any long package install) ---------------------------------

# OpenSSH server
$ssh = Get-WindowsCapability -Online -Name 'OpenSSH.Server~~~~0.0.1.0'
if ($ssh.State -ne 'Installed') {
    Add-WindowsCapability -Online -Name 'OpenSSH.Server~~~~0.0.1.0' | Out-Null
}
Set-Service -Name sshd -StartupType Automatic
if ((Get-Service sshd).Status -ne 'Running') { Start-Service sshd }

# Firewall: all profiles, because UTM's shared network is often classified Public.
$rule = Get-NetFirewallRule -Name 'OpenSSH-Server-In-TCP' -ErrorAction SilentlyContinue
if ($rule) {
    Set-NetFirewallRule -Name 'OpenSSH-Server-In-TCP' -Enabled True -Profile Any
} else {
    New-NetFirewallRule -Name 'OpenSSH-Server-In-TCP' -DisplayName 'OpenSSH Server (sshd)' `
        -Enabled True -Direction Inbound -Protocol TCP -Action Allow -LocalPort 22 `
        -Profile Any | Out-Null
}

# Administrators' keys live in administrators_authorized_keys; sshd ignores the file
# unless only SYSTEM and Administrators can access it.
$authKeys = Join-Path $env:ProgramData 'ssh\administrators_authorized_keys'
if (-not (Test-Path $authKeys)) { New-Item -ItemType File -Path $authKeys -Force | Out-Null }
icacls.exe $authKeys /inheritance:r /grant 'Administrators:F' /grant 'SYSTEM:F' | Out-Null
if ($LASTEXITCODE -ne 0) { throw "icacls failed with exit code $LASTEXITCODE" }

# PowerShell 7, then make it the SSH default shell.
Install-Winget 'Microsoft.PowerShell'
$pwsh = Join-Path $env:ProgramFiles 'PowerShell\7\pwsh.exe'
if (-not (Test-Path $pwsh)) { throw "pwsh not found at $pwsh" }
New-Item -Path 'HKLM:\SOFTWARE\OpenSSH' -Force | Out-Null
New-ItemProperty -Path 'HKLM:\SOFTWARE\OpenSSH' -Name DefaultShell -Value $pwsh `
    -PropertyType String -Force | Out-Null
Restart-Service sshd

# --- Machine configuration --------------------------------------------------------

# Machine Path: sh/bash for just, coreutils (env, mkdir, cp, ...) for recipes and
# their bash shebangs, mise shims for non-interactive shells
Add-MachinePath 'C:\Program Files\Git\bin'
Add-MachinePath 'C:\Program Files\Git\usr\bin'
Add-MachinePath (Join-Path $env:LOCALAPPDATA 'mise\shims')

New-Item -ItemType Directory -Path 'C:\devctl-work' -Force | Out-Null
[Environment]::SetEnvironmentVariable('CARGO_TARGET_DIR', 'C:\devctl-target', 'Machine')

# --- Tools ------------------------------------------------------------------------

Install-Winget 'Git.Git'
Install-Winget 'jdx.mise'
Install-Winget 'Rustlang.Rustup'
Update-SessionPath

& (Join-Path $env:ProgramFiles 'Git\cmd\git.exe') config --system core.longpaths true
if ($LASTEXITCODE -ne 0) { throw "git config --system failed with exit code $LASTEXITCODE" }

# VS Build Tools: install, or add the ARM64 C++ tools to an existing install.
$vsComponents = '--add Microsoft.VisualStudio.Workload.VCTools --includeRecommended --add Microsoft.VisualStudio.Component.VC.Tools.ARM64'
$vsInstallerDir = Join-Path ${env:ProgramFiles(x86)} 'Microsoft Visual Studio\Installer'
$vswhere = Join-Path $vsInstallerDir 'vswhere.exe'
$vsHome = $null
$vsHasArm64 = $false
if (Test-Path $vswhere) {
    $vsHome = & $vswhere -products '*' -latest -property installationPath
    $vsHasArm64 = [bool](& $vswhere -products '*' -latest `
        -requires Microsoft.VisualStudio.Component.VC.Tools.ARM64 -property installationPath)
}
if ($vsHasArm64) {
    Write-Host 'VS Build Tools with ARM64 C++ tools already installed'
} elseif ($vsHome) {
    $p = Start-Process -FilePath (Join-Path $vsInstallerDir 'setup.exe') -Wait -PassThru `
        -ArgumentList "modify --installPath `"$vsHome`" --quiet --norestart $vsComponents"
    if ($p.ExitCode -eq 3010) { $script:RebootNeeded = $true }
    elseif ($p.ExitCode -ne 0) { throw "VS Build Tools modify failed with exit code $($p.ExitCode)" }
} else {
    Install-Winget 'Microsoft.VisualStudio.2022.BuildTools' @(
        '--override', "--quiet --wait --norestart $vsComponents")
}

Write-Host 'Done. Add the Mac public key (one line) to:'
Write-Host "  $authKeys"
Write-Host 'then reboot the VM so the new Path and CARGO_TARGET_DIR take effect.'
if ($script:RebootNeeded) {
    Write-Host 'A reboot is REQUIRED (VS Build Tools): reboot the VM before the first test run.'
}
