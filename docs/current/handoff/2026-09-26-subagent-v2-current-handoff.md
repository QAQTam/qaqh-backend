# Subagent V2 Current Handoff

> 日期：2026-09-26
> 基线：`2.0.0-alpha3`
> 工作方式：直接在 `main` 推进，不创建 worktree
> 状态：alpha3 released checkpoint；Phase 0-7 backend + TUI/WinUI 消费完成；Beta 身份迁移待做
> 范围：`qaqh-domain`、`qaqh-session`、`qaqh-subagent`、`qaqh-runtime`、`qaqh-daemon`、`qaqh-client`

## 1. 一句话状态

Subagent V2 已从“spawn + result injection”升级为：

```text
canonical agent tree
  + stable AgentPath
  + durable parent-child edge
  + canonical mailbox
  + queue/trigger/interrupt delivery
  + unload/reload residency
  + status/residency Team projection
```

当前 alpha3 主链路已完成并冻结。剩余工作转入 Beta 硬门禁：身份迁移、
legacy resolver 删除、Windows 实机验证与 debug backlog。

发布说明：[`releases/2026-09-26-alpha3.md`](../releases/2026-09-26-alpha3.md)。

## 2. 已完成能力

### 2.1 Identity / AgentPath / Graph

- `AgentId = SessionId`，`AgentPath` 是稳定树地址。
- `/root`、`/root/<task>`、absolute/relative resolver 与非法 segment 校验已完成。
- `AgentCatalog` 保存逻辑 metadata；unload 不删除 identity。
- canonical `SubagentSpawned` / `SubagentFinished` 是 parent-child edge 的权威事实。
- `AgentGraphStore` 从 canonical facts 重建，不是第二可写事实源。
- `list_agents(path_prefix?)` 支持同 root tree 内 absolute / caller-relative prefix。
- child reload 后保留 `AgentPath`，不会退化成普通 root session。

### 2.2 Canonical mailbox / delivery

- canonical `InterAgentCommunication`：
  - `message_id`
  - `root_session_id`
  - `author` / `recipient` / `other_recipients`
  - `task_id` / `reply_to` / `causation_id`
  - `delivery = queue | trigger | interrupt`
  - `created_at_ms`
- canonical `InputAccepted.client_request_id` 必须匹配 communication 的 `message_id`。
- `MailboxProjection`：
  - communication 首次出现为 queued；
  - 匹配 InputAccepted 后为 delivered；
  - 重复 message id 幂等；
  - `last_activity_fact_seq` 供 `wait_agent` 使用。
- 目标 session 写入顺序固定为：
  ```text
  InterAgentCommunication
    -> InputAccepted
    -> turn / queue-only injection
  ```

### 2.3 Agent communication tools

- `send_message`：Queue，只进 mailbox，不启动 idle agent。
- `followup_task`：Trigger，idle 时启动 turn。
- `wait_agent`：
  - 只等待 caller mailbox activity；
  - 不返回正文；
  - timeout `1000..=3_600_000ms`，默认 `30000ms`；
  - 25ms 轮询 committed canonical facts，并观察工具取消。
- `interrupt_agent`：
  - 只发送 `ConversationCancel`；
  - root/self interrupt 拒绝；
  - unloaded target 返回 `unloaded`，不 reload；
  - identity、AgentPath、canonical edge 全部保留。
- child completion：
  - queue-only 投递父 mailbox；
  - 不再默认 `TriggerTurn` 父会话；
  - 父代理必须显式 `wait_agent` 或在下一 turn 处理。

### 2.4 Initial task / completion

- subagent initial task 已改为 canonical inter-agent communication。
- collector command、`InterAgentCommunication`、`InputAccepted` 共用同一个 message id。
- `SubagentFinished` terminal notification 独立写入父 canonical log。
- completion result 不再依赖文本前缀或工具卡 JSON 推导 agent identity。

### 2.5 Residency / reload

- `SubagentSpawned.spawn_config` 持久化：
  - tools
  - model / base_url / max_tokens
  - ephemeral
  - timeout_secs
- unloaded child 由 delivery 经 **loaded immediate parent** reload：
  - 读取 parent canonical spawn config；
  - 以 `AgentKind::Subagent` 重建；
  - 恢复 supervisor parent edge；
  - 不重复消费 spawn quota。
