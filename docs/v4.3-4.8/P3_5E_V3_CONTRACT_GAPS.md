# P3-5e 前置工单：`qaqh-ringing-v3` 契约补齐（供后端同事执行）

> **状态（2026-11-27 后端审计与裁决完成）**：已依据本仓宪法与 `ADR-0042` 完成全部阻塞清空与投影补齐，同时**驳回/修正**了工单中 5 处不符合 V3 架构或不必要的设计（见文末 `§6 后端审计裁决与额外修复`）。
>
> **一句话结论**：`P3-5a` 冻结的 V3 契约只够"能跑通控制面"，**不够 `../qaqh-gpui` 现网渲染**——`ToolCallView` 丢掉了 args/output/diff/metrics/code_delta/effects/exec_session_id，`BlockView` 丢掉了 `block_id`/`turn_id`/`actor`/`purpose`/图片/子代理/每块 model+usage，`ExecSessionSnapshot` 丢掉了状态机与尾窗，`InteractionView` 把结构化审批正文退化成一段 JSON 字符串。按现状切端，GPUI 的 diff 面板、工具参数、执行会话观察窗、图片附件、上下文/缓存遥测会**直接消失**。
>
> **本工单只做一件事**：把这些"前端已经在用的事实"补成 V3 强类型字段，投影层同步填上。**禁止**的方案见 §5。

---

## 1. 逐域缺口与建议形状

### A. 工具调用详情（最高优先级：全仓 UI 都靠它）

前端用点：
- `../qaqh-gpui/crates/qaqh-gpui-session/src/store.rs:142-211`（`handoff_facts`：读 `tool.args.handoff_doc/work_summary/auto_continue/modified_files` 与 `tool.output` 的机器面 JSON）
- `.../timeline.rs:1178-1225`（`args_summary`：按工具名从 args 摘标题/路径）、`:1679`（`args["cmd"]`）、`:1688`（`fork_suffix(name, args)`）、`:1706`（`code_delta`）、`:1757`（args 摘要正文）、`:2043`（`metrics.started_at_ms/finished_at_ms`）
- `.../code_panel.rs:423-424`（`tool.diff.is_some() || tool.code_delta.is_some()` 决定是否给 diff 入口）、`:1254-1270`（diff 渲染）
- `.../reducer.rs:242-338`（`ToolDeclared{args}` / `ToolStateChanged{state}` / `ToolFinished{output,output_truncated,diff,metrics,code_delta,effects}` 写入 store）

V3 现状（`crates/qaqh-ringing-v3/src/stream.rs:146-159`）：只有 `tool_call_id / tool_name / status / started_at_ms / finished_at_ms / summary`。

建议补齐（`stream.rs`）：

```rust
/// 工具对代码的净变更统计（前端 code_panel 的 +/- 徽标）。
pub struct CodeDeltaView {
    pub lines_added: u32,
    pub lines_removed: u32,
    pub files_created: u32,
    pub files_deleted: u32,
    pub file: Option<String>,
}

/// 工具执行度量（前端"耗时/字节/重试"行）。
pub struct ToolMetricsView {
    pub started_at_ms: u64,
    pub finished_at_ms: u64,
    pub retry_count: u32,
    pub output_bytes: u64,
    pub progress_bytes_total: u64,
}

/// 工具产生的结构化效果（目前只有子代理派生）。
pub enum ToolEffectView {
    SubagentSpawned { child_session_id: SessionId, name: String, task_text: String },
}

pub struct ToolCallView {
    pub tool_call_id: ToolCallId,
    pub tool_name: String,
    pub status: ToolCallStatus,          // 见下方状态机补充
    pub started_at_ms: u64,
    pub finished_at_ms: Option<u64>,
    pub summary: Option<ContentRef>,     // 保留：短摘要
    // ── 新增 ──
    pub args: Option<ContentRef>,        // 参数 JSON 全文（大参数走 blob）
    pub output: Option<ContentRef>,      // 结果全文（summary 的权威版）
    pub output_truncated: bool,
    pub diff: Option<ContentRef>,        // 写工具的 unified diff
    pub code_delta: Option<CodeDeltaView>,
    pub metrics: Option<ToolMetricsView>,
    pub exec_session_id: Option<ExecSessionId>,
    pub interaction_id: Option<InteractionId>, // 等待审批/Ask 时的权威锚点
    pub effects: Vec<ToolEffectView>,
}
```

