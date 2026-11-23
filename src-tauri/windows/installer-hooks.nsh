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
; force-kill. This shell has no kill-on-close job object for its dsh child, so a
; force-killed shell can leave the dsh process tree orphaned, and killing it
; discards whatever task the user was running without asking. Ask instead:
; the user quits from the tray (where the quit path runs the task-aware
; confirmation), then re-runs the installer.
;
; A silent install (/S, used by the release pipeline and the portable packaging
; step) cannot show a prompt, so it keeps the old force-kill behaviour; the
; portable step only packages files and has no user task to lose.
!macro NSIS_HOOK_PREINSTALL
  ${If} ${Silent}
    DetailPrint "Stopping running DSH Desktop (silent install)..."
    nsExec::Exec 'taskkill /IM dsh-desktop.exe /F'
    Sleep 600
  ${Else}
    nsExec::Exec 'tasklist /FI "IMAGENAME eq dsh-desktop.exe" /NH'
    Pop $0
    ${If} $0 != ""
      ${If} $0 != "INFO: No tasks are running which match the specified criteria."
        MessageBox MB_OK|MB_ICONINFORMATION "DSH Desktop 正在运行。$\r$\n$\r$\n请先在系统托盘右键 DSH Desktop 图标并选择「退出」（若有任务在跑，它会先向你确认），然后再继续安装。" IDOK
        Abort
      ${EndIf}
    ${EndIf}
  ${EndIf}
!macroend

; POSTUNINSTALL intentionally absent: this app never writes to the user PATH
; (there is no CLI registration feature), so there is nothing to clean up. The
; v0.1.87 hook that tried to rewrite HKCU Environment\Path was verified dead by
; compiling it with makensis: NSIS treats $k/$d/$p/$n/$_ as undefined variables
; (warning 6000, then removes them) and the '' quoting truncated the command.
; A no-op that only looks like cleanup is worse than none.
