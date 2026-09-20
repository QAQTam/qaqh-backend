# QAQH session-fact-v2 字段级 Schema、Cursor 与恢复契约

> **状态**：P0 冻结候选；PR #104 `changes_requested`，D1-D41 收口并通过复审前不得标记为已冻结
> **Issue**：[#105](https://cnb.cool/QAQ-Harness/qaqh-backend/-/issues/105)
> **上位架构**：[#103](https://cnb.cool/QAQ-Harness/qaqh-backend/-/issues/103) / PR [#104](https://cnb.cool/QAQ-Harness/qaqh-backend/-/pulls/104)
> **基线**：PR #104 base `betav2 @ 5e0a9a9`；当前 head 以 handoff §1 为唯一来源
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
4. 每个 call 恰好有一个终态 `ToolFinished`；每个实际进入执行阶段的 call 恰好有一个 `ToolIntent`。`deny`、审批拒绝与审批过期不进入执行阶段，只产生唯一 `ToolFinished`，不产生 `ToolIntent`。
5. 同 schema 未知 fact 必须 fail-closed；不得跳过未知 fact 继续解释后续事实。

### 0.3 与 I1-I18 的关系

本 spec 直接冻结 I1、I2、I3、I5、I6、I8、I9、I12、I13、I14、I17、I18 的字段和状态迁移；I4、I7、I10、I11、I15、I16 由本 spec 提供输入字段和验收 hook，具体执行边界分别由 TurnCore、Tool SDK、Policy/Sandbox、TUI reducer、SubagentSupervisor spec 继续细化。

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
| `input_purpose` | `InputPurpose` | 是 | snake_case | `trigger_turn/queue_only`；恢复补 turn 的唯一判别字段 |
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
| `recovery_ref` | `Option<RecoveryRef>` | 否 | skip none | recovery 补写 `TurnStarted` 时必填 |

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
| `idempotency_key` | `Option<String>` | 条件必填 | skip none | `idempotent_replay` 必填；`reconcile` 可作 probe 输入；`no_replay` 必须缺失 |
| `replay_capability` | `ToolReplayCapability` | 是 | tagged enum | `NoReplay/IdempotentReplay/Reconcile` |
| `policy_decision` | `PolicyDecisionRef` | 是 | — | 仅允许 `allow/ask/amend`；`deny` 不产生 `ToolIntent` |
| `effective_args_ref` | `Option<ContentRef>` | 否 | skip none | policy amend 后的实际参数 |
| `effective_args_hash` | `Option<sha256:<hex>>` | 否 | skip none | 与 effective_args_ref 一致 |
| `sandbox_spec_hash` | `sha256:<hex>` | 是 | — | 基于 effective args 的 SandboxSpec hash |
| `side_effect_class` | `SideEffectClass` | 是 | snake_case | 决定 recovery |
| `intent_at_ms` | `i64` | 是 | — | 执行前时间 |

规则：

- 有副作用的 `ToolIntent` 必须先 fsync，再允许 handler 执行。
- `policy_decision.outcome = amend` 时，`effective_args_ref` 与 `effective_args_hash` 必填；实际执行参数以 effective args 为准。
- `sandbox_spec_hash` 必须基于 effective args 计算，不能基于模型原始 args。
- v2.0 每个实际进入执行阶段的 `call_id` 恰好一个 `ToolIntent`，并且每个 `call_id` 恰好一个 `ToolFinished`；`execution_id` 在该 call 内稳定。
- `policy_decision.outcome=deny` 时不得写 `ToolIntent`；审批拒绝/过期同样不得写 `ToolIntent`。这些 call 仍必须写唯一 `ToolFinished`，因此不存在“无 `ToolIntent` 就无终态”的例外。
- 用户显式重试必须创建新的 `call_id`；不得复用旧 call 追加第二个 intent/终态。
- v2.0 `retry_count` 必须为 0；未来若允许 attempt 级重试，必须提升 `payload_version` 并新增聚合终态，不能改变现有唯一性。

`side_effect_class`、`idempotency_key`、`replay_capability` 的组合优先级如下，顺序不可交换：

1. 先查同 `call_id` 的 `ToolFinished`；存在时只返回既有终态，不进入后续判断。
2. `replay_capability = no_replay` 优先于 `side_effect_class` 和 key：禁止重放，写 `indeterminate`。
3. `replay_capability = reconcile` 时先调用 `probe_ref`；得到确定证据才写 `reconciled=true` 的终态，否则写 `indeterminate`。`idempotency_key` 只作为 probe 输入，不把 probe 结果提升为重放许可。
4. `replay_capability = idempotent_replay` 时必须有 `idempotency_key`；允许以同一 `call_id`/`execution_id` 重放一次。任何 `side_effect_class` 缺少 key 时 intent 非法。
5. `side_effect_class=ReadOnly` 不单独授予重放权；`NoReplay` 仍然是最高优先级。`External` 默认必须使用 `Reconcile`，除非工具注册表明确声明其外部操作幂等并提供稳定 key。

#### ToolFinished

| 字段 | 类型 | 必填 | serde | 规则 |
|---|---|---:|---|---|
| `call_id` | `ToolCallId` | 是 | — | 必须与 envelope 一致 |
| `execution_id` | `Option<ExecutionId>` | 条件必填 | skip none | 无 intent 的 `denied/cancelled`（含 policy 前崩溃恢复）为 None；其余终态必填且对应 intent |
| `terminal_status` | `ToolTerminalStatus` | 是 | snake_case | 唯一终态 |
| `output_ref` | `Option<ContentRef>` | 否 | skip none | typed output 内容 |
| `error` | `Option<ToolError>` | 否 | skip none | 终态错误 |
| `metrics` | `ToolMetrics` | 是 | — | 所有终态都必须可物化；无 handler 时使用下述零执行 metrics |
| `reconciled` | `bool` | 是 | — | 是否经 reconciliation 得出 |
| `evidence_ref` | `Option<ContentRef>` | 否 | skip none | probe/canonical evidence 内容引用 |
| `evidence_fact_seq` | `Option<u64>` | 否 | skip none | canonical evidence fact seq |
| `evidence_event_id` | `Option<EventId>` | 否 | skip none | canonical evidence event id |
| `recovery_ref` | `Option<RecoveryRef>` | 否 | skip none | recovery 产生的终态必填 |
| `finished_at_ms` | `i64` | 是 | — | 终态时间 |

规则：

- 一个 `(call_id)` 只允许一个 `ToolFinished`；重复终态必须拒绝或幂等返回既有 fact。
- `execution_id` 必须与同 call 的 `ToolIntent` 一致；只有 call 在 intent 之前结束的 `denied/cancelled` 终态才允许为 `None`，包括 policy deny、审批拒绝/过期，以及 `ToolCallDeclared` 后 policy 决策前崩溃的 recovery cancelled；同一无 intent recovery 路径产生的 `denied/approval_rejected` 也必须为 `None`。
- `metrics.retry_count` 在 v2.0 固定为 0。实际执行过 handler 时，`started_at_ms` 取 handler 开始时间，`finished_at_ms` 取 handler 结束时间，字节计数取 typed output/progress 的真实值。
- 无 handler 的 `denied`、审批拒绝与审批过期也必须写 metrics，规则固定为：`started_at_ms = finished_at_ms`，`retry_count=0`，`output_bytes=0`，`progress_bytes_total=0`。`finished_at_ms` 对 policy deny 取 `ToolCallDeclared` 后 actor 作出 deny 的 canonical `ToolFinished.finished_at_ms`，对 ask 拒绝取 `InteractionResolved.resolved_at_ms`，对 ask 过期取 `InteractionExpired.expired_at_ms`。
- `recovery_ref` 只允许由 `RecoveryStep::ToolFinished` / recovery batch 生成的 `ToolFinished` 设置；正常 live execution、正常 deny/ask/cancel 路径必须为 `None`，不得借用该字段表达普通 provenance。
- `ToolFinished` 是唯一 call 终态；`backgrounded` 也是终态，后续资源事件不得写第二个 `ToolFinished`。

policy 生命周期是规范顺序，不允许由 handler 或 UI 自行改变：

| policy result | canonical 顺序 | `ToolIntent` | `ToolFinished` | `metrics` |
|---|---|---:|---|---|
| `allow` | `ToolCallDeclared -> ToolIntent -> execute -> ToolFinished` | 有 | 唯一终态 | 执行 metrics |
| `amend` | 生成 effective args -> `ToolIntent -> execute -> ToolFinished` | 有 | 唯一终态 | 执行 metrics |
| `ask` 批准 | `InteractionRequested -> InteractionResolved(approved) -> ToolIntent -> execute -> ToolFinished` | 有 | 唯一终态 | 执行 metrics |
| `ask` 拒绝 | `InteractionRequested -> InteractionResolved(rejected) -> ToolFinished(denied)` | 无 | 唯一 `denied` | 零执行 metrics |
| `ask` 过期 | `InteractionRequested -> InteractionExpired -> ToolFinished(cancelled)` | 无 | 唯一 `cancelled`，`error.code=approval_expired` | 零执行 metrics |
| `deny` | `ToolCallDeclared -> ToolFinished(denied)` | 无 | 唯一 `denied` | 零执行 metrics |

`allow`、`ask`、`amend` 是 `ToolIntentPolicyOutcome` 的闭集值；`deny` 是 intent 之前结束的 policy result，不属于该 enum，也绝不能写入 `ToolIntent.policy_decision`。

零执行 metrics 的 canonical JSON 固定为：

```json
{"started_at_ms":1789830000025,"finished_at_ms":1789830000025,"retry_count":0,"output_bytes":0,"progress_bytes_total":0}
```

`started_at_ms` 与 `finished_at_ms` 按上文对应事实的时间字段填入；其余字段不得因 deny/拒绝/过期而省略。

被 `deny`、审批拒绝或审批过期的 call 不执行 handler，不产生 `ToolIntent`，但仍必须写唯一 `ToolFinished`。`execution_id=None` 只允许出现在上述无 intent 的 `denied/cancelled` 终态，以及 §6.2/§6.3.1 定义的 policy 决策前崩溃 recovery cancelled 和 recovery denied。
对应 `error.code` 固定为：policy deny 使用 `policy_denied`，ask 拒绝使用 `approval_rejected`，ask 过期使用 `approval_expired`；三者均 `retryable=false` 且按上表写零执行 metrics。

旧 v1 terminal 名称只允许在兼容 adapter 输入侧出现，必须按下表逐项映射；canonical fact 只接受 §2.5 的唯一 `ToolTerminalStatus`：

| v1 名称 | v2 `ToolTerminalStatus` | 规则 |
|---|---|---|
| `Ok` | `succeeded` | 成功且结果完整 |
| `Error` | `failed` | handler 返回稳定错误 |
| `Partial` | `partial` | 有部分可消费 output，但业务结果不完整 |
| `Backgrounded` | `backgrounded` | 已交付后台资源；call 本身已终态 |
| `Cancelled` | `cancelled` | 执行前/执行中被取消；审批过期也映射到此项 |

`backgrounded` 的语义是 call 的唯一终态：写 `ToolFinished { terminal_status=backgrounded }` 后，后台任务的后续完成、失败、取消只写关联的 `WorkspaceResourceChanged` 或 `SubagentFinished`，通过 `source_call_id` / `parent_call_id` 和 `causation_id` 指回该 `ToolFinished`。后续资源事件不得改写 `ToolFinished`，不得追加第二个终态；需要表达后台结果时使用资源事件的 `revision/status`。

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

规则：first-answer-wins；重复 resolution 返回既有结果，不写第二个 fact。仅 `Pending` 状态可转 `Resolved`。`kind=permission` 时，`decision_ref` 必须指向 canonical `PermissionDecision` JSON：`{"decision":"approved"}` 或 `{"decision":"rejected"}`；`rejected` 必须映射为无 intent 的 `ToolFinished { terminal_status=denied }`。

#### InteractionExpired

| 字段 | 类型 | 必填 | serde | 规则 |
|---|---|---:|---|---|
| `interaction_id` | `InteractionId` | 是 | — | 必须与 envelope 一致 |
| `reason` | `InteractionExpiryReason` | 是 | snake_case | `timeout/session_closed/restart_policy/turn_cancelled` |
| `recovery_ref` | `Option<RecoveryRef>` | 否 | skip none | recovery 产生的 expiry 必填 |
| `expired_at_ms` | `i64` | 是 | — | 闭合时间 |

Interaction 终态状态机：

```text
Pending -> Resolved
Pending -> Expired
Resolved -> terminal
Expired -> terminal
```

turn cancel 与 resolution 的竞态固定由 actor 串行裁决：若 `InteractionResolved` 已提交，则 first-answer-wins，cancel 不得再写 `InteractionExpired`；若 resolution 尚未提交，cancel 必须在同一 transition 内先写 `InteractionExpired { reason=turn_cancelled }`，再写唯一 `ToolFinished { terminal_status=cancelled }`，最后写 `TurnFinished { terminal=cancelled }`。恢复重放必须保持该顺序。

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
| `outcome` | `RecoveryOutcome` | 是 | snake_case | `writable/commit_recovery_required/read_only_upgrade_required/tombstone` |
| `last_good_fact_seq` | `u64` | 是 | — | 最后完整 fact |
| `torn_tail` | `bool` | 是 | — | 是否发现撕裂 |
| `torn_bytes` | `Option<u64>` | 否 | skip none | 撕裂字节数 |
| `actions` | `Vec<RecoveryAction>` | 是 | 空数组序列化 | 已执行的闭合动作 |
| `recovered_at_ms` | `i64` | 是 | — | 恢复时间 |

规则：同一 recovery batch key `(log_id, recovery_input_fingerprint, last_good_fact_seq)` 只允许一个 `SessionRecovered`；重复 load/恢复不得产生第二个终态。新的 batch 必须由 §6.1 的 pre-recovery 输入变化或新的 open state 触发。

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
| `source_call_id` | `Option<ToolCallId>` | 否 | skip none | 后台 tool 的关联 call；只用于关联，不改变 call 终态 |
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

`SubagentSpawned` 没有 `recovery_ref`：spawn edge 只能由正常两阶段 spawn 写入，恢复流程只能补写 `SubagentFinished`，不得伪造或重写 spawn。

#### SubagentFinished

| 字段 | 类型 | 必填 | serde | 规则 |
|---|---|---:|---|---|
| `child_session_id` | `SessionId` | 是 | — | 必须已有 Spawned |
| `parent_call_id` | `ToolCallId` | 是 | — | 对应 parent call |
| `status` | `SubagentTerminalStatus` | 是 | snake_case | `completed/failed/cancelled/timed_out` |
| `result_ref` | `Option<ContentRef>` | 否 | skip none | 最终结果 |
| `finished_at_ms` | `i64` | 是 | — | 终态时间 |
| `recovery_ref` | `Option<RecoveryRef>` | 否 | skip none | recovery 补写 edge 终态时必填 |

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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ResetReason {
    CursorExpired,
    LogIdMismatch,
    UnknownFact,
    UpgradeRequired,
    ReplayOverflow,
    V1EpochMismatch,
    CrossSession,
    SnapshotMissing,
    SnapshotExpired,
    SnapshotHashMismatch,
    StaleWriter,
    ContentQuotaExceeded,
    PerConnectionOverflow,
    ProgressBufferOverflow,
    ActorMailboxOverflow,
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
pub enum InputPurpose { TriggerTurn, QueueOnly }
pub enum TurnMode { Normal, Plan, Ask }
pub enum AssistantBlockKind { Reasoning, Answer, ToolCall }
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolIntentPolicyOutcome { Allow, Ask, Amend }
pub struct PolicyDecisionRef {
    pub outcome: ToolIntentPolicyOutcome,
    pub rule_id: String,
    pub decided_at_ms: i64,
    pub reason_ref: Option<ContentRef>,
}

pub enum SideEffectClass { ReadOnly, WorkspaceWrite, Process, Network, External }
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RingingChannel { Control, Conversation, Tool }
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityState { Idle, Running, Interrupted }
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolReplayCapability {
    NoReplay,
    IdempotentReplay,
    Reconcile { probe_ref: ContentRef },
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolTerminalStatus {
    Succeeded,
    Failed,
    Partial,
    Cancelled,
    TimedOut,
    Backgrounded,
    Indeterminate,
    Denied,
}
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionDecision { Approved, Rejected }
pub enum InteractionExpiryReason { Timeout, SessionClosed, RestartPolicy, TurnCancelled }
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
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum RecoveryOutcome {
    Writable,
    CommitRecoveryRequired,
    ReadOnlyUpgradeRequired,
    Tombstone,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ContentUnavailableReason {
    GarbageCollected,
    Missing,
    HashMismatch,
    Offloaded,
}
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentUnavailable {
    pub content_ref: ContentRef,
    pub reason: ContentUnavailableReason,
    pub observed_at_logical_ms: i64,
    pub source_fact_seq: Option<u64>,
    pub gc_event_seq: Option<u64>,
}
```

`ToolReplayCapability` 的 canonical serde 形状是 internally tagged enum：

```jsonl
{"kind":"no_replay"}
{"kind":"idempotent_replay"}
{"kind":"reconcile","probe_ref":"sha256:2222222222222222222222222222222222222222222222222222222222222222"}
```

`CommitRecoveryRequired` 是 recovery-only 状态。若无法在不越过 committed high-water 的前提下 durable append，则不得伪造 `SessionRecovered`，该状态通过 recovery/reset 面暴露；一旦 repair 能使 marker/high-water 可证明，最终 `SessionRecovered` 必须记录 `commit_recovery_required` 或 repair 后的 `writable`。该状态没有 `duration`/超时放行分支；只能由 repair 路径退出，repair 前不得 publish/ack 未提交前缀。`SessionRecovered { outcome=commit_recovery_required }` 只表示“已 durable 记录仍需 repair 的终态”，绝不授予 writable；只有 repair 完成并写 `outcome=writable` 后才可恢复写入。

`ToolTerminalStatus` 只在上面定义一次；下文出现的旧状态名只是兼容输入，不是第二套 canonical enum。`ContentUnavailable` 的 Rust 类型在此定义，其 canonical JSON 表达、持久化和 rebuild 规则统一见 §5.5。

字段规则：

| 类型 | 规则 |
|---|---|
| `ActorRef.id` | user 为稳定本地身份；api 为 key id；agent/subagent 为 session/call 身份 |
| `PolicyDecisionRef.rule_id` | 必须来自 policy engine，不接受自由文本 |
| `SideEffectClass` | 闭集；新增类别必须提升 `payload_version` |
| `RingingChannel` | 闭集，只能为 `control/conversation/tool`；不得扩展 `session` |
| `ActivityState` | 闭集；`ControlDelta::Activity` 不接受自由字符串 |
| `ToolMetrics` | 时间使用毫秒；计数不允许负数 |
| `ToolError.code` | 稳定 snake_case；message 必须脱敏 |
| `TurnError.code` | 稳定 snake_case；message 必须脱敏 |
| `ResourceId` | `String`，在 `resource_kind` 内唯一 |
| enum 未知值 | canonical fact 同 `payload_version` 内 fail-closed；wire 可映射为 `Unknown` |

`RecoveryAction` 是 tagged enum：

| `kind` | 附加字段 | 含义 |
|---|---|---|
| `turn_interrupted` | `turn_id`, `last_fact_seq` | 闭合未完成 turn |
| `turn_started` | `turn_id`, `input_id`, `mode`, `recovery_ref` | 恢复补写 durable input 的唯一 turn |
| `tool_finished` | `completion: RecoveryToolCompletion` | 恢复补写唯一 `ToolFinished`；`terminal_status` 区分 indeterminate/replayed/reconciled/denied 等子语义 |
| `interaction_expired` | `interaction_id`, `reason` | 超时/重启/turn cancel 闭合 |
| `subagent_finished` | `child_session_id`, `child_log_id`, `terminal_fact_seq`, `terminal_event_id`, `parent_call_id`, `status`, `result_ref`, `finished_at_ms`, `recovery_ref` | 恢复补写 child edge 终态 |
| `upgrade_superseded` | `previous_recovery_id` | 已验证新 payload version，显式退出 read-only |
| `commit_repaired` | `previous_commit_generation`, `committed_fact_seq`, `committed_offset` | 修复或重建 commit marker，使 high-water 可继续使用 |
| `move_torn_tail` | `from`, `to`, `bytes`, `bytes_hash` | 保留并截断 torn tail |
| `projection_rebuilt` | `projection`, `through_fact_seq` | 重建 derived projection |

`tool_finished.completion.terminal_status=denied` 时，`error.code` 只能是 `approval_rejected` 或 `policy_denied`，且必须等于对应 `ToolFinished.error.code`。每个无 intent 的 denied recovery completion 必须在最终 `SessionRecovered.actions` 中产生恰好一个对应的 `tool_finished` action；该路径没有 `execution_id`，不得伪造执行身份。

`RecoveryAction` 与 `RecoveryStep` 的映射规则：

- `turn_interrupted`、`turn_started`、`tool_finished`、`interaction_expired`、`subagent_finished`、`move_torn_tail` 必须分别映射到同名的 `RecoveryStep`；其字段集必须与对应 step 的 `data` 逐字段同构，新增或删除 step 字段时必须同步 action，禁止两处维护不同字段清单。`replayed` / `reconciled` / `indeterminate` / `denied` 只是 `RecoveryToolCompletion.terminal_status` 的子语义，不另设 action。
- `upgrade_superseded` 与 `commit_repaired` 是显式“不产生 step”的 sidecar-only action；前者幂等键为 `(log_id, upgrade-fence.generation+1)`，后者为 `(log_id, previous_commit_generation)`，均不进入 `RecoveryPlan.steps` / `plan_hash`。
- `projection_rebuilt` 是派生 projection 动作，不产生 canonical step。

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

### 2.6 FactPayload 全量 golden JSON

以下 21 行按 §2.1 的 variant 顺序逐一冻结每个 `FactPayload` 的 canonical serde 形状。每行是 payload 值（不是完整 `SessionFact` envelope）；完整 envelope 形状见 §1.1/§10.1.1。UUID、ULID、时间和 hash 是固定 golden 占位值，不得在 fixture 中改成随机值。新增 variant 或改变任一 payload 字段必须提升 `payload_version` 并同时新增本表行。

```jsonl
{"kind":"session_created","data":{"created_at_ms":1789830000000,"cwd":"/workspace","model":"deepseek-v4.1-flash","schema_caps":["reliable_replay","interaction_replay"]}}
{"kind":"input_accepted","data":{"input_id":"input_01J00000000000000000000000","input_kind":"user_text","input_purpose":"trigger_turn","inline_text":"hello","attachments":[],"actor":{"kind":"user","id":"local"}}}
{"kind":"turn_started","data":{"turn_id":"turn_01J00000000000000000000000","input_id":"input_01J00000000000000000000000","mode":"normal"}}
{"kind":"model_round_started","data":{"turn_id":"turn_01J00000000000000000000000","round":0,"request_hash":"sha256:1111111111111111111111111111111111111111111111111111111111111111","context_revision":1}}
{"kind":"assistant_block_sealed","data":{"turn_id":"turn_01J00000000000000000000000","block_id":"block_01J00000000000000000000000","kind":"answer","content_ref":"sha256:2222222222222222222222222222222222222222222222222222222222222222","model":"deepseek-v4.1-flash"}}
{"kind":"tool_call_declared","data":{"turn_id":"turn_01J00000000000000000000000","call_id":"call_01J00000000000000000000000","tool_name":"read_file","args_ref":"sha256:3333333333333333333333333333333333333333333333333333333333333333","args_hash":"sha256:3333333333333333333333333333333333333333333333333333333333333333"}}
{"kind":"tool_intent","data":{"call_id":"call_01J00000000000000000000000","execution_id":"exec_01J00000000000000000000000","replay_capability":{"kind":"no_replay"},"policy_decision":{"outcome":"allow","rule_id":"policy/read","decided_at_ms":1789830000030},"sandbox_spec_hash":"sha256:4444444444444444444444444444444444444444444444444444444444444444","side_effect_class":"read_only","intent_at_ms":1789830000030}}
{"kind":"tool_finished","data":{"call_id":"call_01J00000000000000000000000","execution_id":"exec_01J00000000000000000000000","terminal_status":"succeeded","output_ref":"sha256:5555555555555555555555555555555555555555555555555555555555555555","metrics":{"started_at_ms":1789830000030,"finished_at_ms":1789830000040,"retry_count":0,"output_bytes":12,"progress_bytes_total":12},"reconciled":false,"finished_at_ms":1789830000040}}
{"kind":"interaction_requested","data":{"interaction_id":"int_01J00000000000000000000000","call_id":"call_01J00000000000000000000000","turn_id":"turn_01J00000000000000000000000","kind":"permission","request_ref":"sha256:6666666666666666666666666666666666666666666666666666666666666666","expires_at_ms":1789830300000,"requested_at_ms":1789830000020}}
{"kind":"interaction_resolved","data":{"interaction_id":"int_01J00000000000000000000000","decision_ref":"sha256:7777777777777777777777777777777777777777777777777777777777777777","resolved_by":{"kind":"user","id":"local"},"resolution_seq":1,"resolved_at_ms":1789830000025}}
{"kind":"interaction_expired","data":{"interaction_id":"int_01J00000000000000000000000","reason":"timeout","expired_at_ms":1789830300000}}
{"kind":"turn_finished","data":{"turn_id":"turn_01J00000000000000000000000","terminal":"completed","finished_at_ms":1789830000050}}
{"kind":"turn_interrupted","data":{"turn_id":"turn_01J00000000000000000000000","reason":"crash","last_fact_seq":7,"recovery_ref":{"recovery_id":"recovery_01J00000000000000000000000","recovery_event_id":"01J00000000000000000000007","recovery_input_fingerprint":"sha256:8888888888888888888888888888888888888888888888888888888888888888"}}}
{"kind":"session_recovered","data":{"recovery_id":"recovery_01J00000000000000000000000","recovery_event_id":"01J00000000000000000000007","recovery_input_fingerprint":"sha256:8888888888888888888888888888888888888888888888888888888888888888","outcome":"writable","last_good_fact_seq":7,"torn_tail":false,"actions":[],"recovered_at_ms":1789830000060}}
{"kind":"compaction_applied","data":{"checkpoint_id":"ckpt_01J00000000000000000000000","replaces_through_fact_seq":7,"summary_ref":"sha256:9999999999999999999999999999999999999999999999999999999999999999","context_revision":2,"applied_at_ms":1789830000060}}
{"kind":"session_metadata_changed","data":{"patch":{"model":"deepseek-v4.1-flash"},"source":"user","changed_at_ms":1789830000060}}
{"kind":"session_title_changed","data":{"title":"Golden session","source":"user","changed_at_ms":1789830000060}}
{"kind":"session_deleted","data":{"tombstone_at_ms":1789830000070,"reason":"user"}}
{"kind":"workspace_resource_changed","data":{"resource_kind":"file","resource_id":"res_01J00000000000000000000000","source_call_id":"call_01J00000000000000000000000","revision":1,"summary_ref":"sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa","deleted":false}}
{"kind":"subagent_spawned","data":{"child_session_id":"0198f1a0-0000-7000-8000-000000000003","parent_call_id":"call_01J00000000000000000000000","spawned_at_ms":1789830000080}}
{"kind":"subagent_finished","data":{"child_session_id":"0198f1a0-0000-7000-8000-000000000003","parent_call_id":"call_01J00000000000000000000000","status":"completed","result_ref":"sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb","finished_at_ms":1789830000090}}
```

§2.6、§4.2.1、§10.1.1 的 `input_accepted` 示例统一使用 `input_purpose=trigger_turn`；`queue_only` 只允许出现在恢复 fixture，不得作为这三处 golden 的字段值。

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

### 3.1.1 ProjectionEvent、ProjectionPayload、WireEvent、StreamKey

以下类型是 §3/§4 的唯一 wire/projection 边界；`ProjectionPayload` 的每个 variant 都有 typed payload，禁止只传一个无类型的 `payload_ref`：

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum StreamKey {
    Channel(RingingChannel),
    Resource { kind: ResourceKind, id: ResourceId },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ProjectionEvent {
    pub event_id: EventId,
    pub source_fact_seq: u64,
    pub source_event_id: EventId,
    pub stream_key: StreamKey,
    pub delivery: Delivery,
    /// Reliable 必须为 Some，且 projection_index 必须等于该 slot 的 repr 值。
    pub projection_slot: Option<ProjectionSlot>,
    /// Reliable 必须为 Some，且等于 Delivery::Reliable.cursor.projection_index；
    /// Replaceable/Ephemeral 必须为 None。
    pub projection_index: Option<u16>,
    pub payload: ProjectionPayload,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum ProjectionPayload {
    ConversationDelta(ConversationDelta),
    TimelineDelta(TimelineDelta),
    ControlDelta(ControlDelta),
    ResourceDelta(ResourceDelta),
    MetaDelta(MetaDelta),
    AuditRef(AuditRef),
    Unknown(UnknownProjection),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum ContentValue {
    Inline { text: String },
    Ref { content_ref: ContentRef },
    Unavailable(ContentUnavailable),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum ConversationDelta {
    InputAccepted { revision: u64, input_id: InputId, input_kind: InputKind, input_purpose: InputPurpose, content: ContentValue, attachments: Vec<ContentRef>, actor: ActorRef },
    TurnStarted { revision: u64, turn_id: TurnId, input_id: InputId, mode: TurnMode },
    AssistantBlockSealed { revision: u64, turn_id: TurnId, block_id: BlockId, block_kind: AssistantBlockKind, content: ContentValue, model: String, usage: Option<UsageInfo> },
    ToolCallDeclared { revision: u64, turn_id: TurnId, call_id: ToolCallId, tool_name: String, args: ContentValue, args_hash: ContentHash },
    ToolFinished { revision: u64, call_id: ToolCallId, terminal_status: ToolTerminalStatus, output: Option<ContentValue>, error: Option<ToolError>, metrics: ToolMetrics, reconciled: bool },
    TurnFinished { revision: u64, turn_id: TurnId, terminal: TurnTerminal, usage: Option<UsageInfo>, error: Option<TurnError> },
    TurnInterrupted { revision: u64, turn_id: TurnId, reason: InterruptReason, last_fact_seq: u64, recovery_ref: RecoveryRef },
    CompactionApplied { revision: u64, checkpoint_id: CheckpointId, replaces_through_fact_seq: u64, summary: ContentValue, context_revision: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum TimelineDelta {
    Input { revision: u64, input_id: InputId, content: ContentValue },
    Block { revision: u64, block_id: BlockId, block_kind: AssistantBlockKind, content: ContentValue },
    ToolCall { revision: u64, call_id: ToolCallId, tool_name: String, args: ContentValue },
    ToolResult { revision: u64, call_id: ToolCallId, terminal_status: ToolTerminalStatus, output: Option<ContentValue>, error: Option<ToolError> },
    TurnFinished { revision: u64, turn_id: TurnId, terminal: TurnTerminal },
    TurnInterrupted { revision: u64, turn_id: TurnId, reason: InterruptReason },
    Compaction { revision: u64, checkpoint_id: CheckpointId, summary: ContentValue },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum ControlDelta {
    SessionCreated { revision: u64, session_id: SessionId, cwd: String, model: String, schema_caps: Vec<String> },
    Round { revision: u64, turn_id: TurnId, round: u32, request_hash: ContentHash, context_revision: u64 },
    Activity { revision: u64, turn_id: Option<TurnId>, call_id: Option<ToolCallId>, state: ActivityState },
    ToolIntent { revision: u64, call_id: ToolCallId, execution_id: ExecutionId, policy_decision: PolicyDecisionRef, replay_capability: ToolReplayCapability, side_effect_class: SideEffectClass, intent_at_ms: i64 },
    ToolFinished { revision: u64, call_id: ToolCallId, execution_id: Option<ExecutionId>, terminal_status: ToolTerminalStatus, output: Option<ContentValue>, error: Option<ToolError>, metrics: ToolMetrics, reconciled: bool },
    InteractionRequested { revision: u64, interaction_id: InteractionId, call_id: Option<ToolCallId>, kind: InteractionKind, request: ContentValue, expires_at_ms: Option<i64> },
    InteractionResolved { revision: u64, interaction_id: InteractionId, decision: ContentValue, resolved_by: ActorRef, resolution_seq: u64 },
    InteractionExpired { revision: u64, interaction_id: InteractionId, reason: InteractionExpiryReason },
    SessionRecovered { revision: u64, outcome: RecoveryOutcome, recovery_ref: RecoveryRef, actions: Vec<RecoveryAction> },
    SubagentSpawned { revision: u64, child_session_id: SessionId, parent_call_id: ToolCallId, role: Option<String> },
    SubagentFinished { revision: u64, child_session_id: SessionId, parent_call_id: ToolCallId, status: SubagentTerminalStatus, result: Option<ContentValue> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum ResourceDelta {
    WorkspaceResourceChanged { revision: u64, resource_kind: ResourceKind, resource_id: ResourceId, source_call_id: Option<ToolCallId>, summary: ContentValue, deleted: bool },
    GraphEdge { revision: u64, child_session_id: SessionId, parent_call_id: ToolCallId, status: Option<SubagentTerminalStatus> },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum MetaDelta {
    Created { revision: u64, cwd: String, model: String, parent_session_id: Option<SessionId>, schema_caps: Vec<String> },
    MetadataChanged { revision: u64, patch: SessionMetadataPatch },
    TitleChanged { revision: u64, title: String, source: TitleSource },
    Deleted { revision: u64, tombstone_at_ms: i64, reason: DeleteReason, purge_after_ms: Option<i64> },
    ContextRevision { revision: u64, checkpoint_id: CheckpointId, context_revision: u64 },
    Recovered { revision: u64, outcome: RecoveryOutcome, recovery_ref: RecoveryRef },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AuditRef { pub audit_seq: u64, pub audit_hash: ContentHash }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UnknownProjection { pub raw_ref: ContentRef }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum WirePayload {
    Projection(ProjectionPayload),
    Heartbeat,
    ResetRequired(ResetRequired),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WireEvent {
    pub wire_version: u32,
    pub stream_key: StreamKey,
    pub event_id: EventId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cursor: Option<ReliableCursor>,
    pub delivery: Delivery,
    pub payload: WirePayload,
}
```

字段规则：

- `ProjectionPayload` 不得包含 UI 动画、SSE 文本或 provider 原始 JSON；`ConversationDelta`/`TimelineDelta`/`ControlDelta`/`ResourceDelta`/`MetaDelta` 的 variant 名称和字段是稳定 schema。
- `ContentValue::Ref` 只允许指向 typed projection 内容；大内容必须带 `ContentRef`，但 `ContentValue::Unavailable` 必须保留 `ContentUnavailable` 结构，不得降级为空文本。
- `ProjectionEvent.projection_slot` 和 `projection_index` 只对 `Reliable` 有值；`projection_index` 必须等于 `projection_slot as u16`，`Replaceable`/`Ephemeral` 不得占用 canonical index。
- `StreamKey::Channel` 只允许 `RingingChannel::Control`、`RingingChannel::Conversation`、`RingingChannel::Tool`，JSON channel 值只能是 `control`、`conversation`、`tool`；不存在 `session` channel。
- `WireEvent.delivery` 必须原样复制来源 `ProjectionEvent.delivery`，因此 `Reliable { cursor }`、`Replaceable { revision }` 与 `Ephemeral` 在 wire 上可直接区分；adapter 不得把三者都压成裸 cursor 或裸 payload。`delivery=Reliable` 时 `WireEvent.cursor` 必须为 `Some` 且等于 `delivery.cursor`；`Replaceable`/`Ephemeral` 时必须为 `None`。
- `ControlDelta::Activity.state` 必须使用闭集 `ActivityState`：`turn_started -> running`、`turn_finished -> idle`、`turn_interrupted -> interrupted`。实现不得接受自由字符串；新增状态必须提升 `payload_version`。
- `WireEvent.payload` 使用 typed `WirePayload`；adapter 才把它编码为 SSE JSON，业务层不得读取或解析 provider JSON。
- `AuditRef` 只引用全局 audit seq/hash，不进入 session cursor；`Unknown` 只用于 wire 兼容，canonical unknown fact 仍必须 fail-closed。

`WireEvent.delivery` 的三种 canonical JSON 形状：

```jsonl
{"wire_version":2,"stream_key":{"kind":"channel","data":"control"},"event_id":"01J00000000000000000000021","cursor":{"log_id":"0198f1a0-0000-7000-8000-000000000002","fact_seq":1,"projection_index":2},"delivery":{"Reliable":{"cursor":{"log_id":"0198f1a0-0000-7000-8000-000000000002","fact_seq":1,"projection_index":2}}},"payload":{"kind":"heartbeat"}}
{"wire_version":2,"stream_key":{"kind":"channel","data":"conversation"},"event_id":"01J00000000000000000000022","delivery":{"Replaceable":{"revision":7}},"payload":{"kind":"heartbeat"}}
{"wire_version":2,"stream_key":{"kind":"channel","data":"tool"},"event_id":"01J00000000000000000000023","delivery":"Ephemeral","payload":{"kind":"heartbeat"}}
```

### 3.2 排序

1. 只有 `log_id` 相同才能比较 `fact_seq` 和 `projection_index`。
2. 同一 `log_id` 内按 `(fact_seq, projection_index)` 字典序升序。
3. `fact_seq` 必须连续，不允许 gap；`projection_index` 由 §4.2 的 per-fact/per-slot 表冻结，同一 fact 内稳定，允许稀疏。
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

- `projection_index` 由 §4.2 的静态 `ProjectionSlot` 表决定，不由运行时 delta 顺序或 `ProjectionSet` 字段声明顺序决定。
- slot 允许稀疏；某 projection 不消费该 fact 时跳过该 slot，但不得重编号其它 slot。
- 只有实际产生 delta 的 projection 发布 `ProjectionEvent`；事件数量不改变 index。
- 新增 projection 只能追加更高 slot，且必须提升 `payload_version`；旧 cursor 不得被重排。
- reliable `projection_index` 只允许 `0..=65534`；`65535` 保留给 snapshot `END_OF_FACT`。
- 同一 fact 多次 rebuild 必须得到相同 cursor 映射。

### 3.5 Cursor expiry

`ReplayWindowManifest` 是 replay window 的唯一规范来源，持久化于 `{session}/replay-window.json`：

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReplayWindowReason {
    CapacityFacts,
    CapacityBytes,
    Retention,
    SnapshotRotated,
    Manual,
}

pub struct ReplayWindowManifest {
    pub schema: String, // "qaqh.replay-window/v1"
    pub log_id: LogId,
    pub generation: u64,
    pub earliest_available_fact_seq: u64,
    pub latest_fact_seq: u64,
    pub earliest_cursor: ReliableCursor,
    pub snapshot_cursor: Option<ReliableCursor>,
    pub snapshot_generation: u64,
    pub snapshot_hash: ContentHash,
    pub snapshot_path: String,
    pub snapshot_fact_seq: Option<u64>,
    pub snapshot_created_at_logical_ms: Option<i64>,
    pub snapshot_expires_at_logical_ms: Option<i64>,
    pub logical_now_ms: i64,
    pub retained_from_ms: i64,
    pub retained_until_ms: i64,
    pub window_capacity_facts: u64,
    pub window_capacity_bytes: u64,
    pub retained_facts: u64,
    pub retained_bytes: u64,
    pub reason: ReplayWindowReason,
}
```

默认窗口参数冻结为：`window_capacity_facts = 100_000`、`window_capacity_bytes = 64 MiB`、`replay_window_retention_ms = 30 days`、`snapshot_max_age_ms = 7 days`。`logical_now_ms` 是单调逻辑 clock，不是 `SystemTime::now()`；它必须复制同一 session 最新 durable `ContentClockRecord.logical_now_ms`，不得独立推进，也不得因墙钟回拨减小。manifest 更新顺序固定为：canonical fact durable -> `content/clock.json` durable -> manifest 写入同一 `logical_now_ms` -> replay/GC 判定。

`manifest.generation` 每次 manifest 变更递增；`snapshot_generation` 只在 snapshot 内容或覆盖边界改变时递增。`snapshot_path` 必须是相对 `{session}/snapshots/` 的规范路径，`snapshot_hash` 是该文件 bytes 的 SHA-256。

下界按以下公式计算，所有减法使用饱和减法：

```text
seq_floor       = max(1, latest_fact_seq - window_capacity_facts + 1)
byte_floor      = min f，使得 sum(canonical_json_len(i), i=f..=latest_fact_seq)
                  <= window_capacity_bytes 且加入 f-1 后会超过；至少取 latest_fact_seq
time_floor      = 满足 fact.ts_ms >= logical_now_ms - replay_window_retention_ms 的最小 fact_seq；
                  若全部过期则取 latest_fact_seq
snapshot_valid  = snapshot_fact_seq.is_some()
                  && snapshot_generation/snapshot_hash/snapshot_path 与 manifest 一致
                  && snapshot 未超过 snapshot_max_age_ms
snapshot_floor  = if snapshot_valid { snapshot_fact_seq + 1 } else { 1 }

earliest_available_fact_seq =
    max(seq_floor, byte_floor, time_floor, snapshot_floor)
earliest_cursor = (log_id, earliest_available_fact_seq, 0)
retained_facts  = latest_fact_seq - earliest_available_fact_seq + 1
retained_bytes  = sum(canonical_json_len(f) for f in earliest_available_fact_seq..=latest_fact_seq)
snapshot_cursor = Some((log_id, snapshot_fact_seq, u16::MAX))  // snapshot_valid 时
snapshot_cursor = None                                           // snapshot 缺失或失效时
```

`time_floor`、`snapshot_created_at_logical_ms`、`snapshot_expires_at_logical_ms` 与 Content GC 的 `delete_after_ms` 判定必须读取同一个 `logical_now_ms`。manifest 不是 clock 的 owner；重启时先按 §5.5 从 `content/clock.json` + manifest + canonical facts 恢复 `logical_now_ms`，再计算 `time_floor`，最后把恢复值和重算后的窗口写回 manifest。

| 条件 | 结果 |
|---|---|
| cursor `log_id` 匹配且 cursor fact 仍在 `earliest_available_fact_seq..=latest_fact_seq` | 正常 replay |
| cursor fact 早于 `earliest_available_fact_seq`，且 snapshot 可用 | `ResetRequired { reason=CursorExpired, snapshot_cursor=Some(...) }` |
| cursor fact 早于 `earliest_available_fact_seq`，且 snapshot 缺失 | `ResetRequired { reason=SnapshotMissing }` |
| snapshot generation 或 hash 与 manifest 不一致 | `ResetRequired { reason=SnapshotHashMismatch }` |
| snapshot 文件存在但超过 `snapshot_max_age_ms` | 先重建 snapshot；无法重建则 `ResetRequired { reason=SnapshotExpired }` |
| cursor 来自已迁移/已重建 log | `ResetRequired { reason=LogIdMismatch }` |
| cursor 的 projection_index 超过该 fact 最大值 | 按已超过该 fact 处理；若没有后继 projection，则 `ResetRequired { reason=CursorExpired }` |
| cursor 命中 unknown fact | `ResetRequired` 或 read-only/upgrade-required |
| v1 epoch 不匹配 | `ResetRequired { reason=V1EpochMismatch }` |

`ResetRequired` 必须包含：

```rust
pub struct ResetRequired {
    pub log_id: LogId,
    pub snapshot_cursor: Option<ReliableCursor>,
    pub reason: ResetReason,
}
```

`snapshot_cursor = Some(...)` 时必须指向 snapshot 已覆盖的最后一个 projection 位置；`SnapshotMissing/SnapshotHashMismatch` 时必须为 `None`：

```text
snapshot_cursor = Some((log_id, snapshot_fact_seq, u16::MAX))
```

`u16::MAX` 是保留的 `END_OF_FACT` sentinel，可靠 projection 的 index 只允许 `0..=u16::MAX-1`。即使 `snapshot_fact_seq` 产生 0 个 reliable projection，snapshot cursor 仍有定义。客户端收到 `Some(snapshot_cursor)` 后必须丢弃旧 snapshot，重新拉 baseline，再从该 cursor 订阅；收到 `None` 时必须重新请求 `stream_seq = 0` baseline。`stream_seq = 0` 必须返回真实 baseline，而不是伪造 `(0,0)`。

兼容映射必须保持顺序：同一 `(epoch, channel, session)` 内若 v1 `stream_seq(a) < stream_seq(b)`，则映射后的 reliable cursor 必须严格小于 cursor(b)。无法保持顺序时必须 `ResetRequired`。

snapshot 处理规则：

- snapshot 缺失时 manifest 必须写 `snapshot_generation=0`、`snapshot_hash=sha256(empty)`、`snapshot_path=""`、`snapshot_fact_seq=None`、`snapshot_created_at_logical_ms=None`、`snapshot_expires_at_logical_ms=None`、`snapshot_cursor=None`；不得保留上一代的路径或 hash。
- snapshot 是 `derived/` 数据，不替代 canonical facts；重建必须先验证 `log_id/snapshot_generation/snapshot_hash/snapshot_fact_seq`，再更新 manifest。
- snapshot 缺失时不得伪造空 snapshot；只要 cursor 仍在 facts window 内可正常 replay，cursor 早于下界则必须 `SnapshotMissing`。
- snapshot 过期时先同步重建并 fsync；客户端若已有 cursor 仍在新窗口内，不得因后台重建而中断连接。
- 跨版本旧 cursor 必须走本节 expiry 表，不得把旧 `payload_version` 的 projection_index 解释为新 slot；fixture 见 §10.1 的 `cursor-cross-version.json`。

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
pub type ProjectionId = ProjectionSlot;

pub struct ProjectionSet {
    pub conversation: ConversationProjection,
    pub timeline: TimelineProjection,
    pub control: ControlProjection,
    pub resources: ResourceProjection,
    pub meta: SessionMetaProjection,
}
```

`ProjectionSlot` 是 `projection_index` 的唯一静态注册表；`ProjectionId` 只作为兼容 Rust 别名，不再定义第二套 ordinal：

```rust
#[repr(u16)]
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectionSlot {
    Conversation = 0,
    Timeline = 1,
    Control = 2,
    Resources = 3,
    Meta = 4,
}

pub type ProjectionIndex = u16;
```

对每个 fact，按 slot 升序调用 projection；只有实际产生 delta 的 projection 发布事件，但 index 始终使用固定 slot 值，不压缩、不重编号。表格中的书写顺序不具备规范性。`projection_index` 是 cursor 的一部分，不是运行时 delta 的连续序号。

`ToolProjection` 是 Tool SDK 的 typed output 适配器（model/display），不是 `ProjectionSet` 成员，也不分配 canonical `projection_index`。它先产出 typed output；`ControlProjection` 保存当前 tool 状态，`TimelineProjection` 保存 transcript，`ConversationProjection` 保存模型面，`ResourceProjection` 消费 workspace effects。v2.0 不新增 `ProjectionSet` ordinal。

`AuditStore` 不属于 `ProjectionSet`，也不从 `events.jsonl` 重建。

### 4.2 映射规则

下表是 v2.0 的 per-fact/per-slot 规范表。`projection_index` 必须等于左侧 slot 的 `repr` 值；不允许按是否产生 delta 动态重排。多个可靠 projection 的 cursor 顺序仍按 `(fact_seq, projection_index)` 排序。

| `FactPayload.kind` | `projection_slot` | `projection_index` | `stream_key` | `ProjectionPayload` typed variant | delivery / 辅助流 |
|---|---|---:|---|---|---|
| `session_created` | `control` | `2` | `channel:control` | `ControlDelta::SessionCreated` | `Reliable` + `Replaceable(control:current)` |
| `session_created` | `meta` | `4` | `channel:control` | `MetaDelta::Created` | `Reliable` |
| `input_accepted` | `conversation` | `0` | `channel:conversation` | `ConversationDelta::InputAccepted` | `Reliable` |
| `input_accepted` | `timeline` | `1` | `channel:conversation` | `TimelineDelta::Input` | `Reliable` + `Ephemeral(control:activity)` |
| `turn_started` | `conversation` | `0` | `channel:conversation` | `ConversationDelta::TurnStarted` | `Reliable` |
| `turn_started` | `control` | `2` | `channel:control` | `ControlDelta::Activity(state=running)` | `Reliable` + `Replaceable(control:current)` |
| `model_round_started` | `control` | `2` | `channel:control` | `ControlDelta::Round` | `Reliable` + `Ephemeral(control:activity)` |
| `assistant_block_sealed` | `conversation` | `0` | `channel:conversation` | `ConversationDelta::AssistantBlockSealed` | `Reliable` |
| `assistant_block_sealed` | `timeline` | `1` | `channel:conversation` | `TimelineDelta::Block` | `Reliable` + `Replaceable(timeline:current)`; `assistant_delta` 为 `Ephemeral` |
| `tool_call_declared` | `conversation` | `0` | `channel:conversation` | `ConversationDelta::ToolCallDeclared` | `Reliable` |
| `tool_call_declared` | `timeline` | `1` | `channel:tool` | `TimelineDelta::ToolCall` | `Reliable` + `Replaceable(control:tool_current)` |
| `tool_intent` | `control` | `2` | `channel:tool` | `ControlDelta::ToolIntent` | `Reliable` + `Replaceable(control:tool_current)` |
| `tool_finished` | `conversation` | `0` | `channel:conversation` | `ConversationDelta::ToolFinished` | `Reliable` |
| `tool_finished` | `timeline` | `1` | `channel:tool` | `TimelineDelta::ToolResult` | `Reliable` + `Replaceable(control:tool_current)`; progress 为 `Ephemeral` |
| `interaction_requested` | `control` | `2` | `channel:control` | `ControlDelta::InteractionRequested` | `Reliable` + `Replaceable(control:current)` |
| `interaction_resolved` | `control` | `2` | `channel:control` | `ControlDelta::InteractionResolved` | `Reliable` + `Replaceable(control:current)` |
| `interaction_expired` | `control` | `2` | `channel:control` | `ControlDelta::InteractionExpired` | `Reliable` + `Replaceable(control:current)` |
| `turn_finished` | `conversation` | `0` | `channel:conversation` | `ConversationDelta::TurnFinished` | `Reliable` |
| `turn_finished` | `control` | `2` | `channel:control` | `ControlDelta::Activity(state=idle)` | `Reliable` + `Replaceable(control:current)` |
| `turn_interrupted` | `conversation` | `0` | `channel:conversation` | `ConversationDelta::TurnInterrupted` | `Reliable` |
| `turn_interrupted` | `control` | `2` | `channel:control` | `ControlDelta::Activity(state=interrupted)` | `Reliable` + `Replaceable(control:current)` |
| `session_recovered` | `control` | `2` | `channel:control` | `ControlDelta::SessionRecovered` | `Reliable` + `Replaceable(control:current)` |
| `session_recovered` | `meta` | `4` | `channel:control` | `MetaDelta::Recovered` | `Reliable` |
| `compaction_applied` | `conversation` | `0` | `channel:conversation` | `ConversationDelta::CompactionApplied` | `Reliable` |
| `compaction_applied` | `meta` | `4` | `channel:control` | `MetaDelta::ContextRevision` | `Reliable` + `Replaceable(control:current)` |
| `session_metadata_changed` | `meta` | `4` | `channel:control` | `MetaDelta::MetadataChanged` | `Reliable` + `Replaceable(meta:current)` |
| `session_title_changed` | `meta` | `4` | `channel:control` | `MetaDelta::TitleChanged` | `Reliable` + `Replaceable(meta:current)` |
| `session_deleted` | `meta` | `4` | `channel:control` | `MetaDelta::Deleted` | `Reliable` + `Replaceable(meta:current)` |
| `workspace_resource_changed` | `resources` | `3` | `channel:tool` | `ResourceDelta::WorkspaceResourceChanged` | `Reliable` + `Replaceable(resources:current)`; `activity_delta` 为 `Ephemeral` |
| `subagent_spawned` | `control` | `2` | `channel:control` | `ControlDelta::SubagentSpawned` | `Reliable` + `Replaceable(control:current)` |
| `subagent_spawned` | `resources` | `3` | `channel:control` | `ResourceDelta::GraphEdge` | `Reliable` |
| `subagent_finished` | `control` | `2` | `channel:control` | `ControlDelta::SubagentFinished` | `Reliable` + `Replaceable(control:current)` |
| `subagent_finished` | `resources` | `3` | `channel:control` | `ResourceDelta::GraphEdge` | `Reliable` |

该表的每一行都是规范性映射；`stream_key` 只能取表中 `channel:control`、`channel:conversation`、`channel:tool`，不得出现 `session` channel。新增 fact kind 或 slot 必须提升 `payload_version`，并新增行而不是改变旧行的 index。没有 delta 时可以不发布对应 `ProjectionEvent`，但 cursor 的 slot 解释保持不变。`delivery` 列的 `Reliable` 部分占用 canonical `projection_index`；`Replaceable`/`Ephemeral` 辅助流不占用 canonical index，且必须按 §3.3 回放规则处理。

`input_purpose=queue_only` 的恢复不写 `TurnStarted` fact；因此它不发布 `turn_started` 的 conversation/control projection，也不推进该 slot 的 revision。§2.2 的“允许 0 个 projection=否”约束只适用于**已经存在的 fact**，不能把“没有 fact”解释成“该 fact 有零个 projection”。

### 4.2.1 Golden projection JSON 示例

以下三个示例冻结 `ProjectionEvent`/`ProjectionPayload` 的 canonical JSON 形状。示例中的 UUID、ULID、hash 和 channel 值仅为 fixture 占位值。

`SessionCreated -> control:session_created`（slot `2`）：

```json
{"event_id":"01J00000000000000000000011","source_fact_seq":1,"source_event_id":"01J00000000000000000000001","stream_key":{"kind":"channel","data":"control"},"delivery":{"Reliable":{"cursor":{"log_id":"0198f1a0-0000-7000-8000-000000000002","fact_seq":1,"projection_index":2}}},"projection_slot":"control","projection_index":2,"payload":{"kind":"control_delta","data":{"kind":"session_created","data":{"revision":1,"session_id":"0198f1a0-0000-7000-8000-000000000001","cwd":"/workspace","model":"deepseek-v4.1-flash","schema_caps":["reliable_replay","interaction_replay"]}}}}
```

`InputAccepted -> conversation:input`（slot `0`）：

```json
{"event_id":"01J00000000000000000000012","source_fact_seq":2,"source_event_id":"01J00000000000000000000002","stream_key":{"kind":"channel","data":"conversation"},"delivery":{"Reliable":{"cursor":{"log_id":"0198f1a0-0000-7000-8000-000000000002","fact_seq":2,"projection_index":0}}},"projection_slot":"conversation","projection_index":0,"payload":{"kind":"conversation_delta","data":{"kind":"input_accepted","data":{"revision":1,"input_id":"input_01J00000000000000000000000","input_kind":"user_text","input_purpose":"trigger_turn","content":{"kind":"inline","data":{"text":"hello"}},"attachments":[],"actor":{"kind":"user","id":"local"}}}}}
```

`ToolFinished -> timeline:tool_result`（slot `1`）：

```json
{"event_id":"01J00000000000000000000013","source_fact_seq":5,"source_event_id":"01J00000000000000000000005","stream_key":{"kind":"channel","data":"tool"},"delivery":{"Reliable":{"cursor":{"log_id":"0198f1a0-0000-7000-8000-000000000002","fact_seq":5,"projection_index":1}}},"projection_slot":"timeline","projection_index":1,"payload":{"kind":"timeline_delta","data":{"kind":"tool_result","data":{"revision":3,"call_id":"call_01J00000000000000000000000","terminal_status":"indeterminate","output":null,"error":{"code":"indeterminate_after_crash","message":"non-idempotent execution not replayed","retryable":false}}}}}
```

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
  events.lock                  # OS 级互斥；ownership 仅在 writer-fence.json
  writer-fence.json            # writer_id/generation_epoch/fencing_token 的 CAS lease
  upgrade-fence.json           # 版本无关的 monotonic read-only/writable supersede 状态
  events.commit.json           # 最后一次 durable barrier 的 committed_fact_seq/offset；恢复的权威 high-water
  events.poison.json           # fsync EIO 后的证据；不能替代 events.commit.json
  recovery.intent.json         # 崩溃恢复批次意图，SessionRecovered 后删除
  replay-window.json           # ReplayWindowManifest，cursor expiry 判定
  snapshots/{generation}.json  # snapshot_generation/hash/path 指向的 baseline
  content/{sha256}             # content-addressed
  content/index.jsonl          # ContentRecord，append-only
  content/clock.json           # ContentClockRecord，replay/GC 共用逻辑 clock
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

{data_dir}/quota/{root_session_id}/
  ledger.jsonl                 # root tree reservation/commit/release，append-only
  quota.lock                   # 独立于 events.lock 的跨 session 串行化/fencing 点
```

`events.jsonl` 不原地重写、不压缩、不删除单行。归档/轮转只能通过新 log 或 segment manifest，且必须保持 `fact_seq` 可解释。

`{data_dir}/quota/{root_session_id}/` 是 root 树共享账本，不属于任何 child session 目录，也不受 per-session `events.lock` / `writer-fence.json` 保护。root `SessionActor` / `SubagentSupervisor` 是唯一 owner；root 与全部 child 的 append 必须经同一 `quota.lock` 串行化。child close/delete/cutover 不得删除账本，只有 root 生命周期结束且保留期过后才允许按显式 GC 规则清理。

### 5.2 Append 协议

```text
SessionActor
  -> acquire writer lock
  -> verify writer-fence.json epoch/token
  -> validate fact + assign fact_seq
  -> append JSON line
  -> fsync/group barrier
  -> advance content/clock.json if fact.ts_ms is newer
  -> ProjectionSet.apply(fact)
  -> publish reliable projection
```

规则：

1. 同一 session 同时只有一个 writer。
2. `fact_seq` 在 append 前分配；crash 后从最后完整行恢复。
3. append 成功但 publish 失败：重连必须从 canonical log replay。
4. publish 成功但 append 未完成：实现错误，必须 panic/停止写入，不得继续。
5. `ToolIntent`、`InteractionResolved`、`InteractionExpired`、`ToolFinished` 必须独立 fsync barrier。
6. 无副作用 fact 可以 group commit，但不得跨越 durable barrier 合并。
7. writer lock 使用 OS file lock；锁文件不是事实源。
8. `content/clock.json` 的推进必须发生在 canonical fact `fsync` 且 `events.commit.json` durable 之后；commit marker durable 之前不得推进 clock、更新 replay window 或执行依赖逻辑时间的 GC。恢复时若 `clock.source_fact_seq > events.commit.committed_fact_seq`，该 clock 值必须忽略并从 committed prefix 重建，禁止用于 replay/GC。

generation fencing 的唯一持久化形状：

```rust
pub struct WriterId(pub String);

pub struct WriterFence {
    pub schema: String, // "qaqh.writer-fence/v1"
    pub session_id: SessionId,
    pub log_id: LogId,
    pub writer_id: WriterId,
    pub generation_epoch: u64,
    pub fencing_token: u128, // JSON/CLI 表示为无符号十进制字符串
    pub acquired_at_ms: i64,
    pub lease_expires_at_ms: i64,
}

pub struct AppendRejected {
    pub code: String, // 固定 "stale_writer"
    pub expected_token: u128,
    pub presented_token: u128,
    pub epoch: u64,
}

pub struct EventsCommit {
    pub schema: String, // "qaqh.events-commit/v1"
    pub log_id: LogId,
    pub committed_fact_seq: u64,
    pub committed_offset: u64,
    pub last_barrier_event_id: Option<EventId>,
    pub commit_generation: u64,
}

pub enum UpgradeState { ReadOnly, Writable }


pub struct UpgradeFence {
    pub schema: String, // "qaqh.upgrade-fence/v1"
    pub log_id: LogId,
    pub generation: u64,
    pub state: UpgradeState,
    pub last_recovery_id: Option<RecoveryId>,
    pub updated_at_ms: i64,
}

pub struct EventsPoison {
    pub schema: String, // "qaqh.events-poison/v1"
    pub log_id: LogId,
    pub writer_id: WriterId,
    pub committed_fact_seq: u64,
    pub committed_offset: u64,
    pub failed_offset: u64,
    pub failed_bytes_hash: ContentHash,
    pub error: String,
    pub poisoned_at_ms: i64,
}
```

`WriterFence.fencing_token` 以及 `AppendRejected.expected_token/presented_token` 在 CLI 与 JSON 中统一表示为无符号十进制字符串，格式为 `0|[1-9][0-9]*`，不得使用 JSON number。服务端解析为 `u128`，溢出必须拒绝；递增与相等比较必须基于解析后的数值，不得按字符串字典序比较。`generation_epoch` 仍按 JSON number 表示。

`EventsCommit.commit_generation` 从 `0` 开始，每次成功重写 `events.commit.json` 必须严格 `+1`；`commit_repaired.previous_commit_generation` 固定等于 repair 前的 `EventsCommit.commit_generation`，repair 后新 marker 的 generation 必须为 `previous_commit_generation + 1`。若 marker 缺失/损坏且无法恢复出 previous generation，则不得生成 `commit_repaired`，也不得伪造 `SessionRecovered { outcome=commit_recovery_required }`，只能保持 `CommitRecoveryRequired` runtime state。若 `marker_missing_rebuildable=true`，该路径不生成 `commit_repaired`，只生成 `projection_rebuilt` action 并写 final `SessionRecovered { outcome=writable }`；若 high-water 已知且 repair 可 durable 写入 recovery fact，才允许生成 `commit_repaired` 并记录 `commit_recovery_required` 或 `writable`。

`UpgradeState::Writable` 序列化为 `upgrade-fence.json.state=writable`，与 `RecoveryOutcome::Writable`（`SessionRecovered.outcome=writable`）是两个独立命名空间；规则 10/11 与 upgrade batch 表中的 `state=writable` 一律指前者。

规则：

1. `events.lock` 是 session 内唯一 writer ownership 的原子裁决点；`writer-fence.json` 是持久化 ownership 元数据，用于 stale writer 诊断、epoch 切换与恢复接管，不能替代锁。fence 的读取、获取/续租 CAS、append、barrier 和 `events.commit.json` 更新必须全部在同一 `events.lock` 持锁临界区内完成。
2. 获取 writer 时必须先持有 `events.lock`，再读取当前 fence，生成 `fencing_token > current.fencing_token` 且 `generation_epoch >= current.generation_epoch`，并在锁内 CAS 写入；token 相等视为拒绝。任何 writer 都不得在未持锁时获得或续租 ownership。
3. 正常续租只延长 `lease_expires_at_ms`，不得改变 `writer_id`、`generation_epoch`、`fencing_token`。
4. epoch 切换（migration/cutover/rollback/恢复接管）必须递增 `generation_epoch` 并生成更大的 `fencing_token`；旧 epoch 的 fence 只用于审计。
5. 每次 append 前必须在 `events.lock` 内比较调用方持有的 `(writer_id, generation_epoch, fencing_token)` 与当前 fence。任一不匹配、lease 已过期或 `log_id` 不匹配，返回 `AppendRejected { code="stale_writer", expected_token, presented_token, epoch }`，不得写 fact；锁内比较是最后一个原子裁决点。
6. 旧 writer 收到 `stale_writer` 后必须停止所有 handler、取消未发布 projection、写 audit，并向上层返回 `ResetRequired { reason=StaleWriter }`；禁止用本地 retry 覆盖 fence。`AppendRejected.code="stale_writer"` 在 migration CLI 层映射为退出码 `E_STALE_WRITER`，两处是同一拒绝事实的不同表示。
7. `events.jsonl` 的每次成功 barrier 都要记录内存 `committed_fact_seq/committed_offset`。barrier 成功后、发布 projection 或返回 durable ack 前，必须用 temp + fsync + rename + 父目录 fsync 更新 `events.commit.json`；commit marker 才是跨重启 committed high-water，`events.poison.json` 只是 EIO 证据。commit marker 更新失败时不得发布/ack，session 立即进入 `CommitRecoveryRequired`，只允许 recovery/repair 路径继续。
8. `fsync` 返回 EIO 时，必须保持 `events.commit.json` 指向失败前的 committed offset，再尝试写 `events.poison.json` 并 `ftruncate` 回 committed offset + fsync；poison marker 写入或截断失败时 session 保持不可写并归入 `CommitRecoveryRequired`。启动时若 `events.commit.json` 缺失、损坏或 `log_id` 不匹配，执行序固定为：`(1) 扫描 events.jsonl 得到最后完整行的结束 offset candidate_offset`；`(2) 计算 marker_missing_rebuildable := (a) events.poison.json 不存在；(b) events.jsonl 无尾部半行，且 candidate_offset 之前没有 EIO 记录；(c) 最后完整行的 log_id 与目录身份/writer-fence.log_id 一致`；`(3) 若 marker_missing_rebuildable=false，进入 CommitRecoveryRequired 并停止`；`(4) 若为 true，截断到 candidate_offset、写 recovery evidence/audit 并重建 marker`；`(5) 再验证重建后的 committed prefix`。禁止在扫描 candidate_offset 之前按未知 committed_offset 做截断。若 `events.jsonl` 长度大于已确认的 committed offset，先截断到 committed offset，禁止把失败 segment 的完整前缀当作 canonical。
9. 不允许 `events.lock` 在持锁期间被 unlink/replace；fence CAS 与 append 必须共同校验 `log_id`，锁 inode 变化视为 stale writer。
10. `upgrade-fence.json` 是版本无关的单调 sidecar；任何 writer 在写 unknown kind/version 的 read-only marker 前必须先校验它。若已存在更高 generation 的 `state=writable`，旧 writer 不得追加 read-only marker，只能返回 `ResetRequired { reason=UpgradeRequired }`。
11. 升级 writer 只有在验证全部 payload version 后，才可按 `upgrade-fence generation + 1` 写 `state=writable`，再写 `SessionRecovered { outcome=writable, actions=[upgrade_superseded] }`；旧 writer 永远不得降低 generation 或把 writable 改回 read-only。`upgrade_superseded.previous_recovery_id` 必须等于写 `generation+1` marker 前 `UpgradeFence.last_recovery_id`，且该相等关系必须与 marker 更新在同一批校验；否则 fail-closed，不得生成 action。

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
| `ToolFinished` | 是 | 唯一 call 终态与副作用对账 |
| `InteractionRequested` | 是 | pending 可恢复 |
| `InteractionResolved` | 是 | first-answer-wins |
| `InteractionExpired` | 是 | 终态闭合，禁止恢复后重复 modal |
| `TurnFinished` | 是 | 终态闭合 |
| `TurnInterrupted` | 是 | 异常终态闭合 |
| `SessionRecovered` | 是 | 恢复幂等 |
| `CompactionApplied` | 是 | context revision 边界 |
| `SessionMetadataChanged` | 是 | 用户可见 mutation |
| `SessionTitleChanged` | 是 | 用户可见 mutation |
| `SessionDeleted` | 是 | tombstone 必须先于物理清理 |
| `WorkspaceResourceChanged` | 是 | resource revision 与副作用结果 |
| `SubagentSpawned` | 是 | parent/child edge 可恢复 |
| `SubagentFinished` | 是 | child 终态闭合 |

表中“独立 fsync”指该 canonical fact 的 `events.jsonl` append 必须成为 durable barrier；允许实现使用 group commit 降低系统调用次数，但不得跨越这些 barrier 合并。`ToolFinished` 无论是否有副作用都必须独立 fsync，避免 `denied`/`backgrounded` 等无 handler 终态在崩溃后丢失。

### 5.3.1 IO failpoint 清单

以下 failpoint 名称是测试注入接口的稳定 ID；每个 failpoint 必须支持“执行前崩溃”“系统调用返回 EIO”“fsync 返回 EIO”三种模式，除非备注另有说明：

| failpoint | 注入位置 | 恢复断言 |
|---|---|---|
| `content.write.before_fsync` | content 文件写入后、fsync 前 | 无 canonical 引用；可重写或清理 |
| `content.fsync` | content fsync | 不得 append 引用该 content 的 fact |
| `content.rename` | content 临时文件 rename | 旧文件/新文件至多一个可见，索引不得指向半文件 |
| `recovery.intent.write.before_fsync` | intent 写入后、fsync 前 | 不得修改 events.jsonl |
| `recovery.intent.rename` | intent temp rename | 重启必须能区分旧 intent/新 intent |
| `recovery.intent.parent_fsync` | intent 父目录 fsync | 失败时不得修改 canonical log |
| `events.append.before_fsync` | canonical 行 append 后、fsync 前 | 重启截断 torn tail 或重放已完整行 |
| `events.fsync` | canonical fsync | 不得发布 projection；重启从 last complete fact 继续 |
| `events.append.after_fsync_before_projection` | fsync 后、projection 前 | projection rebuild 必须补齐 |
| `projection.write.before_fsync` | derived 写入后、fsync 前 | derived 可删除并从 facts 重建 |
| `projection.fsync` | derived fsync | canonical 不得回滚 |
| `wire.publish.after_projection` | projection 后、wire emit 前 | 客户端重连从 cursor replay，不丢 reliable event |
| `snapshot.write.before_fsync` | snapshot 写入后、fsync 前 | manifest 不得引用未 durable snapshot |
| `snapshot.manifest.before_rename` | manifest rename 前 | 旧 manifest 仍有效，不出现半 snapshot |
| `content.gc.mark.before_journal_fsync` | mark 决策写入后、fsync 前 | 不得删除 content；重跑 mark 幂等 |
| `content.gc.offload.before_verify` | offload 复制后、hash 校验前 | 不得 unlink 本地文件 |
| `content.gc.delete.before_unlink` | 删除前 | 重新读取必须得到 `ContentUnavailable`，不得伪造内容 |
| `content.gc.unlink.after` | unlink 后、journal Delete 前 | 重启补写 Delete，不重复删除 |

所有 failpoint 测试必须断言 canonical `fact_seq` 连续、唯一终态不变、projection 可重建，并记录 `ResetRequired` 或恢复批次结果。

### 5.4 Torn tail

1. 读到无 `\n` 结尾的最后一行：判定 torn tail。
2. 将原始字节移动到 `events.jsonl.torn.<fact_seq>.<bytes_hash>`，保留证据；兼容读取可接受旧的 `events.jsonl.torn.<fact_seq>`，但同路径冲突时禁止覆盖，必须使用 hash 后缀。
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
    pub byte_len: u64,
    pub created_at_logical_ms: i64,
    pub last_referenced_fact_seq: u64,
    pub retention_classes: Vec<ContentRetentionClass>,
    pub delete_after_ms: Option<i64>,
    pub legal_hold_id: Option<String>,
    pub offload_state: OffloadState,
    pub ref_count: u64,
}

pub enum ContentRetentionClass {
    SessionReplay,
    CompactionCheckpoint,
    InteractionPending,
    AuditEvidence,
    UserAttachment,
}

pub enum OffloadState { LocalOnly, OffloadPending, OffloadVerified, Offloaded }
pub enum ContentGcKind { Mark, LegalHoldCheck, Offload, VerifyOffload, Sweep, Delete, Retry, Recovered }

pub struct ContentGcRecord {
    pub schema: String, // "qaqh.content-gc/v1"
    pub kind: ContentGcKind,
    pub content_ref: ContentRef,
    pub ref_count: u64,
    pub retention_classes: Vec<ContentRetentionClass>,
    pub delete_after_ms: Option<i64>,
    pub logical_ms: i64,
    pub legal_hold_id: Option<String>,
    pub offload_path: Option<String>,
    pub attempt: u32,
    pub error: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentClockRecord {
    pub schema: String, // 固定 "qaqh.content-clock/v1"
    pub logical_now_ms: i64,
    pub source_fact_seq: u64,
    pub source_event_id: EventId,
    pub updated_at_ms: i64, // 仅诊断；不得参与过期判定
}
```

`ContentClockRecord` 是 replay window 与 Content GC 共用的唯一持久化逻辑 clock，文件位置固定为 `{data_dir}/sessions/{session_id}/content/clock.json`，文件内容是一个 JSON object，不允许 JSONL：

```json
{"schema":"qaqh.content-clock/v1","logical_now_ms":1789830000040,"source_fact_seq":5,"source_event_id":"01J00000000000000000000005","updated_at_ms":1789830000040}
```

更新顺序固定为：

1. canonical fact append 并 fsync 成功。
2. 计算 `next_logical_now_ms = max(previous_clock.logical_now_ms, fact.ts_ms)`；若值不变则跳过 clock 写入。
3. 需要推进时，以 temp + fsync + rename + `content/` 父目录 fsync 原子替换 `content/clock.json`，记录触发推进的 `fact_seq/event_id`。
4. clock durable 后，才允许用同一 `next_logical_now_ms` 更新 `ReplayWindowManifest.logical_now_ms`，并执行依赖逻辑时间的 GC/retention 决策。
5. 若 clock 更新成功而 manifest 更新失败，manifest 可暂时落后；恢复算法必须用 clock 与 canonical log 的较大值修复，禁止反向把 clock 减小到 manifest 值。

`logical_now_ms` 永不回退；墙钟回拨、文件 mtime、进程启动时间均不得改变它。`updated_at_ms` 只用于诊断，不得替代 `logical_now_ms`。

重启恢复算法：

```text
complete_facts = 从 events.jsonl 解析出的完整 fact 前缀
max_fact_ts = max(fact.ts_ms for fact in complete_facts)（空 log 时为 0）
clock_value = 若 content/clock.json 存在且 schema 合法，则为 clock.logical_now_ms；否则为 0
manifest_value = 若 replay-window.json 存在且 log_id 匹配，则为 manifest.logical_now_ms；否则为 0
logical_now_ms = max(clock_value, manifest_value, max_fact_ts)

若 clock.json 缺失、损坏或 clock.logical_now_ms < logical_now_ms：
  以 logical_now_ms 写新的 ContentClockRecord；
  source 取 complete_facts 中 ts_ms 最大且 fact_seq 最大的 fact；
  没有 fact 时 source_fact_seq=0、source_event_id="00000000000000000000000000"，并立即写 clock

time_floor = 满足 fact.ts_ms >= logical_now_ms - replay_window_retention_ms
             的最小 fact_seq；全部过期则取 latest_fact_seq
```

恢复完成后，`ReplayWindowManifest.logical_now_ms` 必须被重写为同一 `logical_now_ms`，再重算 `time_floor/earliest_available_fact_seq/retained_*`。若 manifest 与 clock 都缺失，允许先从 canonical log 重建 clock，再重建 manifest；任何缺失都不得导致使用墙钟提前过期或回放已被 GC 的窗口。

默认 retention：

| class | `delete_after_ms`（全部相对逻辑 clock） |
|---|---|
| `SessionReplay` | fact `ts_ms + 30 days` |
| `CompactionCheckpoint` | checkpoint applied `ts_ms + 90 days` |
| `InteractionPending` | terminal 前不删除；terminal 后按 SessionReplay |
| `AuditEvidence` | audit `ts_ms + 180 days` 或 legal hold |
| `UserAttachment` | `None`，不自动删除 |

`ContentRecord` 持久化在 `{session}/content/index.jsonl`，append-only，每条 record fsync 后才能 append 引用它的 fact。`ref_count` 是 derived 计数，不能作为唯一 liveness 依据；mark 必须从 facts/checkpoint/interaction/audit 谓词重新计算。多 retention class 取最晚删除时间；任一 class 为 `UserAttachment` 或存在 `legal_hold_id` 时 `delete_after_ms = None`。

Content lifecycle：

```text
Pending -> Referenced -> Eligible -> Deleting -> Deleted
                                  |-> LegalHoldCheck -> Referenced
                                  |-> Offload -> VerifyOffload -> Deleting
                                  └-> Retry -> Deleting
Referenced <- Recovered
```

- `Pending`：文件已写但尚无 fact 引用；超过 grace 后进入 Eligible。
- `Referenced`：至少一个 active ref。
- `Eligible`：无 active ref 且 `delete_after_ms` 已到；至少保留 24h grace。
- `LegalHoldCheck`：持有 `content.lock` 时先检查 legal hold；命中时回到 Referenced，禁止 offload 后删除。
- `Offload`：只有未命中 legal hold 且策略要求 offload 时执行；复制完成必须记录 `offload_path`。
- `VerifyOffload`：验证远端 hash 等于 `content_ref` 后才能进入 Deleting；验证失败进入 Retry，本地文件不得删除。
- `Deleting`：持有 `content.lock`，与 append/reference 建立互斥。
- `Retry`：删除失败，记录 `attempt/error`；最大 5 次，指数退避。
- `Recovered`：文件仍被合法引用或删除被取消，回到 Referenced。
- `Deleted`：本地文件已不存在；任何 fact 再引用它必须返回 `ContentUnavailable`，不得返回空内容或伪造摘要。

GC journal 幂等键为 `(content_ref, kind, attempt)`；重启后从 `content-gc.jsonl` 恢复未完成状态。超过重试上限必须停止该 ref 的 GC 并告警，不得静默删除或无限重试。

`diagnostics/content-gc.jsonl` 是 append-only 状态日志，记录 `Mark -> LegalHoldCheck -> Offload/VerifyOffload -> Sweep -> Delete/Retry`。重启后从日志尾部恢复 retry 集合；`Retry` 不改变 canonical facts。删除成功写 `Delete`，失败写 `Retry { attempt, error }`；超过重试上限时停止 GC 并告警，不得绕过。

`ContentUnavailable` 的 Rust 类型只在 §2.5 定义；它是 canonical content 读取结果，必须按该结构持久化和重建。其 canonical JSON 表达为：

```json
{"content_ref":"sha256:3333333333333333333333333333333333333333333333333333333333333333","reason":"garbage_collected","observed_at_logical_ms":1789830000000,"source_fact_seq":41,"gc_event_seq":7}
```

重建规则：projection snapshot 必须保存 `ContentValue::Unavailable`；删除 derived 后，rebuild 读取 canonical facts、`content/index.jsonl` 与 `content-gc.jsonl`，对同一 `content_ref` 必须得到同一 `reason` 和 `gc_event_seq`。若文件意外缺失而没有 GC 记录，必须写 `reason=missing` 并 fail-closed；不得把缺失内容当作空文本。

active ref 谓词：

- `SessionReplay`：被 canonical fact 引用，且 `fact.ts_ms + session_replay_retention >= logical_now_ms`。
- `CompactionCheckpoint`：被未过期 checkpoint 引用。
- `InteractionPending`：被 pending interaction 引用。
- `AuditEvidence`：被 audit retention 或 legal hold 引用。
- `UserAttachment`：由用户显式保留，默认不自动删除。

所有 GC 时间判断使用 §5.5 定义的 `ContentClockRecord.logical_now_ms`；该值同时供 replay window 使用。禁止使用墙钟 `now`、文件 mtime 或进程启动时间。逻辑 clock 只允许单调前进，墙钟回拨不得让内容提前过期。

GC 协议：

1. mark 阶段获取 `content.lock`，从上述谓词生成候选保留集；新 fact 引用 content 必须与 sweep 串行化。
2. legal hold 检查必须先于 offload；命中 legal hold 时跳过 offload/delete，保持本地内容并写 `LegalHoldCheck`。
3. 需要 offload 时先复制到不可变远端并 fsync/校验 hash；校验成功后才能把 `OffloadState` 推进到 `OffloadVerified`。
4. sweep 阶段仅删除 `delete_after_ms <= logical_now_ms`、未被 mark 且 offload 已校验（若策略要求 offload）的 content。
5. 删除前必须写 `content-gc.jsonl` 候选、legal hold、offload 与决策记录，并 fsync。
6. 删除失败进入同一 journal 的 retry 状态；不得静默忽略。
7. 旧 fact 引用的 content 被 GC 后，读取必须返回 `ContentUnavailable`；replay/model context 遇到它时必须显式降级并记录 diagnostics。
8. `retention window` 从 `delete_after_ms` 和逻辑 clock 判断，不依赖文件 mtime；四组 fixture 见 §10.1。

### 5.6 背压与容量

| 队列 | 初始上限 | 溢出策略 |
|---|---:|---|
| actor mailbox | 1024 | 拒绝 command / `Busy` |
| per-connection queue | 256 | disconnect + `ResetRequired { reason=PerConnectionOverflow }` |
| replay buffer | 16 MiB | `ResetRequired { reason=ReplayOverflow }` |
| progress buffer | 8 KiB / 50 ms | 丢弃 ephemeral，不丢 reliable |
| 单 fact inline | 64 KiB | 强制 content ref |
| session content quota | `2 GiB` | 拒绝新高成本工具/GC，并写 `ResetReason::ContentQuotaExceeded` |

`session content quota` 的 v2.0 冻结默认值为 `2 * 1024^3` bytes；soft watermark 为 80%，hard watermark 为 95%。soft watermark 只拒绝新的高成本工具和 offload 扩张；hard watermark 拒绝新的 content 写入和 GC offload，但不得删除已有 canonical 内容。actor mailbox、per-connection、replay buffer 与 progress buffer 的溢出分别使用 §2.5 中对应的 `ResetReason`；所有 overflow 必须携带 reason 和终态，禁止静默丢 reliable fact。

`ResetReason` 的唯一 enum 定义位于 §2.5，包含 `content_quota_exceeded`、`per_connection_overflow`、`replay_overflow`、`progress_buffer_overflow`、`actor_mailbox_overflow`、`snapshot_missing`、`snapshot_expired`、`snapshot_hash_mismatch` 与 `stale_writer`。禁止在其它章节再定义同名 enum。

---

## 6. 崩溃恢复状态机

### 6.1 启动流程

```text
load events.jsonl
  -> inspect events.commit.json / events.poison.json
  -> if marker missing: compute candidate_offset and marker_missing_rebuildable
       -> true: rebuild marker as side effect, continue with outcome=writable
       -> false: mark CommitRecoveryRequired and stop before append/publish/ack
  -> truncate anything beyond committed_offset
  -> validate envelope + schema on committed prefix
  -> find last complete line
  -> detect torn tail
  -> rebuild projections
  -> detect open Turn/ToolIntent/Interaction/Input/Subagent edge
  -> compute recovery batch key and plan
  -> append missing recovery facts atomically
  -> mark session writable, CommitRecoveryRequired, read-only upgrade-required, or tombstone
```

恢复必须满足：

- 不重写旧 fact。
- 不生成第二个终态。
- 同一输入重复执行得到同一终态。
- unknown fact 不得被跳过。
- recovery 幂等单位是 **recovery batch**，不是 session load；同一 batch 的 fact 集合必须原子补齐且只写一次。
- `actions` 是对同一 recovery batch 已写 facts 的摘要，不得被消费者当成第二组事实执行。

`RecoveryPlan` 的 canonical Rust 形状如下；`RecoveryIntent` 不是 step，`plan_hash` 也不是 step：

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryPlan {
    pub schema: String, // "qaqh.recovery-plan/v1"
    pub recovery_ref: RecoveryRef,
    pub log_id: LogId,
    pub last_good_fact_seq: u64,
    pub sorted_open_ids: Vec<String>,
    pub torn_tail_bytes_hash: ContentHash,
    pub child_terminal_digest: ContentHash,
    pub steps: Vec<RecoveryStep>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RecoveryToolCompletion {
    pub call_id: ToolCallId,
    pub execution_id: Option<ExecutionId>,
    pub terminal_status: ToolTerminalStatus,
    pub output_ref: Option<ContentRef>,
    pub error: Option<ToolError>,
    pub metrics: ToolMetrics,
    pub reconciled: bool,
    pub recovery_ref: RecoveryRef,
    pub finished_at_ms: i64,
    pub evidence_ref: Option<ContentRef>,
    pub evidence_fact_seq: Option<u64>,
    pub evidence_event_id: Option<EventId>,
}

pub struct RecoverySubagentCompletion {
    pub child_session_id: SessionId,
    pub child_log_id: LogId,
    pub terminal_fact_seq: u64,
    pub terminal_event_id: EventId,
    pub parent_call_id: ToolCallId,
    pub status: SubagentTerminalStatus,
    pub result_ref: Option<ContentRef>,
    pub finished_at_ms: i64,
    pub recovery_ref: RecoveryRef,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum RecoveryStep {
    MoveTornTail { from: String, to: String, bytes: u64, bytes_hash: ContentHash },
    TurnStarted { turn_id: TurnId, input_id: InputId, mode: TurnMode, recovery_ref: RecoveryRef },
    TurnInterrupted { turn_id: TurnId, last_fact_seq: u64 },
    ToolFinished { completion: RecoveryToolCompletion },
    InteractionExpired { interaction_id: InteractionId, reason: InteractionExpiryReason },
    SubagentFinished { completion: RecoverySubagentCompletion },
    SessionRecovered { outcome: RecoveryOutcome, torn_tail: bool, torn_bytes: Option<u64> },
}
```

canonical step 顺序和排序键固定为：

```text
1. MoveTornTail（若存在；最多一个）
2. TurnStarted（按 input_id 字典序）
3. TurnInterrupted（按 turn_id 字典序）
4. ToolFinished（按 call_id 字典序）
5. InteractionExpired（按 interaction_id 字典序）
6. SubagentFinished（按 child_session_id 字典序）
7. SessionRecovered（恰好一个，必须最后）
```

`RecoveryStep::ToolFinished` 的 typed `completion` 必须足以逐字段构造 canonical `ToolFinished`：`call_id`、`execution_id`、`terminal_status`、`output_ref`、`error`、`metrics`、`reconciled`、`recovery_ref`、`finished_at_ms`、`evidence_ref`、`evidence_fact_seq` 和 `evidence_event_id` 均在 plan 中显式存在；nullable 字段允许为 `None`，但不得依赖恢复时重新读取墙钟或临时内存来补值。

`RecoveryStep::TurnStarted` 只用于闭合“已 durable `InputAccepted`、`input_purpose=trigger_turn`、但尚无 `TurnStarted`”的输入；`input_purpose=queue_only` 永不补 turn。`turn_id` 必须是 `turn_<ULID>`，由 `sha256(log_id || ":" || input_id)` 的前 128 bit 按 canonical ULID 编码稳定派生，不能每次恢复重新随机生成；恢复模式固定为 `normal`。`RecoveryStep::SubagentFinished` 只用于 child 已 terminal、parent edge 尚未闭合的情况，字段必须来自 child canonical terminal 与 parent `SubagentSpawned`，不得用墙钟或内存猜测。

`RecoveryToolCompletion` 的终态集合固定为 `indeterminate/succeeded/failed/partial/cancelled/timed_out/denied`。其中 `succeeded/failed/partial/cancelled/timed_out` 必须由 probe/canonical evidence 确定；`denied` 只用于无 intent 的 approval/policy 路径且 `error.code` 只能是 `approval_rejected` 或 `policy_denied`；`cancelled` 的无 intent 路径只允许 `error.code=approval_expired` 或 `recovery_before_policy_decision`。有 `ToolIntent` 时 `execution_id` 必须等于该 intent 的 execution id；只有无 intent 的 `denied/cancelled` 终态才允许为 `None`。`recovery_ref` 必须等于当前 batch；不得用 recovery 伪造 `backgrounded` 或新的 attempt。`MoveTornTail` 的 `from/to` 必须使用 session 目录下的相对 canonical 路径，不得包含临时文件名、绝对路径或墙钟。

`plan_hash` 的规范输入只包含上述 `RecoveryPlan`，不得包含 `plan_hash`、`RecoveryIntent`、`DeleteRecoveryIntent`、墙钟或随机值：

```text
plan_hash = sha256(canonical_json({
  schema: "qaqh.recovery-plan/v1",
  recovery_ref: {
    recovery_id: string,
    recovery_event_id: string,
    recovery_input_fingerprint: "sha256:<64hex>"
  },
  log_id: string,
  last_good_fact_seq: u64,
  sorted_open_ids: [string, ...], // 字典序升序，去重
  torn_tail_bytes_hash: "sha256:<64hex>",
  child_terminal_digest: "sha256:<64hex>",
  steps: [
    {kind: string, data: {...}}, ...
  ]
}))
```

`recovery_input_fingerprint = sha256(canonical_json({log_id, last_good_fact_seq, sorted_open_ids, torn_tail_bytes_hash, child_terminal_digest}))`，同样不包含 `plan_hash` 或 `RecoveryIntent`。因此不存在 `RecoveryIntent(plan_hash)` 自引用。

`child_terminal_digest` 的规范输入是 parent 当前开放 `SubagentSpawned` edge 对应 child log 的稳定终态证据数组，按 `child_session_id` 排序：`{child_session_id, child_log_id, terminal_fact_seq, terminal_event_id, status, parent_call_id, result_ref, finished_at_ms}`；child 尚未 terminal 时使用空数组的 sha256。该字段集必须等于 `RecoverySubagentCompletion` 去掉 `recovery_ref` 后的字段集；`RecoveryAction::subagent_finished` 与 `RecoveryStep::SubagentFinished.completion` 则必须包含完整的 9 字段（含 `recovery_ref`）并逐字段同构。新增 completion 字段时必须同步 digest 与 action。child terminal 证据变化必须产生新的 batch key，禁止复用已闭合的 parent recovery batch。

| 结构 | 字段集合 | 数量 |
|---|---|---:|
| `child_terminal_digest` 输入 | `child_session_id`, `child_log_id`, `terminal_fact_seq`, `terminal_event_id`, `status`, `parent_call_id`, `result_ref`, `finished_at_ms` | 8 |
| `RecoverySubagentCompletion` / `RecoveryStep::SubagentFinished` / `RecoveryAction::subagent_finished` | 上述 8 字段 + `recovery_ref` | 9 |

恢复批次协议：

1. 从 pre-recovery canonical log 计算 `recovery_input_fingerprint` 和 batch key；batch key 为 `(log_id, recovery_input_fingerprint, last_good_fact_seq)`。
2. 若 batch key 对应的 `SessionRecovered` 已存在，则该 batch 已 closed；本次 load 只验证并复用，不再写任何 recovery fact。
3. 若 batch 未 closed，预分配 `RecoveryRef`，计算 `RecoveryPlan` 和 `plan_hash`，再写 `recovery.intent.json`。
4. 按 canonical step 顺序逐个 append + fsync；`SessionRecovered` 必须最后写。每个 recovery fact 都携带同一 `RecoveryRef`；`TurnStarted` 与 `SubagentFinished` 的 `recovery_ref` 字段也必须逐字写入。
5. 任一步骤 crash：重启后以 batch key 找到既有 recovery facts，按 step 幂等键跳过已写项，只补齐缺失项；不得重新生成 identity 或重排 step。
6. `SessionRecovered` 不存在时 session 保持 `recovery_in_progress/read_only`，不得接受新 command；存在后 batch closed。
7. 只有 `SessionRecovered` 之后产生新的 open Turn/Tool/Interaction、pre-recovery canonical log 改变，或 `child_terminal_digest` 改变，才允许创建新的 batch；旧 batch 不得复用。

在修改 `events.jsonl`、移动 torn tail 或写首条 recovery fact 前，必须原子写入 `recovery.intent.json`：

```rust
pub struct RecoveryIntent {
    pub schema: String, // "qaqh.recovery-intent/v1"
    pub recovery_ref: RecoveryRef,
    pub log_id: LogId,
    pub last_good_fact_seq: u64,
    pub sorted_open_ids: Vec<String>,
    pub torn_tail_bytes_hash: ContentHash,
    pub child_terminal_digest: ContentHash,
    pub plan_hash: ContentHash,
}
```

`upgrade_superseded` 与 `commit_repaired` 是 sidecar-only action，不进入 `RecoveryIntent`/`RecoveryPlan`；前者的幂等键是 `(log_id, upgrade-fence.generation+1)`，后者是 `(log_id, previous_commit_generation)`，分别由 `upgrade-fence.json` 与 `events.commit.json` 自身携带并校验。

`recovery_input_fingerprint` 的规范输入：

```text
canonical_json({
  log_id: string,
  last_good_fact_seq: u64,
  sorted_open_ids: [string, ...], // 字典序升序，重复项去除
  torn_tail_bytes_hash: "sha256:<64hex>", // 无 torn tail 时使用 sha256(empty)
  child_terminal_digest: "sha256:<64hex>" // 无开放 child edge 时使用 sha256(empty)
})
```

- `recovery.intent.json` 使用 temp + fsync + rename + 父目录 fsync 写入；父目录 fsync 失败则不得修改 `events.jsonl`。
- 重启时先判定 intent 是否为 active：若同 `recovery_event_id` 的 `SessionRecovered` 已存在，则该 intent 已 closed/stale。
- 若同一 batch key 的既有 recovery facts 计算出不同 `plan_hash`，必须 fail-closed 进入 read-only/upgrade-required，禁止覆盖旧 batch。
- 若同一 batch key 已存在但 `recovery_event_id` 不同，必须 fail-closed；不得把两个 identity 合并成同一 batch。
- closed intent 不得复用到新的 open state；若 `SessionRecovered` 之后又有新 open Turn/Tool/Interaction，或 `child_terminal_digest` 改变，必须基于当前 pre-recovery log 生成新的 `RecoveryRef` 与 fingerprint，并原子覆盖/归档旧 intent。
- active intent 才允许复用其中的 `RecoveryRef` 与 fingerprint。
- `SessionRecovered` fsync 成功后才允许删除 intent；删除失败只留下 stale intent，不影响后续正确性。
- 若 intent 不存在，必须先从 pre-recovery canonical log 计算并落盘，再执行恢复。

### 6.2 恢复矩阵

特殊 batch 集合必须区分，且不得互相追加；普通 recovery batch、unknown/upgrade、commit/poison repair 与 tombstone 各自有独立身份和幂等键：

| batch 类型 | canonical fact / side effect 集合 | 幂等键 | 结论 |
|---|---|---|---|
| no-action | `SessionRecovered { outcome=writable, actions=[], torn_tail=false }` | 标准 batch key | 无 open 状态且无 torn/unknown/tombstone 时，创建一次闭合 batch |
| unknown fact | `SessionRecovered { outcome=read_only_upgrade_required, actions=[], last_good_fact_seq=<unknown 前> }` | 标准 batch key | 不解释 unknown 及其后 fact；后续 load 复用同一 batch |
| upgrade supersede | 先 durable 写 `upgrade-fence {generation+1,state=writable}`，再写 `SessionRecovered { outcome=writable, actions=[upgrade_superseded] }` | `(log_id, upgrade-fence.generation+1)` | sidecar-only batch；不进入 `RecoveryPlan.steps` / `plan_hash`；仅允许在已有 read-only marker 且 pre-recovery log 已变化时创建；旧 writer 不得写此 batch |
| commit/poison repair | 先按 `events.commit.json` 验证/截断或重建 marker：若 `marker_missing_rebuildable=true`，actions=`[projection_rebuilt]`、outcome=`writable`；若 previous generation/high-water 已知，actions=`[commit_repaired]`、outcome=`writable` 或 `commit_recovery_required`；无法证明 high-water 时不写 business fact，只保持 runtime `CommitRecoveryRequired` | `(log_id, previous_commit_generation)`（仅 commit_repaired）；rebuild 路径用标准 batch key | repair durable 成功且 high-water/clock 一致后才回 writable；`commit_repaired` 与 `tool_finished` 是互斥 action，commit repair 永不生成 `tool_finished` |
| tombstone | `SessionRecovered { outcome=tombstone, actions=[] }` | 标准 batch key | 不写其它 recovery fact，不执行物理清理 |

其它恢复矩阵：

| 输入状态 | canonical fact 集合 | 可写性 |
|---|---|---|
| `InputAccepted` 无 `TurnStarted`，`input_purpose=trigger_turn` | `TurnStarted`（按 `(log_id,input_id)` 派生唯一 turn） + final `SessionRecovered` | 恢复后可写 |
| `InputAccepted` 无 `TurnStarted`，`input_purpose=queue_only` | 仅 final `SessionRecovered`；不得补 `TurnStarted` | 恢复后可写 |
| `TurnStarted` 无终态 | `TurnInterrupted` + final `SessionRecovered` | 恢复后可写 |
| `InteractionResolved(rejected)` 无 `ToolIntent`/`ToolFinished` | `ToolFinished { terminal_status=denied, execution_id=None, metrics=<零执行>, error.code=approval_rejected }` + final `SessionRecovered { actions=[tool_finished] }` | 恢复后可写 |
| `ToolCallDeclared` 无 `ToolIntent`/`ToolFinished` 且无 interaction 终态（policy 决策前崩溃） | `ToolFinished { terminal_status=cancelled, execution_id=None, metrics=<零执行>, error.code=recovery_before_policy_decision }` + final `SessionRecovered` | 恢复后可写 |
| `ToolIntent` 无 `ToolFinished`，`NoReplay` | `ToolFinished { terminal_status=indeterminate, metrics=<零执行或已知执行 metrics> }` + final `SessionRecovered` | 恢复后可写 |
| `ToolIntent` 无 `ToolFinished`，`IdempotentReplay` | 一次重放后的成功/失败 `ToolFinished` + final `SessionRecovered`；无法取得结果时写 `indeterminate` | 重放后写 |
| `ToolIntent` 无 `ToolFinished`，`Reconcile` | probe/canonical evidence 确定的成功/失败/partial `ToolFinished { reconciled=true }`，或 `indeterminate` + final `SessionRecovered` | 对账后写 |
| `InteractionRequested` 无终态且未过期 | final `SessionRecovered { actions=[] }`；不重发 modal fact | 可写 |
| `InteractionRequested` 已过期/重启策略取消/turn cancel | `InteractionExpired` + final `SessionRecovered` | 恢复后写 |
| child terminal 且 parent `SubagentSpawned` 无 `SubagentFinished` | `SubagentFinished` + final `SessionRecovered` | 恢复后可写 |
| `events.poison.json` 或 `events.jsonl` 超出 committed offset | 先按 `events.commit.json` 验证/截断，再走 commit/poison repair batch | repair 成功回 writable；无法证明 high-water 时为 `commit_recovery_required` |
| `CompactionApplied` 无 checkpoint | 仅 final `SessionRecovered`；从 active facts 重建 context | 重建后写 |
| torn tail | `SessionRecovered { torn_tail=true, torn_bytes=n }` 作为唯一 final fact | 恢复后写 |

### 6.3 Tool recovery

`ToolIntent` 的恢复裁决顺序：

1. 查找同 `call_id` 的 `ToolFinished`；存在则不再执行。
2. 按 §2.3 的组合优先级读取 `replay_capability`、`side_effect_class`、`idempotency_key`。
3. `NoReplay` 由 builder 写 `indeterminate`；`Reconcile` 只接受 probe/canonical evidence 确定的终态，否则写 `indeterminate`；`IdempotentReplay` 才允许一次同 identity 重放，并以 typed result 构造成功/失败/partial 终态。
4. `reconciled=true` 必须带 `evidence_ref`，或带 canonical fact 的 `evidence_fact_seq + evidence_event_id`；`call_id` 本身不能宣称 exactly-once。
5. 四类 replay capability/副作用/对账组合必须由 §10.1 的显式 fixture 覆盖。

#### 6.3.1 Recovery fact builder

probe evidence 的 canonical shape：

```rust
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolProbeOutcome { Succeeded, Failed, Partial, Indeterminate }

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolProbeEvidence {
    pub schema: String, // "qaqh.tool-probe/v1"
    pub call_id: ToolCallId,
    pub execution_id: ExecutionId,
    pub outcome: ToolProbeOutcome,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub output_ref: Option<ContentRef>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ToolError>,
    pub output_bytes: u64,
    pub progress_bytes_total: u64,
    pub observed_at_ms: i64,
}
```

`ToolProbeEvidence` 的 canonical JSON 形状固定为：

```json
{"schema":"qaqh.tool-probe/v1","call_id":"call_01J00000000000000000000000","execution_id":"exec_01J00000000000000000000000","outcome":"succeeded","output_ref":"sha256:cccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccccc","output_bytes":12,"progress_bytes_total":12,"observed_at_ms":1789830000040}
```

builder 的输入是 pre-recovery canonical log、同 call 的 `ToolCallDeclared`/唯一 `ToolIntent`、当前 `RecoveryRef`，以及按 capability 得到的 probe evidence 或一次 idempotent replay 的 typed result。builder 不读取墙钟、文件 mtime 或进程内存；所有输出必须可从这些输入重算。

构造规则按以下顺序执行，顺序不可交换：

1. 若 canonical log 已有同 `call_id` 的 `ToolFinished`，返回该终态，不产生 recovery step。
2. 若 call 有 `ToolIntent`，校验其唯一性以及 `call_id/execution_id`；同 call 多个 intent 时 fail-closed，不得伪造 completion。
3. 若 call 没有 `ToolIntent` 但有 `ToolCallDeclared`，先检查 interaction：仍有 pending `InteractionRequested` 且未过期时不生成 `ToolFinished`，保留 call open，等待正常 resolution/expiry。若 `InteractionResolved(rejected)` 则构造 canonical `ToolFinished { terminal_status=denied, execution_id=None, metrics=<零执行>, error.code=approval_rejected }` 并产生对应 `tool_finished` action；若 `InteractionExpired` 则构造 `cancelled/approval_expired`；两者都不存在时构造 `cancelled/recovery_before_policy_decision`。无 intent 终态路径的 `execution_id=None`、`reconciled=false`、`output_ref=None`、`metrics` 为零执行 metrics，`finished_at_ms` 取对应 terminal fact 的 `ts_ms` 或 `ToolCallDeclared.ts_ms`。
4. 若 call 既没有 `ToolIntent` 也没有 `ToolCallDeclared`，fail-closed，不得伪造 completion。
5. `Reconcile` 读取 `probe_ref` 对应内容并解析为 `ToolProbeEvidence`；`schema`、`call_id`、`execution_id` 任一不匹配时视为无结论，而不是失败。
6. canonical evidence 只接受与 call 明确关联的终态事实：`SubagentFinished.parent_call_id == call_id` 映射为 `completed -> succeeded`、`failed -> failed`、`cancelled -> cancelled`、`timed_out -> timed_out`。`WorkspaceResourceChanged.source_call_id` 只表示资源 revision，单独出现不能证明普通 tool 成功或失败。
7. `NoReplay` 或无有效 evidence 的 `Reconcile` 生成 `terminal_status=indeterminate`、`output_ref=None`、`error={code:"indeterminate_after_crash",message:"tool outcome could not be determined",retryable:false}`、`reconciled=false`。
8. `IdempotentReplay` 只允许以同 `call_id/execution_id/idempotency_key` 重放一次；typed result 为成功时生成 `succeeded`，为稳定错误时生成 `failed`，为部分可消费结果时生成 `partial`，无法取得 typed result 时生成 `indeterminate`。重放路径的 `reconciled=false`，`evidence_ref=None`。
9. probe `succeeded/failed/partial` 与 canonical `SubagentFinished` 生成确定终态：`reconciled=true`；probe 终态设置 `evidence_ref=probe_ref`，canonical 终态设置 `evidence_fact_seq` 与 `evidence_event_id`，有 `result_ref` 时同时设置 `evidence_ref`。probe `partial` 只映射为 `partial`，并保留 probe 的 `output_ref/error/output_bytes/progress_bytes_total`。
10. `succeeded` 必须有 `output_ref` 或显式允许空输出的 typed result；`partial` 必须有 `output_ref`；`failed` 必须有 `ToolError`。缺少必需 output/error 时降级为 `indeterminate`，不得凭空补 message 或摘要。
11. `metrics.started_at_ms = ToolIntent.intent_at_ms`；无 intent 路径取 `ToolCallDeclared.ts_ms`。`finished_at_ms` 取 `max(intent_or_declared_at_ms, probe.observed_at_ms, canonical evidence fact.ts_ms, idempotent replay observed_at_ms)`；无任何观测时间时取 `intent_or_declared_at_ms`。`metrics.finished_at_ms = finished_at_ms`，`retry_count=0`；`output_bytes/progress_bytes_total` 取 evidence/typed result 的显式计数，canonical evidence 缺失计数时按 `output_ref` 的 ContentRecord byte length 计算，仍不可得则为 0。
12. `RecoveryToolCompletion.execution_id` 在有 intent 时必须等于 intent 的 execution id，在无 intent 的 `ToolCallDeclared` 路径必须为 `None`；`recovery_ref` 必须等于当前 batch 的 `RecoveryRef`；`ToolFinished` canonical fact 的 `output_ref/error/recovery_ref/finished_at_ms/metrics/reconciled/evidence_ref/evidence_fact_seq/evidence_event_id` 全部由 completion 逐字段复制，不允许恢复时再补算。action-only 回执（如已 resolved/rejected 的 interaction）也复用同一 batch `RecoveryRef`，不得把 `SessionRecovered` 自身的 envelope `event_id` 当作新的 recovery identity。

无 intent recovery 的 `ToolError.message` 分别固定为 `approval rejected`、`approval expired`、`recovery before policy decision`，`retryable=false`；不得写空 message。

### 6.4 Interaction recovery

- pending interaction 只重发一次 `InteractionRequested` projection。
- 已 resolved/expired 不重发 modal。
- 重复 resolution 返回既有 `InteractionResolved`。
- first-answer-wins 依据 `interaction_id`，不依据连接顺序。
- TUI/Web 必须维护 `resolved_interaction_ids`。

### 6.5 Recovery fact 幂等

每个 recovery batch 的幂等键和每个 fact 的幂等键如下：

| 对象 | 幂等键 | 重复处理 |
|---|---|---|
| batch | `(log_id, recovery_input_fingerprint, last_good_fact_seq)` | closed batch 直接复用，不追加 fact |
| `SessionRecovered` | `(log_id, recovery_id)` 且必须匹配 batch key | 已有则跳过 |
| `TurnStarted` | `(log_id, input_id, turn_id)` | 已有则跳过；同一 input 不得启动第二个 turn |
| `TurnInterrupted` | `(turn_id, recovery_id)` | 已有则跳过 |
| `ToolFinished` | `(call_id, recovery_id)` | 已有则跳过；不得写第二个终态 |
| `InteractionExpired` | `(interaction_id, recovery_id)` | 已有则跳过 |
| `SubagentFinished` | `(log_id, child_session_id, parent_call_id, recovery_id)` | 已有则校验字段一致并跳过；不得写第二个 edge 终态 |
| `move_torn_tail` | `(log_id, bytes_hash)` | 已移动则验证目标文件，不重复移动 |
| `upgrade_superseded` | `(log_id, upgrade-fence.generation+1)` | 已有更高 generation 的 writable fence 时复用，不重复升级 |
| `commit_repaired` | `(log_id, previous_commit_generation)` | 仅当 previous generation 可证明时使用；已修复同一代 marker 时验证 high-water/offset 后跳过 |
| `projection_rebuilt` | `(log_id, projection, through_fact_seq)` | marker 缺失但 `marker_missing_rebuildable=true` 时使用；不伪造 `previous_commit_generation` |

`recovery_event_id` 是 batch 级逻辑身份，必须在同一 batch 的所有 recovery facts 的 `recovery_ref` / `SessionRecovered.recovery_event_id` 中共享；它不要求等于任一 fact 的 envelope `event_id`，也不得把 `SessionRecovered` 自身的 envelope `event_id` 当作新的 batch identity。`recovery_event_id` 在 plan 阶段预分配；重复 load 只有在 batch key 或 pre-recovery log 改变时才生成新 `RecoveryRef`、新 `plan_hash` 和新 batch；不得按“每次 load”追加 `SessionRecovered`。

upgrade batch 的 closed 判据：若 `upgrade-fence.state=writable` 且 `generation == 当前 UpgradeFence.generation`（即写入 `generation+1` 后的值；含 `generation+1` 已 durable、`SessionRecovered` 尚未写入的崩溃窗口），则该 batch 已由 sidecar 消费；重启只能补写缺失的 `SessionRecovered { outcome=writable, actions=[upgrade_superseded] }`，不得再次递增 generation、不得重复写 `upgrade_superseded`。`previous_recovery_id` 必须等于该 marker 的 `last_recovery_id`。

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
- spawn 顺序固定为 child `SessionCreated` durable 后，parent 才写 `SubagentSpawned`；child 在 edge durable 前不得接收输入。
- `recovery_ref` 只属于 `SubagentFinished`；`SubagentSpawned` 永不来自 recovery batch，也没有该字段。
- edge 无 child log 是完整性损坏，parent 必须进入 `read_only_upgrade_required`，不得伪造 child 或 `SubagentFinished`。
- child log 无 edge 时，child 必须 tombstone/取消，禁止继续执行；child terminal 且 parent edge open 时，只能由 `RecoveryStep::SubagentFinished` 补齐。
- v2.0 只冻结 edge + mailbox；graph scorer/role template 延后。
- parent unload/delete/shutdown/panic 的完成条件固定为：每个 child 先进入 canonical terminal，再 durable 写 parent `SubagentFinished` edge，再由 `SubagentSupervisor` join child handle，最后才允许 parent unload ack、tombstone 或 shutdown 完成。顺序固定为 `child terminal -> parent SubagentFinished -> child join -> parent terminal`，不得只依据内存 child 数或 UI 状态。
- child session 的 fact log 与 parent log 不互相复制。
- child terminal 只从 child 的 committed high-water 读取，并按下表映射；`child_terminal_digest` 必须覆盖映射后的全部字段。
- `SubagentFinished`（含 recovery 补齐路径）只写入 parent log 的 control/resource edge projection，不产生 conversation/timeline projection，也不得改变 child 的 chat 视图；child 终态只通过 child log 自身的事实变化体现。
- `result_ref` 对 `completed` 取最后一个 `AssistantBlockSealed.content_ref`，其他状态为 `None`；`finished_at_ms` 取 `TurnFinished.finished_at_ms`，`TurnInterrupted` 取该 fact 的 envelope `ts_ms`。
- `timed_out` 只能由 `TurnFinished { terminal=failed, error.code=subagent_timed_out }` 映射；`TurnFinished { terminal=cancelled }` 或 `TurnInterrupted` 映射为 `cancelled`。

| child committed terminal | parent `SubagentTerminalStatus` |
|---|---|
| `TurnFinished { terminal=completed }` | `completed` |
| `TurnFinished { terminal=failed, error.code!=subagent_timed_out }` | `failed` |
| `TurnFinished { terminal=failed, error.code=subagent_timed_out }` | `timed_out` |
| `TurnFinished { terminal=cancelled }` / `TurnInterrupted` | `cancelled` |

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

双写期间每条旧记录与 canonical projection 必须建立对账记录。唯一键必须包含 `(legacy_identity, derived_ordinal, canonical_seq)`，其中 `canonical_seq` 同时包含 `fact_seq` 和 `projection_index`：

```rust
pub struct CanonicalSeq {
    pub fact_seq: u64,
    pub projection_index: Option<u16>,
}

pub struct LegacyMappingKey {
    pub legacy_identity: String, // source_generation_id + legacy key 的 canonical 编码
    pub derived_ordinal: u32,    // 同一 legacy identity 派生出的第 n 个 canonical target，从 0 开始
    pub canonical_seq: CanonicalSeq,
}

pub struct LegacyMappingTarget {
    pub key: LegacyMappingKey,
    pub delivery: V1DeliveryKind,
}

pub struct LegacyMapping {
    pub legacy_source: LegacySource,
    pub source_generation_id: String,
    pub legacy_key: String,       // 仅用于重建 legacy_identity，不单独作为唯一键
    pub legacy_msg_id: Option<String>,
    pub session_id: SessionId,
    pub log_id: LogId,
    pub targets: Vec<LegacyMappingTarget>, // 允许迁移阶段 1:N；v1 Last-Event-ID 仍为 1:1
    pub source_hash: ContentHash,
    pub mapped_at_ms: i64,
}
```

`LegacyMapping` 是 derived reconciliation index，不是 canonical source；可从旧源与 canonical log 重新生成。`legacy_identity` 必须包含 `source_generation_id` 与旧源的 canonical identity 编码；每个 `targets[]` 的 `derived_ordinal` 区分同一旧记录产生的多个 target；`canonical_seq` 必须精确到 `(fact_seq, projection_index)`。因此唯一键为：

```text
UNIQUE(legacy_identity, derived_ordinal, canonical_seq)
```

持久化时将每个 `targets[]` 展开为一行，再对上述三列做唯一约束。重写/压缩后 `source_generation_id` 改变，旧 mapping 的 `legacy_identity` 随之失效，不能与新 generation 的记录合并。v1 `Last-Event-ID` 映射仍必须 1:1；只有 migration reconciliation 的 `targets` 可以 1:N。

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
4. **S3 写切换**：停止旧 writer，保留旧文件只读。S3 是自动 rollback barrier。S3 必须由 migration CLI 在 `events.lock` 内完成：撤销旧 writer ownership、递增 `generation_epoch`/`fencing_token` 并 CAS 写 `writer-fence.json`；旧 writer 的下一次 append 必须收到 `stale_writer`。
5. **S4 删除 gate**：对账、replay、恢复、rollback 演练全绿后删除旧目录。

### 8.4 Rollback

rollback 命令必须满足：

- S0-S2：只需切回旧读路径，不要求反向迁移 canonical fact。
- S0-S2：旧 writer 仍在运行，rollback 后继续以旧 writer 为权威。
- S3 及之后：自动 rollback 不再支持；旧 writer 已停止，且 canonical 可能已有旧源不存在的新 fact。
- S3 后如需回退，必须另开前向 migration PR，定义 canonical→旧源回填、审计与二次 cutover；不得直接重启旧 writer 覆盖新事实。
- canonical log 在所有阶段保留完整，不因 rollback 删除。
- root quota ledger 位于 `{data_dir}/quota/{root_session_id}/`，不属于 session 目录；cutover/rollback/child close 都不得删除它，只能按 root 生命周期与显式 GC 规则处理。
- rollback 必须记录到 audit，并给出触发指标。

rollback 触发条件：

- 任一 P0 对账指标超阈值。
- replay live 等价失败。
- projection rebuild 失败。
- cursor 映射无法覆盖活跃 channel。
- 未知 fact 导致活跃 session 进入 read-only。

### 8.5 Migration CLI 契约

cutover/rollback 与 generation 切换使用以下命令；`--format json` 为强制参数，所有命令必须返回 machine-readable output，不得只打印人类文本：

```bash
qaqh migrate status --session <session_id> --format json
qaqh migrate legacy-map --session <session_id> --source <messages_jsonl|ringing_journal|ringing_latest|ringing_timeline|ringing_offload|meta_json> --source-generation <generation_id> --output <path> --format json [--dry-run]
qaqh migrate reconcile --session <session_id> --source-generation <generation_id> --canonical-log <log_id> --format json
qaqh migrate cutover --session <session_id> --to <s0|s1|s2|s3|s4> --writer-id <writer_id> --generation-epoch <u64> --fencing-token <decimal_u128_string> --format json
qaqh migrate rollback --session <session_id> --to <s0|s1|s2> --writer-id <writer_id> --generation-epoch <u64> --fencing-token <decimal_u128_string> --format json
```

所有写命令的参数必须显式给出 `writer_id`、`generation_epoch`、`fencing_token`；`fencing_token` 按 §5.2 使用无符号十进制字符串；服务端必须在 `writer-fence.json` 上执行 §5.2 的 CAS/拒绝协议。`status` 与 `legacy-map --dry-run` 不修改 fence。

输出 schema 固定为 `qaqh.migration-status/v1`：

```json
{
  "schema": "qaqh.migration-status/v1",
  "session_id": "0198f1a0-0000-7000-8000-000000000001",
  "stage": "s0",
  "source_generation_id": "messages-gen-7",
  "canonical_log_id": "0198f1a0-0000-7000-8000-000000000002",
  "writer_id": "writer-1",
  "generation_epoch": 7,
  "fencing_token": "1007",
  "mapping_count": 128,
  "canonical_count": 128,
  "missing_canonical": 0,
  "duplicate_canonical": 0,
  "projection_mismatch": 0,
  "seq_gap": 0,
  "replay_live_mismatch": 0,
  "content_ref_missing": 0,
  "torn_tail_unresolved": 0,
  "ok": true,
  "error_code": null
}
```

错误码闭集为：

| exit code | `error_code` | 含义 |
|---:|---|---|
| 0 | `null` | 命令成功 |
| 2 | `E_USAGE` | 参数缺失、非法 stage 或非法格式 |
| 3 | `E_SESSION_NOT_FOUND` | session/log 不存在 |
| 4 | `E_STALE_GENERATION` | source generation 已失效 |
| 5 | `E_MAPPING_CONFLICT` | `(legacy_identity, derived_ordinal, canonical_seq)` 冲突 |
| 6 | `E_RECONCILE_MISMATCH` | 对账指标非零 |
| 7 | `E_CUTOVER_GATE` | cutover gate 未通过 |
| 8 | `E_ROLLBACK_FORBIDDEN` | S3 后请求自动 rollback |
| 9 | `E_STALE_WRITER` | writer fence epoch/token 不匹配 |
| 10 | `E_IO` | durable write/fsync 失败 |

E2E 证据要求：

1. 对每个 fixture 记录完整命令行、退出码、JSON 输出、审计记录路径和 `writer-fence.json` 前后值。
2. `legacy-map` 必须证明 1:N target 的 `derived_ordinal` 稳定，并用同一 `canonical_seq` 二次运行得到相同结果。
3. `reconcile` 必须在缺失 canonical、重复 canonical、projection mismatch、seq gap、replay/live mismatch、content ref missing、torn tail 七项上分别给出非零退出码或全零证据。
4. `cutover`/`rollback` 必须证明 epoch 递增、fencing_token 严格递增、旧 writer 收到 `E_STALE_WRITER` 且没有 append。
5. S3 后自动 rollback 必须返回 `E_ROLLBACK_FORBIDDEN`，并只允许前向 migration PR。

---

## 9. Invariant → Schema / State / Test 映射

| ID | Schema/状态输入 | 测试 hook |
|---|---|---|
| I1 | writer lock + `fact_seq` 分配 | 并发 append conflict；JSONL 无交叉写 |
| I2 | `fact_seq` 连续 + snapshot cursor | crash 注入后 seq 连续；snapshot 不领先 |
| I3 | ProjectionSet + ContentUnavailable + derived/ | 删除 derived 后 rebuild 等价；GC content 保留 marker |
| I4 | TurnStarted/Finished/Interrupted | 并发输入/cancel/resume 单 active turn |
| I5 | ToolIntent/ToolFinished | 每 call 恰好一个终态；执行阶段恰好一个 Intent；deny/ask/backgrounded 均有终态 |
| I6 | replay_capability + side_effect_class/idempotency | NoReplay/IdempotentReplay/Reconcile crash fixture |
| I7 | typed output/content ref | model/display/resource/service 同源 |
| I8 | ReliableCursor + ReplayWindowManifest | `(fact_seq, projection_index)` 严格递增；snapshot/旧 cursor 按 expiry 表处理 |
| I9 | InteractionResolved/Expired | 重复 resolution first-answer-wins |
| I10 | PolicyDecisionRef/sandbox hash | policy/sandbox/audit 失败 fail-closed |
| I11 | fact → projection 映射 | 同 fact 序列任意重放同一 snapshot |
| I12 | typed WireEvent/ProjectionEvent 与 fact 分离 | client/TUI 不引用存储路径或原始 JSON |
| I13 | 队列容量 + quota + overflow policy | 默认上限生效，高水位 disconnect/reset/拒绝，不 OOM |
| I14 | unknown kind/version | read-only/upgrade-required，不跳过 |
| I15 | SandboxSpec hash + object refs | symlink/device/FIFO/TOCTOU 矩阵 |
| I16 | SubagentSpawned/Finished + supervisor + child_terminal_digest | parent unload/delete/shutdown/panic 在 child 全部 terminal+join 前不完成；spawn 双向孤儿恢复；trigger-turn 重放不产生第二个 turn |
| I17 | EventsCommit + CommitRecoveryRequired + clock | marker 失败/EIO/crash-before-rename 均只有一个恢复解释；clock 不越过 committed high-water |
| I18 | root QuotaLedger + quota.lock | root 与全部 child 经同一 owner/lock 串行 reservation；child 生命周期不删账本；reconciliation 无超卖 |

I17 的 canonical 文本固定为：
- `marker_missing_rebuildable=true` -> `writable` + `actions=[projection_rebuilt]`；
- previous generation/high-water 可证但 repair 尚未 durable -> `commit_recovery_required` + `actions=[commit_repaired]`，session 仍不可写；
- previous generation/high-water 可证且 repair durable -> `writable` + `actions=[commit_repaired]`；
- high-water 不可证 -> 不写 final fact，保持 runtime `CommitRecoveryRequired`。
禁止其它组合。

---

## 10. Fixture 与测试清单

### 10.1 Canonical fixtures

每个 fixture 必须有独立 metadata sidecar，固定字段为 `expected_fact_count`、`expected_terminal_status`、`expected_projection_revision`、`content_unavailable`，并可选扩展 `expected_turn_started_count`、`expected_turn_projection_count`。扩展字段口径固定为：前者计 canonical `TurnStarted` fact 数；后者只计 `Delivery::Reliable` 的 `turn_started` projection event，按 `stream_key` 去重后分别计数，不计 Replaceable/Ephemeral。下表给出 v2.0 必测集合；`fact_count` 指 canonical JSONL 总行数（包括不可解释的 unknown fact 和 final `SessionRecovered`），projection-only fixture 另列 `projection_events`。

| Fixture | 内容 | expected fact count | terminal / 状态断言 | projection revision | ContentUnavailable |
|---|---|---|---|---|---|
| `minimal-session.jsonl` | SessionCreated + InputAccepted + TurnStarted + TurnFinished | 4 | turn=`completed` | 每个 slot 可重建且 revision 单调 | none |
| `multi-round.jsonl` | TurnStarted + 两个 ModelRoundStarted + AssistantBlockSealed | 4 | turn 未闭合时 fail-closed | block seal 顺序稳定 | none |
| `projection-session-created.json` | §4.2.1 control slot 2 golden | fact_count=1; projection_events=1 | control state=`created` | `control.revision=1` | none |
| `projection-input-accepted.json` | §4.2.1 conversation slot 0 golden | fact_count=1; projection_events=1 | input=`accepted` | `conversation.revision=1` | none |
| `projection-tool-finished.json` | §4.2.1 timeline slot 1 golden | fact_count=1; projection_events=1 | `indeterminate` | `timeline.revision=3` | none |
| `tool-success.jsonl` | ToolCallDeclared + Intent + Finished succeeded | 3 | `succeeded` 唯一 | control/timeline 一致 | none |
| `replay-matrix-no-replay-readonly.jsonl` | Intent(no_replay, read_only) + crash | 2（Intent + Finished） | `indeterminate`，不重放 | control/timeline 一致 | none |
| `replay-matrix-no-replay-side-effect.jsonl` | Intent(no_replay, workspace_write) + crash | 2（Intent + Finished） | `indeterminate`，不重放、不二次副作用 | control/timeline 一致 | none |
| `replay-matrix-idempotent-side-effect.jsonl` | Intent(idempotent_replay, key, workspace_write) + crash + replay | 2 | 同 call 最终仅一个成功/失败终态 | replay 前后 revision 不重复 | none |
| `replay-matrix-reconcile-side-effect-conclusive.jsonl` | Intent(reconcile, external, probe_ref) + crash + conclusive probe | 2 | `reconciled=true` 的确定终态 | control/timeline 一致 | none |
| `replay-matrix-reconcile-side-effect-indeterminate.jsonl` | Intent(reconcile, external, probe_ref) + crash + inconclusive probe | 2 | `indeterminate`，`reconciled=false` | control/timeline 一致 | none |
| `tool-indeterminate.jsonl` | Intent + crash + Finished indeterminate | 2 | `indeterminate` | control/timeline 一致 | none |
| `policy-deny.jsonl` | ToolCallDeclared + ToolFinished(denied) | 2 | `denied`，无 Intent，零执行 metrics | control revision 前进一次 | none |
| `policy-ask-rejected.jsonl` | Requested + Resolved(rejected) + Finished(denied) | 3 | interaction=`resolved`; call=`denied`，无 Intent，零执行 metrics | control revision 前进一次 | none |
| `policy-ask-expired.jsonl` | Requested + Expired + Finished(cancelled) | 3 | interaction=`expired`; call=`cancelled`，无 Intent，零执行 metrics | control revision 前进一次 | none |
| `interaction-pending.jsonl` | InteractionRequested + restart | 1 | interaction=`pending` | 只重放一次 | none |
| `interaction-resolved.jsonl` | Requested + 一个 Resolved | 2 | interaction=`resolved`，终态唯一 | control revision 前进一次 | none |
| `interaction-duplicate-resolution.jsonl` | canonical 只含 Requested + Resolved；第二个 resolution 在 `interaction-duplicate-resolution.rejected.json` | 2 | 第二个输入被拒绝，canonical 终态不变 | revision 不前进 | none |
| `recovery-no-action.jsonl` | SessionCreated + 无 open 状态 + 首轮 load | 2（SessionCreated + final SessionRecovered） | `outcome=writable`, actions=[] | 可写后 revision 单调 | none |
| `recovery-unknown-fact.jsonl` | 已知前缀 + unknown kind + 已知后缀 | 3（前缀 + unknown line + final SessionRecovered） | `outcome=read_only_upgrade_required` | 后缀不产生 revision | none |
| `recovery-upgrade-supersede.jsonl` | read-only marker + 新 writer 理解全部 payload version | 2（旧 marker + writable supersede marker） | `outcome=writable`，actions=`upgrade_superseded` | 升级前 revision 不变，升级后恢复单调 | none |
| `recovery-upgrade-supersede-crash.jsonl` | writable marker 已 durable，`SessionRecovered` 前崩溃 | 2（旧 marker + 补写 final SessionRecovered） | 不再次递增 generation；`previous_recovery_id == marker.last_recovery_id` | 升级后恢复单调，无第二个 action | none |
| `recovery-input-admission.jsonl` | InputAccepted(input_purpose=trigger_turn) + crash before TurnStarted | 3（InputAccepted + TurnStarted + final SessionRecovered） | `expected_turn_started_count=1`；同一 input 只有一个 TurnStarted | `expected_turn_projection_count=2`：conversation slot 0 与 control slot 2 各一次；slot 1 仅属 timeline，不参与本断言 | none |
| `recovery-input-queue-only.jsonl` | InputAccepted(input_purpose=queue_only) + crash before TurnStarted | 2（InputAccepted + final SessionRecovered） | `expected_turn_started_count=0`；不补 `TurnStarted` | `expected_turn_projection_count=0`；InputAccepted 的 conversation/timeline revision 正常前进 | none |
| `recovery-subagent-edge.jsonl` | SubagentSpawned + child terminal + parent restart | 3（Spawned + Finished + final SessionRecovered） | child edge 只闭合一次 | control revision 前进一次 | none |
| `recovery-tool-denied.jsonl` | ToolCallDeclared + InteractionResolved(rejected) + parent restart | 3（Declared + Resolved + denied Finished） | `terminal_status=denied`，`execution_id=None`，零执行 metrics；`actions=[tool_finished]` | control/timeline revision 各前进一次 | none |
| `events-poison.jsonl` | committed fact + 失败 segment 完整前缀 + poison marker | 2（committed fact + final SessionRecovered） | `outcome=commit_recovery_required`；不把 poison 后前缀当 canonical | 只重放 committed revision | none |
| `events-commit-marker-missing.jsonl` | 完整 JSONL + marker 缺失且 `marker_missing_rebuildable=true` | 2（原 committed fact + final writable SessionRecovered） | 重建 marker 后恢复；`actions=[projection_rebuilt]`，不得伪造 `commit_repaired` | committed revision 不重复 | none |
| `events-commit-crash-before-rename.jsonl` | marker temp 已 fsync，进程在 rename 前崩溃 | 2（原 committed fact + final writable SessionRecovered） | 不把 temp 当 canonical；从 JSONL + evidence 重建后恢复；`actions=[projection_rebuilt]` | committed revision 不重复 | none |
| `events-commit-marker-corrupt.jsonl` | marker 损坏或 `log_id` 不匹配，high-water 不可证 | 1（仅保留可证明的 committed prefix；不写 final recovery fact） | runtime state=`CommitRecoveryRequired`；无 `SessionRecovered` | 不发布/ack，不推进 clock | none |
| `events-commit-fsync-eio.jsonl` | fact fsync 返回 EIO，旧 marker/high-water 可证明，repair 尚未 durable | 2（committed fact + final `SessionRecovered {outcome=commit_recovery_required, actions=[commit_repaired]}`） | 保持旧 high-water；repair 未 durable 前不可写 | 只重放 committed revision | none |
| `events-commit-marker-write-failure.jsonl` | fact fsync 成功但 marker rename/fsync 失败，旧 marker/high-water 可证明，repair 尚未 durable | 2（已 committed fact + final `SessionRecovered {outcome=commit_recovery_required, actions=[commit_repaired]}`） | 不发布/ack；repair durable 后回 writable | 不越过 committed high-water | none |
| `recovery-tombstone.jsonl` | SessionCreated + SessionDeleted + load | 3（SessionCreated + SessionDeleted + final SessionRecovered） | `outcome=tombstone` | 不再前进 | none |
| `compaction.jsonl` | facts + CompactionApplied | 2 | checkpoint 缺失可重建 | context revision 一致 | none |
| `torn-tail.jsonl` | 完整 fact + 半行 | 2（完整 fact + final SessionRecovered） | `torn_tail=true` | 与截断后 rebuild 一致 | none |
| `unknown-fact.jsonl` | 已知前缀 + unknown kind + 已知后缀 | 3（前缀 + unknown line + final SessionRecovered） | read-only/upgrade-required | 后缀不解释 | none |
| `cursor-multi-projection.jsonl` | 一个 fact 映射 3 projection | 1 | cursor 严格递增 | 各 slot index 固定 | none |
| `cursor-cross-version.json` | 旧 `payload_version` + 旧 projection_index cursor | 1 | `ResetRequired` 或 read-only/upgrade-required | 不按新 slot 重排 | none |
| `replay-window-in-range.json` | cursor 在下界内 | 1 | 正常 replay | 保持 cursor | none |
| `replay-window-before-floor.json` | cursor 早于下界，snapshot 有效 | 1 | `ResetRequired` 到 snapshot | snapshot revision 一致 | none |
| `replay-window-snapshot-missing.json` | cursor 早于下界，snapshot 缺失 | 1 | `ResetRequired {SnapshotMissing, snapshot_cursor=None}` | 不伪造 snapshot | none |
| `replay-window-snapshot-expired.json` | snapshot 过期或 hash mismatch | 1 | `ResetRequired {SnapshotExpired/SnapshotHashMismatch}`，必要时 `snapshot_cursor=None` | rebuild 后 revision 一致 | none |
| `v1-last-event-id.json` | epoch/channel/stream_seq 映射 | 1 | 命中/过期/跨 session reset | cursor 同序 | none |
| `backpressure.jsonl` | replay/progress/content 高水位 | 1 | 明确 reset/拒绝 | 不丢 reliable revision | none |
| `content-retained.jsonl` | 未过期 content 引用 | 1 | 正常读取 | content revision 不变 | `available` |
| `content-gc-unavailable.jsonl` | GC 后旧 fact 引用 | 1 | 读取返回 marker | rebuild 后 marker 一致 | `garbage_collected` |
| `content-legal-hold.jsonl` | legal hold + 到期 content | 1 | 禁止 delete/offload 删除 | revision 不变 | `available` |
| `content-offload.jsonl` | offload 成功但本地删除 | 1 | 读取返回 offload marker | rebuild 后 marker 一致 | `offloaded` |
| `recovery-intent-stale.jsonl` | closed batch 后再产生新 open state | 2（两个不同 batch 的 final SessionRecovered） | 不复用旧 RecoveryRef | 新 batch revision 前进 | none |
| `workspace-effect.jsonl` | ToolFinished(backgrounded) + WorkspaceResourceChanged | 2 | call 终态保持 `backgrounded` | ResourceProjection 只消费 resource fact | none |
| `legacy-mapping-1n.jsonl` | 同一 legacy identity 派生多个 canonical targets | 2 | 每个 target 唯一键不冲突 | target revision 稳定 | none |
| `writer-fence-stale.jsonl` | epoch 切换后旧 writer append | 0（拒绝输入） | `E_STALE_WRITER`，无新 fact | 不前进 | none |

`interaction-duplicate-resolution.jsonl` 的修正语义是：canonical fixture 只有 `InteractionRequested` 与一个 `InteractionResolved` 两行；重复 resolution 只能存在于单独的 rejected input 文件，不能被写入 canonical JSONL，也不能产生第三行或新 revision。

Replay capability/副作用/对账组合的四个规范单元必须全部有独立 fixture：

| 单元 | capability | side effect | 对账输入 | fixture | 期望 |
|---:|---|---|---|---|---|
| 1 | `no_replay` | `read_only` | 无 | `replay-matrix-no-replay-readonly.jsonl` | 不重放，唯一 `indeterminate` |
| 2 | `no_replay` | `workspace_write/process/network/external` | 无 | `replay-matrix-no-replay-side-effect.jsonl` | 不重放，唯一 `indeterminate`，无二次副作用 |
| 3 | `idempotent_replay` | 有副作用且 key 稳定 | typed replay result | `replay-matrix-idempotent-side-effect.jsonl` | 唯一成功/失败终态，重复 load 不重复副作用 |
| 4 | `reconcile` | 有副作用且 probe 可判定 | probe conclusive/inconclusive | `replay-matrix-reconcile-side-effect-conclusive.jsonl` + `replay-matrix-reconcile-side-effect-indeterminate.jsonl` | 前者确定终态且 `reconciled=true`；后者唯一 `indeterminate` |

### 10.1.1 最小 JSONL 样例

以下只示例 canonical 形状，不是完整 fixture。`ts_ms`、UUID、ULID 和 hash 均为占位值。

```jsonl
{"schema":{"name":"qaqh.session-fact","version":2,"payload_version":2},"session_id":"0198f1a0-0000-7000-8000-000000000001","log_id":"0198f1a0-0000-7000-8000-000000000002","fact_seq":1,"event_id":"01J00000000000000000000001","ts_ms":1789830000000,"payload":{"kind":"session_created","data":{"created_at_ms":1789830000000,"cwd":"/workspace","model":"deepseek-v4.1-flash","schema_caps":["reliable_replay","interaction_replay"]}}}
{"schema":{"name":"qaqh.session-fact","version":2,"payload_version":2},"session_id":"0198f1a0-0000-7000-8000-000000000001","log_id":"0198f1a0-0000-7000-8000-000000000002","fact_seq":2,"event_id":"01J00000000000000000000002","ts_ms":1789830000010,"causation_id":"01J00000000000000000000001","payload":{"kind":"input_accepted","data":{"input_id":"input_01J00000000000000000000000","input_kind":"user_text","input_purpose":"trigger_turn","inline_text":"hello","attachments":[],"actor":{"kind":"user","id":"local"}}}}
{"schema":{"name":"qaqh.session-fact","version":2,"payload_version":2},"session_id":"0198f1a0-0000-7000-8000-000000000001","log_id":"0198f1a0-0000-7000-8000-000000000002","fact_seq":3,"event_id":"01J00000000000000000000003","ts_ms":1789830000020,"turn_id":"turn_01J00000000000000000000000","causation_id":"01J00000000000000000000002","payload":{"kind":"turn_started","data":{"turn_id":"turn_01J00000000000000000000000","input_id":"input_01J00000000000000000000000","mode":"normal"}}}
```

Tool intent/finish：

```jsonl
{"schema":{"name":"qaqh.session-fact","version":2,"payload_version":2},"session_id":"0198f1a0-0000-7000-8000-000000000001","log_id":"0198f1a0-0000-7000-8000-000000000002","fact_seq":4,"event_id":"01J00000000000000000000004","ts_ms":1789830000030,"turn_id":"turn_01J00000000000000000000000","call_id":"call_01J00000000000000000000000","payload":{"kind":"tool_intent","data":{"call_id":"call_01J00000000000000000000000","execution_id":"exec_01J00000000000000000000000","replay_capability":{"kind":"no_replay"},"policy_decision":{"outcome":"allow","rule_id":"policy/read","decided_at_ms":1789830000030},"sandbox_spec_hash":"sha256:1111111111111111111111111111111111111111111111111111111111111111","side_effect_class":"workspace_write","intent_at_ms":1789830000030}}}
{"schema":{"name":"qaqh.session-fact","version":2,"payload_version":2},"session_id":"0198f1a0-0000-7000-8000-000000000001","log_id":"0198f1a0-0000-7000-8000-000000000002","fact_seq":5,"event_id":"01J00000000000000000000005","ts_ms":1789830000040,"turn_id":"turn_01J00000000000000000000000","call_id":"call_01J00000000000000000000000","causation_id":"01J00000000000000000000004","payload":{"kind":"tool_finished","data":{"call_id":"call_01J00000000000000000000000","execution_id":"exec_01J00000000000000000000000","terminal_status":"indeterminate","error":{"code":"indeterminate_after_crash","message":"non-idempotent execution not replayed","retryable":false},"metrics":{"started_at_ms":1789830000030,"finished_at_ms":1789830000040,"retry_count":0,"output_bytes":0,"progress_bytes_total":0},"reconciled":false,"recovery_ref":{"recovery_id":"recovery_01J00000000000000000000000","recovery_event_id":"01J00000000000000000000006","recovery_input_fingerprint":"sha256:1111111111111111111111111111111111111111111111111111111111111111"},"finished_at_ms":1789830000040}}}
```

Recovery：

```jsonl
{"schema":{"name":"qaqh.session-fact","version":2,"payload_version":2},"session_id":"0198f1a0-0000-7000-8000-000000000001","log_id":"0198f1a0-0000-7000-8000-000000000002","fact_seq":6,"event_id":"01J00000000000000000000007","ts_ms":1789830000050,"payload":{"kind":"session_recovered","data":{"recovery_id":"recovery_01J00000000000000000000000","recovery_event_id":"01J00000000000000000000006","recovery_input_fingerprint":"sha256:1111111111111111111111111111111111111111111111111111111111111111","outcome":"writable","last_good_fact_seq":4,"torn_tail":false,"actions":[{"kind":"tool_finished","completion":{"call_id":"call_01J00000000000000000000000","execution_id":"exec_01J00000000000000000000000","terminal_status":"indeterminate","output_ref":null,"error":{"code":"indeterminate_after_crash","message":"non-idempotent execution not replayed","retryable":false},"metrics":{"started_at_ms":1789830000030,"finished_at_ms":1789830000040,"retry_count":0,"output_bytes":0,"progress_bytes_total":0},"reconciled":false,"recovery_ref":{"recovery_id":"recovery_01J00000000000000000000000","recovery_event_id":"01J00000000000000000000006","recovery_input_fingerprint":"sha256:1111111111111111111111111111111111111111111111111111111111111111"},"finished_at_ms":1789830000040,"evidence_ref":null,"evidence_fact_seq":null,"evidence_event_id":null}}],"recovered_at_ms":1789830000050}}}
```

### 10.2 必测命令

本阶段只冻结测试清单，不实现测试。后续 P1/P2 实现时应至少提供：

> `qaqh-session` 当前已存在于 workspace，是 P1 的 landing package；如果后续把 canonical log 抽到 `qaqh-store`，必须在同一实现 PR 更新本节命令与 CI，不得留下失效包名。

```bash
cargo test -p qaqh-session --test session_fact_v2 -- --exact session_fact_v2::envelope::roundtrip
cargo test -p qaqh-session --test projection -- --exact projection::golden_projection_shapes
cargo test -p qaqh-session --test projection -- --exact projection::static_slot_mapping
cargo test -p qaqh-session --test tool_recovery -- --exact tool_recovery::replay_capability_priority
cargo test -p qaqh-session --test tool_recovery -- --exact tool_recovery::replay_capability_matrix
cargo test -p qaqh-session --test policy_lifecycle -- --exact policy_lifecycle::ask_deny_terminal
cargo test -p qaqh-session --test recovery -- --exact recovery::batch_idempotency
cargo test -p qaqh-session --test recovery -- --exact recovery::plan_hash_has_no_self_reference
cargo test -p qaqh-ringing --test replay_window -- --exact replay_window::snapshot_expiry_matrix
cargo test -p qaqh-domain --test tool_terminal -- --exact tool_terminal::legacy_parity
cargo test -p qaqh-session --test content_gc -- --exact content_gc::unavailable_and_logical_clock
cargo test -p qaqh-session --test durability -- --exact durability::fsync_failpoint_matrix
cargo test -p qaqh-daemon --test migration -- --exact migration::legacy_mapping_fencing_cli
cargo test -p qaqh-session --test fixtures -- --exact fixtures::duplicate_resolution
```

零匹配不算通过。CI/脚本必须按以下 guard 运行，禁止用子串过滤或裸 `cargo test <name>`：

```bash
run_exact() {
  local pkg="$1" target="$2" path="$3" out
  out="$(cargo test -p "$pkg" --test "$target" -- --exact "$path" 2>&1)"
  printf '%s\n' "$out"
  printf '%s\n' "$out" | grep -Eq 'test result: ok\. 1 passed; 0 failed; 0 ignored'
}
```

每个 fixture 的精确断言必须读取 §10.1 metadata sidecar，逐项比较 `expected_fact_count`、`expected_terminal_status`、`expected_projection_revision`、`content_unavailable`；若存在 `expected_turn_started_count` / `expected_turn_projection_count`，必须按上述口径一并比较。任何字段缺失、`None` 误写为空文本、或 `ContentUnavailable` 被替换为空内容都失败。

12 项 blocking finding 的逐项复现入口：

| finding | 精确测试入口 |
|---:|---|
| 1 projection/wire schema 与 golden JSON | `cargo test -p qaqh-session --test projection -- --exact projection::golden_projection_shapes` |
| 2 replay capability 优先级 | `cargo test -p qaqh-session --test tool_recovery -- --exact tool_recovery::replay_capability_priority` |
| 2a replay capability/副作用/对账四单元 | `cargo test -p qaqh-session --test tool_recovery -- --exact tool_recovery::replay_capability_matrix` |
| 3 policy lifecycle | `cargo test -p qaqh-session --test policy_lifecycle -- --exact policy_lifecycle::ask_deny_terminal` |
| 4 recovery batch 幂等 | `cargo test -p qaqh-session --test recovery -- --exact recovery::batch_idempotency` |
| 5 plan_hash 无自引用 | `cargo test -p qaqh-session --test recovery -- --exact recovery::plan_hash_has_no_self_reference` |
| 6 per-fact/per-slot 与跨版本 cursor | `cargo test -p qaqh-session --test projection -- --exact projection::static_slot_mapping` |
| 7 replay window/snapshot expiry | `cargo test -p qaqh-ringing --test replay_window -- --exact replay_window::snapshot_expiry_matrix` |
| 8 tool terminal parity/backgrounded | `cargo test -p qaqh-domain --test tool_terminal -- --exact tool_terminal::legacy_parity` |
| 9 ContentUnavailable/逻辑 clock/legal hold | `cargo test -p qaqh-session --test content_gc -- --exact content_gc::unavailable_and_logical_clock` |
| 10 quota/fsync/failpoint | `cargo test -p qaqh-session --test durability -- --exact durability::fsync_failpoint_matrix` |
| 11 legacy key/fencing/migration CLI | `cargo test -p qaqh-daemon --test migration -- --exact migration::legacy_mapping_fencing_cli` |
| 12 duplicate resolution fixture | `cargo test -p qaqh-session --test fixtures -- --exact fixtures::duplicate_resolution` |

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
- 同一 fact 的 projection slot 是否因 delta 数量变化而重排。
- snapshot 缺失/过期时是否伪造 baseline 或错误接受旧 cursor。
- deny/ask 拒绝/ask 过期是否仍写唯一 `ToolFinished` 且 metrics 规则一致。
- GC 后 rebuild 是否保留 `ContentUnavailable` marker，是否误用墙钟 now。
- `plan_hash` 是否出现 `RecoveryIntent(plan_hash)` 自引用。
- migration mapping 唯一键或 writer fence 是否允许旧 writer 追加。

---

## 11. 冻结门禁与未决项

### 11.1 本 spec 完成后允许进入的 P0 Gate

- `session-fact-v2` 字段级 schema 已冻结。
- 全部 `FactPayload` variant 的 golden JSON 与 `ActivityState` 闭集已冻结。
- `ProjectionEvent`/`ProjectionPayload`/`WireEvent`/`StreamKey` 与 per-fact slot 表已冻结。
- Tool replay capability、policy lifecycle 与唯一 `ToolFinished` 终态已冻结。
- 复合 cursor、replay、dedupe、expiry 已冻结。
- recovery batch、plan_hash 与幂等键已冻结。
- snapshot/replay window、`ContentClockRecord`、ContentUnavailable、content quota 与 GC 逻辑 clock 已冻结。
- v1 `Last-Event-ID` 映射规则已冻结。
- 双写、legacy mapping 唯一键、writer fencing、cutover/rollback 阈值与 CLI 已冻结。
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
- 验收映射必须覆盖 I1-I18；§9 的 I17（EventsCommit/CommitRecoveryRequired/clock）与 I18（root QuotaLedger/quota.lock）必须与 plan §12 和 fixture 清单同 ID、同包名、同断言。

本 spec 的验收结论由 #106 独立反证报告给出；A 不得自行宣告“已通过独立评审”。