状态机补充：V3 `ToolCallStatus` 目前是 `Pending|Running|Succeeded|Failed|Cancelled|Indeterminate`，前端 `ToolCallState` 需要 `AwaitingInteraction{interaction_id}` 与 `Cancelling`：
- `AwaitingInteraction` 由 `interaction_id.is_some() && status == Pending` 派生（**不需要新变体**，但两字段必须同帧给齐）；
- `Cancelling` **建议新增**：前端 `timeline.rs:768` 有"取消中"这一档颜色/文案，现在无处表达。

### B. 时间线块 `BlockView`（第二优先级）

前端用点：
- `.../timeline.rs:555-557`（`Input{actor,purpose}` 判"用户轮次触发"）、`:3219-3238`（`inter_agent`/`subagent_terminal`/`actor.kind != User` 各走不同渲染分支）、`:3165-3170`（`Input{content}` 正文）
- `.../turn_rail.rs:58-61`（`actor.kind == User && purpose == TriggerTurn` 才是轮次锚点）
- `.../timeline.rs:3137-3160`（`AssistantBlock{block_kind,content,model,usage,generated_ms,ord}`）
- `.../store.rs:272-326`（`streaming: Vec<(BlockId, AssistantBlockKind, String)>` 按 `block_id` 聚合增量）

V3 现状（`session.rs:181-192`）：`{ ord, kind, content, tool_call_id, created_at_ms }`——**连 `block_id` 都没有**，而 `AnswerDelta`/`BlockFinal`（`stream.rs:318-343`）是按 `block_id` 投递的，前端无法把增量并回块。

建议补齐：

```rust
pub struct BlockView {
    pub ord: u64,
    pub kind: BlockKind,                  // 需要新增 BlockKind::Compaction 以外的 Input 细分见下
    pub content: ContentRef,
    pub tool_call_id: Option<ToolCallId>,
    pub created_at_ms: u64,
    // ── 新增 ──
    pub block_id: BlockId,                // 与 AnswerDelta/BlockFinal 对齐（必需）
    pub turn_id: Option<TurnId>,          // 回合归属：截断/轮次导轨/过程分组
    pub actor: Option<BlockActor>,        // User/Api/System/Agent/Subagent
    pub purpose: Option<InputPurpose>,    // TriggerTurn/QueueOnly/Steer/Interject
    pub model: Option<String>,            // 答案块模型名
    pub usage: Option<UsageInfo>,         // 块级用量（composer 上下文/缓存遥测）
    pub generated_ms: Option<u64>,        // 吐字速率权威分母
    pub images: Vec<ImageRefView>,        // 见 C
    pub attachments: Vec<ContentRef>,
    pub inter_agent: Option<InterAgentView>,          // A2A 卡片
    pub subagent_terminal: Option<SubagentTerminalView>,
}

pub enum BlockActor { User, Api, System, Agent, Subagent }

pub enum InputPurpose { TriggerTurn, QueueOnly, Steer, Interject }

pub struct InterAgentView {
    pub message_id: String,
    pub root_session_id: SessionId,
    pub author: String,
    pub recipient: String,
    pub other_recipients: Vec<String>,
    pub task_id: Option<String>,
    pub reply_to: Option<String>,
    pub created_at_ms: u64,
}

pub struct SubagentTerminalView {
    pub child_session_id: SessionId,
    pub parent_call_id: ToolCallId,
    pub terminal: SubagentTerminalKind, // Completed/Failed/Cancelled/TimedOut
}
```

> `BlockKind` 现在的 `User/Answer/Reasoning/ToolCall/ToolResult/System` 够用；`System` 目前同时承载"压缩摘要"与"错误横幅"，**压缩摘要已单独走 `pinned_compaction`**，建议明确写进注释（前端据 `pinned_compaction` 渲染置顶卡、据 `System` 渲染横幅）。

### C. 图片（必须走 blob，禁止 base64 内联）

前端用点：`../qaqh-gpui/crates/qaqh-gpui-render/src/media.rs:3-51`（`qaqh_client::ImageBlock` → base64 解码 → GPUI Image）、`qaqh-gpui-session/src/store.rs:274`（`images: Vec<ImageBlock>`）、`turn_rail.rs:83-91`（判断"本轮带图"）。

现状：V2 用 `ImageBlock{mime_type, data: base64}` 内联整张图；P3-5e 明确要"图片附件直接经 `upload_blob` 传原始 `Bytes` + `ContentRef::Blob`"。V3 现在**没有任何图片字段**。

建议补齐：

