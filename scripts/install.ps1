#Requires -Version 5.1
<#
.SYNOPSIS
    Borg Installer for Windows
.DESCRIPTION
    Downloads and installs the borg AI personal assistant binary.
    Usage: irm https://raw.githubusercontent.com/adelphidata/borg/main/scripts/install.ps1 | iex
.PARAMETER Version
    Install a specific version tag (default: latest)
.PARAMETER InstallDir
    Installation directory (default: ~/.local/bin)
.PARAMETER NoOnboarding
    Skip running 'borg init' after install
.PARAMETER Uninstall
    Remove borg binary and optionally ~/.borg/
#>
param(
    [string]$Version = "latest",
    [string]$InstallDir = "$env:USERPROFILE\.local\bin",
    [switch]$NoOnboarding,
    [switch]$Uninstall
)

$ErrorActionPreference = "Stop"
[Net.ServicePointManager]::SecurityProtocol = [Net.SecurityProtocolType]::Tls12
$Repo = "adelphidata/borg"
$BinaryName = "borg.exe"
$DataDir = "$env:USERPROFILE\.borg"
$GithubApi = "https://api.github.com/repos/$Repo"

function Write-Info    { param([string]$Msg) Write-Host "  $Msg" -ForegroundColor Cyan }
function Write-Success { param([string]$Msg) Write-Host "  $Msg" -ForegroundColor Green }
function Write-Warn    { param([string]$Msg) Write-Host "  $Msg" -ForegroundColor Yellow }
function Write-Err     { param([string]$Msg) Write-Host "  $Msg" -ForegroundColor Red }

# ── Uninstall ──

function Test-Interactive {
    return [Environment]::UserInteractive -and -not [Console]::IsInputRedirected
}

if ($Uninstall) {
    # Prefer the resolved binary on PATH; fall back to InstallDir.
    $resolved = Get-Command $BinaryName -ErrorAction SilentlyContinue
    $binary = if ($resolved) { $resolved.Source } else { Join-Path $InstallDir $BinaryName }
    if (Test-Path $binary) {
        Remove-Item $binary -Force
        Write-Success "Removed $binary"
        $daemon = Join-Path (Split-Path $binary -Parent) "borgd.exe"
        if (Test-Path $daemon) {
            Remove-Item $daemon -Force
            Write-Success "Removed $daemon"
        }
    } else {
        Write-Warn "No borg binary found"
    }
    if (Test-Path $DataDir) {
        if (Test-Interactive) {
            $answer = Read-Host "  Remove data directory $DataDir? [y/N]"
            if ($answer -eq "y") {
                Remove-Item $DataDir -Recurse -Force
                Write-Success "Removed $DataDir"
            } else {
                Write-Info "Kept $DataDir"
            }
        } else {
            Write-Info "Kept $DataDir (non-interactive mode)"
        }
    }
    exit 0
}

# ── Version resolution ──

if ($Version -eq "latest") {
    Write-Info "Resolving latest release..."
    $release = Invoke-RestMethod -Uri "$GithubApi/releases/latest" -Headers @{ "User-Agent" = "borg-installer" }
    $tag = $release.tag_name
    if (-not $tag) {
        Write-Err "Could not determine latest release. Check https://github.com/$Repo/releases"
        exit 1
    }
} else {
    $tag = $Version
}

Write-Info "Installing borg $tag..."

# ── Existing-version short-circuit ──

$existingBinary = Get-Command $BinaryName -ErrorAction SilentlyContinue
if ($existingBinary) {
    $existingVersion = (& $existingBinary.Source --version 2>$null) -split '\s+' | Select-Object -Index 1
    if ($existingVersion) {
        $tagNoV = $tag -replace '^v',''
        if ($existingVersion -eq $tagNoV) {
            Write-Success "Already installed and up to date (borg $existingVersion)"
            exit 0
        }
        Write-Info "Found existing borg $existingVersion at $($existingBinary.Source)"
    }
}

# ── Download ──

