# 官方桌面客户端（apps/desktop）对比分析——值得借鉴的功能实现

- 日期：2026-09-30
- 对比对象：
  - 官方：`deepseek-harness/apps/desktop`（`@deepseek-ai/dsh-desktop` v0.2.0-rc.2，Electron 壳，本地克隆 `E:\code\nodejs\deepseek-harness`，已检出 `dsh-v0.2.0-rc.2`）
  - 本项目：`E:\code\rust\dsh-desktop`（Tauri v2 / Rust 壳，v0.1.68）
- 结论先行：官方壳与本壳的**架构路线不同但解决的问题高度重合**。官方有六项实现直接命中我们已踩过的故障类（版本分裂、运行时安装故障、升级打断任务、崩溃无报告、插件树损坏、双实例竞态），三项是我们没有的新能力（CLI 注册、primary runtime、任务感知退出）。分档建议见下。

---

## 一、架构对照（一句话版）

| 维度 | 官方 apps/desktop | 本项目 dsh-desktop |
|---|---|---|
| 技术栈 | Electron（主进程 tsdown 单 bundle）+ RunAsNode 子进程跑 dsh profile runner | Tauri v2 / Rust 壳 + 便携 Node 运行时跑 dsh web |
| dsh 依赖形态 | **构建期物化**：`app.asar/dsh` 携带完整生产依赖树，profile 只装外部插件 | **运行期 npm 安装**：首启/核心变更时从镜像源装 `@deepseek-ai/dsh` |
| 版本策略 | **单一签名更新单元**：壳+dsh+pnpm+运行时同一版本号，dsh 升级=Desktop release | 壳与核心各自升级，靠版本闸（适配线）+ 升级器 next tag 管控 |
| 更新 | electron-updater + 差分包 + 三段式状态机 + 强制更新策略 + 更新日志审计 | tauri-plugin-updater（GitHub + 镜像双端点） |
| Web 内容 | 自定义协议 `dsh-app://app`：壳静态服务打包前端 + 认证反代到 Host，cookie 归主进程 | webview 直连 `http://127.0.0.1:3088`，token→cookie 认证 |
| 停机 | Host IPC 握手（shutdown → 10s TERM → 5s KILL → complete 确认） | 击杀进程树 + 守护重启上限 |
| 崩溃面 | 四来源 fatal 统一 → 崩溃报告（保留 10 份）→ 恢复对话（可禁第三方插件） | 守护自愈 + rescue.json/last-healthy.json + Rescue Agent（设计中） |
| 平台 | win-x64 + mac-arm64/x64（签名/公证/ entitlements 全套） | Windows 为主（WebView2），macOS 有适配分支 |

---

## 二、值得抄的功能实现（按优先级）

### 第一档：直接命中我们已踩过的故障类

#### 1. 更新前握手 + 任务感知退出（quit/update inspection）
- 官方实现：任何退出/更新路径先问 Host `inspectQuit`——**活动任务（含子代理、审批等待）、排队消息、已排程提醒**三项事实，2 秒 deadline，超时按"有任务"处理；更新安装前若 Host 非优雅退出（`DesktopHostUncleanExitError`）则**拒绝安装并自动重启 Host 恢复工作区**。关窗=隐藏（任务继续跑），首次隐藏前弹一次性确认并落 `background-close-confirmed` 标记。
- 我们的痛点：supervisor 击杀核心从不问"有没有任务在跑"；9-15 我替用户重启应用打断其工作的那类事故，正是缺这层。Rescue Agent 设计文档里 inspect 已有雏形，但未接入退出/更新路径。
- 落点：`supervisor.rs` 加 `inspect()`（经事件流或 CLI 查活动回合/排程任务）；`tray.rs` 退出菜单与升级流程（`install.rs` 的升级器）接入；关窗策略与 `window_state`/托盘联动。
- 优先级：**最高**。这是壳"替后端任务负责"的核心，且是我们救援体系的地基。

