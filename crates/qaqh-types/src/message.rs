use serde::{Deserialize, Serialize};

#[cfg(feature = "ts")]
use ts_rs::TS;

use crate::ToolResult;
use crate::memory::MemoryUsageEstimate;

// ── OpenAI-native content blocks ──

/// 思考内容（[`ContentBlock::Reasoning`]）的类别。
///
/// 与传输 SDK 的 `mutil_ai::ReasoningKind` 同名同形，但**故意不复用**：本类型
/// 是落盘/回放契约（会长期躺在会话历史里），而 `qaqh-types` 刻意不依赖传输 SDK
/// （见 `Cargo.toml`）。两者只在 `qaqh-gate` 一处做**穷尽 match** 映射——SDK 将来
/// 新增变体会在那里编译失败，不会静默漂移。
///
/// 显示规则：只有 `Summary` / `Text` 可展示；`Encrypted` 是必须逐字回放的协议
/// 状态（`response_output_item` 那块），`Redacted` 不可展示。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub enum ReasoningKind {
    /// 服务端给的思考**摘要**（订阅套餐路径实际只给这一种）。
    Summary,
    /// 模型可读的**完整**思考文本。
    Text,
    /// 加密思考（`encrypted_content`）：协议状态，仅用于逐字回放。
    Encrypted,
    /// 被安全策略抹除的思考：不可展示。
    Redacted,
}

impl ReasoningKind {
    /// 是否可作为思考文本展示给用户。
    pub fn is_displayable(self) -> bool {
        matches!(self, Self::Summary | Self::Text)
    }
}

/// Content block within a message, matching OpenAI / DeepSeek Chat Completions API.
///
/// Messages use content blocks instead of flat strings to support mixed text +
/// tool call + tool result + reasoning content within a single turn.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(tag = "type")]
#[allow(clippy::large_enum_variant)] // 装箱改造属结构塑形，另立项
pub enum ContentBlock {
    /// Plain text content from the model or user.
    #[serde(rename = "text")]
    Text { text: String },
    /// Model reasoning/thinking output (shown as collapsible in UI).
    /// Separate from `Text` so the frontend can style reasoning differently.
    ///
    /// `kind` 标注这份思考是「摘要」还是「完整文本」——订阅套餐路径只给摘要，
    /// UI 必须分得清这两者，否则会把摘要当成模型的原话展示。`None` = 本字段
    /// 引入前写入的历史数据（显示侧按 `Text` 处理）。
    #[serde(rename = "reasoning")]
    Reasoning {
        reasoning: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kind: Option<ReasoningKind>,
    },
    /// A tool call the model wants to execute.
    /// Includes the tool name, input arguments, and a unique call ID for tracking.
    #[serde(rename = "tool_use")]
    ToolUse {
        id: String,
        name: String,
        input: serde_json::Value,
    },
    /// Result of executing a previously requested tool call.
    /// Fed back to the model as context for the next inference step.
    #[serde(rename = "tool_result")]
    ToolResult {
        /// Matches the `id` from the corresponding `ToolUse` block.
        tool_use_id: String,
        /// Canonical structured result. `status` is the only execution truth.
        result: ToolResult,
    },
    /// An image (upload or tool-produced attachment).
    /// `mime_type` is the MIME type (e.g. "image/png", "image/jpeg").
    /// `data` is the base64-encoded image data (without the `data:...;base64,` prefix).
    #[serde(rename = "image")]
    Image {
        /// MIME type of the image (e.g. "image/png", "image/jpeg").
        mime_type: String,
        /// Base64-encoded image data (raw, without the data URI prefix).
        data: String,
    },
    /// 磁盘外置的图片引用（A-2 L0：docs/current/architecture.md）。
    ///
    /// 字节存于 `{data_dir}/images/{sha256}.{ext}`（内容寻址、base64 文本
    /// 形态，见 [`crate::image_store`]）；消息历史与内存常驻只携带索引，
    /// gate 请求构建时按需读盘。旧会话的 inline [`ContentBlock::Image`]
    /// 读侧继续兼容；写侧一律产生本变体。
    #[serde(rename = "image_ref")]
    ImageRef {
        /// 内容寻址 id = sha256(base64 文本) 的 hex。
        sha256: String,
        /// MIME type（语义同 `Image.mime_type`）。
        mime_type: String,
        /// base64 文本长度（`[Image #N]` 占位符显示与 token 估算用）。
        bytes_len: usize,
    },
    /// A server-side web search call emitted by the model (Responses API
    /// built-in tool). The search itself runs on the provider; this block is
    /// carried in history and echoed back verbatim on the next turn so the
    /// server restores its search results (stateless multi-turn).
    #[serde(rename = "web_search_call")]
    WebSearchCall {
        /// Output item id (e.g. "ws_1"); must be unique per response.
        id: String,
        /// The server-executed search action, e.g. {"type": "search"}.
        action: serde_json::Value,
    },
    /// Opaque output item returned by the Responses API.
    ///
    /// This block is protocol state rather than user-visible content. It keeps
    /// provider-owned fields such as Codex `phase` and reasoning
    /// `encrypted_content` intact across persistence and tool-loop replay.
    /// UI projections intentionally ignore it.
    #[serde(rename = "response_output_item")]
    ResponseOutputItem { item: serde_json::Value },
}

