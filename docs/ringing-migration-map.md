# Legacy → Ringing 迁移映射清单

> 依据：`PLAN.md`（Ringing 双协议迁移计划）
> 来源盘点：`crates/deepx-proto/src/agent_protocol.rs`（Ui2Agent 21 命令 / Agent2Ui 42 事件）、
> `crates/deepx-proto/src/control.rs`（ControlServerMessage 10 帧 / ControlSnapshot / SessionActivity）
> 状态：阶段 0 产物，供 T4 起所有迁移工作引用。变更 Agent2Ui/ControlServerMessage 时必须同步本表。

## 1. Legacy 载体全景

```text
命令链（2 层）：
  FrontendToDaemon { seed, frame: Ui2Agent } ──WS──▶ daemon ──stdin JSON-LP──▶ agent worker
事件链（3 层）：
  agent stdout JSON-LP ──▶ Agent2Ui ──EventBus──▶ ControlServerMessage::Event/EventBatch ──WS──▶ frontend
  SessionActivity（独立生命周期流，不经过 Agent2Ui）
  ControlSnapshot.session_events（每 session 的 Agent2Ui 投影数组，resume/replay 用）
```

### 1.1 Ui2Agent 命令（21 个，`agent_protocol.rs:79`）

| # | Ui2Agent variant | 目标 RingingCommand | 频道 |
|---|---|---|---|
| 1 | `UserInput { text, images }` | `ConversationSendMessage` | Conversation |
| 2 | `ToolCall { id, name, action, args }` | `ToolInvoke` | Tool |
| 3 | `CreateSession` | `SessionCreate` | Control |
| 4 | `Cancel` | `ConversationCancel` | Conversation |
| 5 | `Shutdown` | `SessionShutdown` | Control |
| 6 | `ReloadConfig` | `AgentReloadConfig` | Control |
| 7 | `UndoTurn { turn_id }` | `ConversationUndoTurn` | Conversation |
| 8 | `Compact` | `ConversationCompact` | Conversation |
| 9 | `ResumeSession { seed }` | `SessionResume` | Control |
| 10 | `NewSession` | `SessionCreate`（变体，需显式区分） | Control |
| 11 | `LoadMoreTurns { before_turn_id, count }` | `ConversationLoadMore` | Conversation |
| 12 | `SetMode { mode }` | `ConversationSetMode` | Conversation |
| 13 | `PermissionResponse { tool_call_id, approved, trust_folder }` | `ToolPermissionRespond`（携带 expected_revision） | Tool |
| 14 | `AskResponse { ask_id, answers }` | `InteractionAskRespond` | Control |
| 15 | `AskDismiss { ask_id }` | `InteractionAskDismiss` | Control |
| 16 | `PlanReview { call_id, approved, message, autonomous }` | `PlanReviewRespond` | Control |
| 17 | `UnloadSkill { name }` | `SkillsRelease` | Control |
| 18 | `ActivateSkill { name }` | `SkillsActivate` | Control |
| 19 | `SkillOperation { operation_id, action, name, expected_revision }` | `SkillsActivate/SkillsRelease`（revision-safe 语义保留） | Control |
| 20 | `ReloadSkills` | `SkillsReload` | Control |
| 21 | （无对应——legacy RPC 域） | 保持 daemon query RPC，不迁移 | — |

### 1.2 Agent2Ui 事件（42 个，`agent_protocol.rs:455`）→ 按频道映射

#### Conversation 频道（13 个）

| # | Agent2Ui variant | Ringing 事件 | 可靠性 | Snapshot 资格 |
|---|---|---|---|---|
| 1 | `TurnStart { turn_id, user_text }` | `TurnStarted` | reliable | 部分（当前 turn） |
| 2 | `TurnEnd { turn_id, stop_reason, usage }` | `TurnCompleted`（成功）/ `TurnFailed`（由 Error/ProviderRetrying 终态推导，**新领域事件**） | reliable | 是 |
| 3 | `RoundDelta { turn_id, round_num, kind, delta }` | `RoundDelta` | reliable（增量追加；`RoundCompleted` 到达后按 round 压缩 journal） | 否 |
| 4 | `RoundComplete { thinking, answer, tool_calls, blocks, is_final }` | `RoundCompleted`（正文大时带 content_ref） | reliable terminal | 是 |
| 5 | `SessionRestored { turns, tokens_used, usage, … }` | 无直接事件 → `ConversationSnapshot` 经 HTTP GET | reliable | **是（快照本体）** |
| 6 | `MoreTurns { turns, has_more }` | `ConversationLoadMore` 的 query 结果（HTTP） | reliable | 否 |
| 7 | `ProviderRetrying { attempt, max_retries, delay_secs, error }` | `ProviderRetrying` | reliable（retry 与最终失败不同 event_id） | 否 |
| 8 | `UsageUpdated { usage, context_limit, model }` | `UsageUpdated` | replaceable（同 turn/round 覆盖） | 是（汇总） |
| 9 | `CacheDiagnostics { prefix_hash, prefix_changed, change_reasons }` | 保留为 domain 诊断（ephemeral 或去重为 replaceable） | ephemeral | 否 |
| 10 | `CompactStart { turns_total, turns_keeping }` | `CompactStarted`（携带 `compact_id`） | reliable | 是 |
| 11 | `CompactEnd { summary_chars, turns_compacted, turns_removed }` | `CompactFinished`（completed/skipped/failed/cancelled 明确状态） | reliable terminal | 是 |
| 12 | `CompactDelta { delta }` | `CompactProgress` | replaceable | 否 |
| 13 | `Cancelled` | `ConversationCancelled` | reliable | 是 |