#### 2. 崩溃报告 + 零依赖致命恢复对话
- 官方实现：四来源 fatal（host/web-boot/renderer/main）统一——先写崩溃报告（完整 error ≤256KiB + Host 诊断 ≤64KiB + 渲染层 console 尾部 64KiB + 版本信息，保留最新 10 份），再弹恢复对话：**退出 / 重启 / 禁用第三方插件**（`sanitizeProfile` 在事务锁下备份 `cordis.patch.yml` 为 `.bak-<ts>`）。`EADDRINUSE` 有专用文案。
- 我们的痛点：守护只有"重启 3 次→报错页"；profile bundle 崩溃的自愈是隐式的；Rescue Agent 依赖 LLM，而**崩溃现场恰恰是 LLM 不可用的时候**（设计文档自己的话）。
- 落点：`diagnostics.rs`（现有诊断）扩展为 crash-report 写入器；`supervisor.rs` 的守护重启上限耗尽路径接恢复对话；"禁用第三方插件"直接复用现有的 profile 重装/清理逻辑 + 补丁备份。
- 优先级：**最高**。与 Rescue Agent 是互补关系：先零依赖自愈，LLM 救援兜底。

#### 3. 单一更新单元：dsh 版本与壳版本强绑定
- 官方实现：`desktopRelease()` 强制 desktop 版本 == 根 dsh 版本，否则**拒绝构建**；升级时壳与 dsh 永远一个版本号。README 决策表原话：独立版本会产生"未测试的组合和模糊的更新可用性"。
- 我们的痛点：壳/核心版本分裂是我们多次事故的总根源（适配线闸、0.1.6-alpha.1 自升级、profile 插件代际错位重装……），版本闸本质上是版本分裂的补丁。
- 落点：升级器（`install.rs` 的升级流）把"允许升级到的 dsh 版本"由壳 release 清单给定（而不是任意 next tag）；CI/打包脚本加断言。仓库已在朝这走（`ce4679e` next tag、`dbf13f2` 基线 rc.2），差的最后一步是把"清单外版本拒绝"做成硬门禁。
- 优先级：**高**。

#### 4. 升级/重装后清理 webview 派生缓存（no-store 语义）
- 官方实现：插件 bundle 响应标 `no-store`，README 原话——per-launch revision 只会在 Chromium 磁盘缓存里堆积。核心升级后壳负责清派生数据。
- 我们的痛点：9-24 的 cookie/缓存连环惨案（cookie jar 140 条 → 431；聚合 bundle 缓存与版本错位）；v0.1.67 上游也在远程路径加了"连接前清空 webview 浏览数据"。
- 落点：核心变更自愈（`refresh_profile_plugins_if_core_changed`）成功后，壳对 webview 执行一次派生数据清理（Tauri 侧清 webview cache，保留 cookie 白名单或一并清）；长期看推动 dsh 上游给 `/plugins/` 响应加 `no-store`。
- 优先级：**高**。

#### 5. 单实例锁的获取时机纪律
- 官方实现：单实例锁**在任何 profile 访问之前**获取——"两个桌面进程会在同一 profile 上竞速"。
- 我们的现状：已有 `tauri-plugin-single-instance`，但 9 月中出现过数据目录里旧副本 exe 与正式安装并存互杀的乱象（孤儿击杀日志连环）。值得核对：锁是否在所有 home/profile 访问（包括自愈、套件安装器）之前获取；多安装形态（安装版/便携包）下锁的身份是否一致。
- 优先级：中高（审计型任务，半天）。

### 第二档：我们没有的新能力

#### 6. CLI 注册管理（Manage dsh Command…）
- 官方实现：菜单项管理 `dsh` 命令注册——macOS 装 `/usr/local/bin/dsh`（需要时请求管理员），Windows 注册进**当前用户 PATH**（保留既有 PATH 条目、报告更高优先级的既有命令、切换前确认、Repair/Remove 语义），CLI 在应用关闭后仍可用，版本跟随 Desktop release。
- 我们的现状：便携 runtime 里已有 `node/dsh`、`dsh.cmd`、`dsh.ps1` shim（0.1.7 新布局自带），但没有注册进用户 PATH 的管理逻辑。这是"应用关了也能用 dsh"的自然延伸，对依赖 dsh CLI 的工作流（我们就有）价值大。
- 落点：新增 `cli_register.rs`（HKCU Environment PATH 编辑 + 优先级检测），设置窗或托盘菜单挂管理入口。

