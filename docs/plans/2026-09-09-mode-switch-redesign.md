# dsh-desktop 模式切换（多环境）·重新设计

日期：2026-09-09 · 状态：**已被 `2026-09-09-role-environments-design.md` 吸收**（本稿贡献隔离机制与 staging/promote 初始化，产品框架由角色化设计取代）· 上游需求：`E:\code\nodejs\dsh\需求文档-dsh-desktop-模式切换.md`

本稿不沿用需求文档的「方案 A / 方案 B」二分，而是按实现方对 dsh 源码的实证重新设计。
结论先说：**需求文档的两个方案都建立在一条已被证伪的假设上**——「AGENTS.md 与 `.agent-presets`
只能按 DSH_HOME 隔离」。实测 dsh 源码后，这两处都可以**按 profile 配置重定向**，因此不必在
「默认环境脏」与「整套 home 分家」之间二选一。

## 一、为什么推翻原方案的 A/B 二分

### 1.1 方案 A 的补救自相矛盾（不是"改变合集预期行为"，而是砍掉合集功能）

需求文档 §4-A 提出：初始化红队模式前先放一个中性 `AGENTS.md` 占位，以阻止合集写入 home 级指令。

但合集的 `AGENTS.md` 是 `dsh-refusal-guard` 插件的**全局指令兜底**（合集 README「运行时插件」表
明确写着 `dsh-refusal-guard` = "反拒绝，AGENTS.md 兜底"）。方案 A 下红队模式与默认模式**共用同一个
home**，即两边都读同一个 `<home>/AGENTS.md`：

- 占位文件是中性内容 → 红队模式读到中性内容 → **红队的反拒绝指令兜底失效**（砍掉目标环境的功能）；
- 占位文件放合集内容 → 默认模式读到红队指令 → 正是要避免的泄漏。

两个方向都不成立。方案 A + 补救 = 拿红队模式的功能换默认模式的干净。

### 1.2 方案 B 的代价付错了地方

方案 B（每模式一个 home）确实隔离彻底，但它**连不该隔离的一起隔离了**：凭据、设置、会话、
工作区、附件全部分家 → 红队模式里要重新填 API Key、重新配置、看不到既有会话。而真正需要隔离的
只有两项：**宿主面插件**（profile 已解决）与**全局指令文件**（配置可解决）。为一个 40 行文本文件
和九个预设目录，让用户重配一整套凭据与设置，性价比不成立。

### 1.3 原方案共同的错误假设

| 假设（需求文档 §2.2） | 实测结论 |
|---|---|
| AGENTS.md 是 home 级文件，只能靠 home 隔离 | **可配置**：`agent-instructions` 行的 `config.dshHome` 决定全局指令目录（§2.3） |
| `.agent-presets` 是 home 级目录，所有 profile 共享 | **可扩展 + 可前置**：`agent-presets` 行的 `config.roots` 排在 home 根之前，读与写都优先落自定义根（§2.4） |

## 二、实证基础（本次核实，附证据位置）

