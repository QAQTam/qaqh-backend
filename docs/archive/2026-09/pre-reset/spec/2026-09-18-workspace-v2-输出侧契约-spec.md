# workspace v2 输出侧契约（草案，2026-09-18）

> 发起方：web 端 todo 投影需求 + workspace 工具层长期演进。  
> 读者：`qaqh-workspace` 工具作者、`qaqh-runtime` 事件/服务层、`qaqh-client`、TUI / Web 前端。  
> 状态：**草案待评审**。本文先定契约与迁移顺序，不代表已实现。  
> 关联：
> - [`2026-09-15-workspace工具层契约重写-spec.md`](./2026-09-15-workspace工具层契约重写-spec.md)（TypedTool / ToolOutcome / ToolError）
> - [`2026-09-18-工具结果展示契约-v1-spec.md`](./2026-09-18-工具结果展示契约-v1-spec.md)（display / metrics / progress）
> - [`2026-09-18-web端todo数据投影契约-spec.md`](./2026-09-18-web端todo数据投影契约-spec.md)（todo 试点需求）

---

## 0. 一句话

workspace v1 解决了“工具卡如何展示一次调用结果”；workspace v2 要解决“工具执行后产生的**类型化输出、资源状态、事件、bootstrap、service 查询**如何从同一个事实源派生”。

Web 端 todo 的 `TodoView / TodoSummary / TodoChanged` 不是孤例，而是 v2 的第一个试点。后续 skills、dashboard、plan、process、subagent 都应遵循同一模式。

---

## 1. 现状判断

### 1.1 已经成立的部分

| 能力 | 现状 |
|---|---|
| 执行状态 | `ToolStatus { ok, error, partial, backgrounded, cancelled }` 已经统一 |
| 运行指标 | `ToolResult.metrics` / `TimelineTool.display.metrics` 已接线 |
| 工具卡展示 | `TimelineTool.display` 已由工具作者投影注册，覆盖主要内置工具 |
| progress | `progress_stream` + `progress_bytes_total` 已上 wire |
| diff | 文件类工具已有展示平面 diff |
| 错误 | `ToolError.code/message/retryable/hint` 已有基础 |

### 1.2 v2 要解决的问题

| 问题 | 说明 |
|---|---|
| 输出形态分裂 | 同一工具经常同时产生：模型文本、summary、`data` JSON、display 投影、service JSON。缺少单一 typed output。 |
| 无 output schema | `input_schema` 已有，但大多数工具没有 `output_schema`，前端只能手解 JSON。 |
| 状态投影分散 | todo、skills、dashboard、plan、process 各有不同风格：JSON 字符串、dashboard snapshot、工具结果、service endpoint。 |
| service 复用工具路径 | 例如 `todo.status` 直接拼工具输出形态，`mode` 硬编码；service 与工具结果没有独立投影边界。 |
| 事件覆盖不足 | todo 写盘后只有 dashboard snapshot；service 路径与部分资源没有 typed replaceable 事件。 |
| 长字段无界 | evidence、部分 MCP/LSP 输出、journal 内容缺少统一 truncation 纪律。 |
| TS 导出缺口 | workspace 内部模型大多未进 `qaqh/` 导出链，前端只能手抄。 |
| 命名冲突 | `qaqh_domain::TodoItem` 与 `qaqh_workspace::todo::TodoItem` 同名异义。 |

---

## 2. 目标与非目标

### 2.1 目标

1. **每个 typed 工具只有一个 canonical output**。
2. canonical output 派生：
   - 模型投影；
   - 工具卡 display；
   - 面板/资源 summary；
   - 资源事件；
   - service 查询响应；
   - TS 类型。
3. 状态类资源必须有：
   - 单调 revision；
   - typed summary；
   - bootstrap 初始值；
   - replaceable 变更事件。
4. 一次性工具不需要强行变成“资源”，但仍要有 typed output 和统一 truncation。
5. `ToolResult` 作为 v2 迁移期 wire 兼容层保留；新能力通过 additive 字段和事件进入，不做一次性全量 breaking。

### 2.2 非目标

- 不在 v2 一次性替换所有 `ToolHandler`。
- 不要求所有工具变 async。
- 不把所有输出都塞进模型上下文。
- 不把 MCP server 任意输出强行改成强类型业务对象；MCP 继续使用宽容 envelope。
- 不在 v2 第一批引入全文回取 CDN / object store；继续使用 `output_ref`。