> **Q3 决策（2026-07-31）**：`SearchStatus` 不保留原事件、不消化进 Tool 频道，改为
> Conversation 频道新事件 `ProviderToolStatus`（定义见 5.1），以 provider `call_id` 为合并键：

| 14 | `SearchStatus { status }`（provider 内建搜索） | `ProviderToolStatus`（新事件，见 5.1） | replaceable（按 call_id 合并/覆盖） | 否 |

#### Tool 频道（10 个）

| # | Agent2Ui variant | Ringing 事件 | 可靠性 | Snapshot 资格 |
|---|---|---|---|---|
| 1 | `ToolResults { results }`（成功结果） | `ToolFinished`（每 result 一个） | reliable terminal | 是 |
| 2 | `ToolResults`（含失败 result） | `ToolFailed`（**拆分**，错误结构化） | reliable terminal | 是 |
| 3 | `ToolExecDelta { tool_call_id, delta }` | `ToolProgress`（`tool_call_id + turn_id + round_num + stream + seq_start/seq_end`） | replaceable（16ms 合并、256KiB tail） | 否 |
| 4 | `ExecProgress { tool_call_id, stream, seq, chunk }` | `ToolProgress`（与 #3 同一事件类型，**流控合并**） | replaceable | 否 |
| 5 | `ToolCallPreview { index, id, name, args_so_far }` | `ToolCallPrepared` + `ToolStarted`（**新事件**，legacy 无对应） | replaceable → reliable | 部分 |
| 6 | `ToolNotice { message, level }` | `ToolNotice` | reliable | 否 |
| 7 | `AuditRecord { tool_name, result_summary, success, time, args }` | `AuditRecorded`（脱敏：args 只进 content store） | reliable | 是 |
| 8 | `CodeDelta { lines_*, files_*, file }` | `CodeChanged` | reliable | 是 |
| 9 | `PermissionRequest { tool_call_id, … }` | `ToolPermissionRequested`（含 risk/consequence 保留） | reliable | 是（pending 状态） |
| 10 | ~~`SearchStatus`~~ → 已移出 Tool 域，见 Conversation 频道 #14 `ProviderToolStatus` | — | — | — |

> 注：10 MiB+ 完整输出只进 content store（`RingingContentRef`），SSE 永不重发完整正文；terminal 前必须 flush/覆盖同 tool 的 progress。

#### Control 频道（15 个）

| # | Agent2Ui variant | Ringing 事件 | 可靠性 | Snapshot 资格 |
|---|---|---|---|---|
| 1 | `SessionCreated { seed }` | `SessionStateChanged`（created） | reliable | 是 |
| 2 | `Error { message }` | `OperationFailed`（`error_id + scope + code + retryable + dedupe_key + operation_id`） | reliable | 否 |
| 3 | `PlanSubmitted { call_id, plan_content, review_type, todo_items }` | `PlanReviewRequested` | reliable | 是（pending 状态） |
| 4 | `PlanResolved { call_id, approved }` | `PlanReviewResolved` | reliable | 否 |
| 5 | `Dashboard { hp_connected, tool_calls_total, … }` | `DashboardUpdated` | replaceable（覆盖式） | 是 |
| 6 | `Done` | `SessionActivityChanged`（Idle） | reliable | 是 |
| 7 | `ShutdownAck` | `AgentLifecycleChanged`（stopped） | reliable | 否 |
| 8 | `Ready` | `AgentLifecycleChanged`（ready） | reliable | 否 |
| 9 | `Pong` | 心跳保留为 HTTP/query 语义，不建事件 | ephemeral | 否 |
| 10 | `SkillsChanged { status }` | `SkillsUpdated` | reliable | 是 |
| 11 | `SkillOperationResolved { operation_id, success, revision, error }` | `SkillsUpdated`（operation 维度，携带 revision） | reliable | 否 |
| 12 | `AskUser { ask_id, mode, questions }` | `InteractionRequested`（ask） | reliable | 是（pending 状态） |
| 13 | `AskResolved { ask_id, resolution }` | `InteractionResolved` | reliable | 否 |
| 14 | `AskRejected { ask_id, message }` | `InteractionResolved`（rejected） | reliable | 否 |
| 15 | `SessionActivity`（独立类型，非 Agent2Ui variant） | `SessionActivityChanged` | reliable | 是 |

