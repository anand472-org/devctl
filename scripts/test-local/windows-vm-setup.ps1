#Requires -RunAsAdministrator
# One-time, idempotent setup of the Windows 11 VM used by `just test-local`.
# Run in an elevated PowerShell inside the VM. Safe to re-run.
$ErrorActionPreference = 'Stop'

function Add-MachinePath([string]$Dir) {
    $current = [Environment]::GetEnvironmentVariable('Path', 'Machine')
    $parts = @($current -split ';' | Where-Object { $_ })
    if ($parts -notcontains $Dir) {
        [Environment]::SetEnvironmentVariable('Path', (($parts + $Dir) -join ';'), 'Machine')
    }
}

function Install-Winget([string]$Id, [string[]]$Extra = @()) {
    winget list --id $Id --exact --accept-source-agreements *> $null
    if ($LASTEXITCODE -eq 0) {
        Write-Host "$Id already installed"
        return
    }
    winget install --id $Id --exact --silent --accept-package-agreements --accept-source-agreements @Extra
    # -1978335189 = no applicable update / already installed
    if ($LASTEXITCODE -ne 0 -and $LASTEXITCODE -ne -1978335189) {
        throw "winget install $Id failed with exit code $LASTEXITCODE"
    }
}

# OpenSSH server
$ssh = Get-WindowsCapability -Online -Name 'OpenSSH.Server~~~~0.0.1.0'
if ($ssh.State -ne 'Installed') {
    Add-WindowsCapability -Online -Name 'OpenSSH.Server~~~~0.0.1.0' | Out-Null
}
Set-Service -Name sshd -StartupType Automatic
if ((Get-Service sshd).Status -ne 'Running') { Start-Service sshd }
if (-not (Get-NetFirewallRule -Name 'OpenSSH-Server-In-TCP' -ErrorAction SilentlyContinue)) {
    New-NetFirewallRule -Name 'OpenSSH-Server-In-TCP' -DisplayName 'OpenSSH Server (sshd)' `
        -Enabled True -Direction Inbound -Protocol TCP -Action Allow -LocalPort 22 | Out-Null
}

# Tools
Install-Winget 'Git.Git'
Install-Winget 'Microsoft.PowerShell'
Install-Winget 'jdx.mise'
Install-Winget 'Rustlang.Rustup'
Install-Winget 'Microsoft.VisualStudio.2022.BuildTools' @(
    '--override',
    '--quiet --wait --norestart --add Microsoft.VisualStudio.Workload.VCTools --includeRecommended --add Microsoft.VisualStudio.Component.VC.Tools.ARM64'
)

# SSH default shell: pwsh (installed above)
$pwsh = Join-Path $env:ProgramFiles 'PowerShell\7\pwsh.exe'
if (-not (Test-Path $pwsh)) { throw "pwsh not found at $pwsh" }
New-Item -Path 'HKLM:\SOFTWARE\OpenSSH' -Force | Out-Null
New-ItemProperty -Path 'HKLM:\SOFTWARE\OpenSSH' -Name DefaultShell -Value $pwsh `
    -PropertyType String -Force | Out-Null
Restart-Service sshd

# Machine Path: sh/bash for just, mise shims for non-interactive shells
Add-MachinePath 'C:\Program Files\Git\bin'
Add-MachinePath (Join-Path $env:LOCALAPPDATA 'mise\shims')

git config --system core.longpaths true

New-Item -ItemType Directory -Path 'C:\devctl-work' -Force | Out-Null
[Environment]::SetEnvironmentVariable('CARGO_TARGET_DIR', 'C:\devctl-target', 'Machine')

Write-Host 'Done. Reboot or sign out/in so the new Path and CARGO_TARGET_DIR take effect.'
Write-Host 'Then add your Mac public key to C:\ProgramData\ssh\administrators_authorized_keys'
Write-Host '(administrator user) or ~\.ssh\authorized_keys (standard user).'