---

## 3. 输出平面模型

workspace v2 的核心是“一次执行，四个投影”：

```text
TypedTool::run(args) -> ToolOutput<O>
                         |
                         +--> model projection      （LLM 可见，有界）
                         +--> display projection    （工具卡，已由 v1 契约约束）
                         +--> state projection      （资源 summary / bootstrap / event）
                         +--> service projection    （typed RPC 响应）
```

### 3.1 `ToolOutput<O>`

v2 输出不是裸 JSON，也不是字符串，而是 typed 值：

```rust
pub struct ToolOutput<O> {
    pub status: ToolStatus,
    pub payload: Option<O>,
    pub error: Option<ToolError>,
}

pub trait WorkspaceOutput: Serialize + JsonSchema {
    /// 稳定输出种类，例如 "todo.write" / "file.edit"。
    const KIND: &'static str;

    /// 单行、非 JSON、有界的模型/用户摘要。
    fn summary(&self) -> String;

    /// 工具卡展示投影；v1 的 ToolDisplay 类型继续复用。
    fn display(&self, args: &serde_json::Value) -> ToolDisplay;

    /// 状态资源工具返回需要发布的状态效果；一次性工具返回空。
    fn effects(&self) -> Vec<WorkspaceEffect> {
        Vec::new()
    }
}
```

约束：

1. `O` 必须派生 `Serialize / Deserialize / JsonSchema`。
2. `O` 的 JSON 字段名统一 `snake_case`。
3. `summary()` 禁止返回 JSON。
4. `display()` 不得重新解析模型文本；只能基于 canonical `O`。
5. `effects()` 只描述“业务上发生了什么”，不由工具直接发布 domain event。
6. runtime 负责把 `WorkspaceEffect` 转成 `ControlEvent` / Ringing event。

### 3.2 与 `ToolResult` 的兼容

v2 不立即替换 `ToolResult`。typed output 在迁移期写入：

```jsonc
// ToolResult.data
{
  "schema": "qaqh.workspace.v2",
  "kind": "todo.write",
  "payload": { "revision": 7, "summary": { "...": "..." } }
}
```

规则：

- `ToolResult.summary` 仍保留，但来自 `WorkspaceOutput::summary()`。
- `ToolResult.model` 由 output 的模型投影生成。
- `ToolResult.display` / timeline display 来自 `WorkspaceOutput::display()`。
- `ToolResult.data` 是可选 typed payload，不作为模型上下文。
- 旧 client 可以继续读取既有字段；新 client 优先消费 typed `data.payload`。

---

## 4. 通用输出纪律

### 4.1 命名与枚举

1. wire 字段统一 `snake_case`。
2. 状态必须使用 enum，不允许自由字符串：
   - todo：`pending | in_progress | completed | cancelled`
   - process：`running | completed | failed | timeout | cancelled`
   - subagent：同 process，不使用 `COMPLETED` 这类显示字符串。
3. 同一状态在存储、wire、service、事件、错误提示中只能有一个名字。

### 4.2 List envelope

列表型输出统一：

```rust
pub struct ListOutput<T> {
    pub items: Vec<T>,
    pub total: u32,
    pub returned: u32,
    pub next_cursor: Option<String>,
    pub truncated: bool,
}
```

规则：

- `truncated = true` 时必须说明是被 cursor 分页、条数上限还是字节预算截断。
- 可续读的列表必须提供 `next_cursor`；一次性截断可以只给 `truncated`。
- 列表项内不再嵌套“完整第二个列表”，需要资源关系时放引用 ID。

### 4.3 Bounded text

可变长文本统一：

```jsonc
{
  "text": "...",
  "original_chars": 8231,
  "truncated": true
}
```

首批字段上限：

| 字段类别 | 上限 |
|---|---|
| title | 沿用工具输入契约，例如 todo title 100 |
| description | 沿用工具输入契约，例如 todo description 200 |
| evidence | 2000 |
| 单条工具 summary | 沿用现有 512 |
| 模型投影 | 沿用现有 24k |
| MCP fallback text | 不双写，正文仍走 `output` |

如果存储中历史数据超限，wire 截断并标记；新写入尽量在写入口拒绝或收敛。