- immediate parent unloaded 时 child reload fail closed。
- Trigger delivery 在写入命令前重新 arm collector。
- idle subagent 纳入 residency LRU，可独立 unload。
- unload 不删除 canonical metadata，后续仍可 list / delivery reload。

### 2.6 Status / residency

- `list_agents` 返回显式：
  - `ListedAgentStatus`
  - `ListedAgentResidency`
- status 来源：
  - canonical `TurnStarted` / `TurnInterrupted` / interaction facts；
  - unloaded child 的 parent `SubagentFinished`；
  - loaded 时叠加 activity tracker。
- residency 来源：
  - registry 显式生命周期表；
  - 不从 worker table 或进程表猜测。
- 不变量：
  - `unloaded != completed`
  - `completed != closed`
  - interrupt 不等于 delete。

### 2.7 Parent-owned direct input gate

`AgentRegistry::send_ringing` 在加载/reload 目标前检查：

```text
target is non-root child
AND ConversationSendMessage
AND inter_agent == None
-> reject
```

结果：

- app-server/UI 不能绕过 parent ownership 直接给 child 注入用户消息；
- 拒绝不会意外 reload child；
- `send_message` / `followup_task` / initial task 不受影响。

### 2.8 Team projection 后端

- `ProjectionSlot::Team = 6`。
- `TeamAgentStatus` / `TeamAgentResidency`。
- `TeamAgentSnapshot` / `TeamInboxSummary` / `TeamSnapshot` / `TeamDelta`。
- reducer 消费：
  - `SessionCreated`
  - `SubagentSpawned`
  - `SubagentFinished`
  - `TurnStarted`
  - `TurnInterrupted`
  - `InterAgentCommunication`
  - `InputAccepted`
- `ProjectionSetSnapshot` 包含 `team`，旧 snapshot 通过 serde default 兼容。
- `TeamDelta::AgentResidencyChanged` 已有真实 producer。

### 2.9 Runtime residency overlay

- canonical `SessionCreated` / `SubagentSpawned` 默认 materialize 为 `unloaded`。
- registry spawn/reload overlay `loaded`。
- registry unload/close overlay `unloaded`。
- `V2ProjectionHub`：
  - 保存当前进程 runtime overlay；
  - bootstrap snapshot 包含 overlay；
  - 广播 ephemeral `TeamDelta::AgentResidencyChanged`。
- overlay 不写 canonical log；daemon 重启后自然回到 `unloaded`。
- 相同 residency 重复设置幂等。

### 2.10 Phase 3 配额与树深

- `subagent.maxDepth` 默认 1，可配置（`qaqh-config` / `qaqh-config-api`）。
  - root depth = 0；`maxDepth = 1` 允许 root spawn child，拒绝 grandchild。
  - `AgentRegistry::set_max_depth` 是唯一运行时入口。
- `subagent.messageInFlightPerPair` 默认 16，0 = unlimited。
  - in-flight 从目标 canonical mailbox 的 queued `InterAgentCommunication`
    （尚无匹配 `InputAccepted`）实时计算，不维护第二事实源。
- `subagent.messageOutboundPerSender` 默认 1024，0 = unlimited。
  - 按 root tree + author AgentPath 计 runtime attempt；daemon 重启后重置。
- `@all` / 其他 `@` broadcast target 在 host 入口稳定拒绝；
  `InterAgentCommunication.other_recipients` 非空在 canonical producer 入口拒绝。
- `close_agent` 已不在 V2 工具表；`interrupt_agent` 保留逻辑身份，
  residency eviction 另行发生。

## 3. 当前验证门禁

最近一次全量验证：

```text
cargo fmt --all -- --check
cargo clippy --workspace --all-targets --offline -- -D warnings
cargo test --workspace --offline -- --test-threads=1
```

定向覆盖包括：