impl ContentBlock {
    /// Estimate owned payload and heap capacity without serializing or
    /// allocating a temporary representation.
    pub fn memory_usage_estimate(&self) -> MemoryUsageEstimate {
        let mut usage = MemoryUsageEstimate {
            heap_estimate_bytes: std::mem::size_of::<Self>() as u64,
            item_count: 1,
            ..MemoryUsageEstimate::default()
        };
        match self {
            Self::Text { text } => usage.add_string(text, true),
            Self::Reasoning { reasoning, .. } => usage.add_string(reasoning, true),
            Self::ToolUse { id, name, input } => {
                usage.add_string(id, false);
                usage.add_string(name, false);
                usage.add_value(input);
            }
            Self::ToolResult {
                tool_use_id,
                result,
            } => {
                usage.add_string(tool_use_id, false);
                usage.add(result.memory_usage_estimate());
            }
            Self::Image { mime_type, data } => {
                usage.add_string(mime_type, false);
                usage.add_image_string(data);
            }
            Self::ImageRef {
                sha256, mime_type, ..
            } => {
                usage.add_string(sha256, false);
                usage.add_string(mime_type, false);
            }
            Self::WebSearchCall { id, action } => {
                usage.add_string(id, false);
                usage.add_value(action);
            }
            Self::ResponseOutputItem { item } => usage.add_value(item),
        }
        usage
    }

    /// Convenience constructor for a text content block.
    pub fn text(text: &str) -> Self {
        ContentBlock::Text {
            text: text.to_string(),
        }
    }

    /// 便捷构造器：思考块。`kind` 为 `None` 表示来源未标注（历史数据）。
    pub fn reasoning(reasoning: &str, kind: Option<ReasoningKind>) -> Self {
        ContentBlock::Reasoning {
            reasoning: reasoning.to_string(),
            kind,
        }
    }

    /// Convenience constructor for an image content block.
    pub fn image(mime_type: &str, base64_data: &str) -> Self {
        ContentBlock::Image {
            mime_type: mime_type.to_string(),
            data: base64_data.to_string(),
        }
    }

    /// 外置图片引用的便捷构造器（不校验磁盘在场——调用方负责先落盘）。
    pub fn image_ref(sha256: &str, mime_type: &str, bytes_len: usize) -> Self {
        ContentBlock::ImageRef {
            sha256: sha256.to_string(),
            mime_type: mime_type.to_string(),
            bytes_len,
        }
    }

    /// 是否是**不可执行**的 tool_use（BUG-2026-09-13-13 / #13）。
    ///
    /// 流中断（读错误 / 空闲超时）的抢救路径、或 provider 漏发
    /// `content_block_start` 时，会落下一个没有调用 id 或没有工具名的
    /// `ToolUse`（`input` 可能是 `null`）。它既无法执行（执行层按 name/id
    /// 查表必失败），也不该出现在任何回放面上（前端会渲染出永远 running
    /// 的幽灵工具卡，序列化成 `arguments:"null"` 还会被部分端点 400）。
    ///
    /// 与 `responses_api.rs` 的既有防护判据一致：**缺 id 或 name 即不可执行**。
    /// 该判据是写侧清洗与回放过滤共用的唯一真源。
    pub fn is_hanging_tool_use(&self) -> bool {
        matches!(
            self,
            ContentBlock::ToolUse { id, name, .. } if id.is_empty() || name.is_empty()
        )
    }
}

// ── Messages ──

