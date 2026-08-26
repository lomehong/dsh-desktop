@echo off
rem 一键修复安装：本地构建最新修复版 DSH Desktop 并静默安装、启动
rem（右键「以管理员身份运行」不需要；普通双击即可）
cd /d "%~dp0"
powershell -NoProfile -ExecutionPolicy Bypass -File "scripts\install-fixed.ps1"
pause
