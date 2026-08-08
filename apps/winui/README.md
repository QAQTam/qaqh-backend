# DeepX WinUI Desktop Shell（windows-reactor）

**全原生 WinUI3 桌面壳**：Mica 窗口承载原生 XAML 视图族（侧栏/标题栏/
ChatView/Composer/首页/技能/设置），`bridge.rs` 通过 `deepx-client`
（Ringing V1 HTTP/SSE）直连 daemon。**WebView2、Electron、SolidJS renderer
已整体移除**——前端 100% Rust 原生，构建链零 node/pnpm 依赖。

## 迁移历程（历史）

- **Electron → WinUI（beta7 起）**：先以 Electron 快速达成功能对等，再替换为
  更贴近 Windows 原生的壳。协议层先行（Ringing V1 定型于 2026-07-31），
  `deepx-client` 承接 Electron main 的传输职责，renderer 经 `window.deepx`
  桥零改动切换宿主。
- **WebView 移除（beta.8）**：`invoke/emit/outbox` 通道整体下线，三 SSE 频道
  事件由 Rust 直连解析（conversation → ChatView/Composer、control/tool →
  交互队列、control → 侧栏/技能/goalBar 快照），命令/查询 Rust 直发。
- **renderer 源码删除（beta.8 收官）**：SolidJS renderer（`apps/winui/renderer`）、
  `deepx-bridge.js`、patch/serve 脚本全部移除；daemon `/debug/` 静态服务保留
  框架，不再承载前端产物。

> 迁移细节与 Web 端 golden reference 见 `docs/winui-native-migration.md`
> （最终迁移文档）与 `docs/windows-reactor-skill.md`。

## Architecture

```text
DeepX.exe（WinUI3 壳，windows-reactor）
├─ 原生视图族（XAML 控件树，reactor diff 只更新变化节点）
│  ├─ sidebar / header / home / skills / settings / interaction_overlay
│  └─ chat 区：composer_bar + chat_view
│     └─ chat_view：16ms XAML 帧合并 → Transcript 状态 → keyed ListView 声明式渲染
│        （turn 壳 + thinking 气泡框 + tool 折叠卡 + live/final 富文本）
└─ bridge.rs（UI 线程侧）
   └─ BridgeCore（tokio 侧）：deepx-client 直连 daemon
      ├─ conversation 频道 → chat_events 队列 / composer 活动 / 交互队列
      ├─ control 频道 → 侧栏/技能/标题栏/交互状态
      ├─ tool 频道 → 权限队列
      └─ timeline 流 → 快照缓存（ChatView 恢复历史）+ 失联检测
```

- **数据流**：daemon 事件经 SSE 三频道 + timeline 流进入 `BridgeCore` 缓存；
  UI 侧 DispatcherTimer 泵（16ms/250ms）drain 后触发重渲染；命令/查询
  （发送消息、会话管理、技能操作）Rust 直发，不经 Web 中转。
- **ChatView**：快照恢复历史（seed 权威标记 + 子对象解包）、流式 live 渲染
  （贴底滚动 + 50ms 跟尾节流 + 16ms 帧合并）、provider 工具状态
  （`provider_tool_status` → 折叠工具卡）、思考链路气泡框。

## 状态矩阵

| 视图 | 状态 | 数据源 |
|---|---|---|
| 侧栏 | ✅ | control 会话快照 + activity 事件 |
| 标题栏 | ✅ | headerDirect 本地组装（navigate/快照/事件） |
| ChatView | ✅ | conversation 事件直连 + timeline 快照 |
| Composer | ✅ | composer 直连（A 组事件 + B 组本地） |
| 首页 | ✅ | sessions 快照 + 新建会话动作直发 |
| 技能页 | ✅ | control `skills_updated` 快照 |
| 设置页 | ✅ | `config.load` + 权限级直连 |
| 交互模态（permission/ask/plan） | ✅ | control/tool 事件状态机 |
| Info 面板 | ✅ | bootstrap `conversation.state` 投影 |

## 目录结构

```text
apps/winui/
├── src/
│   ├── main.rs            # App 入口、开屏覆盖层、视图族布局
│   ├── bridge.rs          # BridgeCore：连接/事件解析/命令直发/健康重建
│   ├── chat_view.rs       # 原生 ChatView（事件泵 + ListView 虚拟化）
│   ├── chat_adapter.rs    # wire 事件 → 渲染协议 + timeline 快照解析
│   ├── composer_bar.rs    # 底部输入栏（发送/附件/门控）
│   ├── sidebar.rs         # 侧栏（会话列表 + 导航）
│   ├── header.rs          # 标题栏（Mica 拖拽区）
│   ├── home_view.rs       # 首页（会话卡片 + 快捷发送）
│   ├── skills_view.rs     # 技能页
│   ├── settings_view.rs   # 设置页
│   ├── info_panel.rs      # 会话用量面板
│   ├── interaction_overlay.rs # 权限/ask/plan 模态
│   └── shell_store.rs     # 侧栏/设置/用量/交互解析
├── scripts/
│   ├── prepare-daemon.ps1 # sidecar 预置（daemon/workspace + manifest）
│   └── assemble-winui.ps1 # 组装 release/winui-app 运行目录
└── Cargo.toml / build.rs  # windows-reactor（F:/windows-rs path 依赖）
```

## Dependencies

- `windows-reactor`：声明式原生 UI 框架（本地 fork `F:/windows-rs/crates/libs/reactor`），
  含 ListView 虚拟化、reconciler diff、贴底滚动（ScrollToVerticalOffset）
- `deepx-client`：Ringing V1 客户端（HTTP/SSE + timeline 流）
- `markdown-winui` / `markdown-core`：流式 markdown 渲染（live/final、表格、代码块）

## Build & run（dev）

```powershell
# 1. 编译 dev daemon（否则 discovery 会回退 PATH 上的安装版 daemon）
cargo build -p deepx-daemon -p deepx-workspace

# 2. 运行壳（自动拉起 daemon）
cargo run -p deepx-winui
```

诊断日志：壳侧写 `%TEMP%\deepx-winui.log`（chat_view）与工作目录
`.deepx-winui.log`（main/bridge，`DEEPX_WINUI_LOG` 可覆盖）。

## Packaging（just winui-package）

```text
winui-app/
├── DeepX.exe                     # 壳（安装器硬编码入口名）
├── <WinAppSDK self-contained DLL / PRI / MUI>
├── resources/
│   ├── deepx-daemon.exe / deepx-workspace.exe / daemon-manifest.json
└── config/config.toml
```

链路：`build-daemon + build-winui` → `prepare-daemon.ps1`（sidecar 预置，
校验版本锁/协议/build_id）→ `assemble-winui.ps1`（运行目录）→
`collect-payload-winui.ps1`（bundle.json）→ `finalize.ps1`（SFX）。

## 参考

- `docs/winui-native-migration.md` — WebView → WinUI3 最终迁移文档
- `docs/windows-reactor-skill.md` — windows-reactor 开发要点
- `docs/winui-chat-rendering-maintenance.md` — ChatView 单一渲染路径与上游同步门禁
- `apps/winui/CHATVIEW-RENDERING-REFERENCE.md` — ChatView 渲染规格
- `crates/markdown-winui/` — 流式 markdown 渲染 crate
