# dsh-desktop 顶栏窗控钮与 dsh 右侧栏（dockkit 条带）重叠·分析与设计方案

日期：2026-09-09 · 状态：**已实施**（实现较本稿两处 refinement：带底色改继承 app token 而非 prefers-color-scheme；补 html 高度收缩 + overflow:hidden 防底部裁切/溢出滚动条，见 §5.1 与文末实施记录）· 关联代码：`src-tauri/src/webview.rs`、tauri-plugin-decorum 1.1.1、`@deepseek-ai/dsh-client-ui-sidebar-right` 0.1.5-alpha.1

## 一、现象

dsh web 端 0.1.5 系列新增右侧栏（「开始」标签 + 文件/预览面板）。壳的窗口右上角同时出现两套控件：

- 截图实测（窗口最大化 2560px 宽、100% 缩放）：右上角 ~174×32px 区域内挤着 5 个图标，其中**两个 ✕ 直接重叠**；
- 更严重的是功能层面（见 §三.4）：点「开始」标签会**最小化窗口**，点右侧栏的「全屏/收起」按钮会**关闭窗口**，顶部 32px 内其余点击全部变成拖动窗口。

## 二、两侧的事实（源码逐层核实）

### 2.1 壳侧：decorum 覆盖式标题栏

`create_overlay_titlebar()`（tauri-plugin-decorum 1.1.1，`src/js/titlebar.js` + `src/js/controls.js`）向页面注入：

```
<div data-tauri-decorum-tb>            ← position:fixed; top:0; left:0; width:100%; height:32px
  <div data-tauri-drag-region>         ← width/height 100%，全条带透明拖拽层
  <button #decorum-tb-minimize>        ← 58×32，flex 尾部
  <button #decorum-tb-maximize>        ← 58×32
  <button #decorum-tb-close>           ← 58×32
</div>
```

壳的 `TITLEBAR_INSET_CSS`（webview.rs:33）把它钉死：`position:fixed;top:0;right:0;z-index:2147483647`，并且**不再让页面让位**（v0.1.x 早期的 40px 下移已移除，README §设计仍写的是旧的下移方案——文档已落后于代码）。

于是窗控钮簇占据 `x∈[宽-174, 宽]、y∈[0,32]`；**其余顶部 32px 全宽**被透明拖拽层盖住（容器 z 序最大，命中测试先于页面）。

### 2.2 web 侧：右侧栏 = dockkit 条带顶到窗口右上角

`dsh-client-ui-sidebar-right` 的设计（lib/client.js 内注释原文）：

> The panel has no header of its own: its two controls — presentation switch and collapse — **ride the docking kit's chrome seat at the end of the top-right pane's tab strip, so the strip is the panel's whole top edge.**

即面板自己没有 header，两个控制钮（全屏切换 `[ ]`、收起 ✕）由 dockkit 放在**右上 pane 标签条带的末端**，条带就是面板的整个上边缘，从页面 y=0 开始铺。DOM 钩子（已在前端产物 index-C2kf62HV.js 中确认存在，属稳定契约）：

- `[data-dockkit-strip]` / `[data-dockkit-strip-tabs]` / `[data-dockkit-strip-fill]` / `[data-dockkit-strip-chrome]`
- `[data-dockkit-tab]` / `[data-dockkit-tab-close]`（「开始」标签及其关闭点）
- 面板 chrome 两键：`[data-sidebar-right-mode]`（全屏/退出全屏）、`[data-sidebar-right-toggle]`（收起，glyph 为 ✕）

实测几何（2560 宽）：「开始」tab 及其 ✕ ≈ x 2390–2440；chrome 两键 ≈ x 2503–2551；全部落在 decorum 三键的 `x∈[2386,2560]、y∈[0,32]` 覆盖区内。

### 2.3 z 序与命中结果

| 区域（窗口坐标） | 最顶层元素 | 实际效果 |
|---|---|---|
| `x∈[宽-174,宽] y∈[0,32]`（decorum 三键） | decorum 按钮（z 2147483647） | 点到的是最小化/最大化/关闭 |
| 其余 `y∈[0,32]` 全宽 | decorum 透明拖拽层 | mousedown → `start_dragging`，**页面收不到事件** |
| `y>32` | 页面正常 | 正常 |

点击语义核实（tauri 2.11.5 `src/window/scripts/drag.js`）：drag-region 是 document 级 mousedown 监听，按 `e.composedPath()[0]` 是否为带 `data-tauri-drag-region` 的元素判定——拖拽层是命中测试的胜者，页面元素根本不进事件路径；`e.preventDefault()` + `stopImmediatePropagation()` 双保险拦截。

