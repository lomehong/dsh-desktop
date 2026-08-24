# DSH Desktop

DeepSeek Harness（dsh）的桌面应用：Tauri 2 原生窗口 + 受监督的 Harness 子进程。

## 架构

与社区先例（dataelement/dsh-desktop、majiayu000/dsh-desk）一致的成熟路线：

- **进程模型**：壳进程拉起 `dsh web --no-open --port 0`（OS 分配随机回环端口），从子进程 stdout 的 `dsh web: http://127.0.0.1:<port>` 行解析实际地址，再做真实 HTTP GET 健康检查后才把窗口导航过去。异常退出自动重启（上限 3 次），退出应用时整树击杀（`taskkill /T` / 进程组信号），不留孤儿 node。
- **WebView 加固**：程序化建窗挂导航守卫——只放行本地加载页与**当前** Harness origin（随机端口，重启后自动更新放行目标；前缀匹配校验边界字符防端口伪装）；其余 http(s) 交给系统浏览器。Harness 页面**零 Tauri IPC 授权**（capabilities 仅 `local: true` 给加载页；自定义命令在命令层校验调用方 URL）。
- **桌面语义**：关闭=最小化到托盘（IM 渠道/长任务不中断）、托盘菜单（显示/重启/日志/数据目录/开机自启/退出）、单实例二次启动聚焦。
- **运行时**：优先使用 `%LOCALAPPDATA%\dsh-desktop\node` 便携运行时（Node + 固定版本 dsh），否则回退系统 `node`/`dsh`；`DSH_HOME` 沿用应用进程环境（默认共享 `~/.dsh`，保留 im-bot 绑定/共享记忆等用户数据）。

## 开发

```powershell
cd src-tauri
cargo run            # 调试运行（前端为 ui/ 静态加载页）
cargo build --release
```

首次构建需 Rust 工具链与 WebView2 运行时。未安装便携/系统运行时时，加载页会给出指引。

## 路线图

- M1 骨架与监督（本仓库当前状态）
- M2 运行时管理：`scripts/pin-runtime.mjs` 离线固定运行时 + 首启安装引导 + 升级命令
- M3 原生集成：以非浏览器客户端订阅 `/api/events.mux` WebSocket 下行流（信封协议已实测：`server-request → session/event`），回合完成/审批请求转 OS 通知与任务栏闪烁——服务端→壳单向，页面零改动
- M4 分发：NSIS 安装器 + tauri-plugin-updater 自动更新 + GitHub Actions Windows CI；macOS 二期

## 安全边界

- 不给 Harness 页面任何 IPC 权限；原生集成只通过壳自身订阅本地事件流实现。
- 导航白名单外的地址一律外抛系统浏览器；`file://` 等协议导航直接拒绝。
