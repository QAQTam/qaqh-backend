# Legacy 协议范围与退役状态

本文记录旧协议范围、Ringing 替代关系和当前退役边界。原生 Ringing 路径的兼容投影已经
退役；底层 legacy worker 边界仍为后续 TUI/WinUI 重做保留。

## 1. 旧协议范围

Legacy 不是单一类型，而是一条完整链路：

```text
client
  └─ /control/v1 WebSocket
       ├─ ControlClientMessage / ControlServerMessage
       ├─ request / response / snapshot / event / heartbeat
       └─ Agent2Ui / Ui2Agent
            └─ daemon registry
                 └─ worker stdin/stdout JSON-LP
```

具体包括：

1. **桌面传输层**：`/control/v1`、`CONTROL_PROTOCOL_VERSION`、client/server hello、
   heartbeat、lease、snapshot、event replay 和 legacy request/response。
2. **daemon ingress**：`DeepxService::handle()` 中将 RPC 方法转换成 `Ui2Agent`，以及
   `AgentRegistry::send()` / `send_all()` 的 legacy JSON-LP 写入。
3. **worker 边界**：stdin 接收 `Ui2Agent` JSON-LP，stdout 输出 `Agent2Ui` JSON-LP；
   Ringing worker 目前仍保留 legacy wire fallback。
4. **旧事件投影**：历史上存在的 `DomainEvent -> Agent2Ui` 兼容投影，以及
   `ControlSnapshot.session_events` 的 legacy 事件数组。原生 Ringing 路径不再执行前者。
5. **旧客户端与验证**：TUI、`deepx-client::DeepxClient`、legacy daemon WebSocket 测试、
   protocol probe 和 canonical snapshot probe。TUI/WinUI 本轮明确不改，后续直接重做。
6. **生命周期管理**：`/control/v1/stop` 仍是 daemon 的管理接口；它与 transcript/command
   数据协议分开，暂不因 Ringing 数据链路而删除。

`deepx-companion` crate 已删除；`deepx-proto` 中遗留的 companion 类型暂作为后续独立清理项，
不混入本次 P1 删除。

## 1.1 Ringing 锁定与本轮同步删除

以下映射已锁定为 Ringing V1 的权威语义，并在删除对应旧事件前完成同步标记：

| 旧 `Agent2Ui` | Ringing 权威事件/传输 | 当前状态 | 删除依据 |
|---|---|---|---|
| `Pong` | `ControlServerMessage::Heartbeat`，由 `/control/v1` 控制传输负责保活 | **已锁定 / 已删除** | 当前 daemon、Electron 和 `deepx-client` 均使用控制层 heartbeat；无 `Agent2Ui::Pong` 生产者或 UI 消费者 |
| `SearchStatus` | `ConversationEvent::ProviderToolStatus`，Ringing conversation channel 的 replaceable 事件 | **已锁定 / 已删除** | Electron Ringing reducer 与回归测试消费 `provider_tool_status`；worker native emitter 已先写 `DomainEvent` |

本轮明确保留的旧协议边界：`Agent2Ui`/`Ui2Agent` 类型、`ControlServerMessage`、
`EventBus`、legacy worker JSON-LP，以及 TUI/WinUI 所需的事件。它们是隔离的后续重做
边界，不代表原生 Ringing 仍依赖旧投影。原生工具终态只使用 canonical `ToolFinished`；
legacy ingress 下的 `Agent2Ui::ToolResults` 仅为后续 TUI/WinUI 兼容保留。

删除规则：旧事件在本轮代码中不再是可构造、可解析或可分 lane 的协议成员；如果旧 worker
仍发送这两个 tag，应视为已退役版本，而不是恢复隐式双写。后续每批删除都必须先在此表中
登记 Ringing 替代、生产者、消费者和回归证据。

## 2. P1-A：runtime service/registry legacy ingress

涉及位置：

- `crates/deepx-runtime/src/service.rs`
- `crates/deepx-runtime/src/registry.rs`
- `crates/deepx-daemon/src/server.rs`
- `crates/deepx-daemon/src/ringing_http.rs`
- （已删除）`crates/deepx-runtime/src/ringing/legacy_projector.rs`

当前 Ringing HTTP 已能走 `RingingWorkerCommandEnvelope` / `DomainEvent`；旧 RPC 入口仍然
通过 `Ui2Agent` 发送，legacy WebSocket 仍然通过 EventBus 接收 legacy 事件。两者不再由
原生 Ringing DomainEvent 兼容投影连接起来。

