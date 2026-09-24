# QAQH Gate v2：多协议与 provider 适配 spec（2026-09-20）

> 状态：**P1 设计补充，待评审**。本 spec 展开总架构 plan §5.3 的 `GateHost` 边界，不改变 SessionActor、TurnCore、ToolRuntime 或 Ringing 的既有架构裁决。
> 基线：`origin/betav2 @ 361995e`
> 关联 issue：`#133`
> 上位架构：[`docs/plan/2026-09-20-qaqh-v2.0-总架构设计-plan.md`](../plan/2026-09-20-qaqh-v2.0-总架构设计-plan.md) §5.3、§11 P2/P3
> 当前实现：`crates/qaqh-gate/src/{lib.rs,types.rs,transport.rs}` 与三协议 adapter。

---

## 0. 结论先行

Gate v2 的目标不是“再写一个统一 provider 插件”，而是把现在的：

```text
chat_stream(ProviderConfig, Vec<Message>, Vec<ToolDef>, ...)
  -> ProviderKind match
  -> 三协议实现
  -> StreamEvent
```

收敛为三层：

```text
TurnCore
  -> GateHost
       -> ProtocolAdapter (ChatCompletions / Responses / AnthropicMessages / future)
            -> EndpointCapabilities + ProtocolCompat
                 -> provider HTTP endpoint
```

必须冻结五条边界：

1. **协议 adapter 按协议族划分，不按 provider 划分。**
2. **provider 差异优先表达为 capability/compat 数据，不进入 core loop。**
3. **TurnCore 只消费 canonical `GateEvent` / `GateOutcome`，不消费 provider wire JSON。**
4. **unknown protocol 必须 fail-closed，不得回退到 OpenAI。**
5. **新增 provider 若属于已有协议族，应只增加 endpoint profile；若属于新协议族，才增加 adapter。**

---

## 1. 当前实现与缺口

### 1.1 当前已具备

- `qaqh-gate` 已支持三类协议：
  - OpenAI Chat Completions
  - OpenAI Responses
  - Anthropic Messages
- `ProviderKind` 在 `chat_stream` / `chat_sync` 内硬分派三协议。
- `EndpointSpec` 已有 protocol/base_url/path/model 与多项 capability 字段。
- `transport.rs` 已集中 retry、backoff、cancel、idle timeout 与 SSE 公共逻辑。
- `StreamEvent` 已将三协议的部分流式输出归一为 chat-like 事件。

### 1.2 当前缺口

- `ProviderKind::from_str` 未知协议默认 `OpenAi`，不是 fail-closed。
- `ProviderConfig` 同时承载 endpoint 连接信息、协议差异、provider quirk 与运行参数。
- `ResponsesCompat` 只覆盖 Responses 协议差异，其他协议差异散落在 bool 字段中。
- `StreamEvent` 是 chat-completion-shaped，缺少 provider continuation、server tool、结构化错误等 canonical 表达。
- `TurnCore` 尚无 typed `GateRequest` / `GateOutcome` contract。
- 没有 adapter registry、capability validation 或 conformance test matrix。
- 没有冻结“新 provider 是否需要改 core”的准入规则。

---

## 2. 目标与非目标

### 2.1 目标

- 让三协议在同一个 GateHost contract 下运行。
- 让新增同协议 provider 只改配置。
- 让新增协议族只新增 adapter 与 conformance tests。
- 让 provider-specific quirk 有明确归宿，不污染 TurnCore。
- 让 continuation、server-side tool、usage/cache、reasoning、cancel 与错误分类可 typed 表达。
- 保持当前重试、流解析、错误脱敏和取消行为不回退。

### 2.2 非目标

- 本 spec 不新增 provider 或 wire protocol。
- 本 spec 不实现 SessionActor/TurnCore/ToolRuntime。
- 本 spec 不改变 Ringing v1/v2 或 client API。
- 本 spec 不要求立即改成 async trait；实现可继续使用同步 callback/stream facade。
- 本 spec 不把 provider-native server tool 塞进本地 ToolRuntime。

---

## 3. 三层架构

### 3.1 Protocol Adapter 层

职责：