### 2.4 结论：结构性冲突，不是样式微调能解决

覆盖式（overlay）标题栏的前提是「web 顶部两角没有可用 UI」。dsh 0.1.5 起右侧栏的条带**专职占据右上角**（这是它的产品设计，后续只会更强），左上角历史上也曾被 macOS 红绿灯/角标挤占过（见 MODE_BADGE 两次挪位记录）。只要壳坚持全宽 overlay，两边对同一片像素的所有权之争必然反复发生。唯一让「拖拽区」与「页面 UI」不竞争的格局是：**壳拥有独占的顶栏带，页面从带下开始铺**——即仓库 README 本来记载、后被代码放弃的 40px 下移方案。

## 三、目标与非目标

**目标**
1. 窗控三钮保持原生位置（右上、贴顶），Snap Layout/悬停语义/CloseRequested→托盘全部不变；
2. dsh 页面全部 UI 可点：右侧栏条带（「开始」tab、tab 关闭点、全屏、收起）不再被窗控钮或拖拽层劫持；
3. 窗口拖拽、双击最大化仍然可用（拖拽区 = 顶栏带全宽）；
4. 对 dsh web 升级健壮：注入只依赖双方稳定契约（`[data-tauri-decorum-tb]`、CSS 变量），不依赖 dsh 的 hash 类名与布局细节；
5. 深浅主题下顶栏带观感可接受；macOS 红绿灯同享顶栏带。

**非目标**
- 不改 dsh web 产物（产品包 npm 更新，改了也会被覆盖）；
- 不做「按页面内容动态让位」的自适应布局（脆弱，见 §四方案 C 的否决理由）。

## 四、方案对比

### 方案 A（推荐）：恢复保留式顶栏带（v2 硬化版）

页面整体下移 40px（`body transform`，使 fixed/absolute 插件 overlay 一并让位），decorum 条带反向平移回窗口顶，并把「本地/远程」模式角标也收回带内。在旧方案基础上硬化四点：

1. **stylesheet 而非 inline style**：v0.1.x 用 `document.body.style` 内联，页面脚本一旦清 inline 就失效；v2 注入 `<style>` 规则（`body{transform:…}`），对运行时 DOM 操作免疫；
2. **单一 CSS 变量** `--dsh-titlebar-h: 40px` 挂 `:root`，下移量、decorum 反向平移、角标回移共用一个来源，改高度只动一处；
3. **顶栏带主题化**：带底色用 `--dsh-titlebar-bg` + `prefers-color-scheme`（浅 `#ffffff` / 深 `#0d121a`，与 dsh 顶栏/暗色底一致），decorum 容器高抬到 40px、按钮 `align-items:flex-start !important` 保持贴顶，观感是「窗控带」而不是「露馅的空条」；
4. **带高 40px ≥ dockkit 条带高（~40px）**：右侧栏条带完整落在带下，不再有任何裁切或叠压。

收益/代价：收益是结构性解决（§二.4），dsh 以后往顶角加什么都落在带下；代价是常驻 40px 带 + 深浅主题底色靠 `prefers-color-scheme` 近似（dsh 若手动切主题与系统不一致会有色差——纯外观，可后续增强为运行时取色）。

### 方案 B（长期正解，但不受我们控制）：上游 embedded 模式

dsh web 的 frame 几何由 `dsh-client-ui-layout` 统一掌管。若上游提供内嵌模式（如读 `localStorage`/URL 参数预留 titlebar inset，或识别 `window.__DSH_BOOT__` 宿主），由 app 自己用主题 token 画带，色差与双主题问题根治。**行动**：给 deepseek-harness 提 feature request；本方案 A 与其完全兼容（届时壳关闭下移即可）。

### 方案 C（否决）：overlay 收缩

思路：把拖拽层宽度压成 0（只留三键）+ 用 `[data-dockkit-strip-chrome]` 给条带尾端加右 padding 把 chrome 左移出 174px。否决理由：
1. 拖拽层压零后**整个窗口没有任何拖拽区**（页面不知道自己是无边框窗口的宿主），窗口只能靠任务栏/Win+方向键挪动——不可接受；
2. 若反过来保留拖拽层，顶部 32px 页面 UI（顶栏、条带）依旧点不到——问题只解决一半；
3. 依赖 `[data-dockkit-*]` 的布局假设（条带高度、padding、未来结构），dsh 一改版即碎，恰是 §三.4 要避免的。

## 五、方案 A 实施细则

### 5.1 `webview.rs` — `TITLEBAR_INSET_CSS` 重写（v2）

