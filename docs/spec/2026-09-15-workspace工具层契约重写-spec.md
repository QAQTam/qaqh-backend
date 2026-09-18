# Workspace 工具层契约重写（Tool SDK v1） - 规格（2026-09-15）

> 状态：**草案待评审**。
>
> 本文定义 `qaqh-workspace` 工具层的目标契约。它不描述某个具体工具的业务逻辑，
> 而是规定所有工具必须遵守的注册、参数、错误、输出、进度和运行时边界。
>
> 落地过程如与本文冲突，先修改本文并完成评审，再修改代码。
>
> **2026-09-18 修订（v1.1，评审阻塞项修复）**：展示投影与进度的 wire 细节以
> [`2026-09-18-工具结果展示契约-v1-spec.md`](./2026-09-18-工具结果展示契约-v1-spec.md)
> 为准；本文的 §5.1 / §6 / §7 / §8 / §9 / §11 已同步修订，两者冲突时以 09-18 联合契约为准。
> 修订点：可恢复错误的承载位、`ToolError.code`、展示投影的 args 访问、超时/取消执行点、
> schema 输出、动态工具命名。

## 0. 元信息

| 项 | 值 |
|---|---|
| 规格日期 | 2026-09-15（UTC+8） |
| 范围 | `qaqh-workspace` 工具 SDK、`ToolManager`、内置工具、动态 MCP/LSP 工具适配层 |
| 相关边界 | `qaqh-types` 的 wire 结果、`qaqh-runtime` 的 actor 与 timeline 适配 |
| 当前状态 | 现有工具可运行，但工具契约、错误 taxonomy、输出投影和 schema 来源不统一 |
| 一句话目标 | 建立一个稳定、可验证、可逐步迁移的工具 SDK，使新增工具不能再依赖字符串约定和隐式行为 |

## 1. 为什么重写

当前工具层的主要问题不是单个工具实现差，而是缺少正式 SDK 契约：

1. `ToolHandler` 使用 `fn(ToolCallCtx) -> ToolResult`，参数和输出均未类型化。
2. 19 个内置工具手写 JSON Schema，schema 与 Rust 参数类型没有编译期关系。
3. 错误 code 是开放字符串，且 `ToolResult::error`、`error_data`、`error_with`、
   `json_err`、`json_err_string` 和 `Err(String)` 多种表达并存。
4. `[ERROR]` / `[PARTIAL]` 文本前缀仍参与旧式 handler 的失败判定。
5. `ToolResult` 同时承担模型输出、展示摘要、错误、diff、图片、外置引用和统计元数据。
6. `ToolExecMeta` 生成后基本没有进入 runtime、timeline 或 client 反馈链路。
7. `tool_side_fold` 按工具名字符串硬编码输出预算，工具无法声明自己的预算。
8. 静态 `ToolHandler` 与动态 `DynamicTool` 是两套平行结构，执行和投影语义容易继续分叉。

本规格不要求对齐某一家厂商。参考 Grok 的工具 SDK 分层和 Codex 的
`RespondToModel` / `Fatal` 控制流边界，但只采纳对当前项目有直接价值的原则。

## 2. 范围与非目标

### 2.1 本规格覆盖

- 工具描述、参数 schema 和注册表。
- 工具执行 trait 与类型擦除适配器。
- 工具错误 taxonomy、错误投影和 fatal 控制流。
- 工具输出的模型投影、展示投影和运行元数据。
- 工具进度和通知的通用表示。
- 静态工具与动态 MCP/LSP 工具的统一描述契约。
- 从现有实现迁移到 Tool SDK v1 的兼容规则。

### 2.2 本规格不覆盖

- 不重写权限准入链路。`admit` 和 `AuthorizedToolCall` 保留。
- 不重写 Ringing、SSE、timeline 存储或 client 传输协议。
- 不引入远程工具服务。工具继续在 daemon actor 进程内执行。
- 不要求所有工具立即改为异步执行。v1 允许同步执行器。
- 不改变工具白名单、Plan 模式和子代理沙箱的既有语义。
- 不把模型供应商协议细节放进工具 SDK。

