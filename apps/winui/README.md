# DeepX WinUI Desktop Shell（windows-reactor）

Mica 窗口 + WebView2 承载 SolidJS renderer，通过完整 `window.deepx`
桥（WebMessage <-> `crates/deepx-client`）与 daemon 通信。
本目录是 **Electron 壳退役后的唯一桌面壳**（1.0.0-beta.7 起）。

---

## 为什么从 Electron 迁到 WinUI（beta7 节点）

### 文档依据

- **`docs/backend-dataflow/convergence-plan.md` §0（2026-08-04 已拍板）**：
  > "TUI/WinUI 后续重做时另起炉灶，不保留兼容边界。"
  —— WinUI 壳不是临时实验，而是架构计划内的正式目标。
- **`apps/winui/renderer/MIGRATION.md`（Tauri → Electron 迁移记录）**：
  团队先尝试过 Tauri（parity 实现），Electron 因功能对等快、生态成熟而胜出；
  本轮 WinUI 是**同一种模式的第二次迭代**：先以 Electron 快速达成功能对等，
  再替换为更贴近 Windows 原生的壳。
- **Ringing V1 协议定型（2026-07-31）**：传输层契约稳定后，壳层替换
  不触及协议语义——`deepx-client`（Rust）把 Electron main 的传输职责
  完整承接，renderer 通过 `window.deepx` 桥零改动切换。

### 动机推断（结合仓库事实）

| 维度 | Electron | WinUI（Rust） |
|---|---|---|
| 运行时 | 自带 Chromium + Node.js 主进程（~200MB） | WebView2 用系统组件；壳为 Rust 原生二进制（release ~4MB） |
| 技术栈 | 主进程 TS + node_modules 供应链（electron-builder 二进制下载与仓库 supply-chain 策略冲突） | 全栈 Rust（后端 16 crates 同栈），无 Node 运行时 |
| 平台定位 | 跨平台，但本产品安装器/更新器/壳均为 **Windows-only**（justfile 的 linux 段是占位报错） | 与定位一致，原生 Windows 优先 |
| 系统集成 | 需要桥接实现 Mica/托盘/对话框 | WinUI3 原生：Mica、`ContentDialog`、`NavigationView`、系统对话框 |
| 进程模型 | 多进程 + 沙箱配置成本 | WebView2 进程由系统管理，壳单进程 |

### beta7 节点的迁移思路

1. **协议层先行**：Ringing V1（HTTP/SSE）是唯一主通道——`deepx-client`
   用 Rust 重写 Electron 的 `controlClient/ringingClient/timelineClient`，
   传输语义（lease、cursor、gap 恢复）完全对齐；
2. **桥对等**：`assets/deepx-bridge.js` 复刻 preload 的 `window.deepx` API
   形状，renderer **零改动**切换宿主（Electron 下桥脚本自动跳过）；
3. **功能对等验证**（beta7 已完成）：timeline、`ringing.status` 状态表、
   三频道 bootstrap/事件回推、主链路（resume → bootstrap → 对话 → 流式）端到端验证；
4. **逐步原生化**（未来）：混合 XAML + WebView2，把壳层 UI（标题栏、
   导航、对话框、设置）迁入原生 XAML——见 `ELECTRON-MIGRATION.md` 路线图。

---

## Architecture

```
DeepX.exe (deepx-winui, STA UI thread)
  ├── Bridge (UI 线程: WebView 句柄 + outbox 泵, DispatcherTimer 50ms)
  │     └── deepx-bridge.js (patch-renderer.mjs 注入 index.html, 定义 window.deepx)
  │           └── WebMessage: invoke/response/event 三态 JSON 协议
  └── BridgeCore (Send, tokio 侧: Client + lease + 事件回调)
        └── deepx-client → daemon (Ringing V1: 3×SSE + bootstrap/command/query/action/timeline)
```

- **桥协议**: `{type:"invoke",id,method,params}` → `{type:"response",id,ok,value|error}`；
  事件 `{type:"event",kind:"ringing.batch"|"ringing.status"|"timeline.entry"|...}`
- **连接语义**: 壳持有 daemon lease，bridge.js 预连接；renderer 的
  browserBridge 因 `window.deepx` 存在而自动关闭（读多写少时兜底）
- **注入方式**: `apps/winui/scripts/patch-renderer.mjs` 把 `deepx-bridge.js`
  以相对路径 `<script src="./deepx-bridge.js">` 插入 renderer 产物
  （daemon 只服务 `/debug/` 下文件，绝对路径会 404）
- 不用 `add_script_to_execute_on_document_created`：该 API 内部
  `pump::wait`（Win32 GetMessageW 泵）在 XAML DispatcherQueue 事件回调里
  死锁（completed 回调走 DispatcherQueue，泵不到）

## 状态矩阵

| 能力 | 状态 | 备注 |
|---|---|---|
| Ringing 传输 / timeline / 桥 / 文件读取 / openPath / DevTools / 文件对话框 | ✅ | 与 Electron 对等（细节见 `ELECTRON-MIGRATION.md`） |
| 更新流 / backend.restart / 托盘关闭行为 / confirm / pet / 材质 | ❌ 未迁移 | 完整差距清单 + 优先级见 `ELECTRON-MIGRATION.md` |
| 混合 XAML 原生 UI | 🔜 路线图 | Phase 0-4 见迁移文档 |

## 目录结构

