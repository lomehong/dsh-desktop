#!/usr/bin/env pwsh
<#
.SYNOPSIS
  本地构建含全部修复的 DSH Desktop 并静默安装（不依赖 CI / GitHub）
.DESCRIPTION
  包含的修复（v0.1.9，本地已验证）：
  1. 启动加载页布局（#boot 包裹层缺失 flex 样式导致的挤压堆叠）
  2. 顶栏最小化/最大化/关闭按钮豆腐块（SVG 图标替换系统字体依赖）
  3. 分身向导焦点反复跳回/默认值被重置（prefillDone 守卫）
  4. 撤销 v0.1.5/v0.1.6 的两处误诊，恢复一贯注入方式
#>
param(
    [switch]$SkipBuild = $false,   # 已构建过则跳过（直接用现有 NSIS 产物）
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
    Write-Host "=== 构建（release + NSIS；有缓存约 1~3 分钟，全新约 5~10 分钟）===" -ForegroundColor Cyan
    Push-Location "$Repo\src-tauri"
    # Tauri 会把 ui/ 嵌入 exe（frontendDist ../ui），构建即包含全部前端修复
    # 放宽 EAP：cargo 的 stderr Info 行在 Stop 下会被当终止错误
    $ErrorActionPreference = "Continue"
    $out = cargo tauri build --bundles nsis 2>&1 | ForEach-Object { "$_" }
    $buildExit = $LASTEXITCODE
    $ErrorActionPreference = "Stop"
    if ($buildExit -ne 0) {
        Write-Host ($out -join "`n") -ForegroundColor Red
        Write-Host "构建失败——把上方日志发给助手排查" -ForegroundColor Yellow
        Pop-Location; exit 1
    }
    Pop-Location
}

$setup = Find-Setup
if (-not $setup) { Write-Host "未找到 NSIS 安装包（先不带 -SkipBuild 跑一次构建）" -ForegroundColor Yellow; exit 1 }
Write-Host "=== 静默安装 $($setup.Name)（$([math]::Round($setup.Length/1MB,1)) MB）===" -ForegroundColor Cyan

# 关闭运行中的实例，安装器不能覆盖运行中的 exe
Get-Process -Name dsh-desktop -ErrorAction SilentlyContinue | Stop-Process -Force
Start-Sleep -Seconds 2

Start-Process -FilePath $setup.FullName -ArgumentList "/S" -Wait

$exe = Join-Path $env:LOCALAPPDATA "DSH-Desktop\dsh-desktop.exe"
if (-not (Test-Path $exe)) { Write-Host "安装后未找到 $exe" -ForegroundColor Yellow; exit 1 }
$ver = (Get-Item $exe).VersionInfo.ProductVersion
Write-Host "已安装 v$ver -> $exe" -ForegroundColor Green

if (-not $NoLaunch) {
    Start-Sleep -Seconds 1
    Start-Process $exe
    Write-Host "已启动。启动页应居中显示、右上角三按钮为 横线/方框/叉号 图标" -ForegroundColor Green
}