## 3. 核心原则

1. **单一描述源**：模型可见名称、描述、参数 schema、能力类别、风险和超时来自同一个 descriptor。
2. **错误是类型，不是前缀**：工具失败必须由结构化错误表达，不能靠 `[ERROR]` 判断。
3. **模型错误与 fatal 分离**：可恢复失败回给模型；内部 fatal 终止当前执行并上抛。
4. **投影分离**：结构化输出、模型投影、展示投影和运行元数据由不同字段承载。
5. **注册可验证**：注册时即可发现重名、缺 schema、非法名称和元数据冲突。
6. **迁移不制造新分叉**：新旧实现可以暂时共存，但新工具只能使用新 API。
7. **wire 兼容与内部重构分离**：内部 SDK 可以演进，client 线协议通过适配层保持稳定。
8. **资源保护不等于模型折叠**：工具内部 IO 限额与给模型看多少是两个独立决策。

## 4. 目标分层

```text
具体工具实现
  |
  v
Tool SDK v1
  - ToolDescriptor
  - TypedTool / ErasedTool
  - ToolError / FatalToolError
  - ToolOutcome / ToolOutput
  - ToolProgress
  |
  v
ToolManager
  - 注册、查找、allowlist、inflight、统计
  |
  v
Authorized execution
  - admit -> AuthorizedToolCall -> execute_authorized
  |
  v
Runtime adapters
  - qaqh-types::ToolResult
  - Ringing ToolFinished
  - TimelineTool
  - audit.csv
```

`qaqh-workspace` 在 v1 内先以 `tool_api` 子模块承载 SDK，不立即拆新 crate。
等契约稳定后，再评估是否提取为独立 leaf crate。

## 5. 工具描述与注册

### 5.1 ToolDescriptor

每个工具必须有且只有一个 descriptor：

```rust
pub struct ToolDescriptor {
    /// 规范名（模型面 / wire）：`^[a-z][a-z0-9_]*$`，长度 ≤ 64。
    pub name: ToolName,
    /// 人类可读原始名。动态工具（MCP/LSP）被规范化后，用它保留上游原名
    /// （如 `get-issue`）；仅供展示，不参与查找。
    pub display_name: Option<String>,
    pub description: String,
    pub input_schema: JsonSchema,
    /// 输出投影的 JSON Schema（`schemars` 生成；动态工具用上游 raw schema）。
    pub output_schema: JsonSchema,
    pub category: ToolCategory,
    pub risk: ToolRisk,
    pub default_timeout: Duration,
    pub exposure: ToolExposure,
    pub source: ToolSource,
    pub output_budget: OutputBudget,
}
```

约束：

- `name` 必须唯一。内置工具名匹配 `^[a-z][a-z0-9_]*$` 且总长 ≤ 64。
- 动态工具（MCP/LSP）名必须由上游名**规范化**得到：小写 → 非 `[a-z0-9_]`
  字符替换为 `_` → 合并连续 `_` → 去首尾 `_`；规范化后与内置名或已注册动态名
  碰撞时注册失败（不得静默覆盖）。上游原名必须写入 `display_name`。
- `description` 不得为空。
- `input_schema` / `output_schema` 必须是合法 JSON Schema object。
- `category` 决定权限风险归类（现有 `classify_risk`）；`risk` 继续参与路径范围
  fail-closed 判定。两者共同构成权限输入，`category` 不单独承担全部决策。
- `source` 区分 `Builtin`、`Mcp`、`Lsp`、`Extension`。
- `output_budget` 由工具声明，不允许由 `tool_side_fold` 再按名称猜。

### 5.2 ToolExposure

v1 至少定义：

```rust
pub enum ToolExposure {
    Direct,
    Deferred,
    Hidden,
    Internal,
}
```

- `Direct`：进入模型首轮工具清单。
- `Deferred`：注册但不在首轮清单，预留给后续工具搜索。
- `Hidden`：可由宿主调用，不进入模型面。
- `Internal`：仅供运行时或内部编排使用，不进入 client 工具清单。