### 4.4 时间与 revision

- 时间统一 unix ms，字段名 `created_at` / `updated_at` / `completed_at`。
- 资源 revision 是 `u64`，只在成功提交后递增。
- revision 必须持久化或可从稳定事实源重建。
- 事件 consumer 必须能通过 revision 丢弃旧事件。

### 4.5 错误与取消

- 新工具不得用 `[ERROR]`、`json_err_string()`、错误 JSON 字符串表达错误。
- 可恢复错误进入 `ToolError`。
- 超时和取消分别是 `Timeout` / `Cancelled`，不得伪装成 success。
- fatal error 不进入普通模型 tool result。

---

## 5. 状态资源与事件框架

### 5.1 v2 资源清单

| 资源 | 是否首批事件 | summary 类型 | 说明 |
|---|---|---|---|
| todo | 是，todo 试点 | `TodoSummary` | web 已提出完整需求 |
| skills | 是 | `SkillsSummary` | 现有 `SkillsStatus` 可演进 |
| plan | 是 | `PlanSummary` | 与 todo 分离，先修 `TodoItem -> PlanReviewItem` |
| workspace activity | 是 | `WorkspaceActivitySummary` | files read / recent edits / code delta |
| process | 延后 | `ProcessSummary` | 仅当 web/TUI 需要后台进程面板 |
| subagent | 延后 | `SubagentSummary` | 现有状态字符串需 typed 化 |
| journal | 否 | 无 | 只读日志，不作为面板资源 |

### 5.2 `WorkspaceState` / bootstrap

`ControlState` 增加一个 additive 字段：

```rust
pub struct WorkspaceState {
    pub todo: Option<TodoSummary>,
    pub skills: Option<SkillsSummary>,
    pub plan: Option<PlanSummary>,
    pub activity: Option<WorkspaceActivitySummary>,
    pub process: Option<ProcessStateSummary>,
}

pub struct ControlState {
    // ...
    pub workspace: Option<WorkspaceState>,
}
```

迁移期保留现有 `dashboard_snapshot`；收敛完成后，dashboard 中与 todo 重复的 `tasks` 逐步退役。

### 5.3 事件

不建议每个资源各自散落一种无规律事件。建议统一外层：

```rust
pub enum WorkspaceChanged {
    Todo { revision: u64, summary: TodoSummary },
    Skills { revision: u64, status: SkillsSummary },
    Plan { revision: u64, summary: PlanSummary },
    Activity { revision: u64, summary: WorkspaceActivitySummary },
    Process { revision: u64, process: ProcessSummary },
}

pub enum ControlEvent {
    // ...
    WorkspaceChanged { seed: String, changed: WorkspaceChanged },
}
```

事件规则：

1. `WorkspaceChanged` 是 replaceable。
2. router 增加 `ReplaceableKey::Workspace(seed, kind)`，避免不同资源互相覆盖。
3. 事件必须在成功提交后发布。
4. service 写路径和工具写路径都必须经过同一个 commit/effect 出口。
5. 只读查询不产生事件。
6. 事件 summary 必须足够渲染轻面板；完整明细通过 typed service 查询获取。

### 5.4 workspace 到 runtime 的边界

`qaqh-workspace` 不依赖 `qaqh-domain::ControlEvent`。

推荐引入：

```rust
pub trait WorkspaceEventSink: Send + Sync {
    fn publish(&self, effect: WorkspaceEffect);
}
```

runtime 装配时把 sink 接到 `RingingHub`。工具/store 只产生 `WorkspaceEffect`，不直接发布 domain event。这样保持现有 R-4 分层约束。

---

## 6. 工具族输出侧设计

### 6.1 todo：第一个 v2 试点

存储保持 `TodoStore.items + status + evidence`，但投影重构。

```rust
pub enum TodoStatus {
    Pending,
    InProgress,
    Completed,
    Cancelled,
}

pub struct TodoView {
    pub id: String,
    pub title: BoundedText,
    pub description: BoundedText,
    pub status: TodoStatus,
    pub evidence: Option<BoundedText>,
    pub order: u32,
}

pub struct TodoCounts {
    pub pending: u32,
    pub in_progress: u32,
    pub completed: u32,
    pub cancelled: u32,
    pub total: u32,
}

pub struct TodoSummary {
    pub revision: u64,
    pub mode: TodoMode,
    pub current_id: Option<String>,
    pub current_title: Option<String>,
    pub counts: TodoCounts,
}
```