- wire request/response/SSE 解析；
- provider 协议字段转换；
- reasoning/tool call/server tool/usage 流解析；
- HTTP status、provider error code、stream truncation 分类；
- retry 分类所需的最小信息；
- 将 provider 原始状态映射为 `ProviderContinuation`。

禁止：

- 判断工具权限；
- 执行本地工具；
- 修改 session canonical state；
- 直接发布 Ringing 事件；
- 让 TurnCore 看到 provider JSON。

### 3.2 Capability / Compat 层

职责：

- 描述 endpoint 支持的能力；
- 描述同一协议下的兼容差异；
- 在请求发出前做 capability validation；
- 让 provider 配置可声明、可测试、可 diff。

禁止：

- 用一个任意 JSON 字段绕过 typed contract；
- 在 core loop 中按 provider id 分支；
- 用“未知值默认 OpenAI”掩盖配置错误。

### 3.3 Canonical Gate 层

职责：

- `GateRequest`：TurnCore 提交一次模型请求；
- `GateEvent`：adapter 流式产生 canonical 事件；
- `GateOutcome`：一次请求的 typed 终态；
- `GateError`：统一错误分类与 retry 建议；
- `ProviderContinuation`：provider 自有状态的受控引用。

TurnCore 只依赖本层。

---

## 4. 冻结类型

### 4.1 ProtocolId 与 Adapter

```rust
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ProtocolId {
    ChatCompletions,
    Responses,
    AnthropicMessages,
}
```

`ProtocolId` 是协议族身份，不是 provider 身份。provider 不得新增自己的 protocol variant。

```rust
pub trait ProtocolAdapter: Send + Sync {
    fn protocol(&self) -> ProtocolId;

    fn validate(
        &self,
        request: &GateRequest,
        capabilities: &EndpointCapabilities,
    ) -> Result<(), GateError>;

    fn stream(
        &self,
        request: GateRequest,
        context: GateContext,
        on_event: &mut dyn FnMut(GateEvent),
    ) -> Result<GateOutcome, GateError>;

    fn sync(
        &self,
        request: GateRequest,
        context: GateContext,
    ) -> Result<GateOutcome, GateError>;
}
```

实现允许在当前同步 transport 上包装；不要求本阶段引入新的 async runtime。

### 4.2 Adapter Registry

```rust
pub struct AdapterRegistry {
    adapters: std::collections::HashMap<ProtocolId, std::sync::Arc<dyn ProtocolAdapter>>,
}

impl AdapterRegistry {
    pub fn resolve(&self, protocol: &ProtocolId) -> Result<std::sync::Arc<dyn ProtocolAdapter>, GateError>;
}
```

规则：

- registry 初始化时必须检测 protocol 重复注册。
- `resolve` 失败返回 `UnsupportedProtocol`。
- 不得有 `_ => OpenAi`、`unwrap_or_default()` 或同类 fallback。
- registry 不按 provider id 选择 adapter；provider 只选择 protocol。

### 4.3 Endpoint Profile

```rust
pub struct EndpointProfile {
    pub provider_id: String,
    pub endpoint_id: String,
    pub protocol: ProtocolId,
    pub base_url: String,
    pub auth: AuthSpec,
    pub retry: RetrySpec,
    pub capabilities: EndpointCapabilities,
    pub compat: ProtocolCompat,
}
```

边界：

- `provider_id` 只用于配置、诊断、审计与 metrics；不得用于 core loop 分支。
- `base_url`、path、auth、retry 属于 endpoint。
- `capabilities` 描述“这个 endpoint 支持什么”。
- `compat` 描述“这个协议实现需要怎样兼容”。

### 4.4 EndpointCapabilities

```rust
pub struct EndpointCapabilities {
    pub streaming: bool,
    pub tool_calling: ToolCallingCapability,
    pub reasoning: ReasoningCapability,
    pub usage: UsageCapability,
    pub cache: CacheCapability,
    pub continuation: ContinuationCapability,
    pub server_tools: Vec<ServerToolCapability>,
    pub modalities: ModalityCapability,
    pub output: OutputCapability,
}

pub struct ToolCallingCapability {
    pub supported: bool,
    pub parallel: bool,
    pub streaming_arguments: bool,
}

pub struct ReasoningCapability {
    pub supported: bool,
    pub effort_levels: Vec<String>,
    pub returns_reasoning_delta: bool,
    pub requires_echo: bool,
}

pub struct UsageCapability {
    pub final_usage: bool,
    pub streaming_usage: bool,
    pub cache_reporting: CacheReportingMode,
}

pub struct ContinuationCapability {
    pub mode: ContinuationMode,
    pub max_state_bytes: Option<u64>,
}

pub enum ContinuationMode {
    Stateless,
    ProviderState,
    ServerSession,
}
```

