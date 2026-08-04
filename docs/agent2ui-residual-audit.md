# Agent2Ui / legacy 协议残留判断（2026-08-05）

> 范围：全仓。目标：electron 主链路已 100% Ringing 化；以下清单为
> TUI/其他 UI 后续同步重构时的收敛依据（本次不删，避免破坏 TUI 编译）。

## 一、本次已清除（electron 链路 legacy 残留）

| 文件 | 处置 |
|---|---|
| `apps/desktop/electron/controlClient.ts` | Ringing-only 化：删除 legacy `/control/v1` WS 握手/roundTrip/心跳/ControlCursor/升级接管（-300 行） |
| `apps/desktop/src/runtime/backendClient.ts` | 删除 `requestLegacy`、`ControlMessage` 处理（session-activity/snapshot 推送已不存在） |
| `apps/desktop/src/runtime/sessionActivityClient.ts` + test | 删除（数据源切换为 `session.activity` 查询 + Ringing control store 实时覆盖） |
| `apps/desktop/src/runtime/controlCursor.ts` + test | 删除（legacy cursor 专用） |
| `apps/desktop/src/runtime/daemonLifecycle.ts` | 删除 `hasActiveDaemonWork`（legacy 升级接管专用） |
| `apps/desktop/src/App.tsx` | activities 数据源切换（修复 Ringing 模式下 TaskSidebar 活动状态不刷新的隐性 bug） |
| `apps/desktop/electron/main.ts` | DaemonControlClient 构造同步 |

## 二、保留的 Agent2Ui / legacy 协议（electron 零依赖）

| 位置 | 内容 | 判断 | 后续动作 |
|---|---|---|---|
| `crates/deepx-proto/src/agent_protocol.rs`（99 处） | `Agent2Ui`（36 变体）、`SessionActivity` 类型定义 | **类型定义保留**：`deepx-client`（TUI 依赖）引用 | TUI 迁移后整体删除 |
| `crates/deepx-proto/src/control.rs` | `ControlServerMessage`/`ControlClientMessage`/`ControlSnapshot`（legacy WS 协议帧） | **保留**：`deepx-client` 消费；daemon 已不实现 legacy WS 数据协议（`server.rs` L235 注释"已拆除"） | 同上 |
| `crates/deepx-runtime/src/event_bus.rs`（40+ 处） | `EventBus`（Agent2Ui 投影总线） | daemon 无消费者（`ControlServerMessage` 无 recv 方）；`publish_activity` 双发路径仍经它发 `SessionActivity`（Ringing 侧不受影响） | 可随 legacy 协议删除，`publish_activity` 迁移到 RingingHub 侧 |
| `crates/deepx-runtime/src/service.rs` L90-115 | `snapshot()`（legacy ControlSnapshot 构造） | **死代码**：全仓无调用方 | 可删（低优先级） |
| `crates/deepx-runtime/src/service.rs` L250-265 | `session.replay_events` legacy RPC | 保留：经 Ringing action 端点暴露，electron 不调用 | TUI 迁移后收敛 |
| `crates/deepx-runtime/src/service.rs` L911-925 | `persisted_session_projection` | 仅被上述两处引用 | 同上 |
| `crates/deepx-runtime/src/registry.rs` L616-650 | `agent2ui_channel`（测试模块） | 生产代码 `Agent2Ui` import 已 unused（编译 warning 证实） | 测试随协议删除 |
| `crates/deepx-msglp`（12 处） | 全部为注释 | **零生产残留** ✓ | 无 |
| `crates/deepx-domain`（6 处） | 注释 | **零残留** ✓ | 无 |
| `crates/deepx-client` + examples | legacy WS 客户端库 | TUI 依赖 | 随 TUI 重构 |
| `apps/deepx-tui`（9 处） | legacy 客户端 | 用户指定本次不处理 | 同步重构目标 |

## 三、结论

- **electron 主链路已完全脱离 Agent2Ui / legacy WS 数据协议**：连接协商
  （`/ringing/v1/clients/open`）、命令（三频道 commands）、查询（queries）、
  事件（三 SSE + bootstrap 快照）、活动状态（control store）全部走 Ringing V1。
- daemon 侧 legacy 类型/方法保留的唯一理由是不破坏 TUI（`deepx-client`）；
  与 electron 无任何数据耦合。
- 后续 TUI 重构时：删 `deepx-proto` legacy 协议定义 → 删 `EventBus`/legacy
  RPC 方法 → 删 `deepx-client`，即可完成全仓 legacy 拆除。