$asset = "borg-windows-x86_64.zip"
$baseUrl = "https://github.com/$Repo/releases/download/$tag"
$tmpDir = Join-Path ([System.IO.Path]::GetTempPath()) "borg-install-$(Get-Random)"
New-Item -ItemType Directory -Path $tmpDir -Force | Out-Null

$archivePath = Join-Path $tmpDir $asset
$checksumPath = Join-Path $tmpDir "checksums.txt"

Write-Info "Downloading $asset..."
Invoke-WebRequest -Uri "$baseUrl/$asset" -OutFile $archivePath -UseBasicParsing

# Checksum verification — a missing checksums.txt likely means a tampered or
# incomplete release, so treat network failure as fatal (matches install.sh).
try {
    Invoke-WebRequest -Uri "$baseUrl/checksums.txt" -OutFile $checksumPath -UseBasicParsing
} catch {
    Write-Err "Failed to download checksums.txt -- cannot verify binary integrity."
    Write-Err "This may indicate a network issue or a tampered release."
    exit 1
}
$checksumLine = Get-Content $checksumPath | Where-Object { $_ -match $asset }
if (-not $checksumLine) {
    Write-Err "No checksum entry found for $asset in checksums.txt"
    Write-Err "The release may be incomplete or corrupted."
    exit 1
}
$expected = ($checksumLine -split "\s+")[0]
$actual = (Get-FileHash -Path $archivePath -Algorithm SHA256).Hash.ToLower()
if ($actual -ne $expected) {
    Write-Err "Checksum mismatch! Expected: $expected Got: $actual"
    exit 1
}
Write-Success "Checksum verified"

# ── Extract & install ──

Expand-Archive -Path $archivePath -DestinationPath $tmpDir -Force

$extractedBinary = Join-Path $tmpDir $BinaryName
if (-not (Test-Path $extractedBinary)) {
    Write-Err "Binary not found in archive. Expected: $BinaryName"
    exit 1
}

if (-not (Test-Path $InstallDir)) {
    New-Item -ItemType Directory -Path $InstallDir -Force | Out-Null
}
Copy-Item $extractedBinary (Join-Path $InstallDir $BinaryName) -Force
Write-Success "Borg installed to $InstallDir\$BinaryName"

$daemonBinary = Join-Path $tmpDir "borgd.exe"
if (Test-Path $daemonBinary) {
    Copy-Item $daemonBinary (Join-Path $InstallDir "borgd.exe") -Force
    Write-Success "Borgd installed to $InstallDir\borgd.exe"
}

# ── PATH setup ──

$userPath = [Environment]::GetEnvironmentVariable("PATH", "User")
$normalizedTarget = $InstallDir.TrimEnd('\','/').ToLowerInvariant()
$pathEntries = ($userPath -split ";") | ForEach-Object { $_.TrimEnd('\','/').ToLowerInvariant() }
if ($normalizedTarget -notin $pathEntries) {
    [Environment]::SetEnvironmentVariable("PATH", "$userPath;$InstallDir", "User")
    $env:PATH = "$env:PATH;$InstallDir"
    Write-Success "Added $InstallDir to user PATH (restart your terminal to take effect)"
} else {
    Write-Info "$InstallDir already in PATH"
}

# ── Cleanup ──

Remove-Item $tmpDir -Recurse -Force -ErrorAction SilentlyContinue

# ── Onboarding ──
# Settings live in ~/.borg/borg.db (DB-only). Use DB existence as the gate so
# reinstalls don't re-launch the wizard over a configured system. Skip when
# non-interactive (e.g. `iex` pipelines) — `borg init` would block on stdin.
$dbPath = Join-Path $DataDir "borg.db"
if (-not (Test-Path $dbPath)) {
    if (-not $NoOnboarding -and (Test-Interactive)) {
        Write-Host ""
        Write-Info "Running borg init..."
        & (Join-Path $InstallDir $BinaryName) init
    } else {
        Write-Info "Run 'borg init' to complete setup"
    }
}