| # | 机制 | 证据 | 对设计的含义 |
|---|---|---|---|
| 1 | `DSH_HOME` 是 home 解析主旋钮 | `dsh-home-paths/lib/index.js`: 优先级「显式配置 > `$DSH_HOME` > `~/.dsh`」 | 壳已控制该 env（supervisor.rs:393），单 home 共享是默认事实 |
| 2 | `--profile <name>` 启动任意具名 profile；`--from-default-profile <template>` 用随包模板**初始化**缺失 profile | `dsh/lib/bin.js`:85（`--from-default-profile` 选项）、help L34-41 | 自定义 profile 可以 web 形态启动：`dsh --profile redteam --from-default-profile web --no-open --port N` |
| 3 | **`--from-default-profile` 不幂等** | `profile-boot-Dk-7KqJc.js`:142-158 `initializeProfileFromDefault`：目标目录存在即 `throw`（"already exists; omit --from-default-profile"） | 壳必须**先探测** `profiles/<name>/package.json` 存在与否，再决定是否携带该 flag；否则第二次启动直接失败 |
| 4 | `desktop` 为保留 profile 名（大小写不敏感） | `bin.js`:28-29 `rejectElectronProfile` | 模式 profile 命名需避开（`redteam` 可用） |
| 5 | 全局指令文件 = `<config.dshHome>/AGENTS.md` | `dsh-agent-instructions/lib/index.js`:141 `USER_GLOBAL_FILE="AGENTS.md"`、:561 `join(config.dshHome, USER_GLOBAL_FILE)`；:26/:69 配置项 `dshHome` | **可按 profile 把全局指令目录指到模式目录** |
| 6 | 覆盖 `dshHome` 不破坏作用域键 | 同文件 :148：displayPath 为 `$DSH_HOME/AGENTS.md` 即判定 user-global；`dsh-home-paths`:94 非默认 home 一律显示为 `$DSH_HOME` | 重定向后 user-global 指令仍正常装载与对账，不会"装了却不认" |
| 7 | 预设发现根 = 随包 system 根 → `...config.roots` → `<home>/.agent-presets` | `dsh-agent-presets/lib/index.js`:1300-1308 `resolvedRoots`；:1242-1244 `roots: z.array(z.object({path, trust}))` | 可**追加模式目录为更高优先级根**；home 根始终被扫描（无法移除）→ 只要不把红队预设放进 home 根，默认模式就看不到 |
| 8 | 写路径 = 第一个 `trust:"user"` 根 | 同文件 :482-486 `writableRoot` | 我们的模式根排在 home 根之前且标 `user` → 合集管理器**部署/删除预设会直接落进模式目录**（无需事后搬运） |
| 9 | 补丁层栈：bundle 层 → profile 层 → **home 层** → `--patch` 覆盖 | `profile-boot-Dk-7KqJc.js`:213-219 / :305-310 | 模式化配置写 `profiles/<profile>/cordis.patch.yml`（profile 私有）；home 层仍为全局共享层（已知边界，见 §6.2） |
| 10 | 合集安装 = `dsh plugin --profile <name> add github:SeaOf0/dsh-redteam-model`；管理器装卸宿主面插件后需重启 dsh | 合集 README「安装」节 | 与 FR-2「切换即重启」同构；profile 级依赖，装前备份失败恢复 |
| 11 | 合集 AGENTS.md 写入策略：**仅当不存在时**写入 `<home>/AGENTS.md` | 合集 README「安装」节末段 | 安装后壳做一次性"归置"即可，不必与合集对抗 |

## 三、新设计：模式作用域（mode-scoped）资产重定向

### 3.1 一句话

**单 DSH_HOME 共享全部用户态（会话/凭据/设置/工作区），每个模式一个 dsh profile 隔离插件面，
再用 profile 级 patch 把两个 home 级泄漏面（全局指令、预设根）重定向到该模式的资产目录。**

### 3.2 隔离矩阵

| 面 | 归属 | 手段 |
|---|---|---|
| 宿主面插件（17 个，工具拦截/逐轮注入等） | **按模式隔离** | profile 级依赖（`profiles/<profile>/package.json` + bundle patch） |
| 全局指令 `AGENTS.md` | **按模式隔离** | profile patch 覆盖 `agent-instructions.config.dshHome` → 模式目录 |
| 模式预设（9 个安全模式） | **按模式隔离** | profile patch 追加 `agent-presets.config.roots` → 模式目录（读写都落这里） |
| 会话 / 工作区 / 附件 | 共享 | home 级（切模式后会话仍在，符合用户直觉） |
| 凭据 / 设置 | 共享 | home 级（不必重填 API Key、不重配） |
| 壳自建预设（persona / 数字分身） | 共享 | `<home>/.agent-presets`（两个模式都可见，符合预期） |

