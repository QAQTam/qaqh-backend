# DeepX WinUI — 原生 UI 迁移计划（P0: 标题栏 + ThreadHeader + 主题同步）

> 最后更新: 2026-08-06
> 前置: renderer 收编完成（`apps/winui/renderer` 源码 + `apps/winui/out/renderer` 唯一产物）
> 关联: `ELECTRON-MIGRATION.md` 混合 XAML 路线图 Phase 1（壳层搬迁）

---

## 概述

将窗口**标题栏原生化**（Mica 延伸、拖拽、深色自动）+ Web 顶部条 **ThreadHeader**（会话标题 + 8 个 actions）迁移到 XAML `TitleBar` 控件，并打通**主题同步**链路。这是混合 XAML 路线图 Phase 1 的第一个落地块，与已完成的侧栏（Phase 2 部分）构成完整壳层框架。

### 迁移原则

1. **壳渲染 + Web 单一数据源** — 沿用侧栏模式：XAML 只渲染，状态归属 Web（`shell.header` 推送 / `shell.headerAction` 回传），避免双写
2. **行为等价** — 8 个 actions 逐个对等，不改变语义
3. **测试先行** — 每步 `cargo check` + `pnpm typecheck/test` 验证
4. **一次一个块** — 桥协议 → 壳组件 → Web 侧，顺序推进

---

## 现状盘点

### 框架能力（已具备，零开发）

| 能力 | 位置 | 说明 |
|---|---|---|
| `SetExtendsContentIntoTitleBar(true)` + `SetTitleBar(tb)` | `reactor/src/host.rs:277-288` | 渲染树出现 `TitleBar` 控件时自动接线 |
| 标题栏主题跟随 | `host.rs:89-106` `update_titlebar_theme` | 深浅色自动（`SetPreferredTheme`） |
| `TitleBar` 控件 | `reactor/src/widgets/title_bar.rs` | title/subtitle + **content 槽**（中间）+ **footer 槽**（RightHeader 右端）+ tall |
| 约束 | `backend/winui/mod.rs:238` `find_titlebar` | **每窗口一个 TitleBar**（找第一个） |

### Web 侧现状

```
ThreadHeader（ChatView.tsx:201 渲染，58px，--bg-glass-bar 半透明 + 底部分隔线）
├── 左：▱ 会话标题（title prop）
└── 右 8 个 actions：
    ① workspace（打开目录选择）   ② open location（shell）  ③ console（shell）
    ④ info（Web 面板 InfoPopover） ⑤ stats（Web 面板）      ⑥ undo（Web 状态）
    ⑦ compact 整理上下文（Web 状态） ⑧ pet 桌宠（壳 stub 恒 false）
```

**分类**：①-③ 壳原生能力（openDialog/openPath/openDevTools 桥已有）；④-⑧ 依赖 Web 内部状态/逻辑。

### 关键约束：必须整体迁移

`ExtendsContentIntoTitleBar` 后若保留 Web ThreadHeader，会出现"窗口标题栏 + Web 顶部条"**双层条**（~106px）。因此 **TitleBar 直接承载 ThreadHeader 全部内容**，Web 侧隐藏（与侧栏 `xamlSidebar` flag 同模式）。

---

## 架构设计

### 布局

```
Grid（main.rs app()）
├── row 0: XAML TitleBar（48px）
│     ├── title 槽：chat 视图 = 会话标题（shell.header 推送）；home/skills/settings = 视图名
│     └── footer 槽：①workspace ②location ③console ┃ ④info ⑤stats ⑥undo ⑦compact ⑧pet
└── row 1: Grid（row0: 侧栏 / row1: WebView2）← 现有结构下移一行
```

- TitleBar 作为 `SetTitleBar` 拖拽区域（host 自动接线），WebView2 从 row 1 开始，**无输入区域重叠**（拖拽/双击最大化由 XAML 区域处理）
- 壳 header 组件：新增 `apps/winui/src/header.rs`（与 sidebar.rs 同层）

### 桥协议扩展（3 个通道）

| 通道 | 方向 | 载荷 | 语义 |
|---|---|---|---|
| `shell.header` | Web → 壳（事件） | `{view, title, workspace, infoOpen, statsOpen, compacting, compactDisabled, undoDisabled, petEnabled}` | Web 状态投影，壳更新 TitleBar（rev 比对驱动，同侧栏 timer 模式） |
| `shell.headerAction` | 壳 → Web（invoke） | `{action: "workspace"\|"location"\|"console"\|"info"\|"stats"\|"undo"\|"compact"\|"pet"}` | 壳点击回传 Web 执行 |
| `shell.theme` | 双向 | `{mode: "light"\|"dark"}` | Web `applyTheme` → 壳 `RequestedTheme`；壳系统主题变化 → Web `prefers-color-scheme` 校正 |

### 数据流

```
Web ChatView 状态 ──effect──> shell.header 事件 ──> 壳 header.rs ──> TitleBar 属性
壳点击 action ──> invoke shell.headerAction ──> Web 执行对应逻辑（状态单一来源）
①-③ 壳直接处理：openDialog / openPath / openDevTools（现有桥）
⑧ pet：壳隐藏（Electron 残留，迁移文档已建议隐藏 UI 入口）
```

---

## 实施步骤

