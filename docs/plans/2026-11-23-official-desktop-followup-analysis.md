# 官方桌面客户端跟进分析（0.2.3-alpha.2）——对照 9/30 基线的增量与修订

- 日期：2026-11-23
- 对比对象：
  - 官方：`deepseek-harness/apps/desktop|desktop-host|desktop-publisher`（三应用全部 0.2.3-alpha.2，Electron 壳，本地克隆 `D:\Develop\Coder\nodejs\deepseek-harness`，master @ `4b399c39` = alpha.2 release merge）
  - 本项目：`dsh-desktop`（Tauri v2 / Rust 壳，v0.1.86，运行时基线 0.2.3-alpha.2）
- 基线文档：`docs/plans/2026-09-30-official-desktop-client-comparison.md`（对照 v0.2.0-rc.2，十项分档）。本文只写**增量与修订**，不重复旧文。
- 官方活跃度：desktop 三应用近 60 天 667 提交（不含 merge），版本始终与 dsh 核心单一发布单元锁定。

--- 

## 一、架构变化（相对 9/30 基线）

### 1. desktop-host 独立应用化 + stdio-IPC 消息协议（旧文第二档 #10 的官方完整形态）

- Host 从「RunAsNode 跑一个 profile runner」升级为独立私有应用 `@deepseek-ai/dsh-desktop-host`（跑完整 dsh harness + workspace/plugin-manager/jobs 等依赖），shell 仍以 `ELECTRON_RUN_AS_NODE` 复用 Electron 可执行文件拉起（`stdio: ['ignore','pipe','pipe','ipc']`——**第 4 通道是 Electron 原生 IPC**）。
- **消息协议（双向、zod 校验）**：
  - 子→父：`starting{runId}` → `ready{url,injections}` → `fatal{message,diagnostic}`；`shutdown-complete`；`platform-session{session}`；`update-tasks{requestId,active}`；`quit-inspection{requestId,activeTasks,scheduledTasks}`；`file-drag-resource{requestId,resource}`；`plugin-operation{record}`
  - 父→子：`desktop-auth{secret}`（父进程生成 32 字节随机 secret，`starting` 后一次性下发 → Host 注册为 `dsh-desktop-session=<secret>` 凭据 cookie，仅主进程持有）、`shutdown`、`update-tasks`、`quit-inspection`、`file-drag-resource/cancel`
- **停机握手**：`shutdown` → Host 排空回 `shutdown-complete` → 壳侧 10s TERM → 5s KILL 升级；**更新交接要求优雅退出，否则抛 `DesktopHostUncleanExitError` 拒绝安装并重启 Host 恢复工作区**。
- 对我们的意义：旧文建议「推动 dsh 上游配合做停机握手」——官方已经把整套协议做完了，且**不依赖 dsh 核心配合**（Host 是官方自己的应用）。我们的 supervisor 仍靠 `kill_tree`：0.2.3 起 dsh 支持 `--host 127.0.0.1 --port 0` + 优雅停机所需的宿主形态已在上游验证，等 0.2.3 稳定线可整段参考。

### 2. 发布管线独立化：desktop-publisher（旧文未覆盖的新形态）

- 发布不再是一步 tauri-action：**先按 SHA-512 对账 COS 对象，再写 channel feed**；不可变二进制禁止覆盖；tag 强制指向打包 commit；灰度只改 feed 的 `stagingPercentage`（0-100）；腾讯 EdgeOne 清缓存；官网安装器单独校验发布；CI 要求环境审批 + 打包 run 资格校验（signed/notarized）。渠道仅 `nightly | stable`。
- 对我们的意义：我们的 GitHub Release 单步发布已经够用（体量小）；灰度百分比与「先对账后改 feed」两条值得在用户量上来后参考。

### 3. 强更/更新体系深化（旧文第二档 #9 的完整形态）