v1 可以只实现 `Direct` / `Hidden` / `Internal`，但类型必须预留。

### 5.3 注册规则

注册必须经过统一校验：

```text
ToolDescriptor
  -> validate()
  -> check_collision()
  -> insert()
```

碰撞或非法 descriptor 必须返回错误，不得静默覆盖。

动态 MCP/LSP 工具与内置工具共用 descriptor、错误和输出契约，但仍允许保留
独立的动态刷新存储。

## 6. 工具执行契约

### 6.1 TypedTool

新工具优先实现 typed 接口：

```rust
pub trait TypedTool: Send + Sync {
    type Args: DeserializeOwned + JsonSchema;
    type Output: ToolOutput + JsonSchema;

    fn descriptor(&self) -> ToolDescriptor;

    fn run(
        &self,
        ctx: &ToolCallContext,
        args: Self::Args,
    ) -> Result<Self::Output, ToolExecutionError>;
}
```

v1 不要求 async。后续若确有异步工具，再增加 `AsyncTypedTool`，不得让整个
SDK 先被 async 细节绑死。

### 6.2 ErasedTool

运行时通过类型擦除适配器统一调用：

```rust
pub trait ErasedTool: Send + Sync {
    fn descriptor(&self) -> ToolDescriptor;

    fn execute(
        &self,
        ctx: ToolCallContext,
        args: serde_json::Value,
    ) -> Result<ToolOutcome, FatalToolError>;
}
```

适配器负责：

1. 按 typed `Args` 反序列化参数。
2. 将参数错误转换为可恢复 `ToolError`。
3. 调用 `TypedTool::run`。
4. 将 typed output 投影为 `ToolOutcome`。
5. 将 fatal error 原样上抛。

### 6.3 ToolCallContext

`ToolCallContext` 是显式执行上下文，至少包含：

```rust
pub struct ToolCallContext {
    pub call_id: String,
    pub session_id: String,
    pub workspace_root: PathBuf,
    pub mode: AgentMode,
    pub permission_level: PermissionLevel,
    pub timeout: Duration,
    pub cancellation: CancellationToken,
    pub progress: Option<ProgressSink>,
    pub source: ToolCallSource,
}
```

现有 `ToolCallCtx` 在迁移期作为兼容字段保留，但新 API 不得继续依赖线程局部状态
隐式传递调用身份。

### 6.4 执行边界（超时与取消）

- **生效超时** = 调用方显式 timeout（若有）覆盖 `descriptor.default_timeout`；
  `ToolCallContext.timeout` 在构造时定稿，工具不得改写。
- v1 执行器是同步的，超时**不保证抢占**：执行器在调用前后测量墙钟，超时后至少要把
  结果归类为 `ToolError { kind: Timeout, retryable: true }`。需要及时中断的工具
  必须自行轮询 `ctx.cancellation`（IO 边界、循环批次处至少各一次），并在观察到取消后
  有界时间内返回。
- 执行器不得混淆超时与取消：超时 → `Timeout`，用户/系统取消 → `Cancelled`；
  两者都是可恢复终态，进入 `ToolOutcome`，不升级为 fatal。
- 未来的异步/可抢占执行器（`AsyncTypedTool`）只增加抢占能力，不改变本节的错误映射。

## 7. 错误模型

### 7.1 可恢复 ToolError

```rust
pub struct ToolError {
    pub kind: ToolErrorKind,
    /// 稳定、机器可读的离线 code。内置 kind 由 kind 推导；`Custom` 必填且必须
    /// 带命名空间（`<namespace>.<snake_case>`）。
    pub code: ToolErrorCode,
    pub detail: String,
    pub retryable: bool,
    pub hint: Option<String>,
    pub details: Option<serde_json::Value>,
    source: Option<anyhow::Error>,
}

/// `^[a-z][a-z0-9_]*(\.[a-z][a-z0-9_]*)+$`，如 `mcp.rate_limited`、
/// `edit.hash_mismatch`。构造期校验，非法 code 直接返回构造错误（不 panic）。
pub struct ToolErrorCode(String);

pub enum ToolErrorKind {
    InvalidArguments,
    NotFound,
    Conflict,
    PermissionDenied,
    Unauthorized,
    Timeout,
    Cancelled,
    Network,
    Execution,
    Unavailable,
    Custom,
}
```

