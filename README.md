# DSH Desktop

DeepSeek Harness（dsh）的桌面应用：Tauri 2 原生窗口 + 受监督的 Harness 子进程。

## 架构

与社区先例（dataelement/dsh-desktop、majiayu000/dsh-desk）一致的成熟路线：

- **进程模型**：壳进程拉起 `dsh web --no-open --port 0`（OS 分配随机回环端口），从子进程 stdout 的 `dsh web: http://127.0.0.1:<port>` 行解析实际地址，再做真实 HTTP GET 健康检查后才把窗口导航过去。异常退出自动重启（上限 3 次），退出应用时整树击杀（`taskkill /T` / 进程组信号），不留孤儿 node。
- **WebView 加固**：程序化建窗挂导航守卫——只放行本地加载页与**当前** Harness origin（随机端口，重启后自动更新放行目标；前缀匹配校验边界字符防端口伪装）；其余 http(s) 交给系统浏览器。Harness 页面**零 Tauri IPC 授权**（capabilities 仅 `local: true` 给加载页；自定义命令在命令层校验调用方 URL）。
- **原生集成（服务端→壳单向）**：壳以非浏览器客户端身份订阅 `<base>/api/events.mux` WebSocket 下行流（世代号防止重启后重复通知），`turn/end` / `approval/requested` / `question/requested` → OS 原生通知 + 任务栏闪烁（窗口未聚焦时弹通知）。
- **运行时管理**：优先使用 `%LOCALAPPDATA%\dsh-desktop\node` 便携运行时（Node 24 + 固定版本 dsh），否则回退系统 `node`/`dsh`；全新机器在加载页一键「安装运行环境」（npmmirror 镜像下载 Node → npm 装固定版 dsh → 自动启动）。托盘可「升级 DSH」。
- **桌面语义**：关闭=最小化到托盘（IM 渠道/长任务不中断）、托盘菜单（显示/重启/升级 DSH/检查应用更新/日志/数据目录/开机自启/退出）、单实例二次启动聚焦。
- **DSH_HOME**：沿用应用进程环境（默认共享 `~/.dsh`，保留 im-bot 绑定/共享记忆等用户数据）。

## 开发

```powershell
cd src-tauri
cargo run                              # 调试运行（前端为 ui/ 静态加载页）
cargo build --release
```

调试期冒烟：`cargo run -- --quit-after-secs 60`（到时走真实退出路径，含整树清理）。

环境变量（可选）：`DSH_DESKTOP_NODE_MIRROR`（自定义 Node 镜像前缀）、`DSH_DESKTOP_NPM_REGISTRY`（npm 源）、`DSH_HOME`（传给子进程）。

## 里程碑状态

- ✅ M1 骨架与监督：随机端口 / URL 解析 / HTTP 就绪 / 守护重启 / 优雅退出无孤儿（已实测验收）
- ✅ M2 运行时管理：一键安装引导 + 托盘升级（全新环境实测：下载 Node → 装 dsh → 启动进 UI）
- ✅ M3 原生集成：WS 订阅事件流 → 通知/闪栏（真实回合实测触发）
- ✅ M4 分发就绪：NSIS（`bundle.targets`）、tauri-plugin-updater（签名公钥已内置，托盘「检查应用更新」）、GitHub Actions Windows CI（构建 + 冒烟 + tag 发布）

## 发布流程

1. 仓库 Secrets 配置 `TAURI_SIGNING_PRIVATE_KEY`（本机生成的私钥内容，见下）与 `TAURI_SIGNING_PRIVATE_KEY_PASSWORD`（空密码则留空）。
2. 打 tag `v*` 触发 release 工作流（draft），产物含 NSIS 安装器与更新器签名清单。

> 更新器签名密钥对生成于本机 `F:\tmp\tauri-keys\dsh-desktop.key(.pub)`——**私钥务必备份到安全位置并从临时目录删除**；公钥已写入 `tauri.conf.json`。私钥丢失将无法再签发更新。

## 已知限制

- 强杀 exe（非托盘退出）不会执行整树清理，会留下孤儿 node；正常退出/托盘退出无此问题。后续可在启动时检测上次崩溃残留并清理。
- `升级 DSH` 固定安装编译期版本（`install.rs` 的 `DSH_VERSION`）；应用自身的更新走 updater 通道。
- macOS/Linux 路径代码已就绪但未实测（二期）。

## 安全边界

- 不给 Harness 页面任何 IPC 权限；原生集成只通过壳自身订阅本地事件流实现。
- 导航白名单外的地址一律外抛系统浏览器；`file://` 等协议导航直接拒绝。
