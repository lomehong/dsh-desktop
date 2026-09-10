# dsh-desktop 角色化控制台（role environments）·设计稿

日期：2026-09-09 · 状态：**待评审（设计稿）** · 上游需求：`E:\code\nodejs\dsh\需求文档-dsh-desktop-模式切换.md`（本稿吸收并取代其 A/B 框架；机制细节见同目录 `2026-09-09-mode-switch-redesign.md`）

需求方补充的原始诉求：**不同角色的人用同一个 dsh 控制台，应看到不同的功能面**——普通员工看到
普通工作台，安全人员看到带安全工具的工作台，开发人员看到带开发工具的工作台。本稿据此重新设计。

## 一、诉求本质：从「两套环境」到「角色环境平台」

| 维度 | 原需求文档 | 补充后的真实诉求 |
|---|---|---|
| 数量 | 默认 / 红队 两个固定模式 | **N 个角色**，可持续新增（安全、开发、运维、审计…） |
| 差异内容 | 插件集不同 | 插件集 + 预设 + **权限档位** + 默认会话模式 + 呈现（名称/角标） |
| 装配方式 | 硬编码两个分支 | **声明式角色包**（谁都能发布一个角色环境，壳只消费声明） |
| 使用者 | 同一人的两种帽子 | **可能是不同的人/不同岗位**，因此引入状态边界与治理 |
| 治理 | 无 | 谁可切到哪个角色、可锁定单角色、切换留痕 |

一句话设计目标：**壳把「角色」变成一等公民——一个角色 = 一份声明（插件/预设/权限/状态边界/
呈现），壳据此装配出一个 profile 环境并在托盘/向导里提供切换。**

## 二、实证基础：状态面**逐项可配**（全部源码核实）

这是本设计能做到「按项粒度隔离」的前提。dsh 把每一处 home 级状态都做成了 bundle 里的一行
（row），带显式配置键，且产品注释明确写着**可以在 profile 的 `cordis.patch.yml` 或 `--patch` 覆盖**。

| 状态面 | row id | 关键配置键 | 开箱默认 |
|---|---|---|---|
| 会话日志 | `session-persistence-jsonl` | `root` | `!!js dshHomePath('sessions')` |
| 会话检索索引 | `session-query-sqlite` | `path`、`openAt` | `:memory:` + `never`（默认关闭检索） |
| KV 存储 | `storage-json`（+`storage-domain`） | `root` | `dshHomePath('storages')` |
| 附件/图片 | `attachment-local` | `dshHome` | home 派生 |
| 设置 | `settings`（dsh-settings-file） | `path` 或 `dshHome` | `<home>/settings.yaml` |
| 凭据 | `credentials`（dsh-credentials-local） | `path` 或 `dshHome` | `<home>/.credentials.yaml` |
| 技能 | `skill-filesystem` | `dshHome`、`agentsHome`、`customSkillDirs`、`bundledSkillDir` | home 派生 |
| 预设 | `agent-presets` | `roots[]`（`path`+`trust`，**前置根优先读写**） | 随包根 → 自定义根 → `<home>/.agent-presets`（末项硬编码不可移除） |
| 全局指令 | `agent-instructions` | `dshHome`（全局指令 = `<dshHome>/AGENTS.md`） | `resolveDshHome()` |
| 权限档 | `permission`（dsh-permission-presets） | `presets{}` + `policy`（可用 `DSH_PERMISSION_MODE` env 驱动） | `workspace-write`/`ask` 起 |

配套的 profile 机制（同前稿，已核实）：

- `dsh --profile <name>` 启动任意具名 profile；`--from-default-profile <template>` **仅初始化缺失**
  profile，目录已存在即报错（`profile-boot` :142-158）→ 壳须先探测再决定是否携带；
- `dsh plugin --profile <name> add <pkg>`：依赖只写该 profile，装前备份、失败恢复；
- 补丁层栈：bundle 层 → **profile 层**（`profiles/<name>/cordis.patch.yml`）→ home 层 → `--patch`；
- `desktop` 为保留 profile 名。

> 结论：**「隔离什么、共享什么」不再是架构二选一，而是每个角色声明里的一列配置。**

## 三、设计总览

```
角色 = 声明（roles.json 一条）
     + profile（插件面：packages 只装进它）
     + 角色补丁（profiles/<profile>/cordis.patch.yml：把状态面按该角色的边界指向各目录）
     + 呈现（默认预设 / 角色名 / 权限档）
```