语义：

- `kind`：机器可读的封闭分类。
- `detail`：模型可见、具体、可操作的说明。
- `retryable`：是否允许模型或宿主重试。
- `hint`：可选修正建议。
- `details`：字段级校验、冲突资源、候选位置等结构化数据。
- `source`：仅开发诊断，不序列化，不发给模型。
- `Custom`：仅用于无法归入既有分类的工具特有错误；`code` 必须是命名空间形态，使模型侧 wire 的 `error_code` 不再依赖 `details` 承载机器码。

### 7.2 FatalToolError

```rust
pub struct FatalToolError {
    pub code: String,
    pub message: String,
    source: Option<anyhow::Error>,
}
```

Fatal 用于：

- 工具注册表损坏。
- 授权凭证与执行上下文不一致。
- 运行时状态损坏。
- 无法继续安全执行的内部 invariant 失败。

Fatal 不得作为普通 `ToolResult::error` 回给模型。

### 7.3 ToolExecutionError

```rust
pub enum ToolExecutionError {
    Recoverable(ToolError),
    Fatal(FatalToolError),
}
```

执行层必须显式匹配二者：

- `Recoverable` 写入 `ToolOutcome.error`（§8.2）并反馈模型。
- `Fatal` 上抛给 actor/runtime，由 runtime 决定终止、重试或上报；上抛前必须把对应
  in-flight timeline 块 seal 为 `Failed`，`failure.code` 取 fatal code（§11），
  不得让 client 停留在 Running 卡片。
- 不变量：`Ok/Backgrounded ⇒ error.is_none()`；`Partial/Cancelled/Error ⇒ error.is_some()`。

### 7.4 禁止事项

- 禁止通过 `[ERROR]` / `[PARTIAL]` 前缀判断失败。
- 禁止新增自由字符串 code。
- 禁止只把错误写进 `data` 而 `error` 为空。
- 禁止工具作者手工拼接模型 XML envelope。
- 禁止把 `anyhow::Error` 原文直接作为模型消息。

## 8. 输出模型

### 8.1 ToolOutput

```rust
pub trait ToolOutput: Serialize {
    /// 模型投影。为空时由适配器按下方规则生成。
    fn model_blocks(&self) -> Vec<ToolContentBlock> {
        Vec::new()
    }

    /// 单行人类可读摘要（展示与模型提示共用）。不得是 JSON（H1）。
    fn summary(&self) -> Option<String> {
        None
    }

    /// 展示投影。`args` 是**已通过 typed 校验的原始参数值**，供 header 提取
    /// 真相字段（path / command / query）；实现不得读取线程局部，也不得重新
    /// 解析未经校验的输入。
    fn display(&self, args: &serde_json::Value) -> ToolDisplay {
        ToolDisplay::default()
    }
}
```

默认行为（修订）：

1. `model_blocks` 为空时：文本输出序列化为 text block；结构化输出**必须显式实现
   `model_blocks`**，否则适配器只投影 `summary` + 有界 JSON（默认 4 KiB，截断需标注），
   并剥离内部 `source`、凭据与完整参数（呼应 §12 第 9 条）。
2. `summary` 为空时，展示层使用 §8.4 的兜底（`"{name} · {state}"`），
   **不得**取模型投影首行；模型投影首行是 JSON 时一律走兜底。
3. `display` 默认无 diff、`header = None`、`body = None`；timeline 负责有界兜底，
   不得产生空白卡片。
4. 展示结构（字段全集与不变量）以
   [`2026-09-18-工具结果展示契约-v1-spec.md`](./2026-09-18-工具结果展示契约-v1-spec.md)
   §3–§4 为准；本节只规定投影职责。

