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

## 混合 XAML + WebView2 路线图（下一步）

`windows-webview::webview()` 返回 `windows_reactor::WebView2`（XAML 控件，可嵌入布局树），混合方案可行：

```
Phase 0  桥协议扩展（nav/settings 通道）+ Grid 布局（XAML TitleBar/NavigationView + WebView2 内容区）
Phase 1  搬壳层：ContentDialog（confirm/权限/更新）+ MenuBar + 状态栏
Phase 2  搬侧边导航 + 会话列表（NavigationView，数据走 Rust 查询）
Phase 3  搬设置页（表单控件齐备，状态同步为核心工作）
Phase 4  可选：Composer 搬迁评估（耦合深，风险高）
```

原则：**单向数据流**（daemon → Rust → XAML 原生渲染 + 同步进 WebView），
避免 web store 与 XAML 状态双写；聊天流/时间线/富文本留在 WebView2。

## 参考

- `apps/winui/README.md` — 壳架构与构建
- `apps/winui/src/bridge.rs` — 桥实现（stub 处即未迁移项）
- `apps/desktop/electron/preload.ts`（已删除）— API 形状基线
- `crates/deepx-client/src/timeline.rs` — timeline 迁移参考实现
