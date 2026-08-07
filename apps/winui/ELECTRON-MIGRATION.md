# Electron → WinUI3 迁移盘点（未迁移清单）

> 状态：Electron 运行时实现已从仓库移除（`apps/desktop/electron/`），
> `deepx-winui`（Rust + WinUI3 + WebView2）成为唯一桌面壳。
> 本文档盘点 **Electron 曾提供、WinUI 尚未迁移** 的能力，作为后续开发依据。

## 迁移总览

| 能力域 | Electron 实现 | WinUI 现状 | 状态 |
|---|---|---|---|
| Ringing V1 传输（三 SSE + lease + 命令/查询/action） | `electron/controlClient.ts`、`ringingClient.ts`、`ringingManager.ts` | `crates/deepx-client`（Rust 重写） | ✅ 已迁移 |
| 会话 timeline（snapshot + SSE + gap 恢复） | `electron/timelineClient.ts` | `deepx-client::timeline::TimelineStream` + `bridge.rs` | ✅ 已迁移 |
| `window.deepx` 桥（backend/ringing/timeline/desktop） | `electron/preload.ts` | `assets/deepx-bridge.js` + `src/bridge.rs` | ✅ 已迁移（形状对齐） |
| 文件读取（base64/文本，附件导入） | `desktop:read-file-*` | `desktop.readFileBase64/readTextFile` | ✅ 已迁移 |
| 打开外部路径/URL | `shell.openPath/openExternal` | `desktop.openPath`（cmd start） | ✅ 已迁移 |
| DevTools 窗口 | `webContents.openDevTools` | `desktop.openDevTools`（UI 线程处理） | ✅ 已迁移 |
| 连接状态推送 | `backend:status` / `ringing:status` 事件 | 事件 + `ringing.status` 状态表 | ✅ 已迁移（含 camelCase 字段对齐） |
| 关闭行为（最小化到托盘/完全退出/取消） | `onWindowClose` 弹窗 + `createTray` + `quitDeepX` | 关窗直接退出 | ❌ 未迁移 |
| 托盘图标与菜单 | `Tray` + 运行时生成 PNG 图标 | 无 | ❌ 未迁移 |
| 文件/目录选择对话框 | `dialog.showOpenDialog` | `desktop.openDialog/openImageDialog`（`bridge.rs` Win32 `IFileOpenDialog`，UI 线程） | ✅ 已迁移（2026-08-06） |
| 确认对话框 | `dialog.showMessageBox` | `desktop.confirm` 恒 `true` | ❌ 未迁移（renderer 暂无调用点） |
| 自动更新（check/stage/apply + 事件） | `deepx-updater` 子进程 + `pending.json` | `desktop.checkUpdate/stageUpdate/applyUpdate` stub | ❌ 未迁移 |
| 后端重启（运行环境切换） | `prepareBackendUpdate` + `resumeAfterBackendUpdate` | `backend.restart` 报未实现 | ❌ 未迁移 |
| 桌面宠物 | spawn node 脚本进程 | `desktop.togglePet/getPetStatus` 恒 `false` | ❌ 未迁移（建议隐藏 UI 入口） |
| 窗口背景材质切换 | `setBackgroundMaterial` | 恒返回 `true` 但无效果（壳固定 Mica） | ❌ 未迁移 |
| 优雅退出（先停 daemon 防孤儿） | `quitDeepX` → `stopDaemon` | 无 | ❌ 未迁移 |

## 未迁移功能清单（按优先级）

### P0 — 核心功能缺失

#### 1. 文件/目录选择对话框（`desktop.openDialog` / `desktop.openImageDialog`） ✅ 已完成（2026-08-06）