- 强更覆盖层 = **独立 preload（closed ShadowRoot + MessagePort）+ 独立文档窗口 + 页内 iframe overlay** 三形态；策略测试用「feishu-test 认证弹窗」；就绪提醒是全应用唯一 Notification。
- 更新调度：系统设置 + `powerMonitor.resume` + 窗口 focus 触发检查；自定义 http-executor 加空闲超时（防静默挂死——我们 0.1.79 的「挂死四层防御」同主题，可对照补 updates 侧）。
- `NSIS --updated` 参数触发更新后提窗；channel 切换 + `stagingPercentage` 灰度 + JSONL 审计。

### 4. 安全模型：dsh-app:// 私有 scheme + 发送方校验

- 渲染层只见 `dsh-app://app`（反代 Host）与 `dsh-app://shell`（本地文档）；所有 IPC 先过 `assertDesktopSender()`（sender 必须是特权 scheme + 主窗 mainFrame）。Platform 凭据用 `sendSync` 一次性 bootstrap，token 永不二次进 IPC。
- 对照我们：loopback + token→cookie 模型功能等价；私有 scheme 的额外收益是**不占端口、无回环暴露面**。我们的 readiness 探活/反代链路已经成型，迁移成本高，列为长期参考。

## 二、对我们正在痛的问题的官方答案

### Computer Use 原生 exe 缺失（我们 11-22 排查的 ENOENT）

- 官方做法：**cua 原生二进制随应用包分发**——electron-builder `files` 白名单显式含 `**/node_modules/@deepseek-ai/dsh-cua-macos/native/bin/**/*`（macOS；Windows 后端在 0.2.3-alpha 尚未进桌面客户端的打包面）。即官方的答案是**构建期打包**，与 npm 运行期安装天然绝缘——我们在 npm 形态踩到的「发布缺 exe」他们靠随包分发规避了。
- 借鉴：短期（a）跟踪上游 npm 修复；（b）我们的「安装运行环境」自愈可加一层：npm 装完检查 `dsh-cua-windows/native/bin/win-x64/dsh-cua-windows.exe` 缺失时，在守护/日志里给出明确指引（上游发布缺陷 + 已知手工解法），而不是让用户对着 ENOENT 猜。长期（c）若 Computer Use 转正，参考官方把 exe 纳入壳的分发面（放进 NSIS/便携包 `extraResources`，安装时注入运行时树）。
- 附带：我们手工构建的 exe（`Temp\cua-build\out-a2`）与官方 Windows 后端的最终分发形态（官方桌面客户端 0.2.3 尚未打包 cua-windows，等 rc 观察）——若官方桌面客户端带上了 Windows 版 exe，可直接「移植官方产物」替代自建。

### 启动自举的「零网络安装」哲学（旧文第二档 #8 的延续）

- 官方 desktop 依旧无 postinstall/无原生编译，全部预编译包 + 构建期物化。我们 0.1.82/83 的「145 → NotFound → 冒烟连红」链条再次证明启动期安装是故障高发面。旧文建议的「便携分发预物化 + 哈希清单校验」优先级应上调：v0.1.85 加的镜像覆盖 env 只是止痛，预物化才是结构解。

## 三、新增可借鉴能力（旧文清单之外的增量）

| # | 能力 | 官方实现要点 | 对我们的落点 | 档 |
|---|---|---|---|---|
| 1 | Parent↔Host IPC 协议 | 见 一.1；消息名/zod 校验/auth-secret/优雅停机握手全套 | supervisor 停机与升级路径重构的蓝本（等 0.2.3 稳定线） | 高 |
| 2 | update-tasks inspect/lock/unlock | 更新前经 IPC 排空任务并加锁；`quit-inspection` 2s deadline | 旧文第一档 #1 的落地细节现在完整可抄 | 高 |
| 3 | Windows 卸载残留清理系列 | 升级卸载清理指向旧安装的注册表项/孤儿 PATH/长路径处理/迁移后旧命令注册清理（#7970 系列） | 我们的 `installer-hooks.nsh` 可对照补齐（升级卸载残留清理），尤其 PATH 孤儿 | 高 |
| 4 | file-drag 授权链 | 拖拽文件经 Host 授权解析本地路径 + sha256，渲染层零路径暴露 | 安全加固项，中 | 中 |
| 5 | browser-guests 分区租约 | webview 按 workspace 分 partition + 租约 acquire/release | 旧文第三档已有；官方实现已落地，做内嵌浏览器时直接抄 | 中 |
| 6 | welcome onboarding 窗 | 600x700 固定 React 窗（本地化/登录/API key/analytics 选择） | 对照我们的分身向导（persona wizard）形态升级参考 | 中 |
| 7 | operation journal/report | 插件操作历史持久化 + 报告（近期修复了按文件身份去重的段保留） | 对照我们的守护台账：操作审计可复用其分段去重思路 | 低 |
| 8 | 渠道（nightly/stable）+ 灰度 | feed channel + stagingPercentage | 用户量上来后参考 | 低 |