### 3.1 角色声明 `roles.json`（壳配置，缺省 = 现网单角色行为）

```json
{
  "current": "staff",
  "locked": false,
  "roles": [
    {
      "id": "staff",
      "label": "普通员工",
      "profile": "web",
      "packages": [],
      "defaultPreset": "standard",
      "permission": { "mode": "workspace-write", "approval": "ask" },
      "state": { "sessions": "shared", "workspaces": "shared", "attachments": "shared",
                 "credentials": "shared", "settings": "shared" }
    },
    {
      "id": "security",
      "label": "安全人员",
      "profile": "redteam",
      "packages": ["github:SeaOf0/dsh-redteam-model"],
      "presets": "modes/security/agent-presets",
      "instructions": "modes/security/home",
      "defaultPreset": "redteam",
      "permission": { "mode": "danger-full-access", "approval": "ask" },
      "state": { "sessions": "isolated", "workspaces": "isolated", "attachments": "shared",
                 "credentials": "shared", "settings": "isolated" },
      "gate": { "kind": "switch-code" },
      "notice": "本角色包含安全测试样本与攻防插件，仅限授权场景使用"
    },
    {
      "id": "dev",
      "label": "开发人员",
      "profile": "dev",
      "packages": [],
      "defaultPreset": "standard",
      "permission": { "mode": "workspace-write", "approval": "ask" },
      "state": { "sessions": "shared", "credentials": "shared", "settings": "shared" }
    }
  ]
}
```

要点：

- **staff / dev 可以是"零插件角色"**：dsh 自带文件/命令/子代理/工作流等开发能力，开发角色 v1
  无需新包，只声明默认预设与权限档（若日后有开发合集，加一条 `packages` 即可）；
- **security 是"合集角色"**：一条 `packages` 声明即装配 17 个宿主面插件 + Redteam Manager；
- **`state` 逐项决策**：默认「会话/凭据/设置共享，只隔离插件与指令」；不同岗位共用一台机器时把
  `sessions/workspaces` 改 `isolated` 即可做到档案互不可见——**不用改一行代码**；
- `gate`、`notice`、`label` 提供治理与呈现。

### 3.2 角色补丁（壳生成，幂等、合并式保留用户行）

以 `security` 为例，壳写入 `<home>/profiles/redteam/cordis.patch.yml`：

```yaml
# ── dsh-desktop 角色作用域（由壳生成；本段可被壳重写，段外用户内容保留）──
- id: agent-instructions            # 全局指令：只本角色可见
  config: { dshHome: <root>/modes/security/home }
- id: agent-presets                 # 预设：合集预设只落本角色根（读写都优先落这里）
  config:
    default: redteam
    roots: [ { path: <root>/modes/security/agent-presets, trust: user } ]
- id: session-persistence-jsonl     # 会话日志：按角色分家（state.sessions=isolated）
  config: { root: <root>/modes/security/state/sessions }
- id: session-query-sqlite          # 检索索引跟随同一分支
  config: { path: <root>/modes/security/state/sessions.sqlite, openAt: first-search }
- id: storage-json
  config: { root: <root>/modes/security/state/storages }
- id: settings                      # 设置按角色分家（可删：删掉即继承 home 共享）
  config: { path: <root>/modes/security/state/settings.yaml }
# credentials 不在列 → 继承 home 的共享 .credentials.yaml（无需重填 API Key）
# attachment-local 不在列 → 附件共享（图片这类无隐私边界的资产）
- id: permission                    # 权限档：本角色更宽，但仍保留 ask 与留痕
  config: { policy: !!js "process.env.DSH_ROLE_PERMISSION ?? 'ask'" }
```

**staff（默认模式）不写任何 patch**：启动参数与 home 内容逐字不变 → 原需求 FR-5「零回归」由构造保证。
`dev` 角色的 v1 补丁可以只有 `permission` 一行（甚至为空）。

### 3.3 三类边界策略（推荐默认）

