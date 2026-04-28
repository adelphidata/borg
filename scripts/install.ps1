#Requires -Version 5.1
<#
.SYNOPSIS
    Borg Installer for Windows
.DESCRIPTION
    Downloads and installs the borg AI personal assistant binary.
    Usage: irm https://raw.githubusercontent.com/borganization/borg/main/scripts/install.ps1 | iex
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
$Repo = "borganization/borg"
$BinaryName = "borg.exe"
$DataDir = "$env:USERPROFILE\.borg"
$GithubApi = "https://api.github.com/repos/$Repo"

function Write-Info    { param([string]$Msg) Write-Host "  $Msg" -ForegroundColor Cyan }
function Write-Success { param([string]$Msg) Write-Host "  $Msg" -ForegroundColor Green }
function Write-Warn    { param([string]$Msg) Write-Host "  $Msg" -ForegroundColor Yellow }
function Write-Err     { param([string]$Msg) Write-Host "  $Msg" -ForegroundColor Red }

# ── Uninstall ──

if ($Uninstall) {
    $binary = Join-Path $InstallDir $BinaryName
    if (Test-Path $binary) {
        Remove-Item $binary -Force
        Write-Success "Removed $binary"
    } else {
        Write-Warn "No borg binary found at $binary"
    }
    $answer = Read-Host "  Remove data directory $DataDir? [y/N]"
    if ($answer -eq "y") {
        if (Test-Path $DataDir) {
            Remove-Item $DataDir -Recurse -Force
            Write-Success "Removed $DataDir"
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

# ── Download ──

$asset = "borg-windows-x86_64.zip"
$baseUrl = "https://github.com/$Repo/releases/download/$tag"
$tmpDir = Join-Path ([System.IO.Path]::GetTempPath()) "borg-install-$(Get-Random)"
New-Item -ItemType Directory -Path $tmpDir -Force | Out-Null

$archivePath = Join-Path $tmpDir $asset
$checksumPath = Join-Path $tmpDir "checksums.txt"

Write-Info "Downloading $asset..."
Invoke-WebRequest -Uri "$baseUrl/$asset" -OutFile $archivePath -UseBasicParsing

# Checksum verification
try {
    Invoke-WebRequest -Uri "$baseUrl/checksums.txt" -OutFile $checksumPath -UseBasicParsing
    $checksumLine = Get-Content $checksumPath | Where-Object { $_ -match $asset }
    if ($checksumLine) {
        $expected = ($checksumLine -split "\s+")[0]
        $actual = (Get-FileHash -Path $archivePath -Algorithm SHA256).Hash.ToLower()
        if ($actual -ne $expected) {
            Write-Err "Checksum mismatch! Expected: $expected Got: $actual"
            exit 1
        }
        Write-Success "Checksum verified"
    } else {
        Write-Warn "No checksum entry for $asset -- skipping verification"
    }
} catch {
    Write-Warn "Could not download checksums -- skipping verification"
}

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
$pathEntries = $userPath -split ";"
if ($InstallDir -notin $pathEntries) {
    [Environment]::SetEnvironmentVariable("PATH", "$userPath;$InstallDir", "User")
    $env:PATH = "$env:PATH;$InstallDir"
    Write-Success "Added $InstallDir to user PATH (restart your terminal to take effect)"
} else {
    Write-Info "$InstallDir already in PATH"
}

# ── Cleanup ──

Remove-Item $tmpDir -Recurse -Force -ErrorAction SilentlyContinue

# ── Onboarding ──

if (-not $NoOnboarding) {
    Write-Host ""
    Write-Info "Running borg init..."
    & (Join-Path $InstallDir $BinaryName) init
}