### 3.3 目录布局

```
<runtime_root>/
├── home/                              # 共享 home（现状不变）
│   ├── .agent-presets/                # 仅"共享预设"（壳自建 + 用户自建）
│   ├── AGENTS.md                      # 默认模式的全局指令（通常不存在）
│   ├── profiles/
│   │   ├── web/                       # 默认模式（现状，不写任何 patch）
│   │   └── redteam/                   # 红队模式
│   │       ├── package.json           # 合集依赖声明（dsh plugin add 写入）
│   │       ├── cordis.patch.yml       # ★ 模式 patch：重定向指令与预设根
│   │       └── node_modules/          # 17 个插件
│   ├── sessions/  storages/  attachments/   # 共享用户态
│   └── ...
├── modes/
│   └── redteam/
│       ├── home/AGENTS.md             # ★ 红队全局指令（默认模式读不到）
│       └── agent-presets/<9 个模式>/  # ★ 红队预设（默认模式不扫这个根）
└── modes.json                          # 壳侧模式表（数据驱动，含默认模式条目）
```

### 3.4 模式 patch 内容（初始化时由壳写入，合并式，保留用户已有行）

`<home>/profiles/redteam/cordis.patch.yml`：

```yaml
# dsh-desktop 模式作用域：本 profile 的全局指令与预设根落在模式目录，
# 与默认模式（profiles/web）完全隔离。由壳生成，段内内容会被壳重写。
- id: agent-instructions
  config:
    dshHome: <runtime_root>/modes/redteam/home
- id: agent-presets
  config:
    roots:
      - path: <runtime_root>/modes/redteam/agent-presets
        trust: user
```

### 3.5 默认模式零改动论证（FR-5）

- 默认模式（`web` profile）**不写任何 patch**，启动参数与现状逐字相同；
- `<home>/.agent-presets` 与 `<home>/AGENTS.md` 里**不出现**任何红队资产（安装期归置保证，§4.2）；
- 合集插件只装进 `profiles/redteam/`，默认模式不加载；
- 因此 AC-3「切回默认后五项全否、行为无差异」由构造保证，而非靠事后检查。

## 四、切换与初始化

### 4.1 模式表（FR-1，数据驱动）

`modes.json`（壳配置，与 `mode.txt`/`launcher.json` 同级；缺省 = 只有默认模式，即现网行为）：

```json
{
  "current": "default",
  "modes": [
    { "id": "default", "label": "默认模式", "profile": "web", "init": null },
    { "id": "redteam", "label": "红队模式", "profile": "redteam", "init": "redteam-v1",
      "assets": "modes/redteam", "confirm": "该模式包含安全测试样本与工具拦截插件，仅限授权场景" }
  ]
}
```

托盘子菜单由该表生成（`MenuEntry::Submenu` + 每项 `Check`，当前模式打勾）；新增模式 = 加一条
JSON 记录 + 一个 init 配方，**不改 if/else 链**。远程模式下子菜单整项隐藏（沿用 D2b 先例）。

### 4.2 首次初始化：staging profile 原子化（FR-3 的强化实现）

需求文档要求「失败不得留下半初始化状态」。本设计不依赖"安装命令幂等"这一较弱保证，而是用
**staging + 提升（promote）** 让失败态天然不存在：

