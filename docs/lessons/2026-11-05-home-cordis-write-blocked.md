# 2026-11-05 home/profiles/web/cordis.yml 写入被系统级拦截 → 服务无声挂死

## 现象

dsh 运行时升级到 0.2.2-alpha.3（随后 rc.1 同样）后，壳拉起的服务进程挂死：
进程活着、零 stdout/stderr 输出、无端口监听，线程停在内核 IOCP 等待
（ZwRemoveIoCompletionEx，libuv 事件循环空闲形态）。UI 无限转圈无任何解释；
手动终端跑 `dsh web` 则在 compose 第一步 writeFileSync(cordis.yml) 处
EPERM（errno -4048）崩退。同一文件写入，壳 spawn 的进程「挂起」、终端进程
「拒绝」——两种表象同源。

## 关键事实（全部实测）

1. **拦截精确到单个文件**：`home/profiles/web/cordis.yml` 写/创建被 EPERM；
   同目录 `probe2.txt` 等任意其它文件名写入正常（用户桌面 pwsh 实测）。
2. **火绒排除**：安全日志全程零拦截记录（用户自查）；信任区加白无效。
3. **dump 线程栈无第三方模块**：14 线程栈上只有 node.exe/ntdll/KERNELBASE/
   kernel32/user32——无任何注入钩子。
4. **重启电脑无法清除**（挂死进程 Access denied 不可终止，需任务管理器用户侧
   结束）；换运行时版本（alpha.3→rc.1）无效；重装应用无效。
5. **home 目录改名重建后立刻恢复**——问题绑定在旧 home 的该文件对象/路径状态上。
6. **拦截者未定案**：火绒已排除；fltmc 显示第三方 filter 仅 sysdiag（火绒 WHQL）
   一个，bfs.sys 为微软 Brokering File System（AppContainer 组件）。

## 恢复路径（分钟级）

将 `runtime_root/home` 改名为 `home.broken-backup`（数据保留）→ 启动应用重建
全新 home → 白名单搬运回数据（sessions/llm-deepseek/套件数据/cordis.patch.yml
用户层；**不搬 profiles/ 整目录**）→ 托盘重装数字分身套件。

## 壳侧防御（本次加入）

1. **spawn 前 写探测**（`install::probe_profile_write`，start_service_locked
   keep-alive 补丁后执行）：对 `profiles/web/cordis.yml` 做 WRITE open /
   create_new 探测，PermissionDenied 直接给可读报错与修复指引（含 home 重建
   路径），不再进入无声挂死流程。
2. **零输出早告警**：spawn 后 30 秒无任何 stdout → 日志 warn + 状态文字提示
   「可能被安全软件拦截」，不打断等待（超长安装属正常）。
3. **180 秒超时报错附指引**：文案加入「安全软件拦截写入 / home 数据异常」
   两个常见原因与处置。
4. **guardian 台账新增 `profile_preset` 规则**：`agent preset <id>: <原因>`
   日志立案（2026-11-03 起两个自定义预设 broken 三天无可见性的教训）。

## 诊断方法论教训（给后续排查者，含 AI 协作者）

1. **agent 诊断沙箱的观测不可作为用户机器的证据**：本次「写文件 EPERM」
   在诊断沙箱与用户桌面**都出现**，但「进程杀不动（Access denied）」「进程
   命令行/Path 读不出」「System32 文件读出内容被中转」等观测**全部是沙箱
   视角假象**——用户桌面令牌下进程可以直接结束。凡涉及进程权限/文件系统
   行为的结论，必须由用户在干净终端（用户自己的 pwsh/cmd）复核。
2. **「挂死」与「EPERM 崩退」要区分调用者**：同一文件写入，交互式终端进程
   被立即拒绝，后台服务进程可能被挂起等待——形态不同不等于问题不同。
3. **转储文件（任务管理器「创建转储文件」）是解析挂死的最强工具**：本次靠
   自写 minidump 解析器读出线程栈模块归组 + syscall 桩 SSN 解码
   （`mov eax, SSN` 在 RIP 前向），定位到「事件循环空等」而非「第三方注入」，
   一举排除安全软件挂起假说、聚焦 compose 静默未走完。
4. **「之前的版本没问题」是极有价值的一手证据**：alpha.2 正常 → alpha.3
   起故障，把排查范围锁定在升级动作触发的状态变化（整树替换 + profile
   清空重装 + 文件状态），而不是漫无目的的环境怀疑。