> `SystemNotice`（PLAN 列出的 Control 事件）legacy 无直接对应，为 Error 的非失败提示面（新领域事件）。

### 1.3 ControlServerMessage 帧（10 个，`control.rs:46`）→ 传输映射

| 帧 | 去向 |
|---|---|
| `ServerHello` | 能力协商 → 由 `POST /ringing/v1/clients/open`（Ringing_v1 / cutover / batch 能力）取代 |
| `Response { request_id, result }` | 对应 HTTP command ack（accepted/rejected）+ 终态事件（causation_id=command_id） |
| `Event { server_epoch, seq, seed, session_seq, event }` | 三条 SSE 对应频道的 `RingingEventEnvelope` |
| `EventBatch { events: Vec<Agent2Ui> }` | SSE 频道内 `RingingEventBatch`（batch 语义保留，但按频道拆分） |
| `SessionActivity { activity }` | Control SSE 的 `SessionActivityChanged` |
| `Snapshot { snapshot: ControlSnapshot }` | `GET /ringing/v1/snapshots/{channel}/{seed}`（**禁止事件数组模拟状态**） |
| `LeaseDenied` | `POST /ringing/v1/leases/renew` 的 4xx + retry_after_ms |
| `Error { code, message, retry_after_ms }` | HTTP 错误语义 + 结构化 `OperationFailed`（error_id） |
| `Heartbeat` | 被逻辑 lease TTL + renew 取代（SSE 断开不撤销 lease） |
| `Shutdown` | `SessionShutdown` 命令的可靠终态 |

## 2. 跨领域 Identity 清单（阶段 1 先迁移，避免 Tool 迁移后从字符串推断）

| identity | 现状（legacy） | Ringing 归属 | 迁移要求 |
|---|---|---|---|
| `turn_id` | 字符串，Agent2Ui/Ui2Agent 各 variant 内嵌 | Conversation 频道核心 | Tool 事件必须携带完整 `turn_id`，禁止从 legacy 字符串推断 |
| `tool_call_id` | ToolResults/ToolExecDelta/ExecProgress/PermissionRequest 内嵌 | Tool 频道核心 | 全部 Tool 事件携带 |
| `ask_id` | AskUser/AskResponse/AskDismiss | Control（Interaction） | Interaction id + expected revision |
| `call_id`（plan） | PlanSubmitted/PlanReview | Control（PlanReview） | 同上 |
| `operation_id` | SkillOperation/SkillOperationResolved | Control（Skills） | 保留 revision-safe 语义 |
| `call_id`（provider 侧） | SearchStatus 无独立 id，仅自由文本 status | Conversation（ProviderToolStatus） | provider web_search_call 的 id，与 DeepX `tool_call_id` 严格区分；同 round 可多次出现 |
| `session_seq` | ControlServerMessage 字段 | 保留为每 session/channel 因果序 | 与 stream_seq（连接序）分离 |
| `server_epoch` | ServerHello/Snapshot/Heartbeat | envelope 级字段，epoch 变化重置 stream_seq | 与 seed 无关 |

## 3. Snapshot 资格矩阵（每频道独立）

| 频道 | Snapshot 构建来源（禁止事件数组模拟） | 承载内容 |
|---|---|---|
| Conversation | `deepx-session` 持久化消息（SessionManager/MessageStore） | turns、rounds、usage 汇总、compact 状态、当前 turn/round |
| Tool | MessageStore + 当前工具运行状态（process_registry） | 工具卡状态、pending permission、进度 tail（≤256KiB/tool）、audit 汇总 |
| Control | session/control 状态（lease、activity、interaction pending、skills、plan review pending） | **不承载** Conversation/Tool 事件数组 |

## 4. 可靠性等级汇总（PLAN 硬规则映射）

- **reliable（进有界 journal，按 cursor 回放）**：全部 terminal（TurnCompleted/TurnFailed/RoundCompleted/ToolFinished/ToolFailed/CompactFinished）、生命周期（TurnStarted/TurnStarted 前 SessionStateChanged/AgentLifecycleChanged/SessionActivityChanged）、interaction 四件套、SkillsUpdated、AuditRecorded、CodeChanged、OperationFailed、ProviderRetrying（最终失败）、ConversationCancelled、**RoundDelta（增量是追加语义，覆盖/合并会吞字；journal 在 RoundCompleted 后按 round 压缩）**。
- **replaceable（按 identity 合并/覆盖，不进 journal 或仅稀疏 checkpoint）**：ToolProgress（ExecProgress/ToolExecDelta）、CompactProgress、UsageUpdated、DashboardUpdated、ToolCallPrepared（可被 ToolStarted 覆盖）、ProviderToolStatus（按 call_id 合并/覆盖）。
- **ephemeral（不 snapshot/journal）**：CacheDiagnostics、Pong、诊断性 live 提示。
- terminal 到达前必须 flush/覆盖同 identity 的 replaceable 事件（`state_revision` 作废旧 progress）。

