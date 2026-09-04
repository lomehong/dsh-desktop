# 远程实例连接·账号化升级——评审主席终审建议书

基线澄清（实测）：反代路线 B 已实现并经用户批准（设计文档附注），红队引用的路线 A 为过时正文；remote.json DPAPI 加密已存在（安装版；便携明文/加密失败回退为已声明威胁模型）；remotes.json 多实例存储骨架已就绪。评审以此为准。

## 一、六问定案

| 问 | 定案 | 采纳/修正 | 备选 | 服务端档位 |
|---|---|---|---|---|
| Q1 | system_registry 为权属唯一源，存活不入库、桌面用现成 readiness 实时探活 | 御符师；红队⑤9 采纳，"结合"否决（常驻同步=过度设计） | 网关设备表（违 C3，不推荐） | 录 3 台=加配置；若注册表查询端点无 owner 过滤=加端点 |
| Q2 | 系统浏览器 SSO + 127.0.0.1 loopback 回调；JWT 仅驻内存不落盘 | 安治；webview 否决（独立 cookie 栈反复输密）；静默续期 v1 不做，401→重登，登出=删凭证 | RFC 8628（现签 agent-{hex} 违 P4，需服务端改签用户 JWT=大改） | gateway redirect 白名单加一条=加配置 |
| Q3 | 登录即连：点实例→dsh-remote 新端点验 SSO JWT（HS256 验签）+校验 uid==owner→签实例级短 TTL token，形状同现配对 token，执行层零改动 | 御符师/SRE；逐实例授权动作否决（单 owner 是演出）；但**首连 TOFU 确认保留**（红队①2 修正）：首连示地址指纹→确认→固定，此为安全确认非授权动作 | 完全静默首连（不推荐） | dsh-remote 加端点 |
| Q4 | 最小窗：登录态+清单（存活徽标）+连接/切换+逐实例清凭据+手动配对入口常驻；关窗不断连，生命周期归主进程 | SRE；完整面板否决；红队③5 采纳但修正：逐实例清凭据属 v1 必要最小管理，不砍 | 完整管理面板（v2+） | 零 |
| Q5 | 降级保留：凭证保持实例级、清单标「未纳管/本地配对」视觉区分；禁止内置常驻后门 token | 接入专家；红队①3 采纳 | v2 废弃 | 零（UI 标注） |
| Q6 | v1 单选切换：remotes.json 多实例存储已在，切换=升活动 remote.json+restart_by_mode（代码已有） | SRE/UX 冲突裁 SRE；多连否决（扩攻击面+事件多路复用成本，3 台无收益） | 多实例并连（v2 按真实需求再评估） | 零 |

## 二、红队裁决逐条落实

①1 **采纳+修正**：JWT 换短 TTL 实例凭证、JWT 不作长连凭据、TLS 兜底，全部采纳；修正其前提——DPAPI 已实现，真正要钉死的是「加密失败回退明文」对全域凭据不可接受：JWT 永不落盘，失败即拒存。①2 采纳（TOFU+服务端 owner 等值校验为唯一越权防线，桌面校验与 RBAC 目录不算数）。①3 采纳。②4 采纳（RFC 8628 v1 否决）。③5 采纳（Q4 修正见上）。③6 采纳。④7 采纳（登录非闸门，18085 不可达仍可手动配对，P6）。④8 采纳。⑤9 六处冲突全部采纳为本表终案；裁决理由：权属必须过 P4（御符师胜）、cookie 栈体验硬伤（安治胜）、单 owner 下授权动作与双源同步均为纯成本（SRE/御符师胜）、执行层单插槽是已验收事实（SRE 胜）。

## 三、整体方案

托盘「连接远程实例」弹独立窗口：未登录显「用御符登录」→系统浏览器开 172.20.10.91:18085 浑天 SSO→重定向 `http://127.0.0.1:<随机端口>/cb` 带凭据→桌面内存持 JWT→GET agent-backend system_registry 按 owner=uid 拉实例清单、readiness 实时标存活→点实例→`POST dsh-remote /__remote/exchange`（携 JWT）→网关验签+uid==owner 校验→签发实例级短 TTL token（DPAPI 加密入 remotes.json、升活动 remote.json）→既有探活/回环反代/事件订阅链路零改动。配对码入口常驻为降级。契约：registry 条目 `{instanceId, address, owner, name}`；exchange 响应与现 token 同形状。

## 四、MVP 分期

- **v1**：loopback 登录、清单+探活、exchange 换凭证+TOFU 首连、最小窗、降级保留、单选切换。
- **v2**：JWT 续期/TTL 策略、RFC 8628 跨设备登录（需服务端改签用户 JWT）、多实例并连评估、注册表条目管理 UI。
- **不做**：内嵌 webview SSO；网关设备表作权属源；registry↔网关同步作业；完整管理面板；JWT 落盘；内置后门 token；v1 静默续期。

## 五、需用户决策（各含推荐）

1. 权属源：**system_registry 唯一源（推荐）** vs 网关设备表。
2. 登录形态：**系统浏览器+loopback（推荐）** vs webview。
3. 首连 TOFU 确认：**保留（推荐）** vs 完全静默。
4. 配对码流：**降级保留（推荐）** vs v2 废弃。
5. gateway TLS：**本期限期上 TLS（推荐，加配置档）** vs 靠短 TTL 凭证缓解。
6. exchange token TTL：**≤24h（推荐，随刷新重取）** vs 与现配对 token 同长期。

