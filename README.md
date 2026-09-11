# DSH Desktop

DeepSeek Harness（dsh）的桌面应用：Tauri 2 原生窗口 + 受监督的 Harness 子进程。

## 架构

与社区先例（dataelement/dsh-desktop、majiayu000/dsh-desk）一致的成熟路线：

- **进程模型**：壳进程拉起 `dsh web --no-open`（端口默认随机：`--port 0` 由 OS 分配，零冲突；配置页可设固定端口——`launcher.json` 持久化，被占用自动回退随机并留证日志），从子进程 stdout 的 `dsh web: http://127.0.0.1:<port>` 行解析实际地址，再做真实 HTTP GET 健康检查后才把窗口导航过去。异常退出自动重启（上限 3 次），退出应用时整树击杀（`taskkill /T` / 进程组信号），不留孤儿 node。
- **WebView 加固**：程序化建窗挂导航守卫——只放行本地加载页与**当前** Harness origin（随机端口，重启后自动更新放行目标；前缀匹配校验边界字符防端口伪装）；其余 http(s) 交给系统浏览器。Harness 页面**零 Tauri IPC 授权**（capabilities 仅 `local: true` 给加载页；自定义命令在命令层校验调用方 URL）。
- **原生集成（服务端→壳单向）**：壳按端点连通性自适应订阅事件流（双协议，不做版本判断）：dsh ≤0.1.1 走 `events.mux` 免认证 WebSocket；≥0.1.2 走 `/api/remote.mux` + 一次性 token 换签名 cookie——（世代号防止重启后重复通知），`turn/end` / `approval/requested` / `question/requested` → OS 原生通知 + 任务栏闪烁（窗口未聚焦时弹通知）。
- **运行时管理**：优先使用 `%LOCALAPPDATA%\dsh-desktop-app-data\node` 便携运行时（Node 24 + 固定版本 dsh；旧 `dsh-desktop` 目录自动迁移，避免与 NSIS 卸载目录冲突），否则回退系统 `node`/`dsh`；全新机器在加载页一键「安装运行环境」（npmmirror 镜像下载 Node → npm 装基线版本 dsh → 自动启动）；npm ≥10 拦截依赖安装脚本时自动解析拦截清单并以 `npm rebuild --allow-scripts` 补跑（保障 koffi/node-pty 等原生模块完整）。托盘「升级 DSH 运行时」= alpha/latest/next 三 tag 预发布感知择新（既有用户可升到最新预发布，基线仅约束全新安装）。
- **桌面语义**：关闭=最小化到托盘（IM 渠道/长任务不中断）、托盘菜单按「窗口 / 服务 / 模式与实例 / 数字分身 / 日志与诊断 / 设置 / 关于 / 退出」分组（2026-09-10 重组：更新类收拢服务段，双通道安装收进数字分身子菜单，日志/目录/诊断归拢，关于独立底部）、单实例二次启动聚焦。
- **无边框窗口**：decorum 保留式顶栏带（Windows 去原生边框 + 扁平自绘最小化/最大化/关闭按钮，保留 Snap Layout；macOS Overlay 红绿灯）。Harness 页面经初始化脚本让出顶部 40px 独占带：`html/body` 高度收缩 + `body transform` 平移（fixed/absolute 定位的 overlay——模式角标、dockkit 浮窗——一并下移；dsh 前端是 `html,body,#root{height:100%}` 链，收缩后正好铺满带下，底部零裁切），decorum 条带反向平移回窗口顶、整带为拖拽区、底色继承 app `--dsw-alias-bg-base` 随深浅主题。2026-09-09 由 overlay 改回让位：dsh 0.1.5+ 右侧栏 dockkit 条带专职占据窗口右上角，overlay 窗控钮与其结构性重叠（视觉上两套 ✕ 叠加，点击被劫持——点「开始」tab=最小化窗口、点侧栏收起=关窗口）。设计文档：`docs/plans/2026-09-09-titlebar-right-sidebar-collision-design.md`。Harness 页面（本地回环或已配对远程 origin）经 `http://*:* ` 通配 capability 仅授予窗口控制最小权限集（无文件/系统访问；能加载的页面由导航守卫约束）。关闭按钮走 `CloseRequested` → 语义仍为最小化到托盘。设置 / 关于窗为无边框自绘标题栏的**暖窗常驻**（`CloseRequested` 拦截为隐藏 + 启动 3 秒后台预热）——打开零渲染进程孵化（懒创建+销毁式打开在 Windows 上要孵化 WebView2 渲染进程 1~3s，杀软扫描加重）。
- **DSH_HOME 完全独立**：dsh 使用专属 home（安装版 `%LOCALAPPDATA%\dsh-desktop-app-data\home`，便携版包内 `Data\home`），profile/预设/技能/凭证/会话全在里面随包走，**与 `~/.dsh` 零依赖**——绝不读写系统 dsh/persona 的共享目录（多版本交叉污染是历史真实故障源；旧版的一次性自动迁移已删除）。分身技能目录也在 home 内（`home/skills`）。

