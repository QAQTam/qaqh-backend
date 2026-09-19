# QAQH session-fact-v2 字段级 Schema、Cursor 与恢复契约

> **状态**：P0 冻结候选
> **Issue**：[#105](https://cnb.cool/QAQ-Harness/qaqh-backend/-/issues/105)
> **上位架构**：[#103](https://cnb.cool/QAQ-Harness/qaqh-backend/-/issues/103) / PR [#104](https://cnb.cool/QAQ-Harness/qaqh-backend/-/pulls/104)
> **基线**：`betav2 @ 87e709bd8ccd1327bde2ea49ef516d5db937dafd`
> **范围**：仅字段级契约、迁移与测试设计；本 spec 不修改生产 Rust 代码
> **独立验收**：[#106](https://cnb.cool/QAQ-Harness/qaqh-backend/-/issues/106)

本文件中的“必须/MUST”“禁止/MUST NOT”“应当/SHOULD”均为规范性要求。任何实现偏离都必须先回写 #103/#105，不能静默带入代码。

---

## 0. 冻结结论

### 0.1 命名与版本

| 项 | 冻结值 |
|---|---|
| canonical schema | `qaqh.session-fact/v2` |
| envelope 版本 | `schema_version = 2` |
| payload 版本 | `payload_version = 2` |
| canonical 文件 | `{data_dir}/sessions/{session_id}/events.jsonl` |
| canonical cursor | `(log_id, fact_seq, projection_index)` |
| v1 wire cursor | `Last-Event-ID = epoch:channel:stream_seq`，只存在于兼容 adapter |
| audit | `{data_dir}/audit/v2.jsonl`，独立于 session projection |

产品版本、Ringing wire 版本、session fact schema 版本必须分开命名。`v2.0` 不等于 `session-fact/v2`。

### 0.2 五项硬不变量

1. `events.jsonl` 是 session 历史唯一 canonical source。
2. `SessionActor` 是 session 状态唯一 writer；其他模块只能提交 command。
3. fact 必须先 durable append，再更新 projection，再发布 wire event。
4. 每个 `ToolIntent` 必须有且只有一个终态 `ToolFinished`，包括 `indeterminate`。
5. 同 schema 未知 fact 必须 fail-closed；不得跳过未知 fact 继续解释后续事实。

### 0.3 与 I1-I15 的关系

本 spec 直接冻结 I1、I2、I3、I5、I6、I8、I9、I12、I13、I14 的字段和状态迁移；I4、I7、I10、I11、I15 由本 spec 提供输入字段和验收 hook，具体执行边界分别由 TurnCore、Tool SDK、Policy/Sandbox、TUI reducer spec 继续细化。

---

## 1. Canonical Envelope

### 1.1 Rust 形状

```rust
pub const SESSION_FACT_SCHEMA: &str = "qaqh.session-fact/v2";
pub const SESSION_FACT_SCHEMA_VERSION: u16 = 2;
pub const SESSION_FACT_PAYLOAD_VERSION: u16 = 2;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionFact {
    pub schema: FactSchema,
    pub session_id: SessionId,
    pub log_id: LogId,
    pub fact_seq: u64,
    pub event_id: EventId,
    pub ts_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub causation_id: Option<EventId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub turn_id: Option<TurnId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub call_id: Option<ToolCallId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interaction_id: Option<InteractionId>,
    pub payload: FactPayload,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FactSchema {
    pub name: String,       // 固定 "qaqh.session-fact"
    pub version: u16,       // 固定 2
    pub payload_version: u16, // 固定 2
}
```

### 1.2 Envelope 字段表

| 字段 | 类型 | 必填 | serde/约束 | 语义 |
|---|---|---:|---|---|
| `schema.name` | `String` | 是 | 必须等于 `qaqh.session-fact` | 存储 schema 判定入口 |
| `schema.version` | `u16` | 是 | 必须等于 `2` | envelope 版本 |
| `schema.payload_version` | `u16` | 是 | 必须等于 `2` | payload 版本 |
| `session_id` | `SessionId` | 是 | lowercase UUIDv7 | session 主键 |
| `log_id` | `LogId` | 是 | lowercase UUIDv7 | 重建、迁移、reset 身份 |
| `fact_seq` | `u64` | 是 | `>= 1`，同 `(session_id, log_id)` 连续 | canonical 顺序 |
| `event_id` | `EventId` | 是 | ULID，全局唯一 | 幂等与因果追踪 |
| `ts_ms` | `i64` | 是 | Unix epoch 毫秒；允许墙钟回拨 | 业务时间，不承担排序 |
| `causation_id` | `Option<EventId>` | 否 | 缺失时不序列化 | 直接原因 |
| `turn_id` | `Option<TurnId>` | 否 | 缺失时不序列化 | turn 关联 |
| `call_id` | `Option<ToolCallId>` | 否 | 缺失时不序列化 | tool call 关联 |
| `interaction_id` | `Option<InteractionId>` | 否 | 缺失时不序列化 | interaction 关联 |
| `payload` | `FactPayload` | 是 | `kind` 为 tag | variant 数据 |

### 1.3 ID 类型

| 类型 | canonical pattern | 规则 |
|---|---|---|
| `SessionId` | `UUIDv7` lowercase | 新 session 使用 UUIDv7；旧 seed 仅作为 v1 alias |
| `LogId` | `UUIDv7` lowercase | 每次重建/迁移/reset 创建新值；同一 canonical log 内不变 |
| `EventId` | `ULID` 26 chars | 由 writer 生成；重放不得重新生成 |
| `InputId` | `input_<ULID>` | 单 session 内唯一 |
| `TurnId` | `turn_<ULID>` | 单 session 内唯一 |
| `ToolCallId` | `call_<ULID>` | 单 session 内唯一；不能作为 exactly-once 依据 |
| `ExecutionId` | `exec_<ULID>` | 单 call 内唯一且稳定 |
| `InteractionId` | `int_<ULID>` | ask/plan/permission 统一命名空间 |
| `BlockId` | `block_<ULID>` | 单 turn 内唯一 |
| `CheckpointId` | `ckpt_<ULID>` | 单 session 内唯一 |
| `RecoveryId` | `recovery_<ULID>` | 单 recovery batch 唯一 |
| `ResourceId` | `res_<ULID>` | 在 resource_kind 内唯一 |
| `ContentRef` | `sha256:<64 lowercase hex>` | 内容寻址；大写 hex 归一为小写 |

### 1.4 Canonical JSON 规则

1. 一行一条 JSON object，UTF-8，无 BOM，`\n` 结尾。
2. `fact_seq` 按 JSON number 写入；实现侧必须拒绝超过 `2^53-1` 的值。
3. ID、timestamp、hash、cursor 不允许依赖 map 遍历顺序。
4. 未知字段在 read 侧忽略；未知 `kind` 在 canonical fact 侧 fail-closed。
5. 不允许 `NaN`、`Infinity`、重复 JSON key、非字符串 map key。
6. 大文本只允许通过 `ContentRef`；单条 fact 内联字符串必须 `<= 64 KiB`。
7. canonical 写入使用稳定字段顺序；兼容 adapter 输出不要求稳定顺序。

`SessionFact` 不存 `Delivery`。可靠性是 projection 属性：同一 canonical fact 可在不同 channel 产生 reliable、replaceable 或 0 个 projection；不能把 delivery 固化进存储事实。

---

## 2. FactPayload 全量 Variant

### 2.1 枚举与 serde 形状

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum FactPayload {
    SessionCreated(SessionCreated),
    InputAccepted(InputAccepted),
    TurnStarted(TurnStarted),
    ModelRoundStarted(ModelRoundStarted),
    AssistantBlockSealed(AssistantBlockSealed),
    ToolCallDeclared(ToolCallDeclared),
    ToolIntent(ToolIntent),
    ToolFinished(ToolFinished),
    InteractionRequested(InteractionRequested),
    InteractionResolved(InteractionResolved),
    InteractionExpired(InteractionExpired),
    TurnFinished(TurnFinished),
    TurnInterrupted(TurnInterrupted),
    SessionRecovered(SessionRecovered),
    CompactionApplied(CompactionApplied),
    SessionMetadataChanged(SessionMetadataChanged),
    SessionTitleChanged(SessionTitleChanged),
    SessionDeleted(SessionDeleted),
    WorkspaceResourceChanged(WorkspaceResourceChanged),
    SubagentSpawned(SubagentSpawned),
    SubagentFinished(SubagentFinished),
}
```

### 2.2 Variant 总表

| `kind` | Rust struct | 权威作用 | 允许 0 个 projection |
|---|---|---:|
| `session_created` | `SessionCreated` | 建立 session 身份与初始配置 | 否 |
| `input_accepted` | `InputAccepted` | 接收用户/API 输入 | 否 |
| `turn_started` | `TurnStarted` | 开始 turn | 否 |
| `model_round_started` | `ModelRoundStarted` | 记录模型请求边界 | 是 |
| `assistant_block_sealed` | `AssistantBlockSealed` | 封存 assistant 内容块 | 否 |
| `tool_call_declared` | `ToolCallDeclared` | 模型声明工具调用 | 否 |
| `tool_intent` | `ToolIntent` | 副作用执行前 durable intent | 否 |
| `tool_finished` | `ToolFinished` | tool call 唯一终态 | 否 |
| `interaction_requested` | `InteractionRequested` | 持久挂起 ask/plan/permission | 否 |
| `interaction_resolved` | `InteractionResolved` | interaction 唯一终态 | 否 |
| `interaction_expired` | `InteractionExpired` | pending interaction 超时闭合 | 是 |
| `turn_finished` | `TurnFinished` | turn 正常终态 | 否 |
| `turn_interrupted` | `TurnInterrupted` | turn 异常/恢复终态 | 否 |
| `session_recovered` | `SessionRecovered` | 标记一次恢复完成 | 是 |
| `compaction_applied` | `CompactionApplied` | 记录上下文压缩检查点 | 否 |
| `session_metadata_changed` | `SessionMetadataChanged` | 显式 metadata mutation | 是 |
| `session_title_changed` | `SessionTitleChanged` | 标题 mutation | 是 |
| `session_deleted` | `SessionDeleted` | tombstone | 是 |
| `workspace_resource_changed` | `WorkspaceResourceChanged` | resource revision | 是 |
| `subagent_spawned` | `SubagentSpawned` | parent/child edge | 是 |
| `subagent_finished` | `SubagentFinished` | child 终态 | 是 |

### 2.3 字段定义

#### SessionCreated

| 字段 | 类型 | 必填 | serde | 规则 |
|---|---|---:|---|---|
| `created_at_ms` | `i64` | 是 | — | 与 envelope `ts_ms` 一致 |
| `cwd` | `String` | 是 | — | 绝对路径；启动时必须验证 |
| `model` | `String` | 是 | — | provider-neutral model id |
| `parent_session_id` | `Option<SessionId>` | 否 | skip none | 子代理/分叉来源 |
| `schema_caps` | `Vec<String>` | 是 | 空数组序列化 | 已知 capability 集合 |

#### InputAccepted

| 字段 | 类型 | 必填 | serde | 规则 |
|---|---|---:|---|---|
| `input_id` | `InputId` | 是 | — | 去重键 |
| `input_kind` | `InputKind` | 是 | snake_case | `user_text/command/approval/resume/system` |
| `content_ref` | `Option<ContentRef>` | 否 | skip none | 大文本引用 |
| `inline_text` | `Option<String>` | 否 | skip none | 仅当无 `content_ref`，`<= 8 KiB` |
| `attachments` | `Vec<ContentRef>` | 是 | 空数组序列化 | 用户附件，产生 `UserAttachment` retention |
| `actor` | `ActorRef` | 是 | — | user/api/system/subagent |
| `client_request_id` | `Option<String>` | 否 | skip none | 连接级幂等辅助，不是 canonical 身份 |

规则：`content_ref` 与 `inline_text` 恰有一个存在。

#### TurnStarted

| 字段 | 类型 | 必填 | serde | 规则 |
|---|---|---:|---|---|
| `turn_id` | `TurnId` | 是 | — | 必须与 envelope 一致 |
| `input_id` | `InputId` | 是 | — | 指向已接受输入 |
| `mode` | `TurnMode` | 是 | snake_case | `normal/plan/ask` |

#### ModelRoundStarted

| 字段 | 类型 | 必填 | serde | 规则 |
|---|---|---:|---|---|
| `turn_id` | `TurnId` | 是 | — | 必须与 envelope 一致 |
| `round` | `u32` | 是 | — | turn 内从 0 递增 |
| `request_hash` | `sha256:<hex>` | 是 | — | canonical request hash |
| `context_revision` | `u64` | 是 | — | conversation projection revision |

#### AssistantBlockSealed

| 字段 | 类型 | 必填 | serde | 规则 |
|---|---|---:|---|---|
| `turn_id` | `TurnId` | 是 | — | 必须与 envelope 一致 |
| `block_id` | `BlockId` | 是 | — | turn 内唯一 |
| `kind` | `AssistantBlockKind` | 是 | snake_case | `reasoning/answer/tool_call` |
| `content_ref` | `ContentRef` | 是 | — | seal 后的完整内容 |
| `model` | `String` | 是 | — | 产生该块的 model |
| `usage` | `Option<UsageInfo>` | 否 | skip none | token usage |

#### ToolCallDeclared

| 字段 | 类型 | 必填 | serde | 规则 |
|---|---|---:|---|---|
| `turn_id` | `TurnId` | 是 | — | 必须与 envelope 一致 |
| `call_id` | `ToolCallId` | 是 | — | 必须与 envelope 一致 |
| `tool_name` | `String` | 是 | — | canonical tool name |
| `args_ref` | `ContentRef` | 是 | — | canonical args 内容 |
| `args_hash` | `sha256:<hex>` | 是 | — | 必须与 args_ref 一致 |

#### ToolIntent

| 字段 | 类型 | 必填 | serde | 规则 |
|---|---|---:|---|---|
| `call_id` | `ToolCallId` | 是 | — | 必须与 envelope 一致 |
| `execution_id` | `ExecutionId` | 是 | — | 每次尝试唯一 |
| `idempotency_key` | `Option<String>` | 否 | skip none | 仅幂等工具可给 |
| `policy_decision` | `PolicyDecisionRef` | 是 | — | `allow/ask/deny/amend` 与规则 id |
| `effective_args_ref` | `Option<ContentRef>` | 否 | skip none | policy amend 后的实际参数 |
| `effective_args_hash` | `Option<sha256:<hex>>` | 否 | skip none | 与 effective_args_ref 一致 |
| `sandbox_spec_hash` | `sha256:<hex>` | 是 | — | 基于 effective args 的 SandboxSpec hash |
| `side_effect_class` | `SideEffectClass` | 是 | snake_case | 决定 recovery |
| `intent_at_ms` | `i64` | 是 | — | 执行前时间 |

规则：

- 有副作用的 `ToolIntent` 必须先 fsync，再允许 handler 执行。
- `policy_decision.outcome = amend` 时，`effective_args_ref` 与 `effective_args_hash` 必填；实际执行参数以 effective args 为准。
- `sandbox_spec_hash` 必须基于 effective args 计算，不能基于模型原始 args。
- v2.0 每个 `call_id` 恰好一个 `ToolIntent`、一个 `ToolFinished`，`execution_id` 在该 call 内稳定。
- 用户显式重试必须创建新的 `call_id`；不得复用旧 call 追加第二个 intent/终态。
- v2.0 `retry_count` 必须为 0；未来若允许 attempt 级重试，必须提升 `payload_version` 并新增聚合终态，不能改变现有唯一性。

#### ToolFinished

| 字段 | 类型 | 必填 | serde | 规则 |
|---|---|---:|---|---|
| `call_id` | `ToolCallId` | 是 | — | 必须与 envelope 一致 |
| `execution_id` | `ExecutionId` | 是 | — | 对应 intent |
| `terminal_status` | `ToolTerminalStatus` | 是 | snake_case | 唯一终态 |
| `output_ref` | `Option<ContentRef>` | 否 | skip none | typed output 内容 |
| `error` | `Option<ToolError>` | 否 | skip none | 终态错误 |
| `metrics` | `ToolMetrics` | 是 | — | 时间、字节、重试 |
| `reconciled` | `bool` | 是 | — | 是否经 reconciliation 得出 |
| `recovery_ref` | `Option<RecoveryRef>` | 否 | skip none | recovery 产生的终态必填 |
| `finished_at_ms` | `i64` | 是 | — | 终态时间 |

`ToolTerminalStatus = succeeded | failed | cancelled | timed_out | indeterminate`。

规则：一个 `(call_id)` 只允许一个 `ToolFinished`；重复终态必须拒绝或幂等返回既有 fact。`execution_id` 必须与同 call 的 `ToolIntent` 一致。

#### InteractionRequested

| 字段 | 类型 | 必填 | serde | 规则 |
|---|---|---:|---|---|
| `interaction_id` | `InteractionId` | 是 | — | 必须与 envelope 一致 |
| `call_id` | `Option<ToolCallId>` | 否 | skip none | permission 类必有 |
| `turn_id` | `TurnId` | 是 | — | 必须与 envelope 一致 |
| `kind` | `InteractionKind` | 是 | snake_case | `ask/plan/permission` |
| `request_ref` | `ContentRef` | 是 | — | 结构化请求 |
| `expires_at_ms` | `Option<i64>` | 否 | skip none | 超时闭合依据 |
| `requested_at_ms` | `i64` | 是 | — | 请求时间 |

#### InteractionResolved

| 字段 | 类型 | 必填 | serde | 规则 |
|---|---|---:|---|---|
| `interaction_id` | `InteractionId` | 是 | — | 必须与 envelope 一致 |
| `decision_ref` | `ContentRef` | 是 | — | 结构化裁决 |
| `resolved_by` | `ActorRef` | 是 | — | 第一答案来源 |
| `resolution_seq` | `u64` | 是 | — | 同 interaction 内从 1 开始，只接受 1 |
| `resolved_at_ms` | `i64` | 是 | — | 裁决时间 |

规则：first-answer-wins；重复 resolution 返回既有结果，不写第二个 fact。仅 `Pending` 状态可转 `Resolved`。

#### InteractionExpired

| 字段 | 类型 | 必填 | serde | 规则 |
|---|---|---:|---|---|
| `interaction_id` | `InteractionId` | 是 | — | 必须与 envelope 一致 |
| `reason` | `InteractionExpiryReason` | 是 | snake_case | `timeout/session_closed/restart_policy` |
| `recovery_ref` | `Option<RecoveryRef>` | 否 | skip none | recovery 产生的 expiry 必填 |
| `expired_at_ms` | `i64` | 是 | — | 闭合时间 |

Interaction 终态状态机：

```text
Pending -> Resolved
Pending -> Expired
Resolved -> terminal
Expired -> terminal
```

- 一个 interaction 只允许一个终态 fact。
- Actor 串行化 resolution 与 expiry：先提交者获胜。
- 若 `Resolved` 已提交，expiry job 不得再写 `Expired`。
- 若 `Expired` 已提交，晚到 resolution 返回既有过期结果，不得改写终态。
- `expires_at_ms` 只决定 actor 接受 resolution 的时间边界，不允许客户端自行裁决竞态。

#### TurnFinished

| 字段 | 类型 | 必填 | serde | 规则 |
|---|---|---:|---|---|
| `turn_id` | `TurnId` | 是 | — | 必须与 envelope 一致 |
| `terminal` | `TurnTerminal` | 是 | snake_case | `completed/failed/cancelled` |
| `usage` | `Option<UsageInfo>` | 否 | skip none | 聚合 usage |
| `error` | `Option<TurnError>` | 否 | skip none | 失败时必填 |
| `finished_at_ms` | `i64` | 是 | — | 终态时间 |

#### TurnInterrupted

| 字段 | 类型 | 必填 | serde | 规则 |
|---|---|---:|---|---|
| `turn_id` | `TurnId` | 是 | — | 必须与 envelope 一致 |
| `reason` | `InterruptReason` | 是 | snake_case | `crash/restart/cancel_before_seal/unknown_fact` |
| `last_fact_seq` | `u64` | 是 | — | 恢复时最后完整 fact |
| `recovery_ref` | `RecoveryRef` | 是 | — | 预分配的 recovery identity |

#### SessionRecovered

| 字段 | 类型 | 必填 | serde | 规则 |
|---|---|---:|---|---|
| `recovery_id` | `RecoveryId` | 是 | — | 一次恢复唯一 |
| `recovery_event_id` | `EventId` | 是 | — | 预分配并写入所有 recovery facts |
| `recovery_input_fingerprint` | `ContentHash` | 是 | — | 固定 pre-recovery 输入指纹 |
| `last_good_fact_seq` | `u64` | 是 | — | 最后完整 fact |
| `torn_tail` | `bool` | 是 | — | 是否发现撕裂 |
| `torn_bytes` | `Option<u64>` | 否 | skip none | 撕裂字节数 |
| `actions` | `Vec<RecoveryAction>` | 是 | 空数组序列化 | 已执行的闭合动作 |
| `recovered_at_ms` | `i64` | 是 | — | 恢复时间 |

规则：同一 recovery input 只允许一个 `SessionRecovered`；重复恢复不得产生第二个终态。

#### CompactionApplied

| 字段 | 类型 | 必填 | serde | 规则 |
|---|---|---:|---|---|
| `checkpoint_id` | `CheckpointId` | 是 | — | checkpoint 唯一 |
| `replaces_through_fact_seq` | `u64` | 是 | — | 被摘要覆盖的边界 |
| `summary_ref` | `ContentRef` | 是 | — | canonical summary |
| `context_revision` | `u64` | 是 | — | 压缩后 context revision |
| `applied_at_ms` | `i64` | 是 | — | 应用时间 |

规则：compaction 不得删除 canonical facts；checkpoint 缺失时必须从 active facts 重建 context。

#### SessionMetadataChanged

| 字段 | 类型 | 必填 | serde | 规则 |
|---|---|---:|---|---|
| `patch` | `SessionMetadataPatch` | 是 | — | 至少一个字段存在 |
| `source` | `MetadataSource` | 是 | snake_case | `user/api/system/migration` |
| `changed_at_ms` | `i64` | 是 | — | 变更时间 |

`SessionMetadataPatch` 字段：`cwd`、`model`、`archived`、`search_visibility`、`parent_session_id`、`schema_caps`，全部 `Option`，未出现表示不变。空 patch 非法。

#### SessionTitleChanged

| 字段 | 类型 | 必填 | serde | 规则 |
|---|---|---:|---|---|
| `title` | `String` | 是 | — | 归一化后不超过 4 KiB |
| `source` | `TitleSource` | 是 | snake_case | `user/auto/migration` |
| `changed_at_ms` | `i64` | 是 | — | 变更时间 |

#### SessionDeleted

| 字段 | 类型 | 必填 | serde | 规则 |
|---|---|---:|---|---|
| `tombstone_at_ms` | `i64` | 是 | — | 删除时间 |
| `reason` | `DeleteReason` | 是 | snake_case | `user/api/retention/rollback` |
| `purge_after_ms` | `Option<i64>` | 否 | skip none | GC 允许时间 |

#### WorkspaceResourceChanged

| 字段 | 类型 | 必填 | serde | 规则 |
|---|---|---:|---|---|
| `resource_kind` | `ResourceKind` | 是 | snake_case | `todo/skill/plan/activity/file` |
| `resource_id` | `ResourceId` | 是 | — | kind 内唯一 |
| `revision` | `u64` | 是 | — | 单调递增 |
| `summary_ref` | `ContentRef` | 是 | — | 当前值 |
| `deleted` | `bool` | 是 | — | tombstone 标志 |

#### SubagentSpawned

| 字段 | 类型 | 必填 | serde | 规则 |
|---|---|---:|---|---|
| `child_session_id` | `SessionId` | 是 | — | child canonical session |
| `parent_call_id` | `ToolCallId` | 是 | — | parent tool call |
| `role` | `Option<String>` | 否 | skip none | v2.0 仅记录，不驱动模板 |
| `spawned_at_ms` | `i64` | 是 | — | 创建时间 |

#### SubagentFinished

| 字段 | 类型 | 必填 | serde | 规则 |
|---|---|---:|---|---|
| `child_session_id` | `SessionId` | 是 | — | 必须已有 Spawned |
| `parent_call_id` | `ToolCallId` | 是 | — | 对应 parent call |
| `status` | `SubagentTerminalStatus` | 是 | snake_case | `completed/failed/cancelled/timed_out` |
| `result_ref` | `Option<ContentRef>` | 否 | skip none | 最终结果 |
| `finished_at_ms` | `i64` | 是 | — | 终态时间 |

### 2.4 必填、可选与未知字段规则

- 表中“必填”字段缺失：该 fact 非法，writer 拒绝 append。
- `Option` 字段使用 `#[serde(default, skip_serializing_if = "Option::is_none")]`。
- 空 `Vec`、空字符串、`false`、`0` 是有效值，不得用 `Option` 偷换缺失。
- 同 schema 未知字段：read 侧忽略，projection 继续。
- 同 schema 未知 `kind`：立即进入 `read_only/upgrade_required`，不得继续解释后续 fact。
- 未知 `payload_version`：同未知 kind 处理。
- 跨 schema 主版本：禁止 in-place 解释，必须迁移或 reset。

### 2.5 支持类型闭集

```rust
pub struct SessionId(pub String);
pub struct LogId(pub String);
pub struct EventId(pub String);
pub struct InputId(pub String);
pub struct TurnId(pub String);
pub struct ToolCallId(pub String);
pub struct ExecutionId(pub String);
pub struct InteractionId(pub String);
pub struct BlockId(pub String);
pub struct CheckpointId(pub String);
pub struct RecoveryId(pub String);
pub struct RecoveryRef {
    pub recovery_id: RecoveryId,
    pub recovery_event_id: EventId,
    pub recovery_input_fingerprint: ContentHash,
}
pub struct ResourceId(pub String);
pub struct ContentHash(pub String); // sha256:<64 lowercase hex>
pub struct ContentRef(pub ContentHash);

pub enum SearchVisibility { Visible, Hidden }
pub enum ResetReason {
    CursorExpired,
    LogIdMismatch,
    UnknownFact,
    UpgradeRequired,
    ReplayOverflow,
    V1EpochMismatch,
    CrossSession,
}
pub enum V1DeliveryKind { Reliable, Replaceable, Ephemeral }
pub enum LegacySource {
    MessagesJsonl,
    RingingJournal,
    RingingLatest,
    RingingTimeline,
    RingingOffload,
    MetaJson,
}

pub enum ActorKind { User, Api, System, Agent, Subagent }
pub struct ActorRef {
    pub kind: ActorKind,
    pub id: String,
    pub display_name: Option<String>,
}

pub enum InputKind { UserText, Command, Approval, Resume, System }
pub enum TurnMode { Normal, Plan, Ask }
pub enum AssistantBlockKind { Reasoning, Answer, ToolCall }
pub enum PolicyOutcome { Allow, Ask, Deny, Amend }
pub struct PolicyDecisionRef {
    pub outcome: PolicyOutcome,
    pub rule_id: String,
    pub decided_at_ms: i64,
    pub reason_ref: Option<ContentRef>,
}

pub enum SideEffectClass { ReadOnly, WorkspaceWrite, Process, Network, External }
pub struct ToolMetrics {
    pub started_at_ms: i64,
    pub finished_at_ms: i64,
    pub retry_count: u32,
    pub output_bytes: u64,
    pub progress_bytes_total: u64,
}
pub struct ToolError {
    pub code: String,
    pub message: String,
    pub retryable: bool,
    pub details_ref: Option<ContentRef>,
}

pub enum InteractionKind { Ask, Plan, Permission }
pub enum InteractionExpiryReason { Timeout, SessionClosed, RestartPolicy }
pub enum TurnTerminal { Completed, Failed, Cancelled }
pub struct TurnError {
    pub code: String,
    pub message: String,
    pub retryable: bool,
    pub details_ref: Option<ContentRef>,
}
pub enum InterruptReason { Crash, Restart, CancelBeforeSeal, UnknownFact }
pub enum MetadataSource { User, Api, System, Migration }
pub enum TitleSource { User, Auto, Migration }
pub enum DeleteReason { User, Api, Retention, Rollback }
pub enum ResourceKind { Todo, Skill, Plan, Activity, File }
pub enum SubagentTerminalStatus { Completed, Failed, Cancelled, TimedOut }
```

字段规则：

| 类型 | 规则 |
|---|---|
| `ActorRef.id` | user 为稳定本地身份；api 为 key id；agent/subagent 为 session/call 身份 |
| `PolicyDecisionRef.rule_id` | 必须来自 policy engine，不接受自由文本 |
| `SideEffectClass` | 闭集；新增类别必须提升 `payload_version` |
| `ToolMetrics` | 时间使用毫秒；计数不允许负数 |
| `ToolError.code` | 稳定 snake_case；message 必须脱敏 |
| `TurnError.code` | 稳定 snake_case；message 必须脱敏 |
| `ResourceId` | `String`，在 `resource_kind` 内唯一 |
| enum 未知值 | canonical fact 同 `payload_version` 内 fail-closed；wire 可映射为 `Unknown` |

`RecoveryAction` 是 tagged enum：

| `kind` | 附加字段 | 含义 |
|---|---|---|
| `turn_interrupted` | `turn_id`, `last_fact_seq` | 闭合未完成 turn |
| `tool_indeterminate` | `call_id`, `execution_id` | 非幂等工具不确定 |
| `tool_reconciled` | `call_id`, `execution_id`, `evidence_ref` | 对账得出终态 |
| `interaction_expired` | `interaction_id`, `reason` | 超时/重启闭合 |
| `torn_tail_truncated` | `bytes`, `last_good_fact_seq` | 截断 torn tail |
| `projection_rebuilt` | `projection`, `through_fact_seq` | 重建 derived projection |

`SessionMetadataPatch` 字段：

| 字段 | 类型 | 规则 |
|---|---|---|
| `cwd` | `Option<String>` | 绝对路径 |
| `model` | `Option<String>` | provider-neutral id |
| `archived` | `Option<bool>` | 归档状态 |
| `search_visibility` | `Option<SearchVisibility>` | `visible/hidden` |
| `parent_session_id` | `Option<SessionId>` | 仅创建/迁移允许变更 |
| `schema_caps` | `Option<Vec<String>>` | 替换式 patch，不做集合 merge |

`SessionMetadataPatch` 所有字段同时为 `None` 时非法；未出现的字段表示不变，`Some` 表示替换值。

---

## 3. 复合 Cursor、排序与 Replay

### 3.1 类型

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReliableCursor {
    pub log_id: LogId,
    pub fact_seq: u64,
    /// 0..=65534 是 reliable projection；65535 仅用于 snapshot END_OF_FACT。
    pub projection_index: u16,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum Delivery {
    Reliable { cursor: ReliableCursor },
    Replaceable { revision: u64 },
    Ephemeral,
}
```

### 3.2 排序

1. 只有 `log_id` 相同才能比较 `fact_seq` 和 `projection_index`。
2. 同一 `log_id` 内按 `(fact_seq, projection_index)` 字典序升序。
3. `fact_seq` 必须连续，不允许 gap；`projection_index` 从 0 开始，同一 fact 内稳定。
4. 完整 cursor 严格递增：`(f1, p1) < (f2, p2)` 当且仅当 `f1 < f2` 或 `f1 == f2 && p1 < p2`。
5. `fact_seq` 只属于 canonical log；wire channel 不得另造 authoritative seq。
6. `log_id` 不一致必须走 `ResetRequired`，不能做数值比较。

### 3.3 Replay 过滤

给定 `since_cursor`：

```text
if since_cursor.log_id != current_log_id:
    ResetRequired { log_id, snapshot_cursor }
else:
    deliver Reliable events where
      fact_seq > since.fact_seq
      OR (fact_seq == since.fact_seq AND projection_index > since.projection_index)
```

规则：

- replay 只回放 `Reliable`。
- `Replaceable` 在订阅时发送一次当前 value，不推进 reliable cursor。
- `Ephemeral` 不回放，不推进 cursor。
- live 与 replay 去重键为完整 `ReliableCursor`；`event_id` 是辅助幂等键。
- replay 期间 live event 必须进入有界 buffer，不能丢 reliable event。
- replay buffer 溢出时必须 `ResetRequired`，不得静默跳 fact。

### 3.4 多 projection

一个 fact 可产生 0..N 个 reliable `ProjectionEvent`。`projection_index` 的分配必须稳定：

- 同一 fact 的 projection 顺序只由 §4.1 的 `ProjectionId` ordinal 决定。
- `ProjectionSet` 字段声明顺序不参与 index 分配。
- 新增 projection 只能追加更高 ordinal，不能重排既有 ordinal。
- 某 projection 不消费该 fact 时，不为它分配 index。
- reliable `projection_index` 只允许 `0..=65534`；`65535` 保留给 snapshot `END_OF_FACT`。
- 同一 fact 多次 rebuild 必须得到相同 cursor 映射。

### 3.5 Cursor expiry

| 条件 | 结果 |
|---|---|
| cursor `log_id` 匹配且 fact 仍在 replay window | 正常 replay |
| cursor fact 早于 `earliest_available_fact_seq` | `ResetRequired` |
| cursor 来自已迁移/已重建 log | `ResetRequired` |
| cursor 的 projection_index 超过该 fact 最大值 | 按已超过该 fact 处理 |
| cursor 命中 unknown fact | `ResetRequired` 或 read-only/upgrade-required |
| v1 epoch 不匹配 | `ResetRequired` |

`ResetRequired` 必须包含：

```rust
pub struct ResetRequired {
    pub log_id: LogId,
    pub snapshot_cursor: ReliableCursor,
    pub reason: ResetReason,
}
```

`snapshot_cursor` 必须指向 snapshot 已覆盖的最后一个 projection 位置：

```text
snapshot_cursor = (log_id, snapshot_fact_seq, u16::MAX)
```

`u16::MAX` 是保留的 `END_OF_FACT` sentinel，可靠 projection 的 index 只允许 `0..=u16::MAX-1`。即使 `snapshot_fact_seq` 产生 0 个 reliable projection，snapshot cursor 仍有定义。客户端收到后必须丢弃旧 snapshot，重新拉 baseline，再从 `snapshot_cursor` 订阅。`stream_seq = 0` 必须返回该 baseline，而不是伪造 `(0,0)`。

兼容映射必须保持顺序：同一 `(epoch, channel, session)` 内若 v1 `stream_seq(a) < stream_seq(b)`，则映射后的 reliable cursor 必须严格小于 cursor(b)。无法保持顺序时必须 `ResetRequired`。

### 3.6 v1 `Last-Event-ID` 映射

v1 cursor 形状：

```text
epoch:channel:stream_seq
```

v2 映射表只作为兼容 adapter，不是 canonical source：

```rust
pub struct V1CursorMapping {
    pub epoch: String,
    pub channel: RingingChannel,
    pub stream_seq: u64,
    pub session_id: SessionId,
    pub log_id: LogId,
    pub fact_seq: u64,
    pub projection_index: u16,
    pub delivery: V1DeliveryKind,
    pub mapped_at_ms: i64,
    pub expires_at_ms: i64,
}
```

映射规则：

| v1 输入 | v2 行为 |
|---|---|
| `epoch` 不等于当前 server epoch | `ResetRequired` |
| `channel` 与订阅 channel 不一致 | 拒绝请求 |
| `stream_seq = 0` | snapshot baseline |
| 命中 reliable mapping | 转换为 `since_cursor`，按复合 cursor 回放 |
| 命中 replaceable mapping | 发送当前 projection value，不推进 cursor |
| 命中 ephemeral mapping | 不回放 |
| mapping 缺失/过期 | `ResetRequired` |
| mapping 的 session 与当前订阅 session 不同 | `ResetRequired`，禁止跨 session 静默映射 |

关键裁决：v1 `stream_seq` 是 `(epoch, channel)` 全局序，不是 per-seed `channel_seq`；兼容层必须以 v1 envelope 的实际投递记录建立映射，不能把 `stream_seq` 直接当 `fact_seq`。

映射基数：

- 每个 reliable v1 envelope 必须 1:1 映射到一个 v2 reliable `ProjectionEvent`。
- 一个 v2 fact 产生多个 reliable projection 时，兼容层必须为每个 projection 分配独立 v1 `stream_seq`。
- 禁止把同一 fact 的多个 reliable projection 合并成一个 v1 envelope，因为 `Last-Event-ID` 只能表达一个 `stream_seq`，合并会造成不可检测的 replay gap。
- replaceable/ephemeral 可以按 v1 语义合并或丢弃，但不得推进 reliable cursor。

映射表生命周期：

- owner 是 Ringing v1 兼容 adapter，不是 SessionActor。
- mapping 是兼容 sidecar，位于 `diagnostics/v1-cursor-map.jsonl`，append-only。
- 每条 mapping 必须在对应 v1 SSE emit 前 fsync；v2 projection 已 durable 才能写 mapping。
- v1 `stream_seq` 分配本身不在 canonical fact 中，因此 mapping 不能保证从 canonical log 精确重建；sidecar 丢失、过期、乱序或跨 session 时必须 `ResetRequired`。
- retention 至少覆盖 Ringing v1 兼容期和可靠 replay window；兼容期结束后整表删除。
- mapping 顺序必须满足 `stream_seq` 与复合 cursor 同序；无法满足时不得 emit，必须 reset。

---

## 4. Fact → Projection 映射

### 4.1 ProjectionSet

```rust
pub enum ProjectionId {
    Conversation = 0,
    Timeline = 1,
    Control = 2,
    Resources = 3,
    Meta = 4,
}

pub struct ProjectionSet {
    pub conversation: ConversationProjection,
    pub timeline: TimelineProjection,
    pub control: ControlProjection,
    pub resources: ResourceProjection,
    pub meta: SessionMetaProjection,
}
```

`ProjectionId` 是 `projection_index` 的唯一 ordinal 注册表。对每个 fact，按 ordinal 升序调用 projection；只有实际产生 delta 的 projection 分配连续 `projection_index = 0..N-1`。表格中的书写顺序不具备规范性，禁止据此分配 index。

`ToolProjection` 是 Tool SDK 的 typed output 适配器（model/display），不是 `ProjectionSet` 成员，也不分配 canonical `projection_index`。它先产出 typed output；`ControlProjection` 保存当前 tool 状态，`TimelineProjection` 保存 transcript，`ConversationProjection` 保存模型面，`ResourceProjection` 消费 workspace effects。v2.0 不新增 `ProjectionSet` ordinal。

`AuditStore` 不属于 `ProjectionSet`，也不从 `events.jsonl` 重建。

### 4.2 映射规则

| Fact | Reliable projection | Replaceable | Ephemeral |
|---|---|---|---|
| `SessionCreated` | control:session_created, meta:created | control:current | — |
| `InputAccepted` | conversation:input, timeline:input | control:activity | — |
| `TurnStarted` | conversation:turn_started, control:activity | control:current | — |
| `ModelRoundStarted` | control:round | control:activity | — |
| `AssistantBlockSealed` | conversation:assistant_block, timeline:block | timeline:current | `assistant_delta` 不回放 |
| `ToolCallDeclared` | timeline:tool_block, conversation:tool_call | control:tool_current | — |
| `ToolIntent` | control:tool_intent | control:tool_current | — |
| `ToolFinished` | conversation:tool_result, timeline:tool_result | control:tool_current | progress 不回放 |
| `InteractionRequested` | control:interaction_requested | control:current | — |
| `InteractionResolved` | control:interaction_resolved | control:current | — |
| `InteractionExpired` | control:interaction_expired | control:current | — |
| `TurnFinished` | conversation:turn_finished, control:activity | control:current | — |
| `TurnInterrupted` | conversation:turn_interrupted, control:activity | control:current | — |
| `SessionRecovered` | control:recovered, meta:recovered | control:current | — |
| `CompactionApplied` | conversation:compaction, meta:context_revision | control:current | — |
| `SessionMetadataChanged` | meta:metadata_changed | meta:current | — |
| `SessionTitleChanged` | meta:title_changed | meta:current | — |
| `SessionDeleted` | meta:deleted | meta:current | — |
| `WorkspaceResourceChanged` | resources:changed | resources:current | activity_delta |
| `SubagentSpawned` | control:subagent_spawned, resources:graph_edge | control:current | — |
| `SubagentFinished` | control:subagent_finished, resources:graph_edge | control:current | — |

Workspace effect 唯一权威路径：

```text
ToolOutput -> WorkspaceEffect
  -> SessionActor 转成 WorkspaceResourceChanged fact
  -> ResourceProjection.apply(WorkspaceResourceChanged)
```

`ResourceProjection` 有两个互斥子域：

- workspace resource 子域：唯一 canonical 输入是 `WorkspaceResourceChanged`。
- subagent graph 子域：唯一 canonical 输入是 `SubagentSpawned` / `SubagentFinished`。

`ToolFinished` 不得直接产生任一子域的 projection；subagent fact 也不得修改 workspace resource 子域。这样 replay 不会从 tool result 与 resource fact 双重应用同一状态。

Interaction replay 抑制规则：

- replay/snapshot 先构建 `resolved_or_expired_interaction_ids`。
- 回放 `InteractionRequested` 时，若该 interaction 已有 `Resolved`/`Expired`，不得再发布 pending modal。
- 只发布当前 terminal projection（Resolved 或 Expired）。
- snapshot 只包含 pending interaction；terminal interaction 只保留 ID，用于去重。
- 该规则对 Reliable replay 和 live 都生效；客户端不能依赖“先看到 Requested 再看到 terminal”来关闭 modal。

### 4.3 Projection 契约

每个 projection 必须实现：

```rust
pub trait Projection: Default + Send {
    type Snapshot: Serialize + DeserializeOwned;
    type Delta: Serialize + Send;
    fn apply(&mut self, fact: &SessionFact) -> Option<Self::Delta>;
    fn snapshot(&self) -> Self::Snapshot;
    fn last_fact_seq(&self) -> u64;
    fn rebuild(facts: impl Iterator<Item = SessionFact>) -> Self;
}
```

规则：

- `apply` 必须是纯函数：同输入 fact 序列得到同 snapshot。
- projection 不修改 canonical fact。
- projection 落后 canonical log 是允许的；领先是禁止的。
- `derived/` 可删除；重建结果必须与增量 apply 等价。
- `Replaceable` 的 revision 由 projection 自己生成，不能作为 canonical cursor。
- `Ephemeral` 不允许进入 `events.jsonl`。

### 4.4 内容引用

- 单条 fact 内联文本上限 `64 KiB`。
- 超过上限必须写 `content/<sha256>`，fact 仅存 `ContentRef`。
- 先写 content 并 fsync，再 append 引用该 content 的 fact。
- content 写入失败时不得执行对应副作用；已经执行时必须写 `indeterminate`。
- model/display/resource/service 从同一 typed output 派生，不能各自解析 `Value`。

---

## 5. 持久化、单 Writer 与 Fsync

### 5.1 文件布局

```text
{data_dir}/sessions/{session_id}/
  events.jsonl                 # canonical, append-only
  events.lock                  # 跨进程 writer ownership
  recovery.intent.json         # 崩溃恢复批次意图，SessionRecovered 后删除
  content/{sha256}             # content-addressed
  content/index.jsonl          # ContentRecord，append-only
  content.lock                 # content append/GC 互斥
  derived/
    conversation.json
    timeline.json
    control.json
    resources.json
    meta.json
  diagnostics/
    timeline.jsonl
    v1-cursor-map.jsonl
    content-gc.jsonl
```

`events.jsonl` 不原地重写、不压缩、不删除单行。归档/轮转只能通过新 log 或 segment manifest，且必须保持 `fact_seq` 可解释。

### 5.2 Append 协议

```text
SessionActor
  -> acquire writer lock
  -> validate fact + assign fact_seq
  -> append JSON line
  -> fsync/group barrier
  -> ProjectionSet.apply(fact)
  -> publish reliable projection
```

规则：

1. 同一 session 同时只有一个 writer。
2. `fact_seq` 在 append 前分配；crash 后从最后完整行恢复。
3. append 成功但 publish 失败：重连必须从 canonical log replay。
4. publish 成功但 append 未完成：实现错误，必须 panic/停止写入，不得继续。
5. `ToolIntent`、`InteractionResolved`、`InteractionExpired`、有副作用的 `ToolFinished` 必须独立 fsync barrier。
6. 无副作用 fact 可以 group commit，但不得跨越 durable barrier 合并。
7. writer lock 使用 OS file lock；锁文件不是事实源。

### 5.3 Fsync 矩阵

| Fact | 独立 fsync | 原因 |
|---|---:|---|
| `SessionCreated` | 是 | 建立 canonical 身份 |
| `InputAccepted` | 是 | 输入不可丢 |
| `TurnStarted` | 是 | 恢复边界 |
| `ModelRoundStarted` | 否 | 可由上下文重建边界 |
| `AssistantBlockSealed` | 是 | 模型内容不可丢 |
| `ToolCallDeclared` | 是 | 副作用前置 |
| `ToolIntent` | 是 | 副作用前 durable intent |
| `ToolFinished` | 有副作用时是 | 终态与副作用对账 |
| `InteractionRequested` | 是 | pending 可恢复 |
| `InteractionResolved` | 是 | first-answer-wins |
| `InteractionExpired` | 是 | 终态闭合，禁止恢复后重复 modal |
| `TurnFinished/Interrupted` | 是 | 终态闭合 |
| `SessionRecovered` | 是 | 恢复幂等 |
| `CompactionApplied` | 是 | context revision 边界 |
| metadata/title/deleted | 是 | 用户可见 mutation |

### 5.4 Torn tail

1. 读到无 `\n` 结尾的最后一行：判定 torn tail。
2. 将原始字节移动到 `events.jsonl.torn.<fact_seq>`，保留证据。
3. 截断到最后一个完整记录。
4. 写 `SessionRecovered { torn_tail: true }`。
5. 不允许把 torn tail 解析为半条 fact。
6. torn tail 之后不得继续 append，直到恢复完成。
7. 重复恢复同一 torn tail 必须幂等。

### 5.5 Content GC

每个 content 必须有独立 derived metadata：

```rust
pub struct ContentRecord {
    pub content_ref: ContentRef,
    pub created_at_ms: i64,
    pub last_referenced_fact_seq: u64,
    pub retention_classes: Vec<ContentRetentionClass>,
    pub delete_after_ms: Option<i64>,
    pub ref_count: u64,
}

pub enum ContentRetentionClass {
    SessionReplay,
    CompactionCheckpoint,
    InteractionPending,
    AuditEvidence,
    UserAttachment,
}

pub enum ContentGcKind { Mark, Sweep, Delete, Retry, Recovered }

pub struct ContentGcRecord {
    pub schema: String, // "qaqh.content-gc/v1"
    pub kind: ContentGcKind,
    pub content_ref: ContentRef,
    pub ref_count: u64,
    pub retention_classes: Vec<ContentRetentionClass>,
    pub delete_after_ms: Option<i64>,
    pub attempt: u32,
    pub error: Option<String>,
    pub ts_ms: i64,
}
```

默认 retention：

| class | `delete_after_ms` |
|---|---|
| `SessionReplay` | fact `ts_ms + 30 days` |
| `CompactionCheckpoint` | checkpoint applied `ts_ms + 90 days` |
| `InteractionPending` | terminal 前不删除；terminal 后按 SessionReplay |
| `AuditEvidence` | audit `ts_ms + 180 days` 或 legal hold |
| `UserAttachment` | `None`，不自动删除 |

`ContentRecord` 持久化在 `{session}/content/index.jsonl`，append-only，每条 record fsync 后才能 append 引用它的 fact。`ref_count` 是 derived 计数，不能作为唯一 liveness 依据；mark 必须从 facts/checkpoint/interaction/audit 谓词重新计算。多 retention class 取最晚删除时间；任一 class 为 `UserAttachment` 时 `delete_after_ms = None`。

Content lifecycle：

```text
Pending -> Referenced -> Eligible -> Deleting -> Deleted
                                  └-> Retry -> Deleting
Referenced <- Recovered
```

- `Pending`：文件已写但尚无 fact 引用；超过 grace 后进入 Eligible。
- `Referenced`：至少一个 active ref。
- `Eligible`：无 active ref 且 `delete_after_ms` 已到；至少保留 24h grace。
- `Deleting`：持有 `content.lock`，与 append/reference 建立互斥。
- `Retry`：删除失败，记录 `attempt/error`；最大 5 次，指数退避。
- `Recovered`：文件仍被合法引用或删除被取消，回到 Referenced。
- `Deleted`：文件已不存在；任何 fact 再引用它必须报 `ContentUnavailable`。

GC journal 幂等键为 `(content_ref, kind, attempt)`；重启后从 `content-gc.jsonl` 恢复未完成状态。超过重试上限必须停止该 ref 的 GC 并告警，不得静默删除或无限重试。

`diagnostics/content-gc.jsonl` 是 append-only 状态日志，记录 `Mark -> Sweep -> Delete/Retry`。重启后从日志尾部恢复 retry 集合；`Retry` 不改变 canonical facts。删除成功写 `Delete`，失败写 `Retry { attempt, error }`；超过重试上限时停止 GC 并告警，不得绕过。

active ref 谓词：

- `SessionReplay`：被 canonical fact 引用，且 `fact.ts_ms + session_replay_retention >= now`。
- `CompactionCheckpoint`：被未过期 checkpoint 引用。
- `InteractionPending`：被 pending interaction 引用。
- `AuditEvidence`：被 audit retention 或 legal hold 引用。
- `UserAttachment`：由用户显式保留，默认不自动删除。

GC 协议：

1. mark 阶段获取 `content.lock`，从上述谓词生成候选保留集；新 fact 引用 content 必须与 sweep 串行化。
2. sweep 阶段仅删除 `delete_after_ms <= now` 且未被 mark 的 content。
3. 删除前必须写 `content-gc.jsonl` 候选与决策记录，并 fsync。
4. 删除失败进入同一 journal 的 retry 状态；不得静默忽略。
5. 旧 fact 引用的 content 被 GC 后，读取必须返回 `ContentUnavailable`，不能返回空内容或伪造摘要。
6. replay/model context 遇到 `ContentUnavailable` 时必须显式降级并记录 diagnostics。
7. `retention window` 从 `delete_after_ms` 判断，不依赖文件 mtime；默认值由配置冻结并纳入 fixture。

### 5.6 背压与容量

| 队列 | 初始上限 | 溢出策略 |
|---|---:|---|
| actor mailbox | 1024 | 拒绝 command / `Busy` |
| per-connection queue | 256 | disconnect + `ResetRequired` |
| replay buffer | 16 MiB | `ResetRequired` |
| progress buffer | 8 KiB / 50 ms | 丢弃 ephemeral，不丢 reliable |
| 单 fact inline | 64 KiB | 强制 content ref |
| session content quota | 可配置 | 拒绝新高成本工具/GC |

禁止静默丢 reliable fact。达到高水位时必须显式 disconnect/reset/拒绝。

---

## 6. 崩溃恢复状态机

### 6.1 启动流程

```text
load events.jsonl
  -> validate envelope + schema
  -> find last complete line
  -> detect torn tail
  -> rebuild projections
  -> detect open Turn/ToolIntent/Interaction
  -> append recovery facts atomically
  -> mark session writable or read-only
```

恢复必须满足：

- 不重写旧 fact。
- 不生成第二个终态。
- 同一输入重复执行得到同一终态。
- unknown fact 不得被跳过。
- 即使没有任何 recovery action，也必须写一个 `SessionRecovered { actions=[] }`，否则 session 会永久停在 recovery/read-only。
- `actions` 是对同一 recovery batch 已写 facts 的摘要，不得被消费者当成第二组事实执行。

恢复批次协议：

1. 从 pre-recovery canonical log 计算确定性的 `RecoveryPlan`，包含 `RecoveryRef { recovery_id, recovery_event_id, recovery_input_fingerprint }` 和所有待写 recovery facts。
2. `recovery_input_fingerprint = sha256(canonical_json(log_id, last_good_fact_seq, sorted_open_ids, torn_tail_bytes_hash))`。
3. 在写任何 recovery fact 前预分配 `recovery_event_id`；每个 recovery fact 都携带同一个 `RecoveryRef`。
4. recovery facts 按固定顺序逐个 append + fsync；`SessionRecovered` 必须最后写，并携带同一个 `RecoveryRef`。
5. 任一步骤 crash：若 canonical log 已有任一带该 `RecoveryRef` 的 recovery fact，重启后直接复用其中的 `recovery_event_id/fingerprint`；否则从 pre-recovery log 重算相同 plan。
6. 重启后按幂等键跳过已写 facts，补齐缺失 facts，最后写 `SessionRecovered`。
7. `SessionRecovered` 不存在时，session 保持 `recovery_in_progress/read_only`，不得接受新 command。
8. `SessionRecovered` 存在后，恢复批次视为闭合；不得再补写该批次的 recovery facts。

在修改 `events.jsonl`、移动 torn tail 或写首条 recovery fact 前，必须原子写入 `recovery.intent.json`：

```rust
pub struct RecoveryIntent {
    pub schema: String, // "qaqh.recovery-intent/v1"
    pub recovery_ref: RecoveryRef,
    pub log_id: LogId,
    pub last_good_fact_seq: u64,
    pub sorted_open_ids: Vec<String>,
    pub torn_tail_bytes_hash: ContentHash,
    pub plan_hash: ContentHash,
}
```

`recovery_input_fingerprint` 的规范输入：

```text
canonical_json({
  log_id: string,
  last_good_fact_seq: u64,
  sorted_open_ids: [string, ...], // 字典序升序，重复项去除
  torn_tail_bytes_hash: "sha256:<64hex>" // 无 torn tail 时使用 sha256(empty)
})
```

- `recovery.intent.json` 使用 temp + fsync + rename + 父目录 fsync 写入；父目录 fsync 失败则不得修改 `events.jsonl`。
- 重启时先判定 intent 是否为 active：若同 `recovery_event_id` 的 `SessionRecovered` 已存在，则该 intent 已 closed/stale。
- closed intent 不得复用到新的 open state；若 `SessionRecovered` 之后又有新 open Turn/Tool/Interaction，必须基于当前 pre-recovery log 生成新的 `RecoveryRef` 与 fingerprint，并原子覆盖/归档旧 intent。
- active intent 才允许复用其中的 `RecoveryRef` 与 fingerprint。
- `SessionRecovered` fsync 成功后才允许删除 intent；删除失败只留下 stale intent，不影响后续正确性。
- 若 intent 不存在，必须先从 pre-recovery canonical log 计算并落盘，再执行恢复。

### 6.2 恢复矩阵

| 输入状态 | 动作 | 写入 fact | 可写性 |
|---|---|---|---|
| `TurnStarted` 无终态 | 丢弃未 seal 流，闭合 turn | `TurnInterrupted` | 恢复后可写 |
| `ToolIntent` 无 `ToolFinished`，幂等 | 允许 reconciliation/replay | 最终 `ToolFinished` | 对账后写 |
| `ToolIntent` 无 `ToolFinished`，非幂等 | 禁止重跑 | `ToolFinished { indeterminate }` | 恢复后写 |
| `InteractionRequested` 无终态且未过期 | 保留 pending | `SessionRecovered { actions=[] }` | 可写 |
| `InteractionRequested` 已过期 | 闭合 | `InteractionExpired` | 恢复后写 |
| `InteractionRequested` 无终态，重启策略取消 | 闭合 | `InteractionExpired { restart_policy }` | 恢复后写 |
| `CompactionApplied` 无 checkpoint | 从 active facts 重建 context | `SessionRecovered` | 重建后写 |
| torn tail | 保存证据并截断 | `SessionRecovered { torn_tail: true }` | 恢复后写 |
| unknown fact/kind | 停止解释，标记只读 | 无 | read-only/upgrade-required |
| `SessionDeleted` | 禁止恢复写入 | 无 | tombstone-only |

### 6.3 Tool recovery

`ToolIntent` 的恢复裁决顺序：

1. 查找同 `call_id` 的 `ToolFinished`；存在则不再执行。
2. 检查 `side_effect_class` 与 `idempotency_key`。
3. 幂等工具允许一次 reconciliation attempt。
4. 非幂等工具禁止重跑，写 `indeterminate`。
5. reconciliation 结果必须记录 `reconciled = true` 与证据引用。
6. 不得用 `call_id` 本身宣称 exactly-once。

### 6.4 Interaction recovery

- pending interaction 只重发一次 `InteractionRequested` projection。
- 已 resolved/expired 不重发 modal。
- 重复 resolution 返回既有 `InteractionResolved`。
- first-answer-wins 依据 `interaction_id`，不依据连接顺序。
- TUI/Web 必须维护 `resolved_interaction_ids`。

### 6.5 Recovery fact 幂等

`SessionRecovered` 的幂等键：

```text
(recovery_input_fingerprint, last_good_fact_seq, log_id)
```

重复恢复不得再写第二个 `SessionRecovered`。`TurnInterrupted` 与 `ToolFinished::Indeterminate` 的幂等键分别为 `(turn_id, recovery_id)` 与 `(call_id, recovery_id)`。`recovery_event_id` 在 plan 阶段预分配；若该 ID 或同一 `recovery_input_fingerprint` 已存在，则复用既有 recovery batch，不重新生成 identity。

---

## 7. Metadata、生命周期与子代理

### 7.1 SessionMetadataChanged 权威字段

- `cwd`：当前工作目录。
- `model`：当前模型选择。
- `archived`：归档状态。
- `search_visibility`：是否进入搜索索引。
- `parent_session_id`：分叉/子代理 parent。
- `schema_caps`：session 可用 capability。

这些字段不得由文件名、mtime、内存缓存或前端状态推断。`meta.json` 只能是 derived cache。

### 7.2 SessionDeleted

- `SessionDeleted` 是 tombstone，不是立即物理删除。
- 物理清理必须晚于 `purge_after_ms`，并写 audit。
- tombstone 后禁止 append 非恢复 fact。
- 旧 session 目录恢复时必须先看 tombstone。

### 7.3 SubagentSpawned/Finished

- `child_session_id` 必须是真实 canonical session。
- `parent_call_id` 必须指向 parent 的 tool call。
- `SubagentFinished` 必须与 `SubagentSpawned` 成对。
- v2.0 只冻结 edge + mailbox；graph scorer/role template 延后。
- child session 的 fact log 与 parent log 不互相复制。

---

## 8. v1 兼容、双写对账、Cutover 与 Rollback

### 8.1 旧数据源

| 旧源 | v2 角色 | cutover 前 | cutover 后 |
|---|---|---|---|
| `messages.jsonl` | legacy transcript | 双写观测 | 只读迁移输入 |
| `ringing/{journal,latest,timeline,offload}` | legacy channel state | 双写观测 | 只读归档 |
| `timeline` snapshot | derived projection | 双写/对账 | 可删除后重建 |
| `offload` | derived content | 保留读取 | content store 迁移 |
| `meta.json` | derived metadata | 继续写 | 从 facts 重建 |

#### 8.1.1 Legacy identity 与翻译矩阵

`messages.jsonl` 不是 append-only：undo/compact 会整文件 rewrite，且 `Message.msg_id` 可为空。迁移必须按“文件代”而不是单条 append 处理：

| 旧源 | legacy identity | 重写/压缩语义 | v2 翻译 |
|---|---|---|---|
| `messages.jsonl` | `(messages_generation_id, byte_offset, ordinal, content_hash)`；有 `msg_id` 时附 `legacy_msg_id` | generation id 存于 `diagnostics/legacy-messages-generation.json`；普通 append 不变，undo/compact rewrite 时递增 | 按下述 message translation 表逐条转换；无法唯一映射时只建 migration shadow，不伪造 fact |
| `ringing/journal` | `(epoch, channel, stream_seq)`；附 `event_id` | append-only；rotate 不改变记录身份 | 按 `V1CursorMapping` 映射到 canonical projection cursor |
| `ringing/latest` | `(channel, seed, checkpoint_generation, stream_seq)` | replaceable snapshot，可覆盖 | 只用于当前值对账，不生成 reliable fact |
| `ringing/timeline` | `(timeline_generation_id, seed, timeline_seq)` | generation id 在 checkpoint/rewrite 时递增 | 映射到 TimelineProjection；`timeline_seq` 不得当 `fact_seq` |
| `ringing/offload` | `content_hash` | 内容寻址，可去重 | 迁移为 `content/<sha256>`，保留原 hash |
| `meta.json` | `(seed, meta_json_sha256)` | 原子 replace | 迁移为显式 `SessionMetadataChanged` / `SessionTitleChanged` fact |

`messages.jsonl` translation：

| legacy role/block | v2 输出 | 规则 |
|---|---|---|
| user text | `InputAccepted { input_kind=user_text }` | 每个 user message 一条 |
| system/developer text | `InputAccepted { input_kind=system, actor=system }` | 保留为模型上下文输入，不伪造 user actor |
| assistant reasoning/text | `AssistantBlockSealed` | 每个 block 一条；block_id 从 `(generation, message_id, block_ordinal)` 派生 |
| assistant tool_calls[] | `ToolCallDeclared` | 每个 tool call 一条；args 写 content ref |
| tool result | migration shadow（默认） | 旧 message 不携带完整 Intent/metrics，禁止直接伪造 `ToolFinished`；只有 canonical 已有匹配 call 时经正常 reconciliation 写终态 |
| assistant 多 block | 多条 `AssistantBlockSealed` | 不允许压成一条 message fact |
| unknown role/block | migration shadow | 登记 finding，不写 canonical fact |

generation 规则：

- `messages_generation_id` 与 `timeline_generation_id` 是兼容 sidecar 中的单调整数，不从整文件 hash 派生；普通 append 不改 generation。
- rewrite/compact 必须原子递增 generation 并写 audit。
- 旧 generation 的 mapping 只用于历史对账，不得与新 generation 的记录合并。
- `ringing/timeline` 只用于对账和 legacy 读 fallback，不作为 canonical projection rebuild 输入；删除它后，TimelineProjection 必须仅从 facts 重建。
- `messages.jsonl` tool result 默认只建 shadow；只有 canonical 中已存在匹配 call/intent 时，才允许经 reconciliation 产生 `ToolFinished`。

规则：

- `source_generation_id` 改变表示旧 mapping 失效；canonical facts 已存在时禁止从新 generation 反向覆盖 canonical。
- `messages.jsonl` 中缺失 `msg_id` 的记录只能使用 generation + ordinal + content hash 作为 shadow identity。
- `Compact` 产生的新文件是新 generation；旧 generation 的 mapping 只用于观测，不用于继续 append。
- journal 与 canonical 的映射必须以实际发布记录为准；不得仅凭 `stream_seq` 猜 `fact_seq`。
- 任何旧源无法建立 1:1 翻译时，必须登记 migration finding，不得伪造 canonical fact。

### 8.2 双写对账

双写期间每条旧记录与 canonical projection 必须建立对账记录：

```rust
pub struct LegacyMapping {
    pub legacy_source: LegacySource,
    pub source_generation_id: String,
    pub legacy_key: String,       // ordinal/hash 的 canonical 编码
    pub legacy_msg_id: Option<String>,
    pub session_id: SessionId,
    pub log_id: LogId,
    pub fact_seq: u64,
    pub projection_index: Option<u16>,
    pub delivery: V1DeliveryKind,
    pub source_hash: ContentHash,
    pub mapped_at_ms: i64,
}
```

`LegacyMapping` 是 derived reconciliation index，不是 canonical source；可从旧源与 canonical log 重新生成。`source_generation_id` 必须参与唯一键，避免 rewrite 后旧 mapping 误命中新文件。

对账指标：

| 指标 | 阈值 | 超阈值动作 |
|---|---:|---|
| 缺失 canonical | 0 | 阻止 cutover |
| 重复 canonical | 0 | 阻止 cutover |
| projection mismatch | 0 | 阻止 cutover |
| seq gap | 0 | 阻止 cutover |
| replay live mismatch | 0 | 阻止 cutover |
| content ref missing | 0 | 阻止 cutover |
| torn tail unresolved | 0 | 阻止 cutover |

连续两个发布周期、全活跃 session 对账为零后，才允许停止旧 writer。

### 8.3 Cutover 阶段

1. **S0 只读观测**：旧 writer 为权威，canonical 只写 shadow。
2. **S1 单 writer facade**：所有旧写路径经 facade，canonical 成为写入事实，旧源双写。
3. **S2 读切换**：timeline/control 先读 projection；旧快照只作 fallback。
4. **S3 写切换**：停止旧 writer，保留旧文件只读。S3 是自动 rollback barrier。
5. **S4 删除 gate**：对账、replay、恢复、rollback 演练全绿后删除旧目录。

### 8.4 Rollback

rollback 命令必须满足：

- S0-S2：只需切回旧读路径，不要求反向迁移 canonical fact。
- S0-S2：旧 writer 仍在运行，rollback 后继续以旧 writer 为权威。
- S3 及之后：自动 rollback 不再支持；旧 writer 已停止，且 canonical 可能已有旧源不存在的新 fact。
- S3 后如需回退，必须另开前向 migration PR，定义 canonical→旧源回填、审计与二次 cutover；不得直接重启旧 writer 覆盖新事实。
- canonical log 在所有阶段保留完整，不因 rollback 删除。
- rollback 必须记录到 audit，并给出触发指标。

rollback 触发条件：

- 任一 P0 对账指标超阈值。
- replay live 等价失败。
- projection rebuild 失败。
- cursor 映射无法覆盖活跃 channel。
- 未知 fact 导致活跃 session 进入 read-only。

---

## 9. Invariant → Schema / State / Test 映射

| ID | Schema/状态输入 | 测试 hook |
|---|---|---|
| I1 | writer lock + `fact_seq` 分配 | 并发 append conflict；JSONL 无交叉写 |
| I2 | `fact_seq` 连续 + snapshot cursor | crash 注入后 seq 连续；snapshot 不领先 |
| I3 | ProjectionSet + derived/ | 删除 derived 后 rebuild 等价 |
| I4 | TurnStarted/Finished/Interrupted | 并发输入/cancel/resume 单 active turn |
| I5 | ToolIntent/ToolFinished | 每 call 唯一终态；取消/超时/错误均有终态 |
| I6 | side_effect_class/idempotency | crash between intent/result → indeterminate |
| I7 | typed output/content ref | model/display/resource/service 同源 |
| I8 | ReliableCursor | `(fact_seq, projection_index)` 严格递增；旧 log 拒绝 |
| I9 | InteractionResolved/Expired | 重复 resolution first-answer-wins |
| I10 | PolicyDecisionRef/sandbox hash | policy/sandbox/audit 失败 fail-closed |
| I11 | fact → projection 映射 | 同 fact 序列任意重放同一 snapshot |
| I12 | WireEvent 与 fact 分离 | client/TUI 不引用存储路径 |
| I13 | 队列容量 + overflow policy | 高水位 disconnect/reset/拒绝，不 OOM |
| I14 | unknown kind/version | read-only/upgrade-required，不跳过 |
| I15 | SandboxSpec hash + object refs | symlink/device/FIFO/TOCTOU 矩阵 |

---

## 10. Fixture 与测试清单

### 10.1 Canonical fixtures

| Fixture | 内容 | 断言 |
|---|---|---|
| `minimal-session.jsonl` | SessionCreated + InputAccepted + TurnStarted + TurnFinished | seq 连续、projection 可重建 |
| `multi-round.jsonl` | 两个 ModelRoundStarted + AssistantBlockSealed | block seal 顺序稳定 |
| `tool-success.jsonl` | ToolCallDeclared + Intent + Finished succeeded | 唯一终态 |
| `tool-indeterminate.jsonl` | Intent + crash + Finished indeterminate | 不重跑非幂等工具 |
| `interaction-pending.jsonl` | InteractionRequested + restart | pending 重放一次 |
| `interaction-resolved.jsonl` | Requested + two Resolved | 只接受第一个 |
| `compaction.jsonl` | facts + CompactionApplied | checkpoint 缺失可重建 |
| `torn-tail.jsonl` | 完整 fact + 半行 | torn 保留、截断、SessionRecovered |
| `unknown-fact.jsonl` | 已知前缀 + unknown kind + 已知后缀 | session read-only，后缀不解释 |
| `cursor-multi-projection.jsonl` | 一个 fact 映射 3 projection | cursor 严格递增 |
| `v1-last-event-id.json` | epoch/channel/stream_seq 映射 | 命中/过期/跨 session reset |
| `backpressure.jsonl` | replay/progress/content 高水位 | 明确 reset/拒绝 |
| `recovery-intent-stale.jsonl` | SessionRecovered 后 intent 删除失败，再产生新 open state | 不复用旧 RecoveryRef，生成新 fingerprint |
| `workspace-effect.jsonl` | ToolFinished + WorkspaceResourceChanged | ResourceProjection 只消费 resource fact，无双重应用 |

### 10.1.1 最小 JSONL 样例

以下只示例 canonical 形状，不是完整 fixture。`ts_ms`、UUID、ULID 和 hash 均为占位值。

```jsonl
{"schema":{"name":"qaqh.session-fact","version":2,"payload_version":2},"session_id":"0198f1a0-0000-7000-8000-000000000001","log_id":"0198f1a0-0000-7000-8000-000000000002","fact_seq":1,"event_id":"01J00000000000000000000001","ts_ms":1789830000000,"payload":{"kind":"session_created","data":{"created_at_ms":1789830000000,"cwd":"/workspace","model":"deepseek-v4.1-flash","schema_caps":["reliable_replay","interaction_replay"]}}}
{"schema":{"name":"qaqh.session-fact","version":2,"payload_version":2},"session_id":"0198f1a0-0000-7000-8000-000000000001","log_id":"0198f1a0-0000-7000-8000-000000000002","fact_seq":2,"event_id":"01J00000000000000000000002","ts_ms":1789830000010,"causation_id":"01J00000000000000000000001","payload":{"kind":"input_accepted","data":{"input_id":"input_01J00000000000000000000000","input_kind":"user_text","inline_text":"hello","attachments":[],"actor":{"kind":"user","id":"local"}}}}
{"schema":{"name":"qaqh.session-fact","version":2,"payload_version":2},"session_id":"0198f1a0-0000-7000-8000-000000000001","log_id":"0198f1a0-0000-7000-8000-000000000002","fact_seq":3,"event_id":"01J00000000000000000000003","ts_ms":1789830000020,"turn_id":"turn_01J00000000000000000000000","causation_id":"01J00000000000000000000002","payload":{"kind":"turn_started","data":{"turn_id":"turn_01J00000000000000000000000","input_id":"input_01J00000000000000000000000","mode":"normal"}}}
```

Tool intent/finish：

```jsonl
{"schema":{"name":"qaqh.session-fact","version":2,"payload_version":2},"session_id":"0198f1a0-0000-7000-8000-000000000001","log_id":"0198f1a0-0000-7000-8000-000000000002","fact_seq":4,"event_id":"01J00000000000000000000004","ts_ms":1789830000030,"turn_id":"turn_01J00000000000000000000000","call_id":"call_01J00000000000000000000000","payload":{"kind":"tool_intent","data":{"call_id":"call_01J00000000000000000000000","execution_id":"exec_01J00000000000000000000000","policy_decision":{"outcome":"allow","rule_id":"policy/read","decided_at_ms":1789830000030},"sandbox_spec_hash":"sha256:1111111111111111111111111111111111111111111111111111111111111111","side_effect_class":"workspace_write","intent_at_ms":1789830000030}}}
{"schema":{"name":"qaqh.session-fact","version":2,"payload_version":2},"session_id":"0198f1a0-0000-7000-8000-000000000001","log_id":"0198f1a0-0000-7000-8000-000000000002","fact_seq":5,"event_id":"01J00000000000000000000005","ts_ms":1789830000040,"turn_id":"turn_01J00000000000000000000000","call_id":"call_01J00000000000000000000000","causation_id":"01J00000000000000000000004","payload":{"kind":"tool_finished","data":{"call_id":"call_01J00000000000000000000000","execution_id":"exec_01J00000000000000000000000","terminal_status":"indeterminate","error":{"code":"indeterminate_after_crash","message":"non-idempotent execution not replayed","retryable":false},"metrics":{"started_at_ms":1789830000030,"finished_at_ms":1789830000040,"retry_count":0,"output_bytes":0,"progress_bytes_total":0},"reconciled":false,"recovery_ref":{"recovery_id":"recovery_01J00000000000000000000000","recovery_event_id":"01J00000000000000000000006","recovery_input_fingerprint":"sha256:1111111111111111111111111111111111111111111111111111111111111111"},"finished_at_ms":1789830000040}}}
```

Recovery：

```jsonl
{"schema":{"name":"qaqh.session-fact","version":2,"payload_version":2},"session_id":"0198f1a0-0000-7000-8000-000000000001","log_id":"0198f1a0-0000-7000-8000-000000000002","fact_seq":6,"event_id":"01J00000000000000000000006","ts_ms":1789830000050,"payload":{"kind":"session_recovered","data":{"recovery_id":"recovery_01J00000000000000000000000","recovery_event_id":"01J00000000000000000000006","recovery_input_fingerprint":"sha256:1111111111111111111111111111111111111111111111111111111111111111","last_good_fact_seq":4,"torn_tail":false,"actions":[{"kind":"tool_indeterminate","call_id":"call_01J00000000000000000000000","execution_id":"exec_01J00000000000000000000000"}],"recovered_at_ms":1789830000050}}}
```

### 10.2 必测命令

本阶段只冻结测试清单，不实现测试。后续 P1/P2 实现时应至少提供：

```bash
cargo test -p qaqh-session session_fact_v2
cargo test -p qaqh-runtime session_fact_v2
cargo test -p qaqh-daemon v1_cursor_mapping
cargo test -p qaqh-ringing cursor_compat
cargo test -p qaqh-domain fact_payload_serde
```

### 10.3 独立反证 hook

B 在 #106 可直接反证：

- 把 `projection_index` 重排后 replay 是否出现 gap/dup。
- 删除 derived/ 后 rebuild 是否与增量一致。
- 同一 `ToolIntent` 注入 crash 后是否可能二次副作用。
- 重复 `InteractionResolved` 是否可能改变 first answer。
- `Last-Event-ID` 跨 session 命中是否被错误接受。
- unknown fact 后续是否被错误解释。
- 背压时 reliable fact 是否被静默丢弃。
- stale recovery intent 是否被错误复用到新 open state。
- workspace effect 是否只经 `WorkspaceResourceChanged` 应用一次。

---

## 11. 冻结门禁与未决项

### 11.1 本 spec 完成后允许进入的 P0 Gate

- `session-fact-v2` 字段级 schema 已冻结。
- 复合 cursor、replay、dedupe、expiry 已冻结。
- recovery fact 与幂等键已冻结。
- v1 `Last-Event-ID` 映射规则已冻结。
- 双写、cutover、rollback 阈值已冻结。
- fixture/反证清单已冻结。

### 11.2 明确延后到 P1/P2

以下不作为 #105 的 P0 未决项：

- SQLite derived metadata 的具体实现。
- segment manifest 的物理分段实现。
- Ed25519 audit 签名与远程 SIEM。
- 单 binary embedded。
- 子代理 graph scorer/role 模板。
- Windows sandbox 的完整实现。

若上述任一项被实现时改变 canonical fact、cursor 或 recovery 语义，必须先回写 #103/#105 并重新评审。

---

## 12. PR 验证要求

#105 的 PR 正文必须列出：

- 本 spec 覆盖的 issue 条目。
- `git diff --check` 结果。
- 文档链接与基线。
- 未运行 Rust 测试的原因（本阶段无生产代码）。
- 独立评审人 @AnyBuddy。
- 已知未决项及 owner/截止条件；P0 不允许存在无 owner 的未决项。

本 spec 的验收结论由 #106 独立反证报告给出；A 不得自行宣告“已通过独立评审”。