### Step 1 — 桥协议（Rust + bridge.js）

| 文件 | 改动 |
|---|---|
| `apps/winui/src/bridge.rs` | `handle_message` 加 `shell.headerAction` 分发（invoke 出口）；`BridgeCore` 加 `header_state: Mutex<Value>` + `emit("shell.header", ...)` 通道（rev 递增，同 sessions 模式） |
| `apps/winui/assets/deepx-bridge.js` | `window.deepx.shell` 扩展：`headerAction(action)` invoke + `onHeader(listener)` 订阅 + `setTheme(mode)` / `onThemeChanged` |

**验证**：`cargo check -p deepx-winui`；`pnpm -C apps/winui/renderer typecheck`

### Step 2 — 壳侧 header 组件

| 文件 | 改动 |
|---|---|
| `apps/winui/src/header.rs`（新） | `TitleBar` 控件：title 槽（TextBlock）+ footer 槽（8 个 subtle 按钮，Symbol 图标 + tooltip）；应用 `shell.header` 状态（标题/启用态）；timer 轮询 rev（同 sidebar 模式） |
| `apps/winui/src/main.rs` | Grid 加 row 0（TitleBar）；布局下移一行 |

**验证**：`cargo check`；手动：标题栏出现、拖拽移动窗口、双击最大化、Mica 延伸

### Step 3 — Web 侧改造

| 文件 | 改动 |
|---|---|
| `apps/winui/renderer/src/runtime/shellBridge.ts` | 扩展 `headerAction` / `onHeader` / `setTheme` / `onThemeChanged` 类型与封装 |
| `apps/winui/renderer/src/components/ChatView.tsx` | `xamlHeader` flag 时隐藏 `<ThreadHeader>`（保留组件与测试，flag 回退） |
| `apps/winui/renderer/src/App.tsx` | header 状态投影 effect（view/title/workspace/各 action 状态 → `shell.header`）；`shell.headerAction` 订阅分发到既有 handler（onToggleInfo/onToggleStats/onUndo/onCompact/onChangeWorkspace/onOpenLocation/onOpenConsole） |
| `apps/winui/renderer/src/components/shell/ThreadHeader.test.tsx` | 保持不变（组件未删） |

**验证**：`pnpm typecheck` + `pnpm test`（262+ 基线）；手动：8 个 actions 行为等价

### Step 4 — 主题同步

| 文件 | 改动 |
|---|---|
| `apps/winui/src/header.rs` 或 `main.rs` | 应用 `shell.theme` → `RequestedTheme`（reactor 支持 `ActualThemeChanged`，engine.rs:732）；监听系统主题变化推 Web |
| `apps/winui/renderer/src/App.tsx` | `applyTheme` 时调 `shell.setTheme`；监听 `onThemeChanged` 校正三套主题 |

**验证**：三套 Web 主题 ↔ 壳标题栏/侧栏一致；系统切换时双向同步

### Step 5 — 收尾

- 全仓 `rg` 检查无遗漏；README（apps/winui）更新目录结构与 dev 流程（如需）
- 手动验证清单全绿

---

## 验证清单（手动）

- [ ] 标题栏：拖拽移动、双击最大化/还原、右键系统菜单
- [ ] Mica 延伸到标题栏（无白色系统条）；深浅色切换标题栏随动
- [ ] ①workspace：打开目录选择对话框（openDialog）
- [ ] ②location：打开会话目录（openPath）；③console：DevTools
- [ ] ④info ⑤stats：面板开合与 Web 内一致（回传执行）
- [ ] ⑥undo：禁用态（undoDisabled）正确；⑦compact：整理中态（compacting）正确
- [ ] ⑧pet：壳隐藏，无入口
- [ ] 会话切换：标题随 `shell.header` 更新；home/skills/settings 显示视图名
- [ ] 浏览器 debug 模式（无 window.deepx）：ThreadHeader 回退显示（flag 关闭路径）

---

## 风险与决策

| 项 | 说明 | 决策 |
|---|---|---|
| TitleBar 每窗口一个 | `find_titlebar` 找第一个 | 本窗口唯一 TitleBar，无冲突 |
| 双层条 | ExtendsContentIntoTitleBar + 保留 Web 头部 | **整体迁移**，Web ThreadHeader 隐藏（flag 可回退） |
| actions 往返延迟 | ④-⑧ 经 invoke 回传 Web | 本地 IPC，<1ms，可接受 |
| pet 按钮 | 壳 stub 恒 false | 壳隐藏入口（迁移文档建议） |
| 主题漂移 | Web 三套主题 vs 壳系统主题 | Step 4 双向同步通道 |

---

## 后续（P1-P4 展望）

| Phase | 内容 | 前置 | 工作量 |
|---|---|---|---|
| P1 | info/stats 面板原生化（XAML Flyout，reactor `flyout.rs` 已有） | P0 | ~0.5d |
| P2 | ContentDialog 壳层（confirm/权限/更新，`content_dialog.rs` 已有） | 可插队 | ~0.5-1d |
| P3 | 设置页（Phase 3；表单控件齐备，配置状态同步为核心） | P0-P2 | 大 |
| P4 | undo/compact 逻辑与 Composer（Phase 4 评估，耦合最深） | 暂不排期 | — |