## 远程连接

桌面壳可作为 [dsh-remote](https://github.com/lomehong/dsh-remote) 网关的客户端连接远程机器上的 dsh：配对后原生窗口直接呈现远端 Harness 页面，托盘本地/远程随时互切。

**前置**：远端部署 dsh-remote ≥ 0.1.1，服务器防火墙放行网关端口。配对码在远端 dsh 设置页「远程访问」Tab 生成；服务器无浏览器时可直接调 API 生成：

```bash
curl -X POST http://127.0.0.1:3080/dsh-remote/api/pairing
```

**配对流程（账号化）**：托盘「连接远程实例…」→ 独立控制窗御符登录（SSO）→ 名下实例清单点选直连；旧「地址 + 配对码」连接屏降级为控制窗内「手动配对」豁免入口（可直接粘贴整条配对链接自动拆填）。

**模式记忆**：壳保存上次模式（`mode.txt`），重启按上次模式直连；托盘「断开远程，回到本地」保留凭据，随时可再连；远程连接失败进入错误态，提供 重试 / 修改远程配置 / 回到本地模式 三个出口。

**原生集成**：事件流经网关带 `x-remote-token` 凭证订阅——回合完成/审批请求的原生通知、任务栏闪烁在远程模式同样工作。

**凭据文件**：`runtime_root/remote.json`（地址 + token）与 `mode.txt` 同目录，便携模式在 Data/ 内随U盘走。安装版 Windows 凭据经 DPAPI 加密（`tokenEnc`，旧明文照读并惰性迁移）；便携版与非 Windows 保持明文（威胁模型同 dsh 会话密钥）。

**安全边界（简述）**：webview 只受导航守卫约束（仅放行已配对 origin）；capability 仅窗口控制；自定义命令对 harness 页面零开放；凭据存 `remote.json`（安装版 Windows 为 DPAPI 加密的 `tokenEnc`，其余明文——与 dsh 会话密钥同威胁模型）；仅支持 http（网关不带 TLS）；一次只连一个远程实例。

**已知限制**：远端 dsh ≥ 0.1.2 的 remote.mux-over-gateway 暂不支持（上游发布后适配）；远端 dsh 当前应为 0.1.1-rc.x（旧 events.mux 协议）。
**模式角标**：窗口顶部居中常驻小徽标（住在壳的顶栏带内）显示「本地」（绿点）或「远程 · 地址」（蓝点蓝底，深浅主题高对比描边），托盘提示同样带实例地址，一眼分清当前连接。代理对本机应答 `/__remote/badge` 供角标查询。

**安全上下文**：页面经壳内本地回环反向代理加载（origin 为 `127.0.0.1:<随机端口>`）——dsh 视为本机浏览器（模型/设置完整可用），回环天然是安全上下文；代理仅绑回环、自动注入凭证。

## 数字分身套件（双通道安装）

托盘「数字分身」子菜单下两个入口，服务两类人：

| 入口 | 面向 | 安装器来源 | 形态 |
|---|---|---|---|
| 安装数字分身套件（本地调试） | 套件开发者 | 用户选的本地 meta-repo 根（路径持久化，失效自动重选） | `link:` 链接插件目录，改源码重启即生效 |
| 安装/更新数字分身套件（生产·GitHub） | 最终用户 | 运行时从 `lomehong/digital-twin` main 拉官方安装器（ghfast 镜像兜底） | 各插件仓库 GitHub Release 的构建物 tarball，目标机无需仓库与工具链 |

两通道共用同一套安全网：

- **装前快照** `profiles/web/package.json` + `pnpm-lock.yaml`（`suite-install-backup/`）；脚本失败或校验失败即回滚，绝不把半成品 manifest 留给下次启动（2026-09-10 真实事故：装完重启撞 `ERR_MODULE_NOT_FOUND` 把服务打崩）；
- **装后探针**：对清单里的套件依赖逐个用宿主 Node import 其主机侧入口（复现 dsh 启动加载路径），把「装完才发现解析断裂」拦在重启之前；失败则回滚 + 精确报错 + 不重启；
- **宿主锚定**（生产通道）：安装器把宿主已有的 `@deepseek-ai/*` 全钉到宿主版本（pnpm ≥10 的设置新家是 `pnpm-workspace.yaml`——`package.json` 的 `pnpm` 字段已不被读取，2026-09-10 实测 pnpm 12 overrides 静默失效 + 构建脚本默认拦截；overrides 与 `allowBuilds`/`onlyBuiltDependencies` 统一写 workspace yaml，pnpm 9/11/12 兼容）——dsh 0.1.x 全在预发布标签上，插件 manifest 的普通 semver 区间（如 `^0.1.2`）匹配不到任何预发布版本会直接 `ERR_PNPM_NO_MATCHING_VERSION`；锚定后解析恒成立、版本与宿主一致、pnpm 复用宿主同一 store 实体；
- **生产即更新**：tarball URL 带 `?release=<tag>`，同 Release 重跑幂等、发了新 Release 重跑即升级；缺任一插件的 Release 资产则中止不动任何状态。

双通道均支持 Windows 与 macOS：官方安装器是「bat 壳 + 内嵌 ESM」的多语言文件——Windows 走 `cmd /c`，macOS 由壳提取内嵌 JS 用便携 Node 执行同一份逻辑（首次自动准备 pnpm；local 通道的 junction 修复由 POSIX 兼容垫片承接）。官方安装器保持唯一实现，零漂移。

两通道互相切换以最后一次安装为准（安装器负责清理另一形态残留）。安装记录写 `installed-suite.json`（含 `channel`）。

## 开发

```powershell
cd src-tauri
cargo run                              # 调试运行（前端为 ui/ 静态加载页）
cargo build --release
```

调试期冒烟：`cargo run -- --quit-after-secs 60`（到时走真实退出路径，含整树清理）。

环境变量（可选）：`DSH_DESKTOP_NODE_MIRROR`（自定义 Node 镜像前缀）、`DSH_DESKTOP_NPM_REGISTRY`（npm 源）、`DSH_DESKTOP_DSH_VERSION`（固定升级目标版本）、`DSH_HOME`（传给子进程）。

命令行参数：`--quit-after-secs N`（到时走真实退出路径，CI 冒烟用）、`--upgrade-dsh`（检查并升级 DSH 后退出）。

## 里程碑状态

- ✅ M1 骨架与监督：随机端口 / URL 解析 / HTTP 就绪 / 守护重启 / 优雅退出无孤儿（已实测验收）
- ✅ M2 运行时管理：一键安装引导 + 托盘升级（全新环境实测：下载 Node → 装 dsh → 启动进 UI）
- ✅ M3 原生集成：WS 订阅事件流 → 通知/闪栏（真实回合实测触发）
- ✅ M4 分发就绪：NSIS（`bundle.targets`）、tauri-plugin-updater（签名公钥已内置，托盘「检查应用更新」）、GitHub Actions Windows CI（构建 + 冒烟 + tag 发布）

## 二期（已完成）

- ✅ **孤儿进程清理**：服务拉起后登记 `runtime.pid`（壳 pid + 子进程 pid + 端口）；下次启动发现「壳已死、子进程仍活且进程名符合 dsh 启动链」则整树击杀（已实测：强杀壳 → 孤儿残留 → 重启自动清理）。正常退出同步删除登记。
- ✅ **升级接远程清单**：托盘「升级 DSH」= 查询 npm `dist-tags.latest`（npmmirror 优先、官方源兜底）与已装版本比较，一致则跳过、不同才安装；`DSH_DESKTOP_DSH_VERSION` 可固定目标版本。`--upgrade-dsh` 命令行参数供 CI/脚本使用（升级后退出，不启动服务）。全新环境首装仍用编译期基线版本（`install.rs` 的 `DSH_VERSION`）保证可复现。
- ✅ **跨平台安装通用化**：Node 发行版按平台选择（win-x64.zip / darwin-{arm64,x64}.tar.gz / linux-{x64,arm64}.tar.xz），curl → PowerShell/wget 下载兜底，bsdtar/gnu tar 解压；npm 命令对 Unix 注入便携 node 的 PATH。macOS CI（构建 + 冒烟 + dmg/updater 发布，Apple Silicon）就绪——**实机行为待首次 CI 验证**。

## 三期（远程连接，已完成）

- ✅ **模式状态机/配对接线**：本地/远程双模式（远程不拉本地子进程），启动按上次模式分叉；配对走 dsh-remote 网关 `POST /__remote/pair` 换 token，凭据原子落盘 `remote.json`（损坏容忍）。
- ✅ **导航守卫复用 + capability 放宽**：复用 same-origin 守卫只放行已配对 origin；capability 放宽为 `http://*:*` 仅授予窗口控制最小权限集（边界仍在导航守卫，自定义命令对远程页面零开放）。
- ✅ **事件流带凭证**：WS 握手注入 `x-remote-token`，探活带 token 头；通知/闪栏/世代号机制远程模式不变。
- ✅ **连接屏与托盘互切**：加载页连接屏（地址 + 配对码 / 粘贴整条配对链接）；托盘按模式动态构建菜单，「连接远程实例…」/「断开远程，回到本地」/「重连远程实例」互切；错误态三出口（重试 / 修改远程配置 / 回到本地模式）。
- ✅ **模式盲项收编**：升级 DSH/分身向导在远程模式隐藏（都依赖本地服务流），`persona_wizard_save`/`persona_wizard_install` 路径按模式分派。

## 发布流程

1. 仓库 Secrets 配置 `TAURI_SIGNING_PRIVATE_KEY`（本机生成的私钥内容，见下）与 `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`（空密码则留空）。
2. 打 tag `v*` 触发 release 工作流（draft），产物含 NSIS 安装器与更新器签名清单。

> 更新器签名密钥对生成于本机 `F:\tmp\tauri-keys\dsh-desktop.key(.pub)`——**私钥务必备份到安全位置并从临时目录删除**；公钥已写入 `tauri.conf.json`。私钥丢失将无法再签发更新。

## 已知限制

- macOS：已有实机迭代（WebKit 垫片、套件双通道适配、401 自愈），但未做全量回归；Linux 有路径代码与发行版矩阵，未出 CI 产物。
- 同一会话不要在网页版与桌面版并发发消息（回合会交错写入同一会话流）。
- `升级 DSH` 作用于便携运行时；使用系统 `dsh` 回退启动时不升级系统安装。
- 远程模式通知事件流已带壳内退避重连守护（0.1.17：2s→30s 封顶持续重试，模式切换/重连即时让位）；页面断连仍需手动「重连远程实例」或错误态「重试」。

## 安全边界

- Harness 页面仅获窗口控制最小权限（最小化/最大化/关闭/拖拽，`http://*:* ` 通配；能加载的页面由导航守卫约束为已配对 origin），无文件/Shell/系统访问；原生集成通过壳自身订阅事件流实现（本地或经远程网关带凭证）。
- 导航白名单外的地址一律外抛系统浏览器；`file://` 等协议导航直接拒绝。
- 已知取舍：无边框模式下 tooltip 会随 body 下移约 40px（fixed 定位副作用），属可接受的显示偏差。