```rust
pub struct ImageRefView {
    pub hash: ContentHash,   // 前端 fetch_blob(hash) → 原始字节 → 解码
    pub len: u64,
    pub mime_type: String,
}
```

落点：`BlockView.images`（用户输入图 / 工具结果图同口径）、`ToolCallView` 不需要单独字段（工具图进 `output` 的 `ContentRef::Blob` 或块级 `images`）。

### D. 执行会话 `ExecSessionSnapshot`

前端用点：`terminal_panel.rs:4-5,226-231,308-328`（`output_preview`/`preview_start_offset`/`next_offset`/`state.is_terminal()`/`output`）、`timeline.rs:1775-1778`（`exec.output_preview`）。

V3 现状（`stream.rs:239-254`）：`{ exec_session_id, command, tty, finished, exit_code, next_offset, output }`。

建议补齐：

```rust
pub struct ExecSessionSnapshot {
    pub exec_session_id: ExecSessionId,
    pub command: String,
    pub tty: bool,
    pub finished: bool,
    pub exit_code: Option<i32>,
    pub next_offset: u64,
    pub output: Option<ContentRef>,
    // ── 新增 ──
    pub state: ExecSessionStateView,     // Running / Stopping{reason} / Finished{outcome}
    pub owner_session_id: SessionId,
    pub origin_turn_id: Option<TurnId>,
    pub origin_call_id: Option<ToolCallId>,
    pub cwd: Option<String>,
    pub started_at_ms: u64,
    pub finished_at_ms: Option<u64>,
    pub output_preview: String,          // ≤16 KiB、UTF-8 边界对齐的尾窗
    pub preview_start_offset: u64,
    pub output_complete: bool,
    pub capture_omitted_bytes: u64,
    pub stream_dropped_bytes: u64,
}

pub enum ExecSessionStateView {
    Running,
    Stopping { reason: ExecStopReasonView },
    Finished { outcome: ExecOutcomeView },
}
pub enum ExecStopReasonView { Interrupt, Terminate, TurnCancel, Timeout, Shutdown }
pub enum ExecOutcomeView {
    Exited { exit_code: i32 },
    Cancelled { reason: ExecStopReasonView, exit_code: Option<i32> },
    TimedOut { exit_code: Option<i32> },
    Interrupted { reason: String },
}
```

> `finished: bool` + `exit_code` 保留即可（前端已有两处只读这两项），`state` 是新增的权威状态机。

### E. 交互 `InteractionView`（权限审批正文不许是 JSON 字符串）

前端用点：`ask_dock.rs:241-245`（`ask::parse_request(&interaction.request)` 解析 `NormalizedAsk` JSON）、`timeline.rs:3402-3450`（审批卡）、`:3884`（`content_text(&interaction.request)` 兜底正文）。

V3 现状（`stream.rs:185-202`）：`{ interaction_id, kind, tool_call_id, title, body: String, options, questions, expires_at_ms }`，而 `v3_bridge/projection/views.rs:37-44` 把权限正文 `body.to_string()`（**JSON-in-string**）塞进 `body`。

建议补齐：

```rust
pub struct InteractionView {
    pub interaction_id: InteractionId,
    pub kind: InteractionKind,
    pub tool_call_id: Option<ToolCallId>,
    pub title: String,
    pub body: String,                       // 人读引导语（不再是 JSON）
    pub options: Vec<String>,
    pub questions: Vec<AskQuestionView>,
    pub expires_at_ms: Option<u64>,
    // ── 新增 ──
    pub turn_id: Option<TurnId>,
    pub choices: Vec<InteractionChoiceView>,   // 前端按钮组（含 tone）
    pub permission: Option<PermissionRequestView>, // 结构化审批正文
}

pub struct InteractionChoiceView {
    pub decision: String,      // approved / approved_for_session / rejected / answered / dismissed
    pub label: String,
    pub description: Option<String>,
    pub tone: ChoiceTone,      // Primary / Secondary / Destructive
}

pub struct PermissionRequestView {
    pub tool_name: String,
    pub action_summary: Option<String>,
    pub justification: Option<String>,
    pub reason: String,
    pub paths: Vec<String>,
    pub category: String,   // read/write/exec/net
    pub level: u8,          // 1..=4
    pub risk: String,       // low/medium/high
    pub consequence: String,
}
```

### F. 会话快照 / 列表

前端用点：`store.rs:1117-1180`（`settings.meta.settings_revision/model_selection/tool_mode/custom_tools`）、`:1469-1482`（`meta.session_id/title/updated_at/cwd/archived/handoff_from_session_id`）、`timeline.rs:803-819`（`SessionRunStatus` 五档徽标）、`store.rs:1232`（`meta.cwd`）。