#### 7. Primary runtime：捆绑 Python/Node/pnpm/wheels 离线首用展开
- 官方实现：随应用携带 python-build-standalone + 锁定 wheel 集（numpy/pandas/python-docx/python-pptx/openpyxl/Pillow/lxml/XlsxWriter）+ office-skills，`load_workspace_dependencies` 首用时离线展开到 `$DSH_HOME/dsh-runtimes/`，`runtime.json` 记 payloadDigest，按内容身份复用/原子替换；office 三技能默认注册，离线可用。
- 我们的痛点：暂无 Python 运行时；若数字分身/办公技能场景需要，这就是参考实现（构建期 lock.json 锁 URL+sha256、身份摘要、失败保留旧安装）。
- 优先级：中（按产品路线决定）。

#### 8. 构建期物化 dsh 依赖树（消灭"启动时安装"故障类）
- 官方实现：`app.asar/dsh` 携带完整生产依赖树（签名 tarball 集 + 捆绑 pnpm `--frozen-lockfile` 物化），**启动期零网络安装**；profile 只装外部插件。决策理由：核心安装发生在启动期会在离线时加活。
- 我们的痛点：镜像完整性自愈、`ERR_PNPM_NO_MATCHING_VERSION`、离线首装失败、运行时下载卡壳……全属这一类。官方的答案不是"装得更稳"而是"根本不在启动期装"。
- 落点：便携分发（U盘包）场景下预物化 `node/node_modules` 并带 `desktop-runtime.json` 式哈希清单 + 启动校验；npm 安装只留给核心升级路径。与现有 `pin-runtime.mjs` 体系兼容。
- 优先级：中高（对分发可靠性是结构性改善）。

#### 9. 更新调度与强制更新策略
- 官方实现：三段式状态机（check/download/install 全用户授权、绑定精确版本号）；轮询 10 分钟 + 指数退避上限 1 小时 + ±20% jitter，focus/电源恢复也触发；HTTP 空闲超时防静默挂死；**服务端强制更新**（`40005` 阻断 + 白名单跳转页 + 失败保留已知阻断）；差分包（blockmap）；JSONL 更新审计日志（字段白名单、跨版本留存）。
- 我们的痛点：tauri updater 裸用（dialog:false），曾出"升级打断会话"的体验问题；无调度、无差分（tauri updater 本身支持）、无强制更新通道。
- 落点：`updater` 接入 `updater` 插件的 `onBeforeExit` 钩子 + 自研调度模块；强制更新策略对接自己的发布清单。
- 优先级：中。

#### 10. Host 停机握手与 stderr 环形缓冲
- 官方实现：`shutdown` IPC → 10s SIGTERM → 5s SIGKILL → `shutdown-complete` 握手，未经请求的 shutdown 视为故障；stderr 尾部 64KiB 环形缓冲随崩溃报告带走。
- 我们的痛点：守护区分崩溃靠 restart 计数与日志 tail；升级/重启时的"优雅停机"无法验证。
- 落点：`supervisor.rs` 的 stop 路径加 stdin/IPC 握手（若 dsh 0.2 支持）+ stderr 环形缓冲并入 rescue.json。
- 优先级：中（依赖 dsh 核心配合，可推动上游）。

### 第三档：视需求/随平台计划

- **登录 shell 环境读取**（macOS/Linux：`<shell> -ilc 'env -0'` 带超时回退）——做 mac 包才需要。
- **webview guest 租约/分区隔离模型**（lease + per-workspace partition + 网络过滤）——若做"工作区内嵌浏览器"再参考。
- **快捷键系统**（chord、`keybindings.json`、IME/录制态输入保护、更新覆盖层阻断转发）——有重度键盘用户需求再做。
- **安装器工程**（自定义 NSIS 页面/进度 DLL/7-Zip 解压/失败报告、PE 批量扫描、双平台签名缓存）——签名缓存思路（内容寻址免硬件 token）在任何引入签名的时刻都值得抄；其余随商业分发需求。

---

## 三、我们已有、官方没有的（差异化优势，无需抄）

- **远程实例管理**：多远程（remotes.json + TOFU）、御符账号、回环反代凭证注入——官方只有账号 OAuth 与 Platform 嵌入视图，没有"壳连接远程 dsh 实例"的形态。
- **数字分身套件一键安装**：双通道 + 进度浮层 + 装前快照回滚 + pnpm 自备——官方无对应物。
- **Rescue Agent / 活鲸覆盖层**（v0.1.68）：LLM 参与的自适应救援——官方刻意保持恢复对话零 LLM；两者结合（官方式先自愈 → LLM 兜底）是最优形态。
- **版本闸（适配线预检）**：官方靠单一更新单元不存在此问题；我们的闸在多版本并存生态里是必要的护栏。
- **固定端口 + launcher.json、托盘通知未读、jumplist、双语托盘菜单**等桌面细节。
- **开机自启**（autostart 插件）——官方未实现。