```
① 选模式 → FlowGate 闸锁（与 install_runtime / D2b 共用）
② 服务已停？否 → 优雅停止 dsh（现有 stop 路径，孤儿 pid 识别照旧）
③ 建 staging：profiles/redteam-init ── dsh --profile redteam-init --from-default-profile web --dump-config
   （只初始化不启动；dump-config 退化为"创建并输出合成树"的现成路径）
④ 写 staging 的 cordis.patch.yml（§3.4，dshHome/roots 指向 modes/redteam/）
⑤ 装插件：dsh plugin --profile redteam-init add github:SeaOf0/dsh-redteam-model
⑥ 触发合集部署（Redteam Manager 或 deploy.mjs）→ 预设与 AGENTS.md 按 §3.4 的根落位
⑦ 安装期归置（幂等补偿，§二.11）：
   - 若 <home>/AGENTS.md 出现 → 移到 modes/redteam/home/AGENTS.md（仅当模式目录尚无）
   - 若 <home>/.agent-presets 出现合集预设 id → 移到 modes/redteam/agent-presets/
   - 记录 modes/redteam/install-receipt.json（合集版本、时间、来源）
⑧ 健康门：以 staging profile 启动 → 解析 stdout URL 行 + HTTP 200 → 判 PASS
⑨ 提升：停 staging → 重命名 profiles/redteam-init → profiles/redteam → modes.json.current = redteam
⑩ 启动正式实例 → 就绪 → 托盘/窗口状态更新
```

**任一步失败**：杀掉被拉起的进程 → 删除 `profiles/redteam-init/` → 删除 `modes/redteam/` 半成品 →
`modes.json.current` 保持不变（一直是上一个可用模式）→ 系统通知给出失败原因（stderr 尾部 N 行，
复用既有摘要模式）。**上一模式全程未被触碰**，因此"回退"不是一次操作，而是从未离开。

### 4.3 日常切换（FR-2/FR-4/FR-7）

```
托盘点选 → FlowGate → 优雅停止 → 以 modes.json 该项的 profile 启动 → 就绪 → 更新托盘勾选与角标
```
- 持久化：`modes.json.current`（与 `fixed_port` 同在 settings 层；`mode.txt` 继续负责本地/远程维度）；
- 失败回退：本轮启动在预算内未就绪 → 自动以 `default` 再起一次并通知「已回退默认模式」；
- 连续失败计数落 `modes.json`（`failures`），达阈值后托盘该模式标灰并提示重装，避免每次开机抖动；
- 现有 `refresh_profile_plugins_if_core_changed()` 按 `home/profiles/*` 目录扫描，红队 profile **自动**
  纳入自愈范围（无需改名单逻辑），仅需确认自愈只清 `node_modules` 不动模式资产目录（资产在
  `modes/` 下，天然安全）。

### 4.4 复用既有机制（不新造轮子）

| 需要 | 复用 |
|---|---|
| 互斥/排队 | `AppState.restarting` FlowGate（D2b/install 已在用） |
| 停/起/等就绪 | `supervisor` 现有拉起点与 URL 行解析、readiness 探活 |
| profile 依赖补装 | `install::install_profile_plugins` / `profile_names()` |
| 核心版本变化自愈 | `refresh_profile_plugins_if_core_changed` |
| 本地/远程切换范式 | `supervisor::restart_by_mode` |
| 子进程日志留证 | `runtime::open_log_append` + stderr 尾部摘要（install.rs 既有模式） |
| 原生确认/进度 | 托盘状态文案 + 系统通知（`status::set/fail`） |

## 五、与需求文档 FR/AC 的映射

| 条目 | 本设计如何满足 |
|---|---|
| FR-1 数据驱动子菜单 | `modes.json` 生成；当前模式 Check 态 |
| FR-2 选中即切换 + 状态反馈 | §4.3；状态文案走 `status::set`，托盘提示同步 |
| FR-3 首次初始化 + 失败不留半成品 | §4.2 staging/promote；**强于**文档要求（失败态不存在） |
| FR-4 持久化 | `modes.json.current` |
| FR-5 默认模式零回归 | §3.5 构造保证 |
| FR-6 安装版 + 便携版 | 模式资产路径一律由 `runtime::runtime_root()` 派生（两形态现有差异已收敛在该函数） |
| FR-7 异常回退 + 孤儿/自愈不破 | §4.3 回退与计数；孤儿 pid 与 profile 自愈路径沿用现网逻辑 |
| FR-8（P2）跳转列表 / 设置页入口 | 后续接 `jumplist.rs` 与设置页；本设计不阻塞 |
| AC-1~AC-7 | 由 §六验证清单逐条覆盖（AC-2 的五项为红队模式健康门的真机部分；AC-3 由 §3.5 保证） |