## 5. 决策记录（2026-07-31 评审定稿）

1. **`ToolStarted` 触发点** → **双事件**：`ToolCallPrepared`（流式预览检测到调用，replaceable，可被覆盖）+ `ToolStarted`（permission 通过后执行真正开始，reliable）。prepared 与 started 之间以 `ToolPermissionRequested` 衔接。
2. **`RoundDelta.kind`** → **增量按 reliable 投递**：合并/覆盖会丢字（重连/慢消费只剩最后一个 delta），因此进有界 journal 按 cursor 回放；终端 `RoundCompleted` 携带权威全量并按 `state_revision` 整体覆盖，同时压缩该 round 的 journal 增量。
3. **`SearchStatus`** → **新增 Conversation 频道事件 `ProviderToolStatus`**（不消化进 Tool 频道、不保留自由文本 status），定义见 5.1。
4. **`CacheDiagnostics`** → **ephemeral 诊断事件**（保留 domain 形态，不进 journal/snapshot），不并入 OperationFailed。
5. **`MoreTurns`** → **HTTP query 直接返回**：`ConversationLoadMore` 命令 + 查询结果走 HTTP，不保留事件形态。
6. **`SystemNotice` 触发面** → **最小集**：仅系统级通知（升级、维护、daemon 重启等），ToolNotice 留在 Tool 频道，业务失败走 OperationFailed（单 error id 最多一个 toast）。
7. **`NewSession` vs `CreateSession`** → **合并为 `SessionCreate { close_current: bool }`**：一个命令 + 显式标志，幂等与切流单一路径。
8. **`Ready/ShutdownAck/Done`** → **修正映射表**：`AgentLifecycleChanged` 只含进程生命周期（ready/stopped），`Done` 走 `SessionActivityChanged`（Idle），三态不合一（transport/进程/业务分离硬规则）。
9. **`SessionRestored` 统计字段** → **ConversationSnapshot 内独立 `UsageSummary` 子结构**（tokens_used、usage、usage_totals、usage_requests、cache_hit_pct、cache_reported_requests 等聚合），`UsageUpdated`（replaceable）直接更新该子结构。
10. **`PermissionRequest` 与 `InteractionRequested`** → **完全分离**：permission 属 Tool 频道（`ToolPermissionRequested`），ask/plan 属 Control（`InteractionRequested`）；前端已有 `PendingInteraction.kind = permission/ask/plan` 判别联合，后端照此建模；两频道各自快照 pending，`SessionActivityChanged(WaitingUser)` 在 Control 汇总。

### 5.1 `ProviderToolStatus`（Q3 定稿）

替代 legacy `search_status`。服务端（provider 侧）自动执行的工具状态，不属于 DeepX 本地可控执行域。

```text
RingingConversationEvent::ProviderToolStatus {
  turn_id: string
  round_num: u32
  call_id: string        // provider web_search_call 的 id，不是 DeepX tool_call_id
  tool_kind: string      // 目前固定 "web_search"，为未来 provider 内建/服务端工具预留
  state: "in_progress" | "searching" | "completed"   // 封闭枚举，不得使用自由字符串
}
```

- 可靠性：`delivery = replaceable`；合并/覆盖 identity key 是 **`call_id`**（同一 round 可有多次独立 web_search_call，分别追踪）。
- 生命周期：正常以 `state = "completed"` 为该 call_id 终态；若 `RoundCompleted`/`TurnCompleted` 到达时该 call_id 仍未 completed，按 replaceable 通用规则作废，**不额外发结束事件**。
- 明确不做：不放进 Tool 频道、不套用 ToolCallPrepared/ToolStarted/ToolFinished 生命周期（无 DeepX 权限校验步骤、无本地 exec 输出流、不经过 admit_batch 授权路径，混入会污染"本地可控执行"语义边界）；state 用封闭枚举防 provider 侧字符串漂移。
- 扩展性：新增 provider 内建/服务端能力（如 custom_tool_call 类）时只增加 `tool_kind` 取值，不新增事件类型。

## 6. 使用约定

- 本表是 **阶段 0 基线的固定产物**；T4（domain/wire 骨架）之后，任何新 Ringing 类型命名以本表为准。
- 迁移一个频道时，先更新本表对应频道行，再动代码。
- 本表禁止记录消息正文、工具参数、provider 响应与凭据样例（PLAN 默认假设）。