---

## 四、官方设计原则（建议原文收进我们的设计文档）

1. **单一签名更新单元**：壳 API、Web 客户端、后端、插件图作为一个组合整体qualification；独立版本 = 未测试组合 + 模糊的更新可用性。
2. **构建期物化，运行期零安装**：核心依赖树启动期安装会在离线时加活。
3. **状态所有权**：CLI 与桌面共享 `$DSH_HOME` 产品数据，但**绝不**共享可执行依赖图、插件激活、锁文件、node_modules；桌面独占自己的 profile。
4. **退出是握手不是命令**：先问后端会打断什么，再决定怎么退。
5. **先报告再对话**：崩溃先落可审计的报告（有界、白名单、保留上限），再给用户恢复选项。
6. **部分导入不可重复**：迁移先改名再逐段导入，失败的段留在改名文件里并留证——避免每次启动重复半截迁移。（这条我们 9-24 的 settings.yaml.imported 事故正好用得上：恢复设置的正确姿势是把 .imported 段落回填进 profile 补丁，而不是删掉改名文件反复触发迁移。）

---

## 五、官方实现清单要点（浓缩备查）

- 进程模型：Electron 主进程 → RunAsNode Host 子进程（`ELECTRON_RUN_AS_NODE=1 --expose-internals`，跑 `dsh-desktop-host` profile runner，固定端口 19387）→ 主窗口 `dsh-app://app` 自定义协议（静态服务 + 认证反代，cookie 归主进程，plugin bundle `no-store`）。
- 运行时：构建期准备 Electron 分发 / 捆绑 pnpm / primary runtime（Python+Node+pnpm+wheels，lock.json 锁 sha256，payloadDigest 身份）/ dsh 闭包 tarball 物化；`desktop-runtime.json` 全量文件哈希清单，打包后/签名后/smoke 前后反复校验。
- 更新：electron-updater（COS/generic + 差分）+ 三段式状态机 + 调度退避 + 强制更新策略（40005）+ JSONL 审计；安装失败自动重启 Host 恢复工作区。
- 安装器：NSIS per-user 强制 + 自定义页面/进度 DLL/7-Zip 解压/失败报告 + 按完整路径检测运行中进程 + 硬件 token 签名 + 内容寻址签名缓存（免 token 恢复已签字节）+ mac 公证。
- 安全：全窗口 sandbox + contextIsolation；webview guest 租约/分区/网络过滤；凭证只在 Host 侧 `.credentials.yaml`（环境变量 > 文件 > .env），渲染层只见布尔；Platform token 走私有 IPC 不进渲染层。
- 测试：~140 specs + C++ 安装器测试 + 真实 updater 本地资格流程 + 打包 run 证据（events.jsonl + fatal.json，secret redactor）。
- 参考：`apps/desktop/README.md`（含 Key technical decisions 表）、`.agents/notes/implemented/…`（ADR）、`apps/desktop-host/src/index.ts`（Host 侧）。

## 六、行动建议清单（按序）

1. `supervisor.rs`：实现任务检查（inspect）接入退出与升级路径（第一档 #1）。
2. 守护重启耗尽 → 恢复对话 + 崩溃报告落盘；附带"禁用第三方插件"自愈（第一档 #2）。
3. 升级器硬门禁：dsh 目标版本必须在壳 release 清单内（第一档 #3）。
4. 核心变更自愈成功后清理 webview 派生缓存；推动上游 `/plugins/` no-store（第一档 #4）。
5. 审计单实例锁获取时机与多安装形态一致性（第一档 #5）。
6. 新增 CLI 注册管理（第二档 #6）。
7. 便携分发预物化 + 哈希清单校验（第二档 #8）。
8. 更新调度/差分/强制更新通道（第二档 #9）。
9. Host 停机握手推动上游（第二档 #10）。
10. primary runtime / 快捷键 / 安装器工程按产品节奏（第二档 #7、第三档）。