工具输出：

| 工具 | Output | 要点 |
|---|---|---|
| `todo_write` | `TodoWriteOutput` | `revision`, `summary`, `created: Vec<TodoRef>`, `updated: Vec<TodoRef>`, `not_found: Vec<String>` |
| `todo_update` | `TodoUpdateOutput` | `revision`, `updated`, `not_found`；不重复返回全量列表 |
| `todo_list` | `TodoListOutput` | `revision`, `summary`, `items: Vec<TodoView>`, cursor/truncation |
| service `todo.status` | `TodoStatusOutput` | `revision`, `summary`, `items`；不再返回 `idle/pending` 双键 |

事件：

```rust
WorkspaceChanged::Todo { revision, summary: TodoSummary }
```

规则：

- 状态词统一为存储 serde 名：`pending/in_progress/completed/cancelled`。
- `idle/complete/canceled` 等别名全部移除。
- `mode` 来自 `TodoStore.mode`，禁止硬编码。
- evidence 上限 2000，wire 超限必须带 `truncated`。
- service 与工具共用 `todo_summary_for()` / `todo_views_for()`，不再各自拼 JSON。
- `todo.cancel` 写路径必须经过统一 `write_store_for()`，否则会漏 revision/event。

### 6.2 skills

skills 已经有 `SkillsStatus`，但事件不完整、bootstrap 不稳定。

```rust
pub struct SkillsSummary {
    pub revision: u64,
    pub available: Vec<SkillInfo>,
    pub active: Vec<String>,
    pub catalog_revision: Option<String>,
    pub operation_revision: Option<u64>,
    pub diagnostics: Vec<Diagnostic>,
}
```

工具输出：

| 动作 | Output |
|---|---|
| activate | `SkillActivateOutput { revision, name, resources }` |
| list | `SkillListOutput { revision, skills, diagnostics, truncated }` |
| resource | `SkillResourceOutput { name, path, content: BoundedText }` |
| validate | `SkillValidateOutput { name, valid, diagnostics }` |

规则：

- activate / reload / operation 后发布 `WorkspaceChanged::Skills`。
- resource 读取是只读，不发事件。
- diagnostics 必须有 severity/source/message，不能是自由字符串列表。
- bootstrap 的 `WorkspaceState.skills` 与事件使用同一个 projector。

### 6.3 plan review

Plan 不是 todo store，不应继续借用 `TodoItem`。

```rust
pub struct PlanReviewItem {
    pub id: String,
    pub title: String,
    pub description: String,
    pub complexity: PlanComplexity,
}

pub struct PlanSummary {
    pub revision: u64,
    pub status: PlanStatus,
    pub pending_interaction_id: Option<String>,
    pub item_count: u32,
}

pub struct PlanView {
    pub id: String,
    pub title: String,
    pub status: PlanItemStatus,
    pub comment: Option<String>,
}
```

规则：

- `qaqh_domain::TodoItem` 改名为 `PlanReviewItem`。
- plan review 请求、裁决、PLAN.md 修改使用同一 revision。
- service `plan.action` 成功后发布 `WorkspaceChanged::Plan`。
- Markdown 文件仍是持久化形态，但 wire 不再手解 markdown 后裸 JSON。

### 6.4 dashboard / 文件活动

现有 `DashboardSnapshot` 已经接近资源投影，但有两条问题：

1. `DashboardTask.status` 是自由字符串且使用 `idle`。
2. runtime 内存视图和 service `session.dashboard` 各拼一份。

v2 演进：

```rust
pub struct WorkspaceActivitySummary {
    pub revision: u64,
    pub documents: Vec<DocumentRef>,
    pub recent_edits: Vec<FileEditRef>,
    pub todo: TodoSummary,
}

pub struct FileEditRef {
    pub path: String,
    pub updated_at: u64,
    pub lines_added: u32,
    pub lines_removed: u32,
}
```

工具输出：

