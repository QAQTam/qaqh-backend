# Workspace 工具层契约重写（Tool SDK v1） - 规格（2026-09-15）

> 状态：**草案待评审**。
>
> 本文定义 `qaqh-workspace` 工具层的目标契约。它不描述某个具体工具的业务逻辑，
> 而是规定所有工具必须遵守的注册、参数、错误、输出、进度和运行时边界。
>
> 落地过程如与本文冲突，先修改本文并完成评审，再修改代码。

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
    pub name: ToolName,
    pub description: String,
    pub input_schema: JsonSchema,
    pub category: ToolCategory,
    pub risk: ToolRisk,
    pub default_timeout: Duration,
    pub exposure: ToolExposure,
    pub source: ToolSource,
    pub output_budget: OutputBudget,
}
```

约束：

- `name` 必须唯一，格式为 `[a-z][a-z0-9_]*`。
- `description` 不得为空。
- `input_schema` 必须是合法 JSON Schema object。
- `category` 是权限决策唯一来源。
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
    type Output: Serialize + ToolOutput;

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

## 7. 错误模型

### 7.1 可恢复 ToolError

```rust
pub struct ToolError {
    pub kind: ToolErrorKind,
    pub detail: String,
    pub retryable: bool,
    pub hint: Option<String>,
    pub details: Option<serde_json::Value>,
    source: Option<anyhow::Error>,
}

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
- `Custom`：仅用于无法归入既有分类的工具特有错误，必须携带命名空间 code。

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

- `Recoverable` 进入 `ToolOutcome` 并反馈模型。
- `Fatal` 上抛给 actor/runtime，由 runtime 决定终止、重试或上报。

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
    fn model_blocks(&self) -> Vec<ToolContentBlock> {
        Vec::new()
    }

    fn summary(&self) -> Option<String> {
        None
    }

    fn display(&self) -> ToolDisplay {
        ToolDisplay::default()
    }
}
```

默认行为：

1. `model_blocks` 为空时，将序列化 JSON 作为 text block。
2. `summary` 为空时，从模型文本生成有界单行摘要。
3. `display` 默认无 diff。

### 8.2 ToolOutcome

```rust
pub struct ToolOutcome {
    pub status: ToolStatus,
    pub output: ToolOutputValue,
    pub model: ToolModelProjection,
    pub display: ToolDisplay,
    pub metrics: ToolExecutionMetrics,
}
```

投影职责：

| 投影 | 消费者 | 禁止内容 |
|---|---|---|
| `output` | 宿主、审计、后续工具 | 不保证直接发给模型 |
| `model` | provider tool result | 不包含内部 source |
| `display` | timeline、TUI、client | 不参与模型决策 |
| `metrics` | telemetry、审计、TUI 运行信息 | 不包含敏感参数正文 |

### 8.3 ToolExecutionMetrics

```rust
pub struct ToolExecutionMetrics {
    pub elapsed: Duration,
    pub output_bytes: u64,
    pub retry_count: u32,
    pub effective_tool_name: Option<String>,
}
```

运行信息必须通过 outcome 进入 runtime 适配层，不得再生成后丢弃。

### 8.4 ToolDisplay

```rust
pub struct ToolDisplay {
    pub summary: Option<String>,
    pub diff: Option<String>,
}
```

timeline 必须优先使用 canonical summary，不得重新从模型输出第一行推导。

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
- `exec` 的 stdout/stderr 通过 `ProgressStream` 表达。
- 非 exec 工具不得再创造私有 progress JSON 协议。

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

Tool SDK v1 内部使用新类型，但对外先适配到现有 `qaqh_types::ToolResult`：

- `ToolStatus` 保持现有五态。
- `ToolErrorKind` 映射到稳定 `error_code`。
- `model_blocks` 渲染为现有模型 envelope。
- `display.diff` 进入现有 `ToolResult.diff`。
- `metrics` 先进入 ToolFinished 扩展字段，未迁移 client 可忽略。

在 client/TUI 完成新字段接入前，不得删除旧 `ToolResult` 字段。

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
10. 迁移期 legacy adapter 只允许调用新 SDK，不允许反向依赖 legacy 行为。

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
| Q1 | 是否立即引入 `schemars` | 已决策：引入。新工具必须使用；旧工具迁移时同步 |
| Q2 | 是否立即引入 async tool trait | 否。v1 保持同步，避免扩大迁移面 |
| Q3 | `Deferred` exposure 是否 v1 实现 | 类型先定义，行为后置 |
| Q4 | `ToolResult` 是否升级为 v2 | 先不改 wire，内部 outcome 适配 |
| Q5 | 动态 MCP output 如何 typed | 保留 JSON output，通过 `ToolOutput` 投影 |
| Q6 | fatal error 在 runtime 中的终止策略 | 先上抛到 actor，由 runtime 统一决定 |

Q1 的落地约束：

- `schemars` 只用于工具参数和输出的 schema 生成。
- 旧工具在迁移批次内同步替换，不新增第二套 schema 工具。
- 若某动态工具只能提供原始 JSON Schema，则通过显式 raw schema adapter 接入，
  不得绕过 descriptor 校验。

## 16. 参考

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