- AgentPath resolver / graph rebuild / cascade unload；
- canonical mailbox queued -> delivered；
- initial task / send_message / followup_task / wait_agent / interrupt_agent；
- completion queue-only；
- unloaded child reload through loaded immediate parent；
- idle LRU unload + reload；
- `list_agents` status/residency；
- parent-owned direct input reject；
- max depth 1 拒绝 grandchild；depth 2 显式 opt-in 后 cascade close 通过；
- message in-flight / outbound 超限稳定拒绝；
- broadcast `@all` 与 `other_recipients` 拒绝；
- `close_agent` 不在 V2 工具表，`interrupt_agent` 保留身份；
- Team reducer、ProjectionSet、runtime residency overlay；
- daemon restart 后 residency 回到 unloaded。

## 4. 接下来优先级

### P0：持久化 content store + 大正文 `content_ref`（已完成主体）

已于 `2480575` 落地：

- durable content store（metadata/body 落盘、懒加载 body、引用计数、TTL/pin）；
- `InputAccepted` 大正文外置为 `content_ref`，不再跳过 canonical fact；
- `InterAgentCommunication` 大正文外置为 `content_ref`，不再直接拒绝；
- ask / plan / permission 正文 pin；permission 在 `ToolFinished` 终结时 unpin；
- 重启后按持久化 `pin_key` unpin。

仍未完成的是「重启后挂起 turn 继续执行」：orphan seal 仍按 dismiss 收尾，
需要产品裁决「重启即 dismiss」或单独实现 turn resume。详见
[`2026-09-26-durable-content-store-handoff.md`](./2026-09-26-durable-content-store-handoff.md)。

### P0：Phase 3 收尾（已完成）

- max depth 默认 1，可配置；默认拒绝 grandchild；
- sender-target in-flight 上限（从 canonical mailbox 实时计算）；
- sender outbound attempt 上限（runtime safety-valve counter）；
- broadcast / `@all` 默认拒绝；
- V2 工具表不再提供 legacy `close_agent`；`interrupt_agent` + residency eviction
  是唯一终止语义。

剩余边界：outbound attempt 计数不跨 daemon 重启持久化（安全阀语义），in-flight
以目标 canonical mailbox 为准，不在 send 入口维护第二事实源。

### P1：Phase 4 前端 Team projection

- TUI/WinUI 消费 TeamSnapshot / TeamDelta；
- roster 以 AgentPath 为主、nickname 为辅；
- unloaded 显示为 unloaded，不是 deleted；
- inbox 显示 author / recipient / task / delivery；
- child transcript；
- `@` 使用结构化 mention；
- 删除从 `spawn_subagent` 工具卡推导身份的旧路径。

### P2：Task board

- task graph；
- owner；
- claim / epoch；
- dependencies；
- artifact / acceptance；
- 与 message 通过 `task_id` 绑定。

已落地：

- spec：[`2026-09-26-team-task-board.md`](../spec/2026-09-26-team-task-board.md)；
- TEAM-01a/b：`qaqh-session::team` canonical types、state-machine reducer、
  durable append-only TeamStore（`team.json` / `events.jsonl` / `events.commit` /
  `events.lock`），覆盖 create/claim/release/dependency/artifact/acceptance/
  complete/close/cancel 与 torn-tail replay；
- TEAM-01c：`TaskBoardHost` + 5 个 runtime 工具（`task_create` / `task_claim` /
  `task_update` / `task_close` / `task_list`），按 caller root tree 打开 team store，
  root 初始化 `TeamCreated`，task 数量上限 1024；
- TEAM-01d：`GET /ringing/v2/sessions/{seed}/team` 返回 team roster +
  task board snapshot；`TeamDelta::TaskChanged` 作为 ephemeral delta 在单流发布；
  task board backend 已可供 TUI/WinUI 消费。

仍未完成：TEAM-01e TUI/WinUI 渲染与 `@` mention 结构化接入。

### P2：Message board

- channel / thread / post / subscription；
- 只通知 running agent；
- post 持久化；
- missed notification 不保证补发；
- 与 task board 通过 `task_id` 关联。

已落地：

- spec：[`2026-09-26-team-message-board.md`](../spec/2026-09-26-team-message-board.md)；
- BOARD-01a：`qaqh-session::team::board` 独立 `BoardFact` / `BoardStore`，
  覆盖 channel / thread / post / subscription、torn-tail replay 与资源上限；
- BOARD-01b：`BoardHost` + 5 个 runtime 工具（`board_channel_create` /
  `board_thread_create` / `board_post` / `board_subscribe` / `board_list`），
  按 caller root tree 打开 board aggregate，并校验同 tree `task_id`；