| 工具 | Output | 状态效果 |
|---|---|---|
| `read` | `ReadOutput { files: Vec<FileReadView> }` | 更新 activity documents |
| `write` | `FileWriteOutput { path, bytes, lines_added, lines_removed, created }` | activity / code delta |
| `edit` | `FileEditOutput { path, hunks_applied, lines_added, lines_removed, diff? }` | activity / code delta |
| `apply_patch` | `PatchApplyOutput { applied, failed, files }` | activity / code delta |
| `delete` | `FileDeleteOutput { path, trash_path }` | activity / code delta |
| `copy_range` | `CopyRangeOutput { source, target, range, mode, lines }` | activity / code delta |
| `confirm_apply` | `ConfirmApplyOutput { pending_id, applied_files }` | activity / code delta |

规则：

- 文件类工具的成功输出必须包含足够的 code delta 信息，或由 runtime 在 commit 边界计算。
- `DashboardTask.status` 改为 `TodoStatus`，消灭 `idle`。
- `DashboardSnapshot` 短期兼容，长期向 `WorkspaceActivitySummary` 收敛。
- `exec`、`git` 这类无单一路径修改可以暂时不进 per-file activity，但要保证 audit / timeline 不缺失。

### 6.5 process / subagent

首批不强制，但如果 web/TUI 要做后台面板，必须 typed 化：

```rust
pub enum ProcessState {
    Running,
    Completed,
    Failed,
    Timeout,
    Cancelled,
}

pub struct ProcessSummary {
    pub id: u32,
    pub kind: ProcessKind,
    pub name: String,
    pub state: ProcessState,
    pub exit_code: Option<i32>,
    pub started_at: u64,
    pub updated_at: u64,
}
```

规则：

- `SubagentStatus.state` 不得继续使用 `"COMPLETED"` 展示字符串。
- process 事件按 `process_id` replaceable。
- bootstrap 只带 active/recent process summary，不带 output 全文。

### 6.6 ask / interaction

ask 已经有 `InteractionRequested` / `InteractionResolved`，不需要再造状态资源。

v2 输出：

```rust
pub struct AskOutput {
    pub interaction_id: String,
    pub questions: Vec<AskQuestion>,
}
```

规则：

- interaction 生命周期继续走 control event。
- 工具输出只包含 canonical 请求信息，不复制完整交互状态。
- bootstrap 的 `pending_interaction` 继续作为轻量状态。

### 6.7 一次性查询 / 执行工具

这些工具不进入 `WorkspaceState`，但必须 typed output。

#### exec

```rust
pub struct ExecOutput {
    pub status: ExecStatus,
    pub exit_code: Option<i32>,
    pub output: BoundedText,
    pub process_id: Option<u32>,
    pub timed_out: bool,
    pub cancelled: bool,
    pub started_at: u64,
    pub duration_ms: Option<u64>,
}
```

要点：

- summary 不再是 JSON。
- progress 继续使用现有 `progress_stream / progress_bytes_total`。
- backgrounded / timeout / cancelled 状态不得伪造成 exit code。

#### read

```rust
pub struct FileReadView {
    pub path: String,
    pub hash: String,
    pub start_line: Option<u32>,
    pub end_line: Option<u32>,
    pub total_lines: u32,
    pub content: BoundedText,
    pub not_modified: bool,
}
```

规则：

- 模型需要行号文本，但 canonical output 保存结构化 range/hash。
- 二进制文件返回明确错误，不猜测文本。
- 批量 read 受条数和总字符预算限制。

#### glob

```rust
pub struct GlobOutput {
    pub root: String,
    pub pattern: String,
    pub matches: ListOutput<String>,
}
```

#### grep

```rust
pub struct GrepMatch {
    pub path: String,
    pub line: u32,
    pub content: BoundedText,
    pub is_context: bool,
}

pub struct GrepOutput {
    pub pattern: String,
    pub path: Option<String>,
    pub matches: ListOutput<GrepMatch>,
}
```

#### web_fetch

```rust
pub struct WebFetchOutput {
    pub url: String,
    pub content: BoundedText,
    pub media_type: Option<String>,
    pub saved_path: Option<String>,
}
```

规则：

- 正文必须 bounded。
- 错误使用 `ToolError`，不再返回错误 JSON 字符串。
- 如果保存到本地文件，`saved_path` 是引用，不在 summary 展开全文。

### 6.8 journal / audit

journal 是只读历史资源，不进入 bootstrap summary。