V3 现状：`SessionSummary`（`session.rs:284-307`）已有 `session_id/title/workspace_id/cwd/archived/model_selection/tool_mode/settings_revision/handoff_from/activity/updated_at_ms`。

需补（小）：
- `custom_tools: Vec<String>`（前端 `store.rs:1172` 读 `meta.custom_tools.join(", ")` 回填自定义白名单编辑框）；
- `created_at_ms: u64`（前端 `store.rs:1476` 用创建时刻做开箱卡兜底时间源，目前只能靠本地缓存猜）；
- `SessionRunStatus::NotRunning` 在 V3 无对应（`ActivityState` 只有四态）。**这不是缺口而是纠偏**：V3 语义下"未加载"不成立，前端按 `activity` 渲染即可——请在 `SessionSummary.activity` 文档里写死这句。

### G. 事件流（缺的很少，但缺的两条很硬）

V3 `SessionEvent`（`stream.rs:266-495`）已有 27 变体，覆盖 `ActivityChanged/TurnStarted/TurnPhaseChanged/TurnFinished/ProviderRetrying/AnswerDelta/ReasoningDelta/BlockFinal/TimelineAppended/TimelineTruncated/ToolCallUpdated/ToolArgumentsDelta/ToolOutputDelta/InteractionRequested/InteractionResolved/ExecSessionUpdated/ExecOutputDelta/DriverChanged/PinnedCompactionChanged/CatalogChanged/SessionMetaChanged/SubagentUpdated/GoalUpdated/HandoffSealed/PromptBlocked/SessionRemoved/ResyncRequired`。

需要补的两条（都因为前端要"整枝删除"与"参数流"）：
1. **`ToolArgumentsDelta` 只有 `delta: String`**（`stream.rs:368-375`），前端 `reducer.rs:197-229` 还需要 `tool_name / lines_added / lines_removed / target_files / args_complete` 才能在"参数生成中"卡片上显示估算行数与目标文件。建议扩为：
   ```rust
   ToolArgumentsDelta {
       session_id: SessionId,
       tool_call_id: ToolCallId,
       tool_name: String,
       delta: String,
       lines_added: Option<u32>,
       lines_removed: Option<u32>,
       target_files: Vec<String>,
       args_complete: bool,
   }
   ```
2. **`TimelineTruncated` 缺 `removed_turn_ids`**（`stream.rs:352-359` 只有 `from_turn_id` + `timeline_epoch`）。前端 `reducer.rs:414-455` 靠"被删回合集合"整枝清掉该回合的工具卡/交互/参数流；只给 `from_turn_id` 时，若回合定序与时间线 ord 不一致就会误删。建议加 `removed_turn_ids: Vec<TurnId>`。

另：`HandoffSealed.summary: ContentRef`（`stream.rs:466-473`）前端需要 `work_summary` 纯文本（`reducer.rs:492-517` 直接写进封箱卡）。保持 `ContentRef` 可以，但**必须保证投影端发 `Inline`**（大摘要之外不要外置），否则前端还得为一张卡片拉 blob。

### H. 模型 / 服务商 / 配置

- `ConfigRequest/Response`（`config.rs`）已经够用：前端把 `JsonText` 反序列化成 `qaqh_profile::{ConfigDto, ProfilesDocument}`（`qaqh-profile` 本来就在依赖里，不是 V2 契约）。
- `ProviderPresetView`（`config.rs:22-34`）的 `wire: String` / `credential: String` 是裸字符串，前端设置页需要**枚举**才能做控件分支。建议改成强类型（复用 `qaqh-core-vocab` 新增的 `Wire` / `CredentialSource`，或直接指向 `qaqh-profile` 的对应枚举）。
- `ModelSelection`（`core-vocab/domain.rs:137-144`）已有 `provider_id/model_id/options`，但缺 `qaqh_types::ModelSelection` 里的**模型能力位**（前端 `store.rs:1760` 的 `model_effort_levels` 需要 `capabilities.reasoning_efforts`）。建议在 `config.rs` 的目录视图里给出每个模型的 `capabilities`（不要塞进 `ModelSelection`——那是"用户选了哪个"，不是"模型能做什么"）。

### I. 账号域（`AccountRequest/Response`）