| 状态面 | 推荐默认 | 理由 | 何时改变 |
|---|---|---|---|
| 宿主面插件 | **必隔离** | 工具拦截/逐轮注入/全局指令属角色功能面 | 永不共享 |
| 预设 / 全局指令 / 技能 | **必隔离** | 提示词与能力面即角色定义；跨角色泄漏会改变行为 | 永不共享 |
| 会话 / 工作区 | **按角色隔离（保守）**或共享（同人多帽子） | 不同岗位共用控制台时，会话是个人档案（隐私/合规）；同一人多角色时共享更顺手 | 由 `state` 声明 |
| 附件 / 图片 | 共享 | 无角色语义，重复占用磁盘无收益 | 由 `state` 声明 |
| 凭据 | 共享 | 避免每个角色重填 API Key | 高安全场景可隔离 |
| 设置 | 共享或隔离 | 共享省心；隔离则各角色可有各自的模型/权限偏好 | 由 `state` 声明 |

### 3.4 目录布局

```
<runtime_root>/
├── home/                     # 共享 home（默认角色 staff 就用它）
│   ├── profiles/web/         # staff（不写 patch）
│   ├── profiles/redteam/     # security（patch + node_modules）
│   ├── profiles/dev/         # dev
│   ├── sessions/  storages/  .credentials.yaml   # 共享项（按 state 声明）
│   └── .agent-presets/       # 仅"跨角色共享预设"（壳自建 persona/数字分身 + 用户自建）
├── modes/
│   ├── security/
│   │   ├── home/AGENTS.md            # 角色全局指令
│   │   ├── agent-presets/<9 模式>/   # 角色预设
│   │   └── state/{sessions,storages,settings.yaml}   # 被隔离的状态
│   └── dev/…
├── roles.json                # 角色清单（current / locked / roles[]）
└── roles-audit.log           # 切换与初始化审计
```

## 四、呈现层：角色「看得见」的四个面

1. **功能面**（自动）：plugins → 工具、设置页、右侧面板（如 attack-atlas、session-pulse 等随角色
   出现/消失）；
2. **会话面**：`agent-presets.config.default` 让新会话默认就是该角色的模式（安全角色开箱即
   `redteam`；普通员工是 `standard`）；
3. **权限面**：`permission` 行的 `presets`/`policy` 决定沙箱与审批档——普通员工可收紧到
   `workspace-write + ask`，安全角色放宽但保留留痕；
4. **标识面**：窗口标题、托盘 tooltip、顶部模式角标显示角色名（现有 MODE_BADGE 扩一个字段），
   一眼分清"我现在是哪个角色"。

## 五、首次装机：角色向导 + 单角色锁定

这是"普通员工看到的就是普通界面"的最直接实现：

1. 安装完成首启（或托盘「设置本机角色…」）→ 向导问：**这台机器给谁用？** 列出 roles.json 的角色；
2. 只装配所选角色（其他角色的 profile/资产**根本不创建**）→ 托盘只显示已装配角色；
3. 可选「**锁定为本角色**」（`locked: true`）：锁定时托盘的「角色」子菜单消失，切换需要
   管理员在设置页解锁——正好对应公司统一装机（员工机 = 普通员工角色，安全人员机 = 安全角色）。

## 六、治理与审计（不同的人共用控制台时的必需项）

- **切换门禁**：`gate` 支持 `switch-code`（一次性口令/配对码，可复用既有配对码 UI）；
  另支持部署期 allow-list（roles.json 里未列出的角色在托盘不可见）；
- **审计**：每次初始化/切换写 `roles-audit.log`（时间、from、to、结果、失败原因），并同步壳日志；
- **远程模式正交**：远程实例不提供角色切换（角色是本机环境概念）→ 子菜单隐藏（沿用 D2b 先例）；
- **合规提示**：含攻防样本的角色在向导与切换确认里明示（`notice`），并在 README 记录杀软放行要求。

## 七、切换与初始化机制（沿用并强化前稿）

- **staging + 提升**：初始化在 `profiles/<role>-init` 上完成（建 profile → 写补丁 → 装包 → 触发
  合集部署 → **归置**：把合集可能写到 home 根的 `AGENTS.md`/预设移入角色目录 → 健康门（起得来 +
  HTTP 200）→ 重命名提升 → 写 `roles.json.current`）；任一步失败即整体删除 staging，`current`
  从未改变 → **不存在半初始化状态**；
- **日常切换**：FlowGate → 优雅停止 → 以目标 profile 启动 → 就绪 → 更新角标/勾选/审计；
- **失败回退**：预算内未就绪 → 自动回退到上一可用角色 + 通知；连续失败计数达阈值后该角色标灰并
  提示重装；
