# DSH Desktop

DeepSeek Harness（dsh）的桌面应用：Tauri 2 原生窗口 + 受监督的 Harness 子进程。

## 架构

与社区先例（dataelement/dsh-desktop、majiayu000/dsh-desk）一致的成熟路线：

- **进程模型**：壳进程拉起 `dsh web --no-open --port 0`（OS 分配随机回环端口），从子进程 stdout 的 `dsh web: http://127.0.0.1:<port>` 行解析实际地址，再做真实 HTTP GET 健康检查后才把窗口导航过去。异常退出自动重启（上限 3 次），退出应用时整树击杀（`taskkill /T` / 进程组信号），不留孤儿 node。
- **WebView 加固**：程序化建窗挂导航守卫——只放行本地加载页与**当前** Harness origin（随机端口，重启后自动更新放行目标；前缀匹配校验边界字符防端口伪装）；其余 http(s) 交给系统浏览器。Harness 页面**零 Tauri IPC 授权**（capabilities 仅 `local: true` 给加载页；自定义命令在命令层校验调用方 URL）。
- **原生集成（服务端→壳单向）**：壳以非浏览器客户端身份订阅 `<base>/api/events.mux` WebSocket 下行流（世代号防止重启后重复通知），`turn/end` / `approval/requested` / `question/requested` → OS 原生通知 + 任务栏闪烁（窗口未聚焦时弹通知）。
- **运行时管理**：优先使用 `%LOCALAPPDATA%\dsh-desktop-app-data\node` 便携运行时（Node 24 + 固定版本 dsh；旧 `dsh-desktop` 目录自动迁移，避免与 NSIS 卸载目录冲突），否则回退系统 `node`/`dsh`；全新机器在加载页一键「安装运行环境」（npmmirror 镜像下载 Node → npm 装固定版 dsh → 自动启动）。托盘可「升级 DSH」。
- **桌面语义**：关闭=最小化到托盘（IM 渠道/长任务不中断）、托盘菜单（显示/重启/升级 DSH/检查应用更新/日志/数据目录/开机自启/退出）、单实例二次启动聚焦。
- **无边框窗口**：decorum 悬浮标题栏（Windows 去原生边框 + 扁平自绘最小化/最大化/关闭按钮，保留 Snap Layout；macOS Overlay 红绿灯）。Harness 页面经初始化脚本整体下移 40px 让出顶栏——用 `body transform` 而非 `html padding`，使 fixed/absolute 定位的插件 overlay（右侧按钮簇、机器人状态栏）一并下移；decorum 悬浮条反向平移回顶部。Harness 页面（本地回环或已配对远程 origin）经 `http://*:* ` 通配 capability 仅授予窗口控制最小权限集（无文件/系统访问；能加载的页面由导航守卫约束）。关闭按钮走 `CloseRequested` → 语义仍为最小化到托盘。
- **DSH_HOME**：沿用应用进程环境（默认共享 `~/.dsh`，保留 im-bot 绑定/共享记忆等用户数据）。

## 远程连接

桌面壳可作为 [dsh-remote](https://github.com/lomehong/dsh-remote) 网关的客户端连接远程机器上的 dsh：配对后原生窗口直接呈现远端 Harness 页面，托盘本地/远程随时互切。

**前置**：远端部署 dsh-remote ≥ 0.1.1，服务器防火墙放行网关端口。配对码在远端 dsh 设置页「远程访问」Tab 生成；服务器无浏览器时可直接调 API 生成：

```bash
curl -X POST http://127.0.0.1:3080/dsh-remote/api/pairing
```

**配对流程**：托盘「连接远程实例…」→ 加载页连接屏输入地址 + 配对码（或直接粘贴整条配对链接，自动拆填）→ 配对成功即导航到远程实例。

**模式记忆**：壳保存上次模式（`mode.txt`），重启按上次模式直连；托盘「断开远程，回到本地」保留凭据，随时可再连；远程连接失败进入错误态，提供 重试 / 修改远程配置 / 回到本地模式 三个出口。

**原生集成**：事件流经网关带 `x-remote-token` 凭证订阅——回合完成/审批请求的原生通知、任务栏闪烁在远程模式同样工作。

**凭据文件**：`runtime_root/remote.json`（地址 + token）与 `mode.txt` 同目录，便携模式在 Data/ 内随U盘走。

**安全边界（简述）**：webview 只受导航守卫约束（仅放行已配对 origin）；capability 仅窗口控制；自定义命令对 harness 页面零开放；凭据明文存 `remote.json`（与 dsh 会话密钥同威胁模型）；仅支持 http（网关不带 TLS）；一次只连一个远程实例。

**已知限制**：远端 dsh ≥ 0.1.2 的 remote.mux-over-gateway 暂不支持（上游发布后适配）；远端 dsh 当前应为 0.1.1-rc.x（旧 events.mux 协议）。
**模式角标**：窗口左上角常驻小徽标显示「本地」或「远程 · 地址」，托盘提示同样带实例地址，一眼分清当前连接。代理对本机应答 `/__remote/badge` 供角标查询。

**安全上下文**：页面经壳内本地回环反向代理加载（origin 为 `127.0.0.1:<随机端口>`）——dsh 视为本机浏览器（模型/设置完整可用），回环天然是安全上下文；代理仅绑回环、自动注入凭证。

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

- macOS：代码与 CI 就绪，但无实机验收记录；首次 macOS CI 运行后可能需要微调（GUI 冒烟对 runner 会话有依赖）。Linux 有路径代码与发行版矩阵，未出 CI 产物。
- 同一会话不要在网页版与桌面版并发发消息（回合会交错写入同一会话流）。
- `升级 DSH` 作用于便携运行时；使用系统 `dsh` 回退启动时不升级系统安装。
- 远程模式通知事件流已带壳内退避重连守护（0.1.17：2s→30s 封顶持续重试，模式切换/重连即时让位）；页面断连仍需手动「重连远程实例」或错误态「重试」。

## 安全边界

- Harness 页面仅获窗口控制最小权限（最小化/最大化/关闭/拖拽，`http://*:* ` 通配；能加载的页面由导航守卫约束为已配对 origin），无文件/Shell/系统访问；原生集成通过壳自身订阅事件流实现（本地或经远程网关带凭证）。
- 导航白名单外的地址一律外抛系统浏览器；`file://` 等协议导航直接拒绝。
- 已知取舍：无边框模式下 tooltip 会随 body 下移约 40px（fixed 定位副作用），属可接受的显示偏差。