- **影响**：`ComposerDock.tsx` 图片/文本附件上传、`App.tsx` 工作区目录选择、`SettingsView.tsx` tokenizer 路径选择全部不可用（对话框返回 `null`）
- **方案**：Rust 侧 Win32 `IFileOpenDialog`（COM；`directory` → `FOS_PICKFOLDERS`，`multiple` → `FOS_ALLOWMULTISELECT`），必须跑在 UI 线程（STA）
- **实现**：`src/bridge.rs` `Bridge::handle_message` 拦截 `desktop.openDialog/openImageDialog`（与 `openDevTools` 同模式，UI 线程直接执行）；`show_open_dialog()` 封装 `IFileOpenDialog`（`CoCreateInstance` + `FOS_FORCEFILESYSTEM|FOS_FILEMUSTEXIST` + 可选 `FOS_PICKFOLDERS`/`FOS_ALLOWMULTISELECT` + `SetTitle`/`SetFileTypes`）；取消（`ERROR_CANCELLED`）返回 `null`，单选返回字符串、多选返回数组；`shell_item_path()` 经 `SIGDN_FILESYSPATH` 取路径并 `CoTaskMemFree`
- **依赖**：`apps/winui/Cargo.toml` 新增 `windows` crate（features: `combaseapi`/`shobjidl_core`/`shtypes`/`windef`/`winerror`）
- **后续**：混合 XAML 阶段可将 `IFileOpenDialog` 换为原生 FolderPicker/FileOpenPicker，仅改 `show_open_dialog` 内部实现，桥契约不变

### P1 — 明显功能缺失

#### 2. 自动更新链路（`checkUpdate` / `stageUpdate` / `applyUpdate` / `onUpdateAvailable` / `onUpdateFailed`）

- **影响**：`App.tsx` 启动检查更新与更新提示 UI 失效
- **现状**：底层 `deepx-updater` 二进制与安装布局已由 `just winui-package` 构建（`build-updater`），仅缺壳侧封装
- **方案**：Rust 侧封装：读 `<install>/.deepx-update/pending.json`（check）、调 `deepx-updater stage/apply-staged/handoff`、事件推送 `update:available/failed`
- **工作量**：约 0.5-1 天

#### 3. 后端重启（`backend.restart`，运行环境切换）

- **影响**：`SettingsView.tsx` 的 workspace 运行模式切换不可用
- **方案**：`deepx-client::Client::stop_daemon(false)` → 等退出 → `discovery::ensure_daemon_running` 重连；409 Busy 映射 `{ok:false, reason:"busy"}`
- **工作量**：约 2 小时

#### 4. 关闭行为 + 托盘 + 优雅退出

- **影响**：关窗直接退出（无"最小化到托盘"选项）；退出时 daemon 可能残留孤儿进程
- **方案**：XAML `ContentDialog`（`windows-reactor` 已有绑定）三选弹窗 + `Shell_NotifyIcon` 托盘（需新增绑定）+ 退出时经 discovery 直接 `POST /control/v1/stop`
- **工作量**：约 1-2 天

### P2 — 低影响

#### 5. 确认对话框（`desktop.confirm`）

- renderer 当前无调用点；用 XAML `ContentDialog` 实现即可（绑定已有）

#### 6. 桌面宠物（`togglePet` / `getPetStatus`）

- 宠物是 Electron 的 node 脚本进程，WinUI 无 node 集成；**建议在 renderer 隐藏宠物 UI 入口**（`ChatView.tsx`），而非移植

#### 7. 窗口背景材质（`setBackgroundMaterial`）

- 需要把 material 请求路由到 UI 线程更新窗口 `Backdrop`（`windows-reactor` 需确认 API）

#### 8. DevTools 快捷键

- `desktop.openDevTools` IPC 已实现，但无 F12/菜单快捷键绑定（Electron 默认有）

## 已确认等价/优于 Electron 的部分

- timeline：Rust `TimelineStream` 与 Electron `timelineClient.ts` 行为一致（cursor 校验、gap 恢复、重连退避），并修复了 Electron 版未覆盖的 `close()` 停止语义
- `ringing.status`：返回三频道状态表（`{control, conversation, tool}`），renderer `ringingMonitor.activate` 不再静默提前返回
- 桥协议：`deepx-bridge.js` 与 `preload.ts` API 形状完全对齐，renderer 无需改动