V3 `account.rs` 的 `SiwcStatusView / DsAccountStatusView / DsBalanceView / DsBonusView / DsAccountModelView` 字段看着齐（比前端现在用的 `qaqh_client::service_types::*` 更完整），迁移时按字段逐个核对即可，**预期无需新增类型**。唯一注意：前端 `daemon.rs:1074` 用 `bonus.order_id` 做已读移除，`DsBonusView` 必须保留 `order_id`。

---

## 2. 变更落点（改哪里 + 破坏面）

| 缺口 | `qaqh-ringing-v3` | `v3_bridge` 投影 | 破坏面（需要同步的测试/投影） |
|---|---|---|---|
| A 工具详情 | `src/stream.rs` | `projection/events.rs:150-250`、`session/views.rs:340-400` | `qaqh-ringing-v3/src/tests.rs` 的 postcard round-trip；`v3_bridge/regression_tests.rs` |
| B 块字段 | `src/session.rs` | `projection/timeline.rs`、`session/views.rs`（`block_view`） | `v3_server` 快照守卫、`v3_bridge/tests.rs` |
| C 图片 | `src/session.rs` | 同上 + `blob.rs`（按 hash 归属） | 无既有测试覆盖，需新守卫：图片必须 `ContentRef::Blob` |
| D 执行会话 | `src/stream.rs` | `projection/views.rs:87-120`（`exec_view`） | `v3_bridge/regression_tests.rs` exec 相关 |
| E 交互 | `src/stream.rs` | `projection/views.rs:15-85`（`interaction_view`，删掉 `body.to_string()`） | `views.rs` 内既有 ask 投影测试要改成断言 `questions` + `permission` |
| F 会话摘要 | `src/session.rs` | `session/views.rs`（summary 构造处） | `v3_bridge/session/*` 快照断言 |
| G 事件扩展 | `src/stream.rs` | `projection/events.rs`（ToolArgumentsDelta / TimelineTruncated 分支） | `qaqh-ringing-v3/src/tests.rs`、`v3_server/invariants` |
| H 配置枚举 | `src/config.rs` + `qaqh-core-vocab`（或 `qaqh-profile`） | `config_account.rs`（presets 投影） | `config_account` 单测 |
| I 账号 | 无（核对字段） | `config_account/account_map.rs` | 现有 account 映射测试 |

**执行要求**：每个缺口补齐时，必须同步在 `crates/qaqh-daemon/src/v3_bridge/regression_tests.rs` 加一条"红→绿"投影断言（现在那里已有 315 行同类测试可照抄），否则 V3 契约会再次在投影端被悄悄削薄——这正是本轮问题的成因。

---

## 3. 前端迁移顺序（依赖本工单的落地顺序）

1. **A + B + C**（工具详情 / 块字段 / 图片）→ 前端 `timeline.rs`、`code_panel.rs`、`store.rs` 的核心渲染一次性迁完；
2. **D + E**（执行会话 / 交互）→ `terminal_panel.rs`、`ask_dock.rs`；
3. **F + G**（摘要 / 事件）→ `reducer.rs`、`sidebar.rs`；
4. **H + I**（配置 / 账号）→ `settings_ui.rs`、`daemon.rs`。

顺序不是偏好：`timeline.rs`（3,980 行）是唯一无法"局部先迁"的文件，它必须等 A/B/C 齐全，否则会写两遍。

---

## 4. 验收口径

```bash
# 契约层
cargo test -j2 -p qaqh-ringing-v3 -p qaqh-core-vocab
cargo test -j2 -p qaqh-daemon --bin qaqh-daemon v3_bridge
cargo test -j2 -p qaqh-daemon --bin qaqh-daemon v3_server
# 反证：投影不得丢字段（新增断言必须真的会红）
cargo test -j2 -p qaqh-daemon --bin qaqh-daemon regression
```

---

## 5. 明确禁止的做法（写死在这里，避免下一轮重犯）

1. **不要在 `qaqh-ringing-v3` 里放 `serde_json::Value` 字段**。V3 的地基是"纯 postcard 强类型"，一个 `Value` 就等于给静态契约开一条动态逃生口（`ContentRef::Inline{text}` 里塞 JSON 是同一件事，只是把逃生口藏进了字符串——现在 `views.rs:41` 的 `body.to_string()` 就是这么来的，属于**必须改掉的既有债**）。
2. **不要在 `v3_bridge` 里拼 JSON 字符串冒充结构化字段**（例如把 `PermissionRequestDetails` 序列化进 `body`）。结构化正文要开结构体字段。
3. **不要为前端新增"V2 形状镜像"DTO**（如把 `TimelineEntry` 原样搬进 V3）。V3 的块模型是 `BlockView`，补齐方式是在它身上加事实字段，不是把 V2 枚举搬过来。
4. **不要用 base64 内联图片**。图片一律 `ContentHash` + `upload_blob/fetch_blob`。