```rust
pub struct JournalEntryView {
    pub seq: u64,
    pub ts: u64,
    pub kind: JournalKind,
    pub file: Option<String>,
    pub summary: String,
}

pub struct JournalOutput {
    pub action: JournalAction,
    pub entries: ListOutput<JournalEntryView>,
}
```

规则：

- 不把所有 before/after 全文塞进 wire。
- export/patch 场景可以使用单独 output，且必须有 truncation。
- audit 数据不进模型面。

### 6.9 read_image

```rust
pub struct ReadImageOutput {
    pub path: String,
    pub images: Vec<ImageRef>,
    pub note: Option<BoundedText>,
}
```

规则：

- base64 图片继续走 `ToolResult.images`，不进模型文本。
- canonical output 只保存引用和元数据。

### 6.10 dynamic MCP / LSP

动态工具无法要求 server 提供强类型业务 output，v2 使用宽容 envelope：

```rust
pub struct DynamicToolOutput {
    pub provider: DynamicProvider,
    pub server: Option<String>,
    pub tool: String,
    pub content: BoundedText,
    pub structured: Option<serde_json::Value>,
}
```

规则：

- `structured` 只透传 server 声明或可识别的结构化内容。
- 无专属投影的 MCP per-server 工具继续使用 current fallback：`Other { label: full name }` + `Body::None`。
- 不得把 server 任意 JSON 当作 workspace 资源状态写入 bootstrap。

---

## 7. service 查询层

v2 service response 不再手拼 JSON 字符串。

| 当前/未来查询 | v2 response |
|---|---|
| `todo.status` | `TodoStatusOutput` |
| `todo.list` | `TodoListOutput` |
| `skills.*` 查询 | `SkillListOutput` / `SkillsSummary` |
| `session.dashboard` | `WorkspaceActivitySummary` 或兼容 wrapper |
| `plan.read` | `PlanListOutput` |
| `plan.context_stats` | `ContextStatsOutput` |
| `stats.token_usage` | `TokenUsageOutput` |
| `journal` | `JournalOutput` |

规则：

1. service 查询调用 projector，不调用工具 handler。
2. service 写操作调用和工具相同的 commit 入口。
3. service 的 typed response 与工具 `data.payload` 共享类型。
4. 错误统一 `ServiceError { code, message, retryable, hint }`。
5. service 不接收 `String` 再由前端 parse。

---

## 8. 模型投影策略

typed output 并不等于把完整 JSON 发给模型。

| 输出类别 | 模型投影策略 |
|---|---|
| mutation | 单行结果 + 必要 ID / revision / continuation |
| read | 保留模型需要的行号文本 / hash / range，超出即 truncated |
| search | 紧凑 match list，避免重复 path |
| list | 有界表格或紧凑列表，不用 JSON dump |
| error | human message + 可执行 hint；details 只进展示面 |
| state summary | agent 只在显式工具/上下文需要时读取，不自动广播全文 |

规则：

1. summary 不得由模型投影首行倒推。
2. 模型投影必须从 typed output 生成。
3. 所有 continuation / next action 必须是工具可以再次调用的参数形态。
4. 工具卡 display 与模型投影都来自同一个 output。

---

## 9. 迁移阶段

### Phase W0：契约与类型底座

- 引入 `WorkspaceOutput` / `ToolOutput` / `WorkspaceEffect`。
- 定义 `ListOutput`、`BoundedText`、revision 规则。
- qaqh-types 增加 workspace v2 输出类型和 TS 导出。
- service error 统一 typed。
- runtime 增加 `WorkspaceEventSink`。

### Phase W1：todo 试点

- 实现 `TodoView / TodoSummary / TodoCounts / TodoStatus`。
- `todo_write / todo_update / todo_list` typed 化。
- service `todo.status / todo.set / todo.cancel` typed 化。
- 统一写入口，补 `TodoChanged`。
- bootstrap `ControlState.workspace.todo`。
- dashboard 的 todo status 同步 typed。

### Phase W2：skills + workspace activity

- `SkillsSummary` / `SkillsChanged` / bootstrap skills。
- dashboard/文件活动工具统一 activity summary。
- `DashboardTask.status` typed。
- `write/edit/delete/apply_patch/copy_range/confirm_apply` 的 code delta 与 activity 投影统一。