## 混合 XAML + WebView2 路线图（真实进度，截至 2026-08-07）

`windows-webview::webview()` 返回 `windows_reactor::WebView2`（XAML 控件，可嵌入布局树），混合方案可行：

```
Phase 0  桥协议扩展（nav/settings 通道）+ Grid 布局（XAML TitleBar/NavigationView + WebView2 内容区） ✅ 完成（header.rs + sidebar.rs + 视图族行高切换）
Phase 1  搬壳层：ContentDialog（confirm/权限/更新）+ MenuBar + 状态栏
         ✅ 部分完成：权限/Ask 交互模态已原生（interaction_overlay.rs，P-6 覆盖层
         模式，2026-08-07）；confirm/更新 ContentDialog 与 MenuBar/状态栏 未动
Phase 2  搬侧边导航 + 会话列表（NavigationView，数据走 Rust 查询） ✅ 完成（sidebar.rs + shell_store.rs）
Phase 3  搬设置页（表单控件齐备，状态同步为核心工作） ✅ 完成（settings_view.rs，2026-08-06）
Phase 4  可选：Composer 搬迁评估（耦合深，风险高）
         ✅ 阶段 A+B 完成（composer_bar.rs，2026-08-07）：主输入/slash/附件/队列/
         goalBar 原生，发送协议仍在 Web（状态单源）；图片缩略图预览（%TEMP% 副本）
         ✅ 读路径直连（2026-08-07）：composer A 组字段（isStreaming/gate/
         model/context）改由 conversation 事件 Rust 直连解析——ComposerActivity
         卡死检测（4min 阈值，对齐 Web isSessionStreaming）+ usage_updated 缓存
         （model/context_limit/prompt_tokens），hasPendingGate 复用交互队列机器；
         B 组（mode/permissionLevel/queue/sendAck/submitError——写路径伴生状态）
         保留 Web 投影，composer_snapshot 合并读取（投影代码零改动）；
         `composerDirect` flag 注入后置位（flag 关即回退纯投影）
Phase 5  交互模态族收尾：PlanReviewPanel → 统一交互弹窗 ✅ 完成（2026-08-07）：
         交互模态体系收敛——permission/ask/plan 三模板共用 P-6 覆盖层容器
         （interaction_overlay.rs），协议统一（pendingInteractions 单一队列）
         ✅ 读路径直连（2026-08-07）：交互队列状态机迁 Rust——daemon
         control/tool 事件在 bridge.rs 直接解析组装 InteractionState
         （InteractionMachine，permission 优先、ask/plan 单一活动槽、幽灵
         自愈），不经 WebView；`interactionDirect` flag 注入后 Web 停发
         setInteraction 投影（flag 关即回退投影路径，桥契约不变）；
         缓存跟随 active_seed（后台会话交互挂起，切回才显示，对齐 Web
         activeEntry 语义）；写路径（emit action → Web handler → daemon
         协议请求）仍两跳，为下一个迁移块
Phase 6  聊天视图（ChatView）迁移评估（难度极高，见 ChatView 迁移分析——富文本/
         markdown/流式渲染需 Rust 渲染管线，暂留 WebView2）
Phase 7  Info 面板合并 ✅ 部分完成（2026-08-07）：任务进度区块移入 info_panel
         （dashboard 投影）；stats 图表不迁移——调研发现 Web 侧 telemetry 历史
         从未被填充（死字段），图表无数据可显示；"上下文占用" Info 面板已有
         等价进度条；telemetry 采集补全列待办
```

原则：**单向数据流**（daemon → Rust → XAML 原生渲染 + 同步进 WebView），
避免 web store 与 XAML 状态双写；聊天流/时间线/富文本留在 WebView2。

### 已交付 XAML 视图清单（真实进度）

