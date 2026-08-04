<#
.SYNOPSIS
    Install or uninstall toaster-talk-server as a native Windows service.

.DESCRIPTION
    Windows counterpart to `make install` / `make uninstall` (see the Makefile
    and POLICY.md's Deployment section for the Unix/systemd equivalent). Copies
    the release binaries into Program Files, writes a default config and master
    secret under ProgramData if none exist yet, and registers the server as a
    Windows service (Service Control Manager integration lives in
    toaster-talk-server/src/main.rs's windows_service_support module).

    Must be run from an elevated (Administrator) PowerShell session.

.PARAMETER Uninstall
    Stop and remove the service and installed binaries. Config, the master
    secret, and the room store under ProgramData are left in place (matching
    the Unix `make uninstall`, which never touches /etc or /var/lib either).

.EXAMPLE
    .\deploy\install.ps1
    Build first with: cargo build --release

.EXAMPLE
    .\deploy\install.ps1 -Uninstall
#>
[CmdletBinding()]
param(
    [switch]$Uninstall
)

$ErrorActionPreference = 'Stop'

$ServiceName = 'ToasterTalk'
$InstallDir  = Join-Path $env:ProgramFiles 'ToasterTalk'
$ConfigDir   = Join-Path $env:ProgramData 'ToasterTalk'
$RepoRoot    = Split-Path -Parent $PSScriptRoot
$ReleaseDir  = Join-Path $RepoRoot 'target\release'

function Assert-Admin {
    $principal = New-Object Security.Principal.WindowsPrincipal([Security.Principal.WindowsIdentity]::GetCurrent())
    if (-not $principal.IsInRole([Security.Principal.WindowsBuiltInRole]::Administrator)) {
        throw "This script must be run from an elevated (Administrator) PowerShell session."
    }
}

function Uninstall-ToasterTalk {
    Assert-Admin

    $existing = Get-Service -Name $ServiceName -ErrorAction SilentlyContinue
    if ($existing) {
        if ($existing.Status -ne 'Stopped') {
            Write-Host "Stopping service $ServiceName ..."
            Stop-Service -Name $ServiceName -Force
        }
        Write-Host "Removing service $ServiceName ..."
        # sc.exe rather than Remove-Service for compatibility with Windows PowerShell 5.1
        # (Remove-Service requires PowerShell 6+).
        & sc.exe delete $ServiceName | Out-Null
    } else {
        Write-Host "Service $ServiceName is not installed."
    }

    if (Test-Path $InstallDir) {
        Write-Host "Removing $InstallDir ..."
        Remove-Item -Recurse -Force $InstallDir
    }

    Write-Host "Uninstalled. Config, master secret, and room store under $ConfigDir were left in place."
}

function Install-ToasterTalk {
    Assert-Admin

    $serverExe = Join-Path $ReleaseDir 'toaster-talk-server.exe'
    $cliExe    = Join-Path $ReleaseDir 'toaster-talk-cli.exe'
    if (-not (Test-Path $serverExe) -or -not (Test-Path $cliExe)) {
        throw "Release binaries not found under $ReleaseDir — run 'cargo build --release' first."
    }

    New-Item -ItemType Directory -Force -Path $InstallDir | Out-Null
    New-Item -ItemType Directory -Force -Path $ConfigDir | Out-Null
    Copy-Item -Force $serverExe $InstallDir
    Copy-Item -Force $cliExe $InstallDir
    Write-Host "Installed binaries to $InstallDir"

    $secretPath = Join-Path $ConfigDir 'master.secret'
    if (-not (Test-Path $secretPath)) {
        # 48 random bytes, base64-encoded — the Windows equivalent of the
        # /dev/urandom recipe in deploy/config.example.toml.
        $bytes = [byte[]]::new(48)
        [Security.Cryptography.RandomNumberGenerator]::Fill($bytes)
        [Convert]::ToBase64String($bytes) | Set-Content -NoNewline -Path $secretPath -Encoding ascii
        Write-Host "Generated master secret at $secretPath"
    } else {
        Write-Host "Master secret already exists at $secretPath — left unchanged."
    }

    $configPath = Join-Path $ConfigDir 'config.toml'
    if (-not (Test-Path $configPath)) {
        $config = @"
# ToasterTalk MCP Server — Windows configuration
# Installed at $ConfigDir\config.toml by deploy\install.ps1.
# See deploy\config.example.toml for the full field reference (same schema on
# every platform; only the path-shaped defaults below differ — see SCHEMA.md).

bind        = "127.0.0.1"
port        = 6669
log_level   = "info"
mgmt_socket = '\\.\pipe\toaster-talk'

master_secret_file = "master.secret"

[rooms]
store_path       = '$ConfigDir\rooms.yaml'
max_rooms        = 50
default_ttl_days = 30
max_ttl_days     = 365

[messages]
default_ttl_secs             = 660
max_ttl_secs                 = 3600
sweep_interval_secs          = 60
idle_threshold_secs          = 300
member_idle_ttl_secs         = 1800
seat_idle_ttl_secs           = 86400
default_receive_timeout_secs = 30
max_await_reply_secs         = 3600
max_receive_timeout_secs     = 3600
max_inbox_messages           = 100
"@
        Set-Content -Path $configPath -Value $config -Encoding ascii
        Write-Host "Default config written to $configPath"
    } else {
        Write-Host "Config already exists at $configPath — left unchanged."
    }

    $binPath = "`"$InstallDir\toaster-talk-server.exe`" --config `"$configPath`""
    $existing = Get-Service -Name $ServiceName -ErrorAction SilentlyContinue
    if ($existing) {
        Write-Host "Service $ServiceName already registered — updating binary path."
        & sc.exe config $ServiceName binPath= $binPath | Out-Null
    } else {
        New-Service -Name $ServiceName `
            -BinaryPathName $binPath `
            -DisplayName 'ToasterTalk MCP Server' `
            -Description 'Inter-agent message passing MCP server (see README.md).' `
            -StartupType Automatic | Out-Null
        Write-Host "Registered service $ServiceName."
    }

    Write-Host ""
    Write-Host "Installed. Start with: Start-Service $ServiceName"
    Write-Host "Check status with:     Get-Service $ServiceName"
    Write-Host "Reload config without a restart: & `"$InstallDir\toaster-talk-cli.exe`" reload"
}

if ($Uninstall) {
    Uninstall-ToasterTalk
} else {
    Install-ToasterTalk
}