保留端口守卫（`http:` + 非空 port 才生效，加载页 tauri.localhost 天然空操作）。注入一条 `<style id="dsh-desktop-titlebar-inset">`：

```css
:root { --dsh-titlebar-h: 40px; }
html { height: calc(100% - var(--dsh-titlebar-h)) !important; overflow: hidden !important; }
body { margin: 0 !important; height: 100% !important; transform: translateY(var(--dsh-titlebar-h)); }
[data-tauri-decorum-tb] {
  position: fixed !important; top: 0 !important; left: 0 !important; width: 100% !important;
  height: var(--dsh-titlebar-h) !important;           /* 32→40：带即拖拽区全高 */
  align-items: flex-start !important;                  /* 按钮贴顶（decorum inline 是 end） */
  transform: translateY(calc(0px - var(--dsh-titlebar-h)));  /* body 让位后反向平移回窗口顶 */
  background: var(--dsw-alias-bg-base, #fff);
  z-index: 2147483647 !important;
}
```

要点（含对本稿初版的两处 refinement，均为实现时对 dsh 前端产物实测后的修正）：

1. **底部零裁切靠高度收缩，不是靠运气**：dsh 前端是 `html,body,#root{height:100%}` 链（dist CSS 实测，无 100vh 根）。`html` 高度收缩到 `100% - 带` 后，整条 100% 链正好铺满带下区域，底部状态栏完整可见；transform 平移产生的 40px 视觉溢出由 `html overflow:hidden` 消除——这正是 v0.1.9（f7bd88a）当年的同款教训；
2. **`body transform` 而非 padding**（v0.1.10 曾用 padding）：padding 下 fixed 定位后代相对视口定位——模式角标、dockkit floatHost（`position:fixed;inset:0` 浮窗层，dist 实测）会留在窗口顶被带盖住（浮窗标题在带下点不到）。transform 使 body 成为 fixed 后代的包含块，所有 overlay 一致下移，`bottom:0` 仍贴窗口底；
3. **带底色继承 app token**：`--dsw-alias-bg-base` 定义在 `body` 上（浅=neutral-bluish-00，深=`body[data-ds-dark-theme]` 下 neutral-bluish-950），decorum 容器是 body 子元素，直接 `var(--dsw-alias-bg-base,#fff)` 继承——深浅主题、手动切主题全部自动跟随，无色差。初版的 `prefers-color-scheme` 近似方案废弃（dsh 主题可手动切、与系统不一致会色差）；
4. **带高单一来源 `--dsh-titlebar-h`**：html 收缩、body 平移、decorum 反向平移、模式角标回移四处共用；改带高只动一处；
5. **`!important` 均为覆盖 inline/decorum 注入样式**（height/align-items/position）所必需；带高 40px ≥ dockkit 条带高，右侧栏条带完整落在带下。

### 5.2 `webview.rs` — `MODE_BADGE_JS` 收回带内

角标挂在 body 下，会随页面 +40px；把其 inline `transform` 从 `translateX(-50%)` 改为
`translate(-50%, calc(-1 * var(--dsh-titlebar-h, 0px)))`，角标回到窗口顶部居中，正好住在带内，且不再与页面内容同层。配色（2026-09-05 那轮）不动。

### 5.3 不变的 部分

- `DECORUM_ICON_CSS`（字形回退链、尺寸、悬停色）原样保留；
- `SECURE_CONTEXT_SHIM_JS` 不动；
- 导航守卫、capabilities、CloseRequested→托盘、window_state 全部不动。

### 5.4 文档与测试

- README §设计两处更新：`「Harness 页面经初始化脚本整体下移 40px」`条目改为与 v2 实现一致（stylesheet + CSS 变量 + 主题化带 + 角标入带），并补一句动机：dsh 0.1.5 右侧栏 dockkit 条带占据右上角，overlay 方案与其结构性冲突（附本文档链接）；
- `webview.rs` tests 增加字符串断言（沿用 `secure_context_shim_only_fills_missing_api` 风格）：
  - `titlebar_inset_css_keeps_port_guard_and_offset_contract`：含端口守卫、`--dsh-titlebar-h`、`translateY(var(--dsh-titlebar-h))`、decorum 反向平移、`z-index:2147483647`；
  - `mode_badge_counter_shift_uses_titlebar_var`：badge cssText 含 `var(--dsh-titlebar-h`。

### 5.5 兼容与回滚