- BOARD-01c：`GET /ringing/v2/sessions/{seed}/team` 增加 `board` snapshot；
  `TeamDelta::BoardChanged` 作为 ephemeral delta 在单流发布；
- notification 仅向 running + loaded subscriber 做 queue-only best-effort，
  post 先 durable，通知失败/跳过不回滚；idle/unloaded agent 不启动。

仍未完成：BOARD-01d TUI/WinUI board 渲染与 unread 水位。

### P3：Steer / Interject

- 在 queue / trigger / interrupt 稳定后实现；
- 明确 safe point；
- 优先级；
- 配额；
- 防止 agent 互喷。

已落地：

- spec：[`2026-09-26-steer-interject.md`](../spec/2026-09-26-steer-interject.md)；
- SUBV2-11a：`InterAgentDelivery::{Steer, Interject}` 与
  `ConversationInputPurpose` / canonical `InputPurpose` 扩展；
- SUBV2-11b：safe point 固定为“工具批完成、下一模型请求前”，同 lap 排序为
  `interject -> steer -> queue`，单 safe point 上限为 8 steer / 4 interject，
  超出部分保留到下一 safe point；
- SUBV2-11c：`steer_agent` / `interject_agent` 工具、canonical
  `InterAgentCommunication` + `InputAccepted`、child -> root steer/interject
  安全门；
- `interrupt` 仍是唯一取消当前 turn 的 delivery；steer/interject 不中止工具批。

仍未完成：SUBV2-11d TUI/WinUI 区分展示 steer / interject / interrupt。

### P3：Beta 身份迁移

- wire/runtime 从 `seed` 改名 `session_id`；
- TUI/WinUI 删除 8 位 hex 假设；
- 删除剩余 `generate_seed()` 路径；
- 删除 legacy alias / migration resolver；
- 清理旧目录与 legacy writer。

已推进：

- runtime 生产路径已不再调用 `generate_seed()` / `generate_unique_seed()`；
- 无 manager 的 ephemeral session 也使用 canonical UUIDv7 `SessionId`；
- legacy seed allocator / resolver 仍保留在 `qaqh-session`，供旧会话兼容窗口使用。

## 5. 关键文件

- `crates/qaqh-session/src/session_fact_v2/agent.rs`
- `crates/qaqh-session/src/session_fact_v2/types.rs`
- `crates/qaqh-session/src/session_fact_v2/validation.rs`
- `crates/qaqh-session/src/projection/mailbox.rs`
- `crates/qaqh-session/src/projection/team.rs`
- `crates/qaqh-session/src/projection/set.rs`
- `crates/qaqh-runtime/src/agent_catalog.rs`
- `crates/qaqh-runtime/src/agent_graph.rs`
- `crates/qaqh-runtime/src/registry.rs`
- `crates/qaqh-runtime/src/host_impl.rs`
- `crates/qaqh-runtime/src/ringing/v2.rs`
- `crates/qaqh-runtime/src/service.rs`
- `crates/qaqh-subagent/src/host.rs`
- `crates/qaqh-subagent/src/lib.rs`
- `crates/qaqh-runtime/tests/host_direct.rs`

## 6. 接手注意事项

- canonical facts 是唯一可写事实源；graph/catalog/projection 都是可重建索引。
- `AgentId = SessionId`；`AgentPath` 是稳定树地址。
- 不恢复 v1 wire 兼容；不要通过正文 regex 或工具卡 JSON 推导 identity。
- communication 必须先于匹配的 `InputAccepted` 写入。
- message id 必须贯穿 command、communication、InputAccepted。
- `unloaded != completed != closed`。
- residency 是 runtime overlay，不得升级成 durable canonical fact。
- child reload 必须经 loaded immediate parent，不能回退 `get_or_spawn`。
- completion 必须保持 queue-only，禁止恢复无界父 turn 唤醒。
- `close_agent` 是 legacy；V2 使用 `interrupt_agent + residency eviction`。
- 大正文不得伪造 dangling `content_ref`；必须先有 durable content store。
- 不创建 worktree；直接在 `main` 推进，避免重复编译产物。
