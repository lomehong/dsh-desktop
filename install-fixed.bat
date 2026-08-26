@echo off
rem One-click rebuild + install latest DSH Desktop (auto-runs PowerShell script)
cd /d "%~dp0"
powershell -NoProfile -ExecutionPolicy Bypass -File scripts\install-fixed.ps1
if errorlevel 1 (
  echo.
  echo Build or install failed. See above output for details.
) else (
  echo.
  echo Done. New version should be running.
)
pause