### 8.2 ToolOutcome

```rust
pub struct ToolOutcome {
    pub status: ToolStatus,
    /// canonical 输出：宿主 / 审计 / 后续工具可消费；不保证直接发给模型。
    pub output: ToolOutputValue,
    /// 可恢复错误。与 status 的不变式见 §7.3。
    pub error: Option<ToolError>,
    pub model: ToolModelProjection,
    pub display: ToolDisplay,
    pub metrics: ToolExecutionMetrics,
}

pub enum ToolOutputValue {
    Empty,
    Text(String),
    Json(serde_json::Value),
    ContentRef(ContentRef),
}
```

投影职责：

| 投影 | 消费者 | 禁止内容 | 进 wire |
|---|---|---|---|
| `output` | 宿主、审计、后续工具 | 不保证直接发给模型 | 仅保留现有 `TimelineTool.output`（迁移期） |
| `error` | 模型 + timeline failure | fatal、内部 source | failure.code/message（及现有 `ToolResult.error`） |
| `model` | provider tool result | 不包含内部 source | 否 |
| `display` | timeline、TUI、client | 不参与模型决策 | 是（新增 `TimelineTool.display`，可选） |
| `metrics` | telemetry、审计、TUI | 敏感参数正文 | 是（`ToolResult` 扩展 + `display.metrics`） |

### 8.3 ToolExecutionMetrics

```rust
pub struct ToolExecutionMetrics {
    pub elapsed: Duration,
    /// 本次调用产出的展示文本字节数（UTF-8，截断后）。
    pub output_bytes: u64,
    pub retry_count: u32,
    /// 别名/MCP 解析后的实际工具名；无别名时 = descriptor.name。
    pub effective_tool_name: Option<String>,
    /// 调用来源是否为用户直接发起（宿主侧来源，非工具自报）。
    pub user_initiated: bool,
}
```

运行信息必须通过 outcome 进入 runtime 适配层，不得再生成后丢弃；wire 字段与来源
映射见 09-18 联合契约 §3.4。

### 8.4 ToolDisplay

展示结构（`summary` / `diff` / `header` / `body` / `metrics`）与全部不变量由
[`2026-09-18-工具结果展示契约-v1-spec.md`](./2026-09-18-工具结果展示契约-v1-spec.md)
§3–§4 定义，并作为唯一事实源；本节不再重复字段定义，只保留职责：

- `summary`：canonical 单行摘要，禁止 JSON。
- `diff`：文件变更统一 diff。
- `header`：由工具作者声明（`display(&args)`），timeline 不得从 `args_json` 反推。
- `body`：类型化正文；为 `None` 时 timeline 必须给有界兜底。
- `metrics`：来自 §8.3，不得二次生成。

## 9. 进度模型

### 9.1 ToolProgress

```rust
pub enum ToolProgress {
    Text {
        stream: ProgressStream,
        text: String,
    },
    Phase {
        phase: String,
        message: String,
    },
    Content {
        blocks: Vec<ToolContentBlock>,
    },
    Custom {
        subkind: String,
        payload: serde_json::Value,
    },
}
```

### 9.2 规则

- 进度是临时展示数据，不是最终结果。
- 每个进度帧必须有界。
- 接收端可以丢弃旧进度，但最终 outcome 不得依赖进度帧才能重建。
- `exec` 的 stdout/stderr 通过 `ProgressStream` 表达（SDK 内部保留流标识）。
- 进度帧必须携带累计字节数（截断前的观测总量）；wire 字段为
  `progress_stream` / `progress_bytes_total`，语义见 09-18 联合契约 §5。
- 非 exec 工具不得再创造私有 progress JSON 协议；自定义子类型必须先在
  descriptor/文档登记 schema，并保证客户端可忽略。

## 10. 执行流水线

目标流水线：

```text
ToolInvocation
  -> admit
  -> AuthorizedToolCall
  -> resolve descriptor
  -> build ToolCallContext
  -> validate args against typed Args
  -> TypedTool::run
  -> ToolOutput projection
  -> ToolOutcome
  -> runtime adapter
       -> qaqh_types::ToolResult
       -> ToolFinished
       -> TimelineTool
       -> audit
```