| 场景 | 行为 |
|---|---|
| 加载页（tauri.localhost） | 端口守卫空操作，加载页继续铺满（与现状一致） |
| 本地 harness / 远程反代页 | 同一注入路径，行为一致（远程页右上也可能是 dsh 新 UI，同样受益） |
| macOS | 下移同样生效，红绿灯落在带内（比现在浮在页面上更干净）；decorum 容器在 macOS 由红绿灯替代，反向平移规则空匹配，无副作用 |
| dsh 深浅主题与系统不一致 | 带底色继承 `--dsw-alias-bg-base`（定义在 body 上），随 dsh 主题自动切换，无色差 |
| dsh 侧栏「全屏」模式（`position:fixed;inset:0`） | 覆盖带下全区域，其条带/退出钮全部可点（修复前点「全屏」=关窗） |
| 未来 dsh 若改 100vh 根（放弃 100% 链） | 100vh 不随 html 收缩，会出现 40px 底部溢出——`html overflow:hidden` 兜底成静默裁切且无滚动条；届时需重新评估（测试断言不会拦截该场景，见 §5.6 诊断项） |
| 回滚 | 还原 TITLEBAR_INSET_CSS/MODE_BADGE_JS 两个常量即可；可选加固（P2）：`DSH_DESKTOP_TITLEBAR=overlay` 环境变量切换回 overlay 注入，留逃生门 |

### 5.6 已知限制与后续增强（P2，不阻塞）

- 带底色运行时取色：首帧后读 `[data-dockkit-surface]`/顶栏元素的 computed background 回填 `--dsh-titlebar-bg`，根治双主题色差（若上游做 embedded 模式则废弃此项）；
- diagnostics 增加一份 decorum 容器与 `[data-dockkit-strip]` 的几何快照，便于以后秒判此类重叠回归。

## 六、验证清单（真机）

1. 右上角：三枚窗控钮独占贴顶，无任何 dsh 图标与其重叠；hover 变色/红色关闭钮正常；
2. 点右侧栏「开始」tab → 打开/聚焦侧栏（不再最小化）；点 tab ✕ → 关标签（不再最小化）；
3. 点侧栏 chrome `[ ]` → 全屏切换；点收起 ✕ → 侧栏滑出（**不再关窗**）；侧栏全屏模式下退出钮可点；
4. 顶栏带任意空白处拖拽移动窗口、双击最大化/还原、最大化悬停 Snap Layout 预览正常；
5. dsh 顶栏（会话标题、标准·领悟、Session 日志、文件夹钮）在带下全部可点；
6. 「本地/远程」角标居中显示于带内，不与页面内容重叠；
7. 深浅主题各看一眼带底色；窗口还原/多显示器挪动后（window_state 回放）一切照旧；
8. `cargo test -p dsh-desktop`（webview tests）通过。

## 七、实施顺序

1. `TITLEBAR_INSET_CSS` v2 + `MODE_BADGE_JS` 计数平移 + 新增两条测试（一次提交）；
2. README 对应段落更新 + 链接本设计文档；
3. 真机过 §六清单（重点 2/3 两项——它们是修复前的真实事故点）；
4. （并行）向 deepseek-harness 提 embedded-mode feature request（方案 B）。

## 八、实施记录（2026-09-09）

- `src-tauri/src/webview.rs`：`TITLEBAR_INSET_CSS` 重写为 v2（§5.1，含两处 refinement）；`MODE_BADGE_JS` 角标反向平移进带；新增测试 `titlebar_inset_css_reserves_titlebar_band`、`mode_badge_counter_shifts_with_titlebar_band`（契约字符串断言，沿用现有测试风格）。
- `README.md`：「无边框窗口」条目改为 v2 事实（含 overlay→让位的动机与文档链接）；「模式角标」条目修正为顶部居中/带内（原文「左上角」系 2026-09-04 挪位后的陈旧描述）。
- 本文档：状态改已实施；§5.1/§5.5 按 refinement 修订。
- 待办：真机 §六清单验证（构建 `cargo tauri build` 或 `cargo run` 后逐项过）；§五.5.6 的取色增强与 diagnostics 几何快照维持 P2。

### v2.1 追加（同日，用户真机反馈）

真机确认重叠/点击劫持已解决；剩余观感问题：顶带与页面同为 `bg-base` 且无分界，读不出这是一条标题栏。按用户草图（顶带底部一条全宽分隔线、徽标与窗控钮留带内）在 decorum 容器上加 `box-sizing:border-box` + `border-bottom:1px solid var(--dsw-alias-border-l1,…)`（app 边框 token，随主题切换）——分隔线画在带内（y=39..40）。测试同步补分隔线与 box-sizing 两条断言。