capability validation 必须 fail-closed：

- 请求需要 tools 但 endpoint 不支持 → `UnsupportedCapability`。
- 请求需要 streaming 但 endpoint 不支持 → `UnsupportedCapability`。
- 请求 reasoning effort 不在 allowlist → 按既有 ladder 规则 clamp 或拒绝，但必须由显式策略决定。
- 请求 continuation 但 endpoint 不支持 → `UnsupportedCapability`。

### 4.5 ProtocolCompat

```rust
pub enum ProtocolCompat {
    ChatCompletions(ChatCompletionsCompat),
    Responses(ResponsesCompat),
    AnthropicMessages(AnthropicMessagesCompat),
}
```

规则：

- compat 类型必须与协议族一一对应。
- compat 字段不得与 capability 混用：
  - “支持不支持”属于 capability；
  - “同协议下的字段差异”属于 compat。
- provider 特例如果无法表达为 protocol+capability+compat，必须做成 adapter-local hook，不得进入 TurnCore。

### 4.6 支撑类型

以下类型属于 Gate v2 contract；`ContentRef`、`ContentHash`、`UsageInfo`、`Message`、`ToolDef` 复用现有 canonical 类型。

```rust
pub type RequestId = String;

pub enum AuthSpec {
    None,
    Bearer,
    Header { name: String },
}

pub struct GateContext {
    pub endpoint: EndpointProfile,
    pub credentials: EndpointCredentials,
    pub cancellation: CancellationToken,
    pub logical_now_ms: i64,
}

pub struct EndpointCredentials {
    pub api_key: String,
    pub headers: Vec<(String, String)>,
}

pub enum ToolChoice {
    Auto,
    None,
    Required,
    Named { name: String },
}

pub enum OutputConstraint {
    Text,
    JsonObject,
    JsonSchema { name: String, schema_ref: ContentRef },
}

pub struct ReasoningRequest {
    pub effort: Option<String>,
    pub budget_tokens: Option<u32>,
}

pub struct CacheRequest {
    pub prompt_cache_key: Option<String>,
}

pub struct GateMetadata {
    pub session_seed: String,
    pub turn_id: String,
    pub request_tag: String,
    pub trace_id: Option<String>,
}

pub enum CancelReason {
    User,
    TurnCancelled,
    SessionClosed,
    Shutdown,
    Timeout,
}

pub enum ServerToolKind {
    WebSearch,
    CodeInterpreter,
    FileSearch,
    Other(String),
}

pub enum ServerToolStatus {
    Completed,
    Failed,
    Cancelled,
}

pub struct GateInteractionRequest {
    pub interaction_id: String,
    pub kind: GateInteractionKind,
    pub request_ref: ContentRef,
    pub expires_at_logical_ms: Option<i64>,
}

pub enum GateInteractionKind {
    ProviderInputRequired,
    ProviderApprovalRequired,
    Other(String),
}
```

规则：

- `EndpointCredentials.api_key` 只在 gate runtime 内存在，不得序列化进 fact、日志或 GateEvent。
- `GateMetadata` 只用于关联、诊断与 provider management headers，不进入 canonical fact identity。
- `ServerToolKind::Other` 与 `GateInteractionKind::Other` 必须带稳定 provider code，并由 adapter 声明是否可忽略。


---

## 5. Canonical Gate Contract

### 5.1 GateRequest

```rust
pub struct GateRequest {
    pub request_id: RequestId,
    pub model: String,
    pub messages: Vec<Message>,
    pub continuation: Option<ProviderContinuation>,
    pub tools: Vec<ToolDef>,
    pub tool_choice: ToolChoice,
    pub output: OutputConstraint,
    pub reasoning: ReasoningRequest,
    pub cache: CacheRequest,
    pub metadata: GateMetadata,
}
```