## 六、风险表

| 风险 | 缓解 | 负责平面 |
|---|---|---|
| JWT 明文过 http 被 LAN 嗅探 | JWT 不落盘+秒级换短 TTL 实例凭证；上 TLS（决策 5） | 御符/安全 |
| 实例未录 registry/条目过期 | 清单标「未注册」并引导配对码降级 | 御符/SRE |
| 清单投毒→导航恶意 origin | TOFU 首连确认+导航守卫 origin 边界不变 | 桌面/安全 |
| 加密失败回退明文（便携） | JWT 永不落盘；token 明文为已声明威胁模型，文档钉死 | 桌面 |
| 18085 不可达死锁 | 登录非闸门，手动配对常驻 | UX/SRE |
| HS256 密钥分发 dsh-remote | 验签非签发不违单一事实源；或网关回调 agent-backend 验 | 架构/御符 |

---

## 七、定案附记（2026-09-04，御符侧 5 项调研回执后用户拍板）

1. **实例数据源**：路线①先行——v1 按 `ai_agents.hostname` 聚合出实例清单；路线②
   （system_registry 加 instance 语义）后置为 v2 正式形态。项 1（owner 列 + 存量
   迁移）随②后置，v1 权属过滤走 ai_agents.owner_user_id。
2. **验签形态：B（sso-verify 内省端点）**。否决 gateway 反代注入，理由：不让常驻
   数据面流量过 gateway（避免新增转发单点）；数据面保持桌面↔dsh-remote 直连。
   owner 等值校验上收御符：`POST /api/v1/auth/sso-verify {jwt, hostname}` 服务端
   一跳完成验签 + uid==owner(hostname)，dsh-remote 零密钥零 owner 配置，端点限速。
   拓扑事实（用户确认）：gateway 172.20.10.91 与三台设备同内网/VPN 均可达（B 不依赖，
   备案供未来演进）。
3. **address 权威来源（本表契约 `{instanceId,address,owner,name}` 的 address 修正）**：
   心跳 IP 不可充当 address。定案：dsh-remote 把网关 authority 暴露为宿主内状态，
   **yuyi 通道心跳增补 remoteGateway 字段透传** → ai_agents.device_info.upsert
   `remote_gateway_address` → /me/instances 输出；dsh-remote 不持御符凭证不直报。
   详见 `2026-09-04-instance-address-report.md`。
4. 御符侧落地面重排：项 4（redirect 白名单 18499）→ 项 2（/me/instances 含 address
   修正，address 未上报前可 null）→ 项 3（sso-verify 按上述契约）→ 项 5（loopback
   回调 code→JWT 兑换，需提供回调参数与兑换端点 spec 给桌面）。

## 八、御符侧落地回执（2026-09-04，commit b489a06 已部署，桌面实测核验）

- **项 4 ✅** redirect 白名单 `127.0.0.1:18499` + `localhost:18499`（gateway 重启生效）。
- **项 3 ✅ sso-verify 上线**（形态 B 契约全兑现）：验签在网关（仅网关持 jwtSecret）；
  owner 回源 `GET /internal/owner-by-hostname`（内部 HMAC，InternalOnly）；verifyLimiter
  限速；502 仅后端不可达。桌面实测：空 body → 400 `VERIFY_BAD_REQUEST` ✅；
  对方实测含 ownership_mismatch 403 / 未知 hostname 403。
- **项 2 ✅ /me/instances 上线**：`GET /api/v1/me/instances`，响应
  `{instances:[{instanceId:"inst-<hostname>", deviceId, name, address(可 null),
  hostname, ownerUserId, agentCount, lastSeenAt, createdAt}]}`。桌面实测：未带凭证
  → 302 `/api/v1/auth/huntian/sso-login?redirect=%2Fapi%2Fv1%2Fme%2Finstances`
  ——**SSO authorize 入口形状探明**：`/api/v1/auth/huntian/sso-login?redirect=<url>`，
  配合项 4 白名单，桌面浏览器入口即
  `…/sso-login?redirect=http%3A%2F%2F127.0.0.1%3A18499%2Fcb`。
- **心跳增补 ✅** `UpdateDeviceInfo` 的 device_info 改**深合并**（key 级覆盖）——
  dsh-remote 上报 `remote_gateway_address` 与御驿 Hub 运行环境字段共存；格式校验
  由上报方（yuyi 链路）负责（host:port + 非回环，见本目录 2026-09-04 文档）。
- **项 5 spec（端点本周落地，桌面先行 fragment 形态）**：
  `POST /api/v1/auth/sso/code-exchange` body `{code, redirect_uri}`
  → 200 `{token, token_type:"Bearer", expires_in}`；400 `INVALID_CODE`；
  410 `CODE_EXPIRED`（TTL 60s、单次用后即焚、redirect_uri 与签发一致防挪用）。
- **待御符侧确认的桌面联调项**：① fragment 回调的键名（`#token=`?）；
  ② /me/instances 的 API 鉴权头形态（`Authorization: Bearer`?）；
  ③ sso-login 的 redirect 参数是否接受完整外部 URL（回环回调）而非仅站内路径。