```
apps/winui/
├── src/                  Rust 壳（main.rs / bridge.rs / sidebar.rs / header.rs / shell/）
│   ├── header.rs         XAML 标题栏组件（TitleBar：title 槽 + footer 8 按钮，P0）
│   └── shell/mod.rs      壳组件共享工具（poll_rev：rev 轮询样板，P-4 预埋）
├── renderer/             Web renderer 源码（SolidJS + Vite，2026-08-06 从 apps/desktop 收编）
├── out/renderer/         构建产物（唯一产物目录；vite outDir + 桥注入）
├── assets/               deepx-bridge.js（桥注入脚本源）
├── scripts/              patch-renderer.mjs / serve-renderer.mjs / assemble-winui.ps1
├── release/winui-app/    发布运行目录（assemble 快照，可再生）
├── ELECTRON-MIGRATION.md 未迁移功能盘点 + 混合 XAML 路线图
├── PLAN-NATIVE-UI.md     P0 标题栏原生化规划（架构决策）
├── WORKFLOW-NATIVE-UI.md P0 具体工作流（落地版：任务拆解/预埋设计/后端边界确认）
└── Cargo.toml / build.rs
```

## Dependencies

- `windows-reactor` / `windows-webview` 为 **path 依赖**（`F:/windows-rs-master`，
  0.100.0 待 crates.io 发布后切换）
- `build.rs` `as_self_contained()`：首次构建从 NuGet 部署 WinAppSDK 2.3.1
  runtime + `Microsoft.Web.WebView2.Core.dll` 到 target/release

## Build & run（dev）

```powershell
# 1. 并行 dev daemon（独立 USERPROFILE 隔离，不动安装版 daemon）。
#    注意：必须编译出 dev daemon（cargo build -p deepx-daemon），否则壳的
#    discovery 会回退 PATH 连上**安装版 daemon**，服务的是安装目录的旧产物：
$env:USERPROFILE = "F:\DeepX\.deepx-test-home"
cargo build -p deepx-daemon
cargo run -p deepx-daemon -- run

# 2. renderer 产物 + 桥注入：
pnpm -C apps/winui/renderer build
node apps/winui/scripts/patch-renderer.mjs

# 3. 服务 dev 产物（file:// 会被 Chromium CORS 拦截 module bundle，
#    必须走 HTTP；勿用 DEEPX_UI_DIR 的 file:// 路径）：
node apps/winui/scripts/serve-renderer.mjs   # http://127.0.0.1:8642/

# 4. 壳（同样 USERPROFILE 隔离 + 指向本地服务）：
$env:USERPROFILE = "F:\DeepX\.deepx-test-home"
$env:DEEPX_DEBUG_URL = "http://127.0.0.1:8642/"
cargo run -p deepx-winui
```

URL 解析顺序：`DEEPX_DEBUG_URL` → 本地 renderer（WebView2 虚拟主机映射
`https://appassets.local/`，`DEEPX_UI_DIR` 或 exe 旁 `resources/out/renderer`）→
daemon discovery `/debug/` → about:blank。
本地映射模式下页面**不依赖 daemon 就绪**（秒开；daemon 连接由桥后台重试），
安装版始终命中 `resources/out/renderer`。`DEEPX_UI_DIR` 语义从 file:// 升级为
虚拟主机映射目录（WebView2 下 file:// 会拦 ES module 产物，勿回退 file://）。
调试日志：`$env:DEEPX_WINUI_LOG` 指向的文件（GUI 子系统无控制台）。

> 踩坑记录（2026-08-06）：`DEEPX_DEBUG_RENDERER_DIR` 是 **daemon 进程**的环境
> 变量（`debug_http::renderer_root`），壳不读它。稳定版 daemon 常驻时无法注入
> 该变量，且它服务的是安装目录的旧产物（旧 `deepx-bridge.js` 无
> `shell.onNavigate`，XAML 侧栏的导航事件会被静默丢弃）。dev 循环一律用
> `serve-renderer.mjs` + `DEEPX_DEBUG_URL`。

## Packaging（just winui-package）

```
build-daemon → prepare-daemon.mjs (sidecar) → build-desktop → build-winui
  → assemble-winui.ps1 (release/winui-app/)
  → collect-payload-winui.ps1 -Kind full → finalize.ps1 -Kind full
  → packages/DeepXInstaller-Full-*.exe
```

安装布局（对齐 Electron 结构，安装器/快捷方式/daemon 发现零改动）：

```
<install>/
  DeepX.exe                ← 壳（安装器硬编码入口名）
  *.dll / *.pri            ← WinAppSDK self-contained 运行时
  resources/
    deepx-daemon.exe / deepx-workspace.exe / daemon-manifest.json
    out/renderer/**        ← daemon 静态服务
  config/config.toml
```

daemon 发现顺序（`discovery::daemon_executable`）：
`DEEPX_BACKEND_ROOT/target/debug` → `<cwd>/target/debug` →
`<exe_dir>/resources/deepx-daemon`（生产）→ `<exe_dir>/deepx-daemon` → PATH。

## 参考

- `ELECTRON-MIGRATION.md` — Electron → WinUI 未迁移清单 + 混合 XAML 路线图
- `crates/deepx-client/` — Ringing V1 传输（三 SSE + timeline + lease）
- `apps/winui/renderer/MIGRATION.md` — Tauri → Electron 历史迁移记录
- `docs/backend-dataflow/convergence-plan.md` — legacy 拆除拍板（WinUI 另起炉灶）
- `docs/backend-dataflow/protocol-anchor.md` — Ringing V1 协议契约