约束：

- `messages` 是 canonical conversation，不是 provider wire payload。
- provider 原始 output item、encrypted reasoning、server session handle 不得伪装成普通 `ContentBlock` 长期存放。
- `ProviderContinuation` 必须受长度限制，并以 `ContentRef` 或等价 durable reference 保存。
- `request_id` 用于日志、retry、metrics 与 provider management headers，不进入 canonical fact 身份。

### 5.2 ProviderContinuation

```rust
pub struct ProviderContinuation {
    pub protocol: ProtocolId,
    pub provider_state_ref: Option<ContentRef>,
    pub server_session_id: Option<String>,
    pub expires_at_logical_ms: Option<i64>,
}
```

用途：

- Responses 的 `previous_response_id` 或 provider-owned state；
- stateful proxy 的 session handle；
- Anthropic/其他协议的等价 continuation。

规则：

- adapter 只能读取自己 protocol 的 continuation。
- continuation 不可解释时不得猜测；必须重新走无状态请求或返回 `ResetRequired` 对应的 gate error。
- continuation 不能替代 canonical history；canonical facts 始终可重放。

### 5.3 GateEvent

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GateEvent {
    Started {
        provider_request_id: Option<String>,
    },
    TextDelta {
        block_id: String,
        text: String,
    },
    ReasoningDelta {
        block_id: String,
        text: String,
    },
    ToolCallStarted {
        call_id: String,
        name: String,
    },
    ToolCallArgumentsDelta {
        call_id: String,
        partial_json: String,
    },
    ToolCallCompleted {
        call_id: String,
        name: String,
        args_ref: ContentRef,
        args_hash: ContentHash,
    },
    ServerToolStarted {
        tool_call_id: String,
        kind: ServerToolKind,
    },
    ServerToolProgress {
        tool_call_id: String,
        status: String,
    },
    ServerToolCompleted {
        tool_call_id: String,
        status: ServerToolStatus,
        result_ref: Option<ContentRef>,
    },
    Usage {
        usage: UsageInfo,
    },
    Continuation {
        state: ProviderContinuation,
    },
    Retrying {
        attempt: u32,
        max_attempts: u32,
        delay_ms: u64,
        error: GateError,
    },
    Completed {
        stop_reason: Option<String>,
        usage: Option<UsageInfo>,
    },
    Failed {
        error: GateError,
    },
    Cancelled {
        reason: CancelReason,
    },
}
```

规则：

- `ToolCallArgumentsDelta` 只是流式片段；最终执行前必须得到 `ToolCallCompleted` 与完整 args hash。
- server-side tool 不进入本地 ToolRuntime，但必须有 typed event 和最终状态。
- `Usage` 可在流中出现多次，最后一次为权威。
- `Retrying` 不是 TurnCore 决策事件；它只用于 UI/diagnostics 与 metrics。

### 5.4 GateOutcome

```rust
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum GateOutcome {
    Text {
        message: Message,
        usage: Option<UsageInfo>,
        continuation: Option<ProviderContinuation>,
    },
    Tools {
        message: Message,
        calls: Vec<GateToolCall>,
        usage: Option<UsageInfo>,
        continuation: Option<ProviderContinuation>,
    },
    Suspend {
        interaction: GateInteractionRequest,
        continuation: Option<ProviderContinuation>,
    },
    Abort {
        error: GateError,
    },
}

pub struct GateToolCall {
    pub call_id: String,
    pub name: String,
    pub args_ref: ContentRef,
    pub args_hash: ContentHash,
}
```

规则：

- `GateOutcome` 是 TurnCore 唯一消费的终态。
- `Tools` 只表示模型声明了工具调用，不代表授权或执行。
- `Suspend` 只在协议本身要求等待 provider-side interaction 时使用；本地 permission/ask/plan 仍由 InteractionRegistry 管理。
- `Abort` 必须携带结构化 `GateError`，不得只给字符串。

### 5.5 GateError

```rust
pub struct GateError {
    pub class: GateErrorClass,
    pub provider_code: Option<String>,
    pub message: String,
    pub retryable: bool,
    pub retry_after_ms: Option<u64>,
    pub http_status: Option<u16>,
}

