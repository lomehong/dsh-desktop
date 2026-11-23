; DSH Desktop NSIS 安装钩子（tauri.conf.json bundle.windows.nsis.installerHooks 引用）。
;
; PREINSTALL：结束所有运行中的 dsh-desktop 实例后再覆盖文件。
; 背景（2026-09-26 实机事故）：本应用常驻托盘，升级覆盖安装时旧实例锁住
; dsh-desktop.exe，NSIS 覆盖失败但注册表/清单版本号照常更新——
; 造成「装了最新版，跑的还是旧程序」的假象（任务栏旧图标、新功能缺失）。
;
; 强杀后由两道既有机制兜底，不留坏状态：
; 1) 下次启动 cleanup_stale_orphan 按 runtime.pid 识别并整树击杀孤儿 dsh 子进程；
; 2) 窗口状态不落盘坏值（隐藏/最小化态的采样有守卫）。
;
; POSTUNINSTALL：升级卸载的注册表/PATH 残留清理（对照官方 desktop 的
; windows-uninstall-registry 系列，2026-11-23 跟进分析）。卸载把安装目录整个
; 删除后，用户 PATH 里指向该目录的条目成为孤儿；用 PowerShell 过滤重写
; HKCU Environment（.NET 写用户环境变量自带 WM_SETTINGCHANGE 广播），
; NSIS 侧无需引入字符串替换函数库。
!macro NSIS_HOOK_PREINSTALL
  DetailPrint "正在关闭运行中的 DSH Desktop…"
  nsExec::Exec 'taskkill /IM dsh-desktop.exe /F'
  Sleep 600
!macroend

!macro NSIS_HOOK_POSTUNINSTALL
  DetailPrint "正在清理用户 PATH 中的安装目录残留…"
  nsExec::Exec 'powershell -NoProfile -NonInteractive -ExecutionPolicy Bypass -Command "$k = ''HKCU:\Environment''; $d = ''$INSTDIR''; $p = [Environment]::GetEnvironmentVariable(''Path'', ''User''); if ($p) { $n = (($p -split '';'' | Where-Object { $_ -and -not $_.Trim().StartsWith($d, [System.StringComparison]::OrdinalIgnoreCase) }) -join '';''); if ($n -ne $p) { [Environment]::SetEnvironmentVariable(''Path'', $n, ''User'') } }"'
!macroend
