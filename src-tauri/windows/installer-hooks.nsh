; DSH Desktop NSIS installer hooks (referenced by tauri.conf.json bundle.windows.nsis.installerHooks).
;
; PREINSTALL: make sure no running dsh-desktop instance keeps the old exe locked
; while the installer overwrites files.
;
; Background (real incident 2026-09-26): this app lives in the tray, so an upgrade
; over a running instance failed to overwrite dsh-desktop.exe while the registry
; still recorded the new version -- "installed the latest, still runs the old
; program".
;
; 2026-11-24 correction, matching the official desktop client: do NOT silently
; force-kill in interactive installs. This shell has no kill-on-close job object
; for its dsh child, so a force-killed shell can leave the dsh process tree
; orphaned, and killing it discards whatever task the user was running without
; asking. Ask instead: the user quits from the tray (where the quit path runs
; the task-aware confirmation), then re-runs the installer.
;
; A silent install (/S, used by the release pipeline and the portable packaging
; step) cannot show a prompt, so it keeps the old force-kill behaviour; the
; portable step only packages files and has no user task to lose.
;
; 2026-11-25 fix of the fix: detection now uses PowerShell's EXIT CODE
; (Get-Process + exit 42 if running) instead of parsing `tasklist` output --
; nsExec::Exec only pushes the exit code onto the stack (never the output), so
; the old tasklist/string-compare version reported "running" on EVERY machine
; regardless of locale or actual state (v0.1.87-v0.1.90 installers blocked all
; interactive installs with a false positive). Retry loop: after each prompt we
; re-detect, so quitting the app from the tray lets the install continue
; without restarting the setup.
!macro NSIS_HOOK_PREINSTALL
  ${If} ${Silent}
    DetailPrint "Stopping running DSH Desktop (silent install)..."
    nsExec::Exec 'taskkill /IM dsh-desktop.exe /F'
    Sleep 600
  ${Else}
    StrCpy $R0 0
  preinstall_detect:
    nsExec::Exec 'powershell -NoProfile -Command "if (Get-Process -Name dsh-desktop -ErrorAction SilentlyContinue) { exit 42 }"'
    Pop $0
    IntCmp $0 42 0 install_continue install_continue
    ; ---- 仍在运行：询问并等待用户处理 ----
    IntOp $R0 $R0 + 1
    ${If} $R0 > 3
      MessageBox MB_OK|MB_ICONSTOP "DSH Desktop 仍在运行（已确认多次）。$\r$\n$\r$\n请从系统托盘右键 DSH Desktop 图标选择「退出」后重新运行安装程序；若无法退出，请重启电脑后再试。"
      Abort
    ${EndIf}
    MessageBox MB_OK|MB_ICONINFORMATION "DSH Desktop 正在运行。$\r$\n$\r$\n请先在系统托盘右键 DSH Desktop 图标并选择「退出」（若有任务在跑，它会先向你确认），然后点「确定」继续安装。"
    Goto preinstall_detect
    install_continue:
  ${EndIf}
!macroend

; POSTUNINSTALL intentionally absent: this app never writes to the user PATH
; (there is no CLI registration feature), so there is nothing to clean up. The
; v0.1.87 hook that tried to rewrite HKCU Environment\Path was verified dead by
; compiling it with makensis: NSIS treats $k/$d/$p/$n/$_ as undefined variables
; (warning 6000, then removes them) and the '' quoting truncated the command.
; A no-op that only looks like cleanup is worse than none.