pub enum GateErrorClass {
    Auth,
    RateLimit,
    Timeout,
    Transport,
    ContextLength,
    InvalidRequest,
    UnsupportedCapability,
    UnsupportedProtocol,
    ProviderUnavailable,
    Protocol,
    Cancelled,
    Indeterminate,
}
```

规则：

- adapter 必须把 provider error 映射到 `GateErrorClass`。
- `provider_code` 可保留 provider 原码，但不得替代 canonical class。
- 可重试性由 adapter 声明，由 transport retry executor 执行。
- 错误 message 必须经过凭据脱敏与长度限制。

---

## 6. Transport、Retry 与 Cancel

### 6.1 Retry

- retry policy 属于 endpoint transport，不属于协议 adapter。
- adapter 只返回 `Attempt::Ok | Retry | Fatal` 等价分类。
- `429/500/503`、provider retry-after、idle timeout 继续由 transport 统一处理。
- 重试不得重复执行已经进入本地 ToolRuntime 的副作用。
- `GateEvent::Retrying` 必须保留 attempt、max、delay 与结构化 error。

### 6.2 Cancel

- v2 使用 cancellation token 或等价可组合取消句柄。
- 当前 `Arc<AtomicBool>` 只能作为迁移期 shim。
- adapter 必须在取消后尽快结束流，并产生唯一 `Cancelled` 或 `Abort`。
- cancel 不得让 TurnCore 进入两个终态。

---

## 7. Server-side Tool

provider-native server tool（web search、code interpreter 等）规则：

- 由 adapter 解析为 `GateEvent::ServerTool*`；
- 不进入本地 `ToolRuntime`；
- 不产生本地 `ToolIntent/ToolFinished`；
- 若需要持久化，使用 `ContentRef` + provider call id；
- 不得把 provider 原始 JSON 塞回 `Message.content` 作为长期 canonical state；
- UI 只消费 typed server-tool projection。

当前 `ContentBlock::WebSearchCall` 与 `ContentBlock::ResponseOutputItem` 在迁移期可继续读，但写侧必须迁移到上述模型。

---

## 8. Provider 接入规则

### 8.1 同协议、已有 capability

只增加 `EndpointProfile`：

- base_url/path/auth/model；
- capability flags；
- protocol-specific compat；
- retry。

不得改 core loop。

### 8.2 同协议、新 compat

只增加 typed compat 字段或 adapter-local mapping：

- 不能新增 provider id 分支；
- 不能绕过 capability validation；
- 必须补该 compat 的 conformance test。

### 8.3 新协议族

必须新增：

1. `ProtocolId` variant；
2. `ProtocolAdapter` 实现；
3. adapter registry 注册；
4. protocol-specific compat；
5. conformance test suite；
6. 至少一个 endpoint profile。

不得通过复用 `OpenAi` protocol 冒充新协议。

### 8.4 无法建模的 provider 特例

只有在 protocol + capability + compat 都无法表达时，才允许 adapter-local hook。

约束：

- hook 必须 typed；
- hook 不得进入 TurnCore；
- hook 必须有独立测试；
- 若 hook 改变请求/响应语义，必须提升 protocol/payload version。

---

## 9. 迁移方案

### P1.5A：类型与 registry

- 定义 `ProtocolId`、`GateRequest`、`GateEvent`、`GateOutcome`、`GateError`、`EndpointCapabilities`、`ProtocolCompat`。
- 定义 adapter registry 与 unknown protocol fail-closed。
- 不接 runtime。

### P1.5B：包装现有三协议

- 将 `chat_completions_api`、`responses_api`、`message_api` 包成三个 adapter。
- 现有 `StreamEvent` 在 adapter 边界转换为 `GateEvent`。
- 保持 HTTP、retry、SSE、错误脱敏行为不变。

### P1.5C：GateHost facade

- `GateHost` 通过 registry 解析 adapter。
- `chat_stream` / `chat_sync` 暂时保留为 deprecated shim，内部委托 GateHost。
- 同一时刻只允许一个 protocol owner，禁止新旧路径各自发请求。

### P1.5D：TurnCore 接入

- TurnCore 改为构造 `GateRequest`、消费 `GateEvent`、读取 `GateOutcome`。
- 删除 TurnCore 对 `ProviderKind`、provider-specific bool 和 raw `StreamEvent` 的依赖。
- 保持 turn/tool/interaction 状态机语义不变。

### P1.5E：删除旧分支

- 删除 `ProviderKind` hard match。
- 删除 provider id 在 core loop/gate dispatch 中的分支。
- 将 `ProviderConfig` 拆为 `EndpointProfile` + runtime credential。
- 保留的兼容字段必须标注迁移期与删除条件。

---

## 10. Conformance 测试

每个 adapter 必须通过同一套 canonical suite：

| 场景 | 断言 |
|---|---|
| text streaming | 产生稳定 `TextDelta` 与唯一 `Completed` |
| reasoning | reasoning delta 与最终 usage/stop reason 一致 |
| single tool call | 参数完整、hash 稳定、无重复完成事件 |
| parallel tool calls | 每个 call id 恰好一个完成事件 |
| tool call malformed/truncated | 映射为 `GateErrorClass::Protocol`，不执行 |
| usage/cache | final usage 权威，cache 字段不丢失 |
| continuation | 状态可序列化、可恢复，不可解释时 fail-closed |
| server tool | started/progress/completed 顺序稳定 |
| retryable error | `Retrying` 含结构化 error，重试后终态唯一 |
| non-retryable error | 不重试，映射正确 class |
| cancel | 及时结束且唯一 terminal |
| unknown response field | 可忽略字段不破坏 typed contract |
| unknown event/kind | fail-closed 或按 schema 版本处理 |
| credential leak | error/message 不包含 API key |
| no raw provider JSON | canonical events 不含 provider wire payload |

三协议必须全部通过后，才允许 Gate v2 成为 runtime 默认路径。

---

## 11. Gate 与完成条件

Gate v2 可宣告完成，必须同时满足：

- `ProtocolAdapter` 不再依赖 `ProviderKind` hard match。
- unknown protocol 返回 `UnsupportedProtocol`，回退次数为 0。
- 三协议 conformance suite 全绿。
- TurnCore 不依赖 provider id、provider wire 类型或协议分支。
- 新增同协议 provider 不需要修改 core loop。
- 新增协议族只需要新增 adapter + registry 注册 + conformance suite。
- 旧 `chat_stream` shim 已删除或有明确删除日期。
- `ProviderConfig` 中无法 typed 化的字段数量不再增长。

---

## 12. 风险与回滚

### 12.1 风险

- 迁移期同时存在旧 `StreamEvent` 与新 `GateEvent`，可能出现语义漂移。
- continuation 处理错误会导致多轮对话丢上下文。
- capability validation 过严会阻断现有 provider。
- adapter 包装层若复制 transport，会重新产生行为漂移。

### 12.2 控制

- 同一阶段只允许一个请求执行路径。
- adapter 包装不得复制 retry/SSE/取消实现。
- 三协议在每一步都跑 conformance suite。
- 旧路径保留只作为 shim，不保留第二套协议实现。

### 12.3 回滚

- P1.5A/P1.5B 可独立回滚，不影响 runtime。
- P1.5C 可回滚到旧 `chat_stream`，但必须保持单一请求路径。
- P1.5D 完成后不得同时保留两套 TurnCore gate 调用路径。

---

## 13. 与总架构 plan 的关系

本 spec 是 plan §5.3 的展开，不修改以下既有裁决：

- Gate 负责协议转换、重试、流解析、错误分类，不负责工具权限。
- TurnCore 负责 turn 状态机，不直接 append fact / publish projection。
- ToolRuntime 负责本地工具 admit、执行、取消、审计与终态。
- canonical log/projection 仍是状态事实源。

建议后续把本 spec 登记为：

```text
P1.5：Gate v2 与多协议适配
```

并放在 P2 `SessionActor + TurnCore` 之前，避免 TurnCore 固化当前 chat-centric `chat_stream` API。

---

## 14. 参考

- `docs/plan/2026-09-20-qaqh-v2.0-总架构设计-plan.md` §5.3、§11
- `crates/qaqh-gate/src/lib.rs`
- `crates/qaqh-gate/src/types.rs`
- `crates/qaqh-gate/src/transport.rs`
- `crates/qaqh-types/src/provider.rs`
- `crates/qaqh-types/src/message.rs`
