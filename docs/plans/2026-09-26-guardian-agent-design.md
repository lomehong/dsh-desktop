# 壳内置守护 Agent（guardian）设计

日期：2026-09-26 · 状态：已实施（v0.1.58 开发中） · 模块：`src-tauri/src/guardian.rs` + `ui/guardian.html`

## 目标与边界

壳内常驻、**纯 Rust、独立于 dsh 运行时**的守护 Agent：

1. **保障稳定**：周期探活 + 运行日志持续分类 + 白名单修复动作（全自动，带防风暴安全阀）
2. **收集问题**：结构化问题台账落盘 `runtime_root/guardian/issues.json`（上限 200 条，tmp+rename 原子写）
3. **反馈**：守护报告窗（台账式）+ 通知中心摘要 + 托盘角标/入口；未知问题可选 LLM 诊断

不做：跨机器上报、聊天式对话 UI；远程模式只探活/重连（不守护远端服务本身）。
LLM 传输走 curl.exe 外部进程（零新依赖，对齐 install.rs 下载链路先例），key 经 `-H @file` 传入不进进程列表。

## 决策记录（用户拍板，2026-09-26）

| 决策点 | 选择 |
|---|---|
| AI 大脑 | 规则引擎 + LLM 增强（未配 key 时纯规则照常工作） |
| 修复自治 | 全自动（白名单动作直接执行，事后通知） |
| 呈现 | 守护报告窗 + 通知中心 + 托盘入口 |
| 范围 | 本地模式全套 + 远程模式网关探活/自动重连 |

## 架构

### 采集层
- **探活**：`readiness::http_reachable(origin)`（链路通即活，401 登录墙算活——远程模式 origin 是本地反代，语义统一）。连续 3 次失败立案 `service_unreachable`，恢复自动销案。
- **日志 tail**：增量读 `dsh-desktop.log`（记 offset，文件变小=轮转则重置，半行跨 tick 缓冲），逐行过规则表。
- **watch_child 终态接管**：`status.error` + child==None（自动重启放弃躺平）→ 对错误文本分类立案修复。不改 watch_child 现有语义。

### 决策层
- 规则表 `classify_line`（纯函数+单测）：`node_crash`（复用 supervisor::crash_banner）/ `profile_bundle` → RefreshProfilePlugins；`plugin_dep_missing`（本地链接插件缺依赖，壳修不了）→ 仅建议；`module_missing` / `port_in_use` → ClearFixedPort；`auth_401` → RestartService；`npm_error` → RepairRuntime。同类立案 10 分钟去抖。
- LLM 增强：规则未命中/手动触发 → 证据脱敏（`token=` 打码 + 4KB 截断）→ OpenAI 兼容 `/chat/completions` → 结构化 `{diagnosis, action, advice}`。**action 必须命中白名单（`Action::parse`）否则降为 null**——LLM 永远不能创造新动作。

### 执行层（白名单）
| 动作 | 实现 | 闸锁 |
|---|---|---|
| RestartService | `supervisor::restart_by_mode` | 自管 |
| ClearFixedPort | 清 `launcher.json` 固定端口 + restart_by_mode | 自管 |
| RefreshProfilePlugins | `install::install_profile_plugins` + 追加重启 | with_gate（内部不带锁） |
| RepairRuntime | `install::install_and_start` | 自管（install_runtime 取锁） |
| ReinstallOfficial（高风险） | `install::force_reinstall_official` + 重启 | with_gate |

**安全阀**（全自动的工程底线）：同类修复最小间隔 30 分钟（高风险 6 小时）；连续失败 3 次 → 冷却 30 分钟只记录建议；「起不来」类案子修复前先复核健康度——watch_child 已救活则销案（防自愈后补刀）；修复后探活验证（≤240s）通过才记 resolved；Agent 可关（guardian.json `enabled`，报告窗/托盘可切）。

## 反馈面

- 报告窗 `ui/guardian.html`（mini 窗，模式抄通知中心）：台账卡片（严重度色点/类别中文/结果徽章）+ 展开证据/建议 + 执行修复 / AI 诊断按钮 + LLM 配置面板；监听 `guardian-updated`。
- 立案/销案/修复失败各一条 `notifications::record` + `tray::bump_unread`。
- 托盘「窗口」组新增「守护 Agent」（本地/远程两套 spec + on_menu_event，i18n zh/en）。
- 命令（全部 caller_is_local 守卫，config 会带出 API key 故不豁免）：`guardian_state/toggle/run_once/fix/diagnose/config_load/config_save/open`。

## 隐私

- `guardian.json` 含 LLM API key：**诊断包导出（diagnostics.rs 白名单制）天然不入包**；LLM 临时请求文件用后即删；`[守护]` 日志只写结论不写 key。

## 与既有设施的耦合点（升级跟进清单）

| 依赖 | 官方/上游改动时 | 兜底 |
|---|---|---|
| `crash_banner` / `local_plugin_hint` 判定 | 已抽成 pub(crate)，本仓自有 | 无风险 |
| FlowGate 排队语义 | 本仓自有 | 无风险 |
| dsh 日志行格式（`[err]`/`[out]` 前缀） | 上游 tee 逻辑改动 | 规则匹配宽松（子串），前缀变化仅影响崩溃横幅精确度 |
| dsh 401 行为（登录墙） | 上游改认证 | auth_401 规则失效，探活不受影响 |

## 后续路线（v2 候选）

- 补跑被拦 npm 脚本动作（需把 `rerun_blocked_install_scripts` 的包名解析抽 pub）
- 远程网关直连探活（绕过本地反代）、多实例轮询
- 修复动作成功率统计面板、LLM 周报