### Phase W3：plan / process / subagent

- `TodoItem -> PlanReviewItem`。
- plan read/action typed。
- process/subagent 状态 typed。
- 按 UI 需求决定是否进 bootstrap。

### Phase W4：一次性工具全面 typed

- exec/read/glob/grep/web_fetch/journal/read_image。
- 逐步删除 `[ERROR]` / JSON 字符串错误路径。
- 新工具禁止绕过 `WorkspaceOutput`。

### Phase W5：清理

- service JSON-string projection 移除。
- dashboard 中重复的 todo 字段退役。
- 旧 `ToolDisplayFn` 注册表可被 typed output 的 `display()` 替代。
- legacy adapter 只保留给尚未迁移工具。

---

## 10. 验收标准

### 10.1 通用

1. 新 typed 工具的 output 有 JSON Schema。
2. `data.payload` 能 round-trip，不丢字段。
3. summary 永远是单行非 JSON。
4. display、model projection、service projection、event 来自同一 output 或同一 projector。
5. 长 text 都有 truncation 标记。
6. TS 导出覆盖 v2 输出类型。
7. 新工具不得使用 `[ERROR]` / `json_err_string()`。
8. 状态枚举不得出现自由字符串。

### 10.2 状态资源

1. 成功写路径 revision 单调递增。
2. 同一次写盘只发布一个事件。
3. service 写路径与工具写路径都产生事件。
4. bootstrap summary、query summary、event summary 一致。
5. replaceable key 按 seed + resource kind 隔离。
6. 旧 revision 事件可被 consumer 丢弃。

### 10.3 todo 专项

1. 四状态固定为 `pending/in_progress/completed/cancelled`。
2. 全仓 todo/dashboard 路径不再出现 `idle` 键。
3. evidence 超 2000 wire 截断并标记。
4. `todo.status` 与 `TodoChanged` 的 counts/current 一致。
5. `todo.cancel` 也走统一写入口。

---

## 11. 决策记录

| ID | 决策 | 理由 |
|---|---|---|
| D1 | typed output 类型放 `qaqh-types` | workspace/domain/client 都可依赖，避免反向依赖 |
| D2 | `ToolResult` 保留为迁移 wire | 避免 v2 一次性 breaking |
| D3 | 工具不直接发 domain event | 保持 workspace -> runtime 分层 |
| D4 | 事件统一 `WorkspaceChanged` | 避免每个资源散落无规律事件 |
| D5 | service 与工具共用 projector，不共用 handler | 查询是读模型，执行是写模型 |
| D6 | MCP 保持宽容 envelope | server 输出不受控，不能强行变成业务资源 |
| D7 | 状态资源必须有 revision | web/TUI 才能做事件去重 |
| D8 | 状态名必须 enum | 避免再次出现 `idle/pending` 分裂 |
| D9 | summary 禁止 JSON | 沿用展示契约 H1 |
| D10 | 长文本必须有 truncation | 事件和 bootstrap 都不能无界增长 |

---

## 12. 开放问题

| ID | 问题 | 默认建议 |
|---|---|---|
| Q1 | `WorkspaceState` 是否首批包含 process | 否，先 todo/skills/plan/activity |
| Q2 | `DashboardSnapshot` 何时退役 | 等 `WorkspaceActivitySummary` 双端切换后 |
| Q3 | revision 放 store 内还是 sidecar | 优先 store 内，用 serde default 迁移旧文件 |
| Q4 | evidence 超限是拒绝还是截断 | 新写入拒绝；历史读取截断标记 |
| Q5 | `ToolResult.data.payload` 是否所有工具都发 | 状态资源和结构化查询发；大文本工具可省略 |
| Q6 | display 是否立即从 `ToolDisplayFn` 切到 output method | 不急，等 typed output 稳定后统一切 |

---

## 13. 结论

workspace v2 的输出侧不是“把 JSON 变漂亮”，而是把工具输出升级为契约：

```text
one typed output
  -> one model projection
  -> one display projection
  -> one resource summary
  -> one event
  -> one service query shape
  -> one TS type
```

todo 是第一个试点。成功后，skills、plan、dashboard、process、subagent 都按同一套输出侧纪律迁移；exec/read/grep 等一次性工具则只要求 typed output 与 truncation，不强行进入状态资源体系。