## 四、旧十项建议的落地状态核对（我们侧，截至 v0.1.86）

| 旧文项 | 状态 |
|---|---|
| #1 任务感知退出 | 未落地（守护 inspect 雏形在，未接退出/升级路径） |
| #2 崩溃报告 + 恢复对话 | 部分：守护 Agent + 活鲸覆盖层（v0.1.68）有了；崩溃报告落盘 + 「禁用第三方插件」恢复对话仍缺 |
| #3 单一更新单元/清单硬门禁 | 部分：适配线闸 + 逐版核查纪律（本文档系列），「清单外拒绝」的硬门禁未做 |
| #4 webview 派生缓存清理 | 部分：远程路径已有（v0.1.67 上游）；本地核心变更自愈后的清理未做 |
| #5 单实例锁时机审计 | 未做 |
| #6 CLI 注册管理 | 未做 |
| #7 primary runtime（Python 离线包） | 未做（按产品路线） |
| #8 便携分发预物化 | 未做（0.1.82/83 事故后优先级应上调，见本文 二） |
| #9 更新调度/差分/强更 | 未做 |
| #10 Host 停机握手 | 未做（官方完整形态已就绪，见 一.1） |

## 五、修订后的行动建议（按序，替代旧文第六节）

1. **安装运行环境自愈的可观测性补丁**（小，随下版）：npm 装完自检 cua-windows 原生 exe 缺失时给出明确指引（上游 0.2.3-alpha 发布缺陷，见 11-22 排查）——不再让用户对着 ENOENT 猜。
2. **便携分发预物化 + 哈希清单**（旧 #8 上调）：U 盘包/安装器携带物化 node_modules + `desktop-runtime.json` 式清单，启动校验；npm 在线安装只留给核心升级。
3. **崩溃报告 + 恢复对话**（旧 #2 续）：守护重启耗尽 → 落崩溃报告 → 「禁用第三方插件」恢复选项。
4. **任务感知退出**（旧 #1 续）：0.2.3 稳定线落地后，参照官方 `quit-inspection` 协议接入托盘退出与升级路径。
5. **installer-hooks 对照官方卸载清理**（新 #3）：升级卸载时的注册表/PATH 残留清理。
6. updates 侧空闲超时对照补强（新 一.3；我们 0.1.79 的挂死防御已覆盖服务侧，更新下载侧可对照）。
7. 其余沿旧文（CLI 注册、primary runtime、渠道灰度）按产品节奏。

## 六、差异化优势复核（无变化 + 一条新增强化）

- 旧文第三节列举的差异化优势（远程实例管理、套件一键安装、Rescue/活鲸、版本闸、托盘细节、自启动）官方 0.2.3-alpha.2 仍**均未实现**——自启动、badge、jumplist 官方依旧没有。
- 新增强化：**多远程实例 + 御符账号**在我们这已是成熟链路；官方 0.2.3 才刚把「远程连接」拆成独立 client 包（remote-connection），形态仍在演进。

--- 

- 附：官方实现清单增量备查——11 个 preload（app/browser/mandatory/mandatory-overlay/menu/platform-account/platform/theme/update-dialog/welcome/windows）；IPC 面分组（DESKTOP_IPC/PLATFORM_IPC/updates/update-dialog/mandatory/welcome/shortcuts/file-drag）；powerMonitor resume 触发更新检查；`session-end`/断电通知处理；dsh-cua-macos 原生二进制随包分发。