退役工作：

1. 盘点 `DeepxService::handle()` 的每个 legacy command/query/action 方法，标注其 Ringing
   command、query、action 或 snapshot 替代物。
2. 将业务执行入口收敛到 `DomainCommand` / Ringing command；保留一个隔离的 legacy adapter，
   不再让新代码直接构造 `Ui2Agent`。
3. 将 `AgentRegistry::send()`、`send_all()`、legacy session projection 和 EventBus 投影
   迁移到仅兼容路径。
4. `LegacyProjector` 已在原生 Ringing 路径退役并删除；待 TUI/WinUI 重做完成后，再单独
   评估 `/control/v1` 数据连接、旧 snapshot event array 和剩余 legacy reducer/fixtures。
5. `/control/v1/stop` 作为独立生命周期接口单独评估，不与数据协议删除绑定。

## 3. P1-B：msglp/ringing_v1 legacy-shaped engine/emitter

涉及位置：

- `crates/deepx-msglp/src/ringing_v1/engine.rs`
- `crates/deepx-msglp/src/ringing_v1/types.rs`
- `crates/deepx-msglp/src/ringing_v1/paced_emitter.rs`
- `crates/deepx-msglp/src/ringing_v1/engine_input.rs`
- `crates/deepx-msglp/src/ringing_v1/engine_turn.rs`
- `crates/deepx-msglp/src/ringing_v1/engine_tool.rs`
- `crates/deepx-msglp/src/ringing_v1/engine_compact.rs`
- `crates/deepx-msglp/src/ringing_v1/engine_misc.rs`
- `crates/deepx-msglp/src/ringing_v1/loop_core.rs`

当前 worker 已能输出原生 Ringing worker envelope。Engine trait 和 Emitter 仍保留
`Ui2Agent` / `Agent2Ui` 的 legacy worker 边界，但原生 Ringing 命令的工具终态只输出
canonical `ToolFinished`；`PacedEmitter` 会阻止该路径的旧 `ToolResults` 双发。

退役工作：

1. 将 engine dispatch 的输入从 `Ui2Agent` 迁移为 `RingingCommand` 或明确的 domain command。
2. 将核心输出从 `Agent2Ui` 迁移为 `DomainEvent`；有序 transcript 继续使用独立
   `TimelineIntent`，不从 legacy 事件反推。
3. 将 `PacedEmitter` 的 pacing、causation、terminal 和 progress 语义保留到 native emitter；
   legacy worker 输出只作为 TUI/WinUI 后续重做的隔离边界。
4. 删除 `WriterEvent::Legacy`、legacy wire reader/writer fallback 和仅服务 legacy 的测试夹具。
5. 为每个命令和终态补齐 Ringing fixture：send、cancel、compact、undo、permission、ask、
   plan、skills、session lifecycle、progress、reconnect 和 terminal receipt。

## 4. 新协议出问题时的对照方式

出现 Ringing 回归时，按以下顺序对照旧协议，而不是恢复隐式双写：

1. **命令**：比较旧 `Ui2Agent` variant、字段默认值、seed、turn/tool/interaction id 和
   原先的 worker 行为。
2. **事件**：比较旧 `Agent2Ui` 的顺序、终态、重复事件、progress 丢弃策略和 snapshot 内容。
3. **传输**：比较 hello、cursor、重连、snapshot、lease 和错误码；确认问题属于 transport、
   envelope、domain projection 还是 UI reducer。
4. **因果关系**：确认 Ringing `command_id -> causation_id -> terminal event` 没有断链。
5. **恢复**：用旧 snapshot/replay 场景验证新 bootstrap/SSE 恢复，而不是重新引入第二条
   无权威的 legacy 队列。

只有在定位到明确边界后，才选择优化 Ringing 实现或重新搭线；不恢复 `Agent2Ui -> Ringing`
的临时桥接。

## 5. 删除前置条件

- Electron Ringing 主链测试稳定，协议字段和语义冻结；
- Ringing command/query/action 与 legacy 方法的映射表保持同步；
- 三频道 SSE、bootstrap、cursor、lease、terminal receipt 和交互恢复均有回归 fixture；
- TUI 与 WinUI 的后续重做方案已准备，但不阻塞当前主服务；
- 混合版本和失败恢复行为已明确；
- 删除顺序经过 `cargo check`、daemon integration test、Electron test 和 release packaging
  校验。
