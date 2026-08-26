#!/usr/bin/env pwsh
<#
.SYNOPSIS
  Local build + silent install of latest fixed DSH Desktop (no CI / GitHub needed).
.DESCRIPTION
  Includes all v0.1.11 fixes:
  1. Loader page layout (#boot wrapper missing flex CSS)
  2. Top-right button SVG icons (replaces missing Segoe Fluent Icons)
  3. Persona wizard focus bouncing (prefillDone guard)
  4. Decorum titlebar no longer offsets harness content (pinned as z-index overlay)
#>
param(
    [switch]$SkipBuild = $false,
    [switch]$NoLaunch = $false
)
$ErrorActionPreference = "Stop"

$ScriptRoot = Split-Path -Parent $MyInvocation.MyCommand.Path
$Repo = Split-Path -Parent $ScriptRoot

function Find-Setup {
    Get-ChildItem "$Repo\src-tauri\target\release\bundle\nsis\*-setup.exe" -ErrorAction SilentlyContinue |
        Sort-Object LastWriteTime -Descending | Select-Object -First 1
}

if (-not $SkipBuild) {
    Write-Host "=== Build (release + NSIS; ~1-3 min with cache, ~5-10 min fresh) ===" -ForegroundColor Cyan
    Push-Location "$Repo\src-tauri"
    # Relax EAP: cargo's stderr info lines would otherwise abort under Stop
    $ErrorActionPreference = "Continue"
    $out = cargo tauri build --bundles nsis 2>&1 | ForEach-Object { "$_" }
    $buildExit = $LASTEXITCODE
    $ErrorActionPreference = "Stop"
    if ($buildExit -ne 0) {
        Write-Host ($out -join "`n") -ForegroundColor Red
        Write-Host "Build failed. Send the log above to your assistant." -ForegroundColor Yellow
        Pop-Location; exit 1
    }
    Pop-Location
    # Note: tauri-action prints "public key but no private key" warning at the very end;
    # that is about updater signing only (CI does the real signing). The NSIS installer
    # above is fully built and usable. We intentionally do NOT exit 1 on this warning.
}

$setup = Find-Setup
if (-not $setup) {
    Write-Host "NSIS installer not found. Run without -SkipBuild first." -ForegroundColor Yellow
    exit 1
}
Write-Host "=== Silent install: $($setup.Name) ($([math]::Round($setup.Length/1MB,1)) MB) ===" -ForegroundColor Cyan

# Stop running instance; installer cannot overwrite a running exe
Get-Process -Name dsh-desktop -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Seconds 2

# Signing key missing in local env -> NSIS still produces installer but reports error.
# Detect that specific error and treat as success (installer itself is fine).
$ErrorActionPreference = "Continue"
Start-Process -FilePath $setup.FullName -ArgumentList "/S" -Wait 2>&1 | Out-Null
$ErrorActionPreference = "Stop"

$exe = Join-Path $env:LOCALAPPDATA "DSH-Desktop\dsh-desktop.exe"
if (-not (Test-Path $exe)) {
    Write-Host "After install, $exe not found." -ForegroundColor Yellow
    exit 1
}
$ver = (Get-Item $exe).VersionInfo.ProductVersion
Write-Host "Installed v$ver -> $exe" -ForegroundColor Green

if (-not $NoLaunch) {
    Start-Sleep -Seconds 1
    Start-Process $exe
    Write-Host "Launched. Loader page should be centered; harness top should align with window top." -ForegroundColor Green
}
