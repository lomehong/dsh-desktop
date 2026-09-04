# dsh-desktop 远程连接（阶段3）账号化升级·设计文档

日期：2026-09-04 · 状态：**已批准，实施中**（联调三确认项已由御符侧源码核实，见文末）· 上游契约：`2026-08-29-remote-account-upgrade-review.md`（§七定案、§八落地回执）、`2026-09-04-instance-address-report.md`

## 目标

托盘「连接远程实例」升级为账号化流：系统浏览器浑天 SSO 登录 → 内存持 JWT →
拉 owner 实例清单（实时存活徽标）→ 点实例换实例级 token → 既有直连链路零改动。
配对码入口常驻降级。v1 单选切换。

## 已锁定的上游契约（御符侧已上线/已定 spec）

| 契约 | 形状 | 状态 |
|---|---|---|
| SSO 入口 | `GET http://172.20.10.91:18085/api/v1/auth/huntian/sso-login?redirect=<url编码回调>` | 上线（桌面 302 实测探明） |
| 回调白名单 | `http://127.0.0.1:18499/cb`（+ localhost:18499） | 上线（项 4） |
| JWT 验签 | `POST /api/v1/auth/sso-verify {jwt, hostname}` → 200 `{ok,uid,usr,rol}` / 403 / 502 | 上线（项 3，桌面空 body 实测 400 `VERIFY_BAD_REQUEST`）——**dsh-remote 调用，桌面不直接用** |
| 实例清单 | `GET /api/v1/me/instances` → `{instances:[{instanceId,deviceId,name,address?,hostname,ownerUserId,agentCount,lastSeenAt,createdAt}]}`；address=null 表示未上报网关地址 | 上线（项 2） |
| code 兑换 | `POST /api/v1/auth/sso/code-exchange {code, redirect_uri}` → 200 `{token,token_type:"Bearer",expires_in}` / 400 `INVALID_CODE` / 410 `CODE_EXPIRED` | spec 已定，端点本周落地（项 5） |

dsh-remote 侧（我们仓库）新增契约：`POST /__remote/exchange` body `{jwt}` →
200 `{ok:true, token, deviceId, name}`（**与现配对响应同形状**）；
401/403（含 ownership_mismatch）→ 桌面映射中文错误。dsh-remote 收到后调
御符 sso-verify `{jwt, hostname=自身}`，200 即签实例级短 TTL token（≤24h，
随刷新重取）。

## 架构

```
AppState 增 sso_jwt: Mutex<Option<String>>   ← 永不落盘（评审 ①1 修正案）
登录：托盘/清单窗「用御符登录」→ 系统浏览器开 SSO 入口（redirect=回环回调）
      → 本壳 loopback 监听 127.0.0.1:18499（固定端口，白名单匹配；占用即明确报错）
回调双形态（feature 同体，运行时按请求形状自动分派）：
  fragment 形态（项 5 落地前）：GET /cb → 200 极简 HTML（内联 JS 读 location.hash
    的 token → fetch POST /cb/token {token} → 显示「登录成功，可关闭此页」）
    ——fragment 不会到达服务端，必须由回调页 JS 回交；这是唯一能收 fragment 的办法
  code 形态（项 5 落地后切换）：GET /cb?code=… → 壳直接 POST code-exchange
    {code, redirect_uri:"http://127.0.0.1:18499/cb"} → JWT 入内存；
    400/410 →「登录已超时或已使用，请重试」
清单：GET /api/v1/me/instances（鉴权头待联调确认，暂按 Authorization: Bearer）
      → 独立最小窗（Q4）：登录态+清单（存活徽标）+连接/切换+逐实例清凭据+配对入口
存活：readiness 探活 http://<address>/ ——无实例 token 时网关回 401 亦=存活
      （需 readiness 支持「401=可达」；实现时核对现 http_ok 仅认 200 的分支）
连接：address=null 的实例只显名不可连（「未开启远程访问」）；有 address →
      TOFU 首连确认（地址+指纹→固定）→ POST /__remote/exchange {jwt} →
      实例 token DPAPI 加密入 remotes.json（加密失败拒存）→ 升活动 remote.json +
      restart_by_mode → 既有探活/回环反代/事件订阅链路零改动
降级：配对码入口常驻（Q5）；401 清单失效 → 清 JWT 回「用御符登录」（Q2 静默续期不做）
```

## 组件改动

- `src-tauri/src/sso.rs`（新）：loopback 回调监听（复用 remote_proxy.rs 的裸
  TcpStream 风格，零新依赖）、fragment/code 双形态分派、JWT 内存存取、
  浏览器拉起（现有 open 逻辑复用）、code-exchange 调用（remote.rs 同款 http_post_json）
- `src-tauri/src/instances.rs`（新）：/me/instances 拉取与解析、readiness 并发探活
- `src-tauri/src/remote.rs`：exchange 调用（`POST /__remote/exchange`）、
  remotes.json DPAPI 存取扩展（沿用现 DPAPI 实现；加密失败拒存）
- `src-tauri/src/main.rs`：新命令（全 caller_is_local）：`sso_login`、`get_instances`、
  `connect_instance(instanceId)`（TOFU 确认 + exchange + 升活动）、
  `forget_instance(instanceId)`、`pairing_fallback`（现 connect_remote 保留）
- `src-tauri/src/tray.rs`：「连接远程实例…」改拉独立清单窗（Tauri WebviewWindow，
  非 harness 页、无导航守卫负担）
- `ui/instances.html`（新，独立窗页面）：登录态/清单/徽标/连接按钮/清凭据/配对降级入口

## 测试

- 单测：回调请求行解析（fragment/code/未知路径/坏请求容忍）、code-exchange
  状态映射（200/400/410/超时）、instances JSON 解析（address null 容忍）、
  exchange 状态映射、401=存活的探活判定
- 冒烟：本地模式路径不回归（`--quit-after-secs`）
- 真机验收（需用户配合登录浑天 SSO）：登录→清单出 3 实例→点实例（TOFU 确认）
  →进入远程 UI→重启按实例直连→逐实例清凭据→配对码降级→错误 JWT→401 回登录
- 前置：dsh-remote 仓库落地 `/__remote/exchange`（调 sso-verify 已上线的 18085）

## 里程碑

M1 sso.rs 回调双形态（fragment 先行可用）→ M2 instances 拉取+探活+最小窗 →
M3 exchange+TOFU+DPAPI 入库+切换 → M4 项 5 落地后切 code 形态 → M5 真机验收+README。

## 待定/联调项 → 已全部确认（2026-09-04 御符侧源码核实）

1. fragment 回调键名：**`#token=<urlencoded JWT>`**（sso.go `redirect + "#token=" + url.QueryEscape(localToken)`）；回调页 JS 按 `#token=` 解析 + `decodeURIComponent`。键名不改（御衡前端/opencode 适配器已按此消费）。
2. /me/instances 鉴权头：**`Authorization: Bearer <SSO JWT>`**（对方端到端实测 200 `{instances:[...]}`）。
3. sso-login redirect：**接受完整外部 URL**（isAllowedRedirect 按 host 对白名单精确匹配，scheme 限 http/https；`127.0.0.1:18499` 已在白名单），兼容站内相对路径。
4. dsh-remote exchange 端点：M3 前置，本仓配合排期。
