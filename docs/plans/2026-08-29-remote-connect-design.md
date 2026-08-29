# dsh-desktop 远程连接（阶段2）设计文档

日期：2026-08-29 · 状态：已批准（路线 A：直连导航 + capability 通配放宽；导航守卫为唯一页面边界）

## 背景与目标

dsh-remote 插件（阶段1，已真机验收）在服务端建立了带配对认证的网关。本阶段改造
dsh-desktop（v0.1.16），使其不仅能管理本地 dsh，还能**作为客户端连接远程 dsh**：
配对 → 导航到远程 origin → 原生通知订阅远程事件流 → 托盘本地/远程切换。

对接契约（已在阶段1真机验收实测）：

- `POST http://<gateway>/__remote/pair` body `{"code":"..."}` → 200
  `{"ok":true,"token":"...","deviceId":"...","name":"..."}`；403 码无效/已过期；429 限速
- 后续一切请求（含 WS upgrade）带 `x-remote-token: <token>` 头；浏览器路径用 cookie
- 网关把全部路径反代到回环 dsh；`/api/events.mux`（dsh 0.1.1-rc.2 旧协议）透传已实测 101

## 已定决策（用户确认）

1. **信任架构 = 路线 A**：直连导航到远程 origin + `remote-harness.json` capability
   urls 放宽 `http://*:*`。边界不变的理由：拦截陌生页面的是**导航守卫**（只放行
   已配对 origin），capability 只是"已加载页面可用 IPC"；放宽的权限仅窗口控制 +
   decorum，自定义命令全部被 `caller_is_local` 拦截。否决 B（Rust 本地回环反代，
   实现量大+多一跳）与 C（远程页面零 IPC、隐藏标题栏，UX 降级）。
2. **连接 UX 与模式记忆**：托盘「连接远程实例…」→ 加载页连接屏（地址 + 配对码，
   支持粘贴整条配对链接自动拆填）→ 配对成功存凭据并导航；保存「上次模式」，下次
   启动按上次模式直连；远程失败错误态提供「重试 / 修改远程配置 / 回到本地模式」；
   托盘常驻互切；v1 只存一份远程配置（重新配对即覆盖）。
3. v1 事件协议按当前实况（远端 dsh 0.1.1-rc.2 → 旧协议 events.mux + 网关凭证）；
   **远端 dsh ≥0.1.2 的 remote.mux-over-gateway 列为已知限制**，上游发布后单独适配。

## 架构

```
AppState 增加 mode: Mutex<Mode>          Mode::Local | Mode::Remote
远程模式：不拉本地子进程、不写 pid 登记；watch_child 空转；退出清理空操作
启动序列：清孤儿 → 分身向导(便携) → 读上次模式 → start_service() | connect_remote_flow()
凭据文件 remote.json（与 runtime.pid 同目录，便携随U盘）：
  { address, origin, token, pairedAt }   明文，与 dsh 会话密钥同威胁模型
导航守卫：复用 origin: Mutex<Option<String>>（same_origin 边界检查零修改）
事件流：WS 握手请求头附 x-remote-token；跳过本地 token→cookie 交换；
        代际号/通知/闪栏/「壳是通知方不是审批方」全部不变
就绪探测：wait_http_ok 支持附加头，远程 GET <origin>/ 带 token，200 即就绪
```

## 组件改动

- `src-tauri/src/remote.rs`（新）：凭据存取（remote.json 原子写/损坏容忍）、address
  与配对链接解析（只许 http、裸 host:port）、配对 HTTP 调用（POST /__remote/pair，
  403/429/超时映射中文错误）、连通性探测
- `src-tauri/src/main.rs`：AppState.mode；新命令 `connect_remote(address, code)`、
  `get_remote_config`、`disconnect_remote`、`use_local_mode`（全部 caller_is_local）；
  启动序列按上次模式分叉
- `src-tauri/src/supervisor.rs`：`connect_remote_flow`（就绪探测 → origin 入 allowlist →
  events::spawn(带凭据) → navigate）；`disconnect_remote`（杀本地不适用，置空 origin →
  回加载页 → 走本地 start_service）；托盘「重启服务」按模式分派
- `src-tauri/src/events.rs`：spawn 增加可选 token 参数；WS 握手请求头注入；
  远程模式跳过 cookie 交换
- `src-tauri/src/readiness.rs`：wait_http_ok 支持附加请求头
- `src-tauri/src/webview.rs`：TITLEBAR_INSET_CSS 的 hostname 守卫放宽为「任意带端口的
  http 页面」（能加载的页面只有守卫放行的 origin）
- `src-tauri/capabilities/remote-harness.json`：urls → `http://*:*`
- `ui/index.html`：连接屏（#connect，与向导同风格）；错误态按钮扩展
  （重试 / 修改远程配置 / 回到本地模式）
- `src-tauri/src/tray.rs`：按模式动态构建菜单（连接远程实例… / 断开远程，回到本地）

## 数据流

配对：加载页连接屏 → `connect_remote` → 解析 address/code → POST /__remote/pair →
存 remote.json → connect_remote_flow：GET <origin>/ 带 token 探活 → origin 入 allowlist →
events::spawn(origin, token) → navigate_to_harness(origin + `/`，无 token 参数)。

启动：读上次模式（本地默认）→ 本地走现有序列（零改动）；远程读 remote.json →
connect_remote_flow（跳过配对）。

切换：托盘互切 → 杀本地子进程或置空 origin → 回加载页 → 走对方序列。

## 错误处理

配对 403 →「配对码无效或已过期」；429 →「尝试过于频繁」；连接超时/拒绝 →「无法连接
远程实例 <address>」；探活超时 →「远程实例未就绪」。远程模式运行中事件流断开只影响
通知（静默重试由 spawn 的世代机制在重连时接管），不触发本地重启。

## 安全边界与已知取舍

- 导航守卫是唯一页面边界；capability 放宽不构成攻击面（能加载的只有用户亲手配对的网关）
- 自定义命令对远程页面零开放（caller_is_local 已有，逐命令保留）
- 凭据明文落盘（与 dsh 宿主同威胁模型）；只支持 http（网关不带 TLS 是阶段1决策）
- 一次只配一个远程实例；断开保留凭据；远端 dsh ≥0.1.2 remote.mux-over-gateway 延后

## 测试

Rust 单测：address/配对链接解析（含非法输入拒绝）、remote.json 读写/损坏容忍、
模式状态机、事件流请求头构造。`--quit-after-secs` 冒烟（本地路径）保持不回归。
手动验收：真机对 192.168.1.146:3090 配对 → 完整 UI + 远端回合触发原生通知 →
切回本地 → 重启按上次模式直连 → 断网看错误态 → 吊销后 401 → 错误态按钮。

## 里程碑

M1 remote.rs（凭据/解析/配对调用）→ M2 模式状态机 + 远程启动序列 + 命令层 →
M3 导航/capability/顶栏样式 → M4 事件流带凭据 + 探测头 → M5 加载页连接屏 + 托盘 →
M6 README + CI + 真机验收。