## 六、边界与残留问题

### 6.1 待需求方确认（仅两项）

1. **Q-隔离**：接受「共享会话/凭据/设置，隔离插件与指令」（本设计，推荐）；还是坚持连会话/凭据
   也按模式分家（则退回方案 B 的代价）？——**推荐前者**。
2. **Q-来源**：红队合集 v1 走 `github:` 在线拉取（安装命令原生支持、可重跑）；离线包留待后续。
   另需接受：合集含真实攻防样本文件，需在目标机杀软处放行（壳不做完整性校验，以免误杀即失败）。

### 6.2 已知边界（不阻塞，写进文档与 UI 提示）

- **home 层 patch 仍是共享层**：`<home>/cordis.patch.yml` 按 dsh 设计对所有 profile 生效（"machine-local
  preferences"，见 profile-boot :222-227）。壳不写该文件；若用户日后往里写红队专属行，会横向影响默认模式。
- **`<home>/.agent-presets` 始终被扫描**：这是 dsh 硬编码的用户根（agent-presets :1307），无法配置移除。
  本设计通过"红队预设不落这里"实现隔离；若合集将来强制写入该目录，需回到 §4.2⑦ 的归置补偿。
- **会话跨模式可见**：红队会话在默认模式的会话列表里可见（同一 home）。这是共享 home 的必然结果，
  也是本设计有意选择（会话连续性）；如需隐藏需 dsh 侧支持按 profile 过滤会话，属上游议题。
- **红队模式与远程模式正交**：远程模式下面向本地 dsh 的模式切换无意义 → 子菜单隐藏（同 D2b）。

### 6.3 与工作区未提交改动的关系

`src-tauri/` 当前有未提交的 D2b（数字分身一键安装）改动，与本设计同触 `tray.rs` / `i18n.rs` /
`install.rs`，且已提供 FlowGate、文件夹选择器、外部脚本编排的现成范式。建议：**先落 D2b，再落本设计**，
或两者合并为一次提交（模式表与托盘菜单一次性成型）。

## 七、实施计划（文件级）

| 文件 | 改动 |
|---|---|
| `src-tauri/src/modes.rs`（新） | 模式表读写（`modes.json`）、模式 patch 生成（§3.4）、资产目录布局、install-receipt |
| `src-tauri/src/modes_init.rs`（新） | §4.2 staging/promote 状态机 + 幂等归置 + 健康门 + 补偿回滚 |
| `supervisor.rs` | spawn 加 profile 维度（两个分支：便携/安装）；`profile_for_current_mode()`；启动失败回退 |
| `tray.rs` | 「模式」子菜单（数据驱动）+ 事件分发 + 远程模式隐藏 |
| `settings.rs` | `modes.json` 持久化（与 launcher.json 同层） |
| `install.rs` | 复用 `install_profile_plugins` 支持指定 profile；红队合集安装编排调用 |
| `i18n.rs` | 模式名/进度/错误文案（中英） |
| `README.md` | 「模式切换」一节 + 隔离语义说明 |
| 测试 | 模式表 JSON 往返、patch 生成幂等与合并、staging 失败补偿（无残留）、profile 参数拼装（首启带 flag / 二次不带）、回退计数阈值、i18n 键完整性 |

真机验证（便携 + 安装各一遍）：AC-1~AC-7；重点 ① 默认模式零回归（对拍切换前后 `dsh web` 启动参数
与 home 内容）② 首次安装断网/失败后无残留且原模式照常 ③ 红队模式 AC-2 五项生效、切回后五项全否。

工作量粗估：实现 2-3 天，双形态真机验证 1-2 天（与需求文档估计同量级，但隔离强度更高、用户态零割裂）。