- **自愈不破**：`refresh_profile_plugins_if_core_changed` 按 `home/profiles/*` 扫描 → 新角色 profile
  自动纳入；角色资产在 `modes/` 下，不受 profile 插件目录清理影响。

## 八、替代路线对比：按 OS 账号隔离

| 维度 | 本设计（单 home + 角色补丁） | 按 Windows 账号/便携包隔离 |
|---|---|---|
| 隔离强度 | 逐项可控（插件/预设/指令/会话/凭据…） | 彻底（连 OS 账号、DPAPI 都分开） |
| 切换体验 | 托盘一键，秒级 | 需切换系统账号或换便携包，重 |
| 磁盘 | 仅多插件依赖 + 角色资产 | 每角色一份 home（含依赖） |
| 适合 | 用户诉求（同一控制台切角色） | 一台机器服务多人的强合规场景 |

建议：本设计为主；文档里保留"按账号隔离"作为强合规场景的部署建议（两者不冲突）。

## 九、与需求文档 FR/AC 的映射（角色化泛化）

| 原条目 | 角色化后的满足方式 |
|---|---|
| FR-1 数据驱动模式菜单 | `roles.json` 生成「角色」子菜单 + 当前角色勾选；`locked` 时隐藏 |
| FR-2 选中即切换 + 状态反馈 | §七；状态文案/托盘/角标同步 |
| FR-3 首次初始化不留半成品 | staging/promote（强于原要求） |
| FR-4 持久化 | `roles.json.current` |
| FR-5 默认角色零回归 | 默认角色不写 patch、不建资产（构造保证） |
| FR-6 安装版 + 便携版 | 路径一律由 `runtime::runtime_root()` 派生 |
| FR-7 异常回退 + 孤儿/自愈不破 | §七回退与计数；孤儿 pid 与 profile 自愈沿用现网 |
| FR-8（P2）跳转列表/设置页入口 | 后续；本设计不阻塞 |
| AC-1~AC-7 | 按角色泛化：AC-2 的"合集五项生效"作为 **security 角色的能力自检**；AC-3 变为"切回 staff 后全部为否"；AC-7 双形态 × 角色矩阵 |

## 十、实施计划（文件级）

| 文件 | 改动 |
|---|---|
| `src-tauri/src/roles.rs`（新） | roles.json 读写/校验、角色补丁生成（§3.2）、审计日志 |
| `src-tauri/src/roles_init.rs`（新） | staging/promote 状态机、资产归置、健康门、补偿回滚、角色向导数据 |
| `supervisor.rs` | spawn 增加 profile 维度（两分支）、按角色注入 env（如 `DSH_ROLE_PERMISSION`）、失败回退 |
| `tray.rs` / `i18n.rs` | 「角色」子菜单（数据驱动）、门禁交互、中英文案、锁定态 |
| `settings.rs` | `roles.json` 持久化（与 launcher.json 同层） |
| `install.rs` | 复用 `install_profile_plugins` 支持指定 profile；角色包安装编排 |
| `webview.rs` | 模式角标扩展为「角色 · 本地/远程」 |
| `README.md` + `docs/` | 角色语义、状态边界表、杀软放行与合规说明 |
| 测试 | roles.json 往返与校验、补丁生成幂等/合并/按 state 分支、staging 失败零残留、profile 参数拼装（首启带 flag、二次不带）、回退阈值、审计落盘、i18n 键完整性 |

真机验收：便携 + 安装各一遍，覆盖 staff（零回归对拍）→ dev（零插件角色）→ security（合集五项 +
切换/回退/锁定 + 断网失败零残留）。

工作量粗估：实现 3-4 天（比固定双模式多 1 天，主要在新角色装配与治理），双形态真机验证 1-2 天。

## 十一、需要你确认的三件事

1. **状态边界默认档**：默认「会话/工作区按角色隔离、附件与凭据共享」是否合适？还是希望默认全共享
   （同人多角色的个人机）／全隔离（多人共用一台机器）？
2. **角色包来源与形态**：v1 接受 `packages: ["github:..."]` 在线拉取（安全角色即合集仓库）；
   是否需要同时支持"本地目录/离线包"（适合内网批量装机）？
3. **门禁强度**：默认不设口令（信任本机使用者）＋支持"锁定单角色"是否够？还是安全角色必须口令/配对码？