| 视图 | 壳组件 | 状态 | 日期 |
|---|---|---|---|
| 侧栏（会话列表） | `sidebar.rs` + `shell_store.rs` | ✅ | Phase 2 |
| 标题栏（ThreadHeader 8 actions + 主题同步） | `header.rs` | ✅ | 2026-08-06 |
| 首页（StartupView） | `home_view.rs` | ✅ | P1 |
| 设置页 | `settings_view.rs` | ✅ | 2026-08-06 |
| 技能页 | `skills_view.rs` | ✅ | WORKFLOW §8 |
| Info 面板（InfoPopover） | `info_panel.rs` | ✅ | P4a |
| 交互模态（权限/Ask） | `interaction_overlay.rs` | ✅ | 2026-08-07 |
| Composer 底部栏（阶段 A+B） | `composer_bar.rs` | ✅ | 2026-08-07 |
| PlanReviewPanel / 更新确认 / 托盘 / 关闭行为 | — | ⏳ 待办 | — |
| 聊天流（ChatView） | — | 🔴 留 WebView2 | 难度分析见上 |

### 剩余未迁移总清单（截至 2026-08-07，真实进度）

**A. 壳能力（Electron 曾提供，WinUI 未迁）**

| 项 | 现状 | 优先级 |
|---|---|---|
| 托盘图标与菜单（Tray） | 无 | P1 |
| 关闭行为（最小化到托盘/退出确认）+ 优雅退出（停 daemon 防孤儿） | 关窗直接退出 | P1 |
| 自动更新链路（checkUpdate/stageUpdate/applyUpdate stub） | 底层 deepx-updater 已就绪，缺壳封装 | P1 |
| backend.restart（运行环境切换） | 未实现 | P1 |
| desktop.confirm 确认对话框 | renderer 无调用点（XAML ContentDialog 绑定已有） | P2 |
| DevTools 快捷键（F12） | 未绑定 | P2 |
| 桌面宠物 | 建议隐藏 UI 入口，不移植 | P2 |
| 窗口背景材质 setBackgroundMaterial | 壳固定 Mica | P2 |

**B. Web 组件仍在 WebView2**

| 组件 | 现状 | 说明 |
|---|---|---|
| ChatView 主视图（transcript 全栈） | 🔴 留 Web | 难度分析见 `PLAN-NATIVE-CHATVIEW.md`（富文本基座前置） |
| ├─ process 时间线族 | 🟡 可先迁 | 纯 JSON，1-2 天 |
| ├─ 回合壳（气泡/状态/usage） | 🟡 可迁 | turnProjection 移植 |
| ├─ MarkdownBody 富文本 | 🔴 前置依赖 | reactor 无 RichTextBlock |
| ├─ GitDiffPanel / ChangeReviewPanel | 🔴 | diff 高亮无现成方案 |
| └─ ContextPanel + StreamMetricsChart（stats） | 🟡 | Info 已迁，stats 未迁（chart 可自绘） |
| PlanReviewPanel（plan 交互） | 🟡 部分 | interaction 覆盖层只接管 permission/ask |
| Toast 通知 | 🟡 简单 | InfoBar 可迁 |
| SessionCard / StartupView 等 flag 隐藏组件 | ✅ 壳接管 | Web 代码保留（debug 回退） |

**C. 增强项待办**

| 项 | 说明 |
|---|---|
| goalBar 展开列表（TodoStatusStrip 二期） | 现为计数徽标 + 当前任务 |
| FileOpenPicker 升级 | 全 XAML 终局待办（reactor 无 HWND 封装） |
| reactor 富文本基座 | ChatView 前置（RichTextBlock + markdown 管线） |
| Web store 层终局迁移 | ringingStores/timelineMonitor → bridge 缓存（shell_store 模式） |

## 参考

- `apps/winui/README.md` — 壳架构与构建
- `apps/winui/src/bridge.rs` — 桥实现（stub 处即未迁移项）
- `apps/desktop/electron/preload.ts`（已删除）— API 形状基线
- `crates/deepx-client/src/timeline.rs` — timeline 迁移参考实现
