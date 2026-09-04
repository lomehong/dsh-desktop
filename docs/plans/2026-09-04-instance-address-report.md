# 远程实例 address 权威来源·上报设计（验签形态 B 配套）

日期：2026-09-04 · 状态：定案（用户批准） · 关联：`2026-08-29-remote-account-upgrade-review.md`、与 OpenCode@clawith-test 的契约往来

## 问题

评审终审契约定了 registry 条目 `{instanceId, address, owner, name}`，桌面拿 `address`
做 exchange 与后续直连链路（探活/回环反代/WS）。验签形态已拍板 **B（sso-verify 内省端点）**：
桌面直连 dsh-remote 不变、数据面不过御符 gateway。于是 address 必须是**可直连的
dsh-remote 网关 authority（host:port）**——而御符两侧数据源都没有它：

- `ai_agents`：hostname / ip_address（心跳 IP，无端口、非网关地址）/ owner_user_id / device_info
- `system_registry`：业务系统注册表（erm/isrm/yuyi），无设备语义

心跳 IP 不能充当 address：连不上、没端口、还可能漂移。

## 定案：dsh-yuyi（yuyi 通道）上报，dsh-remote 只暴露事实

**结论：上报实现在 yuyi 通道（dsh-yuyi 插件/宿主内置 yuyi 客户端）的心跳里；
dsh-remote 不直连御符、不持任何御符凭证，只负责把网关地址暴露为宿主内可读状态。**

```
dsh-remote 插件                yuyi 通道                  御符
 网关绑定端口成功 ──写宿主内状态──▶ 心跳 payload 增补          ──▶ ai_agents.device_info
 {address, enabled}              remoteGateway 字段(透传)        upsert remote_gateway_address
                                                                     │
                                        /me/instances ◀──────────────┘
                                        address = remote_gateway_address（可 null）
```

### 为什么不是 dsh-remote 直报

1. **凭证面**：dsh-remote 的威胁模型是「配对设备=完全主人」，让它揣一张能上报任意
   hostname 数据的御符凭证，等于给「清单投毒→导航恶意 origin」新增一条通路（评审风险表
   已有此条，TOFU+origin 边界兜底——但不该主动扩面）。
2. **重复建设**：心跳/重连/限速/离线语义 yuyi 通道全有；dsh-remote 直报要在御符侧
   加接收端点，服务端工作量 +1。
3. **存活语义脱钩**：dsh-remote 只在远程访问开启时活跃。它自己心跳，「设备在线但未开
   远程」与「设备离线」无法区分；yuyi 心跳与设备存活同源，`ai_agents.updated_at`
   语义不破。

### 代价与兜底

- **一致性时差**：address 变更（端口重选）随下个心跳生效（分钟级）。桌面侧既有
  readiness 探活 + TOFU 首连确认兜底：陈旧地址只会探活失败报「实例不可达（地址可能
  已变更）」，不会误连；降级入口复用现配对码连接屏手填。
- **宿主内读状态**：dsh 宿主有插件体系（`plugin-*.js`）。若已有插件状态共享/心跳
  扩展机制则直接复用；否则约定状态文件
  `<dsh-home>/plugins/dsh-remote/gateway-state.json`（原子写，损坏容忍）。

## 契约

### 1. dsh-remote 暴露状态（事实产生者）

网关绑定成功后写：`{ "address": "<host>:<port>", "enabled": true, "startedAt": <ms> }`；
关闭远程访问/停用时写 `{ "enabled": false }`。`address` = 对外 authority，host 取
非回环网卡地址（与配对链接同一套计算逻辑，复用）。

### 2. yuyi 心跳增补（通道，只透传不解释）

payload 增加可选字段：

```json
"remoteGateway": { "address": "192.168.1.146:3090", "enabled": true }
```

读不到状态/未装 dsh-remote → 字段整体缺省（≠ enabled:false，区分「未安装」与「已停用」）。

### 3. 御符侧（项 2 落地时顺带）

- 心跳处理：`device_info` 内 upsert `remote_gateway_address`（JSON 字段，零 schema 迁移）。
- 校验：host:port 格式、host 非回环；**不做可达性验证**（存活由桌面 readiness 实时
  探活，符合评审 Q1「存活不入库」）。

### 4. /me/instances 输出（修正对方草案 b 的 address 字段）

| 字段 | 来源 | 说明 |
|---|---|---|
| instanceId | hostname 派生稳定 id | 路线①聚合 |
| deviceId / name / hostname | ai_agents.hostname | 同前 |
| **address** | **device_info.remote_gateway_address** | 可 null；null → 桌面标「未开启远程访问」，仅显名不可连 |
| ownerUserId | ai_agents.owner_user_id | 过滤 uid==owner |
| agentCount / lastSeenAt / createdAt | 聚合 | 附加字段照对方草案 |

## sso-verify 契约（形态 B 的配套端点，一并定案）

owner 等值校验**上收御符服务端**，dsh-remote 零 owner 配置：

```
POST /api/v1/auth/sso-verify
body: { "jwt": "<SSO JWT>", "hostname": "<dsh-remote 自报主机名>" }
200 { "ok": true, "uid": "...", "usr": "...", "rol": "..." }
403 { "ok": false }   // jwt 无效/过期 ‖ uid ≠ owner(hostname)
```

- 御符侧一跳完成「验签 + uid==owner(hostname)」（owner 取 ai_agents.owner_user_id）。
- dsh-remote 只信 verify 的 200，不自持 jwtSecret、不自持 owner。
- 必须**限速**（dsh-remote 会调它，防枚举/重放）；JWT 校验 exp，短 TTL。
- hostname 由 dsh-remote 自报 + owner 比对同源校验：自报错 hostname 只会 403
  （自己 deny 自己），不构成越权面。

## v1/v2 分期对齐

- **v1**（本周可落）：项 4 redirect 白名单 + 项 2 /me/instances（含 address 修正，
  心跳字段未上报前 address=null 不阻塞清单展示）+ 项 3 sso-verify（上述契约）+
  项 5 loopback 回调 code 端点。
- **v2**：路线② system_registry instance 语义落地时，上报端（remoteGateway 字段）
  不变，仅御符落库目标从 device_info 换成 registry 行——上报契约一次定型。
- 项 1（owner 归属列 + 存量迁移）：随②后置；v1 权属过滤走 ai_agents.owner_user_id。