/// A conversation message using OpenAI-native content-block format.
///
/// Roles:
/// - `"user"` — contains `Text`, `Image`, and/or `ToolResult` blocks
/// - `"assistant"` — contains visible `Text`/`Reasoning`/`ToolUse` blocks and
///   may carry hidden `ResponseOutputItem` protocol state
/// - `"system"` — system-level context and instructions
/// - `"tool"` — tool execution results
///
/// The optional `name` field distinguishes same-role participants
/// (e.g. `name="docs"` for injected document context, `name="code"` for
/// code snippets). It maps to OpenAI's `name` parameter.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Message {
    /// Monotonic per-session message ID for ordering and dedup.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub msg_id: Option<u64>,
    pub role: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default)]
    pub content: Vec<ContentBlock>,
}

impl Message {
    /// Estimate owned heap bytes and payload sizes for diagnostics.
    pub fn memory_usage_estimate(&self) -> MemoryUsageEstimate {
        let mut usage = MemoryUsageEstimate {
            heap_estimate_bytes: (std::mem::size_of::<Self>()
                + self
                    .content
                    .capacity()
                    .saturating_mul(std::mem::size_of::<ContentBlock>()))
                as u64,
            item_count: 1,
            ..MemoryUsageEstimate::default()
        };
        usage.add_string(&self.role, false);
        if let Some(name) = &self.name {
            usage.add_string(name, false);
        }
        for block in &self.content {
            usage.add(block.memory_usage_estimate());
        }
        usage
    }

    /// Role constants — the open roles of the context flow.
    pub const ROLE_SYSTEM: &'static str = "system";
    pub const ROLE_USER: &'static str = "user";
    pub const ROLE_ASSISTANT: &'static str = "assistant";
    pub const ROLE_TOOL: &'static str = "tool";
    /// Responses-protocol-only role for runtime-injected instructions
    /// (skills envelope, subagent reports, …). Downgrades to `system` on
    /// Chat Completions providers, maps to a `developer` item on Responses.
    pub const ROLE_DEVELOPER: &'static str = "developer";

    /// Create a system message with a single text block.
    pub fn system(content: &str) -> Self {
        Self {
            msg_id: None,
            role: Self::ROLE_SYSTEM.into(),
            name: None,
            content: vec![ContentBlock::text(content)],
        }
    }
    /// Create a developer message (Responses runtime-injection role). Falls
    /// back to `system` semantics on Chat Completions providers — the
    /// downgrade happens at the gate conversion layer, not at storage.
    pub fn developer(content: &str) -> Self {
        Self {
            msg_id: None,
            role: Self::ROLE_DEVELOPER.into(),
            name: None,
            content: vec![ContentBlock::text(content)],
        }
    }
    /// Create a user message with a single text block.
    pub fn user(content: &str) -> Self {
        Self {
            msg_id: None,
            role: Self::ROLE_USER.into(),
            name: None,
            content: vec![ContentBlock::text(content)],
        }
    }
    /// Create a tool result message, feeding tool output back to the model.
    pub fn tool(tool_call_id: &str, result: &str, success: bool) -> Self {
        let result = if success {
            ToolResult::ok(result)
        } else {
            ToolResult::error(result)
        };
        Self::tool_result(tool_call_id, result)
    }

    /// Create a tool result message with the canonical structured result.
    pub fn tool_result(tool_call_id: &str, result: ToolResult) -> Self {
        Self {
            msg_id: None,
            role: Self::ROLE_TOOL.into(),
            name: None,
            content: vec![ContentBlock::ToolResult {
                tool_use_id: tool_call_id.into(),
                result,
            }],
        }
    }
}

// ── Tool Call (kept for IPC, XML/DSML parsing, and backward compat) ──

/// A tool call invocation, used in JSON-based tool call protocols.
///
/// Note: new code prefers `ContentBlock::ToolUse` for OpenAI-native format.
/// `ToolCall` remains for XML/DSML parsing and backward compatibility.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ToolCall {
    /// Unique call identifier for tracking and result matching.
    pub id: String,
    /// Always `"function"` for function-call-style tools.
    #[serde(rename = "type")]
    pub call_type: String,
    pub function: FunctionCall,
}

/// The function details within a tool call.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FunctionCall {
    /// Name of the tool to invoke (e.g. "read", "exec").
    pub name: String,
    /// JSON-encoded arguments string.
    pub arguments: String,
}