权限、资源绑定和 workspace 复核继续由现有授权链路负责。

## 11. Wire 兼容

Tool SDK v1 内部使用新类型，对外适配到现有 wire 结构；展示面的唯一契约是
[`2026-09-18-工具结果展示契约-v1-spec.md`](./2026-09-18-工具结果展示契约-v1-spec.md)。

| 内部 | 适配目标 | 规则 |
|---|---|---|
| `ToolStatus` | `qaqh_types::ToolStatus` / `TimelineToolState` | 保持现有五态映射 |
| `ToolError` | `qaqh_types::ToolError` | `code` 直接落 wire，`detail`→message；`details` 不进模型面 |
| `ToolError` | `TimelineFailure` | `Partial/Cancelled/Error` 必须产出；`code` 来自 `ToolError.code` |
| `model` | provider tool result envelope | 不携带内部 source |
| `display.summary/diff` | `TimelineTool.summary` / `diff` | 双写；缺失/非法时按 09-18 契约 §7.1 兜底 |
| `display.header/body/metrics` | `TimelineTool.display`（新增可选） | 字段级 `serde(default, skip_serializing_if)` |
| `metrics` | `ToolResult` 扩展字段 + `display.metrics` | 同源于 `ToolExecutionMetrics`，旧 client 可忽略 |
| `Fatal` | 不上模型 wire | runtime 必须先把 in-flight 块 seal 为 `Failed` 再上抛/终止 |

- 新字段一律 optional；旧 client 忽略即可继续消费旧字段。
- `display` 必须进 timeline **快照**（翻页/重连后不退化），不只是 SSE 实时帧。
- 在 client/TUI 完成新字段接入前，不得删除旧 `ToolResult` / `TimelineTool` 字段。

## 12. 硬性规则

以下规则必须由测试或静态检查守护：

1. 所有注册工具必须有 descriptor 且通过校验。
2. 工具名和动态工具名不得碰撞。
3. 新工具不得使用 `json_err`、`json_err_string` 或 `[ERROR]` 控制流。
4. 迁移完成的模块不得再直接构造 legacy `ToolResult::error`。
5. 所有 `ToolErrorKind::Custom` 必须带命名空间 code。
6. 每个工具至少有成功、参数错误、执行错误三类测试。
7. 模型投影与展示投影必须来自同一 canonical outcome。
8. `ToolExecMeta` 不得继续成为只生成不消费的数据。
9. 新工具不得把完整参数或凭据写入 summary、日志或 audit。
10. 迁移期 runtime/manager 只依赖新 SDK；`LegacyToolAdapter` 是唯一的 legacy
    依赖点，禁止新代码反向依赖 legacy 行为。
11. `ToolOutcome.status` 与 `error` 必须满足 §7.3 不变量。
12. summary 不得取自模型投影首行；所有展示摘要必须通过 H1 的 JSON 判定。
13. 超时/取消必须按 §6.4 归类，不得伪装成成功或 fatal。

## 13. 迁移策略

迁移分两类：

### 13.1 兼容适配

旧工具暂时通过 `LegacyToolAdapter` 包装：

```text
ToolHandler
  -> LegacyToolAdapter
  -> ErasedTool
```

这样 `ToolManager` 和 runtime 可以先切到新执行入口，不必一次改完 19 个工具。

### 13.2 分批迁移

按风险从低到高：

1. 只读工具。
2. 文件修改工具。
3. exec/process。
4. todo/skills/read_image/subagent。
5. MCP/LSP 动态工具。

每批迁移完成后删除对应 legacy 分支，不保留双实现长期共存。

## 14. 验收标准

### 14.1 契约验收