---

## 6. 后端审计裁决与额外修复（2026-11-27）

### 已驳回 / 修正的提案

1. **驳回 H 组“指向 `qaqh-profile` 枚举”与“在 `config.rs` 新增模型 capabilities 目录视图”**：
   - `qaqh-profile` 依赖 `qaqh-types`（含 `serde_json` 与 `skip_serializing_if`），违反 `ADR-0042` 铁律 1；已改为在 `qaqh-core-vocab` 新增零 JSON 依赖的 `Wire` 与 `CredentialSource` 枚举。
   - `ConfigRequest::ListProviders` 已返回完整 `ProfilesDocument`（含 `ModelDef.capabilities`、`reasoning_levels`、`defaults`、`context_window`），`qaqh-gpui` 既有的 `store.rs:1760` 即直读 `ProfilesDocument`，无需在 `config.rs` 再造第二份目录视图。
2. **驳回 A 组同时保留 `summary` 与 `output` 两个重复字段**：
   - `v3_bridge` 原 `summary` 实质就是 `tool.output`，不存在独立的短摘要源。已直接用 `output: Option<ContentRef>` 替代 `summary`（单源，大输出走 Blob），并额外补齐前端 `TimelineTruncated` 与 `HandoffSealed` 必需的 `turn_id: Option<TurnId>`。
3. **修正 A 组 `ToolCallStatus` 与 E 组 `InteractionChoiceView.decision` 弱类型**：
   - `ToolCallStatus` 直接新增 `AwaitingInteraction` 与 `Cancelling`（不再靠 `Pending + interaction_id.is_some()` 隐式推导）。
   - `InteractionChoiceView.decision` 改为强类型枚举 `InteractionDecisionKind`（拒绝裸 `String`）。
4. **明确 `ToolArgumentsDelta` 维持增量 `delta` 语义**：
   - 保持 `delta: String` 增量推送与 `DeltaCoalescer` 同流合帧，仅补充 `tool_name` / `lines_added` / `lines_removed` / `target_files` / `args_complete` 最新快照字段（`args_complete == true` 时立即 flush）。

### 后端主动发现并一并修掉的隐藏阻塞 Bug

1. **`AnswerDelta` / `ReasoningDelta` 全量当增量发（重大渲染 Bug）**：`v3_bridge/projection/events.rs` 原直接透传 V2 `MessageDelta` 的累计前缀文本，前端 `push_str` 会拼成重复乱码；已改为按 `block_id` `strip_prefix` 产出真增量，非单调收缩 Fail-Closed。
2. **`SessionSummary.updated_at_ms` 1000 倍单位错位 + 缺 `custom_tools`/`created_at_ms`**：`service.list_sessions` 返回的 `meta.updated_at` 实为秒级且未带 `custom_tools`；`v3_bridge` 已直读 `sessions` 表毫秒字段与 `custom_tools`，同时在 `SessionSnapshot` 上补齐 `custom_tools` / `handoff_from` / `created_at_ms`。
3. **`CommandRequest::SendPrompt` 缺图片入口（前端迁移方案 §3 遗漏）**：仅靠 `attachments: Vec<ContentRef>` 无法区分文本附件与多模态图片且丢失 `mime_type`；已在 `SendPrompt` 新增 `images: Vec<ImageRefView>`，支持纯图无文本投递并在 `v3_bridge` 边界做魔数/MIME 校验。
4. **`ToolStateChanged` / `ToolFinished` 丢掉 `ToolDeclared` 参数与起始时间**：`v3_bridge` 原按状态事件重建空 `ToolCallView` 导致全量替换把 `args` 抹空；已改为事件投影缓存 + DB 回查合并，保证每次 `ToolCallUpdated` 均携带完整 `args` 与上下文。
5. **补齐 `TurnSummary`、`TurnStarted`、`ProviderRetrying`、`HandoffSealed` 与 `UsageInfo` 映射**：`TurnSummary` 补齐 `turn_id`/`input_id`/`phase`/`usage`；`TurnStarted` 补齐 `input_id`；`ProviderRetrying` 补齐 `max_attempts`；`HandoffSealed` 补齐 `sealed_session_id`/`turn_id`/`at_ms` 且强制 `Inline`；`map_usage` 按 `total_input`（`hit + miss` 优先）映射。