- descriptor 校验测试覆盖重名、空描述、非法名称和非法 schema。
- typed 参数反序列化失败映射为 `InvalidArguments`。
- fatal error 不进入普通模型 tool result。
- 所有错误都可由 `ToolErrorKind` 分派。
- 模型投影、展示投影和 metrics 来自同一个 `ToolOutcome`。
- `output_schema` 与 `input_schema` 均由 typed 类型生成且通过 JSON Schema 校验。
- descriptor 名称规范化：非法内置名、动态名碰撞、超长名均被拒绝；原名可从
  `display_name` 读回。
- 超时与取消分别映射 `Timeout` / `Cancelled`；超时结果不进入成功路径。
- fatal 上抛后 timeline 中不残留 Running 块，`failure.code` 等于 fatal code。
- H1 对全部既有工具成立（含未迁移工具，靠 §8.1 的兜底）。

### 14.2 迁移验收

- 每批工具迁移后，旧行为回归测试全部通过。
- 全仓扫描不得出现该批模块的 legacy error 构造。
- `cargo test -p qaqh-workspace` 通过。
- `cargo test -p qaqh-runtime` 通过。
- `cargo clippy --workspace --all-targets` 通过。

### 14.3 发布验收

- 现有 client 不需要修改即可继续消费 `ToolResult`。
- TUI 接入新 metrics 后可以看到 elapsed、output size 和 effective tool name。
- timeline 不再自行推导 tool summary。

## 15. 开放问题

| ID | 问题 | 默认建议 |
|---|---|---|
| Q1 | 是否立即引入 `schemars` | 已决策：引入。`Args` 与 `Output` 都派生 JsonSchema，descriptor 的 input/output schema 由其生成；旧工具迁移时同步 |
| Q2 | 是否立即引入 async tool trait | 否。v1 保持同步，避免扩大迁移面 |
| Q3 | `Deferred` exposure 是否 v1 实现 | 类型先定义，行为后置 |
| Q4 | `ToolResult` 是否升级为 v2 | 先不改 wire，内部 outcome 适配 |
| Q5 | 动态 MCP output 如何 typed | 保留 JSON output，通过 `ToolOutput` 投影 |
| Q6 | fatal error 在 runtime 中的终止策略 | 先上抛到 actor，由 runtime 统一决定；上抛前必须 seal 对应 timeline 块为 Failed |
| Q7 | 全文回取端点 | v1 不做：`output_ref` 语义不变，展示层用 `truncated` + `output_bytes` 标注可见丢弃 |
| Q8 | progress 流标识是否进 wire | 进（开放字符串 `progress_stream`）+ 累计字节 `progress_bytes_total`，见 09-18 契约 §5 |
| Q9 | 展示类型的 crate 归属 | SDK 类型在 workspace，wire 类型在 domain，映射在 runtime；禁止 domain 反向依赖 |

Q1 的落地约束：

- `schemars` 只用于工具参数和输出的 schema 生成。
- 旧工具在迁移批次内同步替换，不新增第二套 schema 工具。
- 若某动态工具只能提供原始 JSON Schema，则通过显式 raw schema adapter 接入，
  不得绕过 descriptor 校验。

## 16. 参考

- 展示契约（跨仓唯一事实源）：
  [`2026-09-18-工具结果展示契约-v1-spec.md`](./2026-09-18-工具结果展示契约-v1-spec.md)
- TUI 消费面：
  [`qaqh-tui-app/docs/spec/2026-09-17-工具结果消费面与契约需求-spec.md`](../../../qaqh-tui-app/docs/spec/2026-09-17-工具结果消费面与契约需求-spec.md)

以下仓库仅作为设计参考，不复制其实现：

- Grok Build:
  - `crates/common/xai-tool-runtime/src/tool.rs`
  - `crates/common/xai-tool-runtime/src/error.rs`
  - `crates/common/xai-tool-runtime/src/render.rs`
  - `crates/common/xai-tool-protocol/src/error_wire.rs`
- Codex:
  - `codex-rs/tools/src/tool_executor.rs`
  - `codex-rs/tools/src/function_call_error.rs`
  - `codex-rs/tools/src/tool_output.rs`
  - `codex-rs/tools/src/tool_call.rs`
