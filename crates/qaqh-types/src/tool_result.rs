//! Canonical tool execution result shared by the tool runtime and Ringing.
//!
//! A tool has one authoritative status. Human summaries, compact metadata and
//! the bounded model projection are separate fields so transport and UI code
//! never have to infer failure from the shape of textual output.

use crate::memory::MemoryUsageEstimate;
use serde::{Deserialize, Serialize};
#[cfg(feature = "ts")]
use ts_rs::TS;

pub const TOOL_SUMMARY_MAX_CHARS: usize = 512;
// Keep the model projection near the planned six-thousand-token budget.
// The limit is expressed in Unicode characters because provider tokenizers
// are not available at this shared contract boundary.
pub const TOOL_MODEL_MAX_CHARS: usize = 24_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    Ok,
    Error,
    Partial,
    Backgrounded,
    Cancelled,
}

impl ToolStatus {
    pub fn is_success(self) -> bool {
        matches!(self, Self::Ok | Self::Backgrounded)
    }

    pub fn is_failure(self) -> bool {
        matches!(self, Self::Error | Self::Partial | Self::Cancelled)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct ContentRef {
    pub content_id: String,
    pub media_type: String,
    pub sha256: String,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolContinuation {
    pub tool: String,
    pub args: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolModelPayload {
    pub text: String,
    pub truncated: bool,
    pub total_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continuation: Option<ToolContinuation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolError {
    pub code: String,
    pub message: String,
    pub retryable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

/// An image attachment carried by a tool result.
///
/// Stored alongside [`ToolResult`] so the message layer can append
/// `ContentBlock::ImageRef` blocks (falling back to inline `ContentBlock::Image`
/// only when disk externalization fails) to the resulting tool message. Never
/// serialized into the model text projection — the gate lowers images
/// to provider-native media parts at request-build time.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolImage {
    pub mime_type: String,
    /// Raw base64 payload (no `data:` prefix).
    pub data: String,
}

/// 工具运行元数据（09-18 展示契约 §3.4 / base spec §11）。
///
/// `elapsed_ms = None` 表示本次结果未接线 metrics（授权拒绝、历史归档等）；
/// 全空对象不序列化，旧 wire 保持不变。
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResultMetrics {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub elapsed_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "is_zero_u64")]
    pub output_bytes: u64,
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub retry_count: u32,
    /// 别名/MCP 解析后的实际工具名；None = 与卡片 name 相同。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_tool_name: Option<String>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub user_initiated: bool,
}

impl ToolResultMetrics {
    pub fn is_empty(&self) -> bool {
        self.elapsed_ms.is_none()
            && self.output_bytes == 0
            && self.retry_count == 0
            && self.effective_tool_name.is_none()
            && !self.user_initiated
    }
}

fn is_zero_u64(value: &u64) -> bool {
    *value == 0
}

fn is_zero_u32(value: &u32) -> bool {
    *value == 0
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ToolResultDisplay {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
    /// 终态行差（文件变更类工具）。0/0 = 无变更或未接线；旧 client 忽略即可。
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub lines_added: u32,
    #[serde(default, skip_serializing_if = "is_zero_u32")]
    pub lines_removed: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<ToolResultDisplayHeader>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub body: Option<ToolResultDisplayBody>,
    /// 结构化终态。旧 client 忽略即可；新 client 不再从文本猜 `[OK]`。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<ToolResultDisplayOutcome>,
}

/// 展示面终态（#336 P2）。
///
/// 这是 `ToolOutcome` 的 wire 投影，不是第二个执行事实源：`state` 由
/// `ToolStatus` / `ToolErrorKind` 单点派生，`exit_code` / `truncated` 由工具
/// body 派生，`duration_ms` / `output_bytes` 由框架 metrics 填充。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct ToolResultDisplayOutcome {
    pub state: ToolResultDisplayOutcomeState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub duration_ms: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_bytes: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub truncated: Option<bool>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
#[serde(rename_all = "snake_case")]
pub enum ToolResultDisplayOutcomeState {
    Succeeded,
    Failed,
    Cancelled,
    TimedOut,
    Backgrounded,
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolResultDisplayHeader {
    Path {
        path: String,
        op: ToolResultDisplayPathOp,
    },
    Shell {
        command: String,
    },
    Query {
        query: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        scope: Option<String>,
    },
    Other {
        label: String,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolResultDisplayPathOp {
    Read,
    Write,
    Edit,
    List,
    Patch,
    Delete,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ToolResultDisplayBody {
    None,
    Text {
        text: String,
        #[serde(default)]
        truncated: bool,
    },
    Diff {
        unified: String,
        #[serde(default)]
        files: Vec<String>,
    },
    Shell {
        output: String,
        #[serde(default)]
        exit_code: Option<i32>,
        #[serde(default)]
        truncated: bool,
    },
    /// v2：stdout / stderr 分离展示；旧 client 遇到未知变体应回退旧字段。
    Streams {
        stdout: String,
        stderr: String,
        #[serde(default)]
        exit_code: Option<i32>,
        #[serde(default)]
        truncated: bool,
        #[serde(default)]
        interleaved: bool,
    },
    Subagent {
        name: String,
        session_id: String,
    },
    #[serde(other)]
    Unknown,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ToolResult {
    pub status: ToolStatus,
    pub data: serde_json::Value,
    /// Images to attach to the tool result message (e.g. `read_image`).
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub images: Vec<ToolImage>,
    /// 展示平面的 unified diff（文件修改类工具的原始 diff 文本）。
    ///
    /// ⚠ 绝不进入模型投影（`project_for_model` 不携带它）：模型看到的仍是
    /// 紧凑摘要行，diff 只供 timeline/前端抽屉消费。发送方在成功路径上
    /// 显式填充；缺失时默认 None（向后兼容）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub diff: Option<String>,
    /// Canonical display projection emitted by a typed tool.
    ///
    /// Legacy handlers leave this `None`; the runtime then falls back to the
    /// registered text projector. Typed tools use this field so header/body
    /// never have to be reconstructed from `model.text`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    display: Option<ToolResultDisplay>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ToolError>,
    /// 运行元数据（框架填充，工具实现不写）。
    #[serde(default, skip_serializing_if = "ToolResultMetrics::is_empty")]
    pub metrics: ToolResultMetrics,

    // ── 投影区（私有）──────────────────────────────────────────────
    //
    // 以下三项是同一份工具输出的不同投影，历史上是 `pub` 字段，任何调用方都
    // 能单独改写其中一个而让另外两个失真（实例：`externalize_large_content`
    // 改 model.text 却让 summary 停留在原文；`amend_synthetic_repair` 改
    // model.text 而 summary 仍是占位符）。现收为私有，一律经本模块的构造函数
    // 或受控改写接口整体更新，**漂移因此变成编译错误**。
    //
    // serde 仍会序列化这三项：线上 JSON 与 ts-rs 生成的 TS 契约保持不变。
    //
    // `summary` 并非纯派生——部分工具会往里追加提示行（见 [`Self::push_hint`]）。
    /// 展示/模型提示行（≤ [`TOOL_SUMMARY_MAX_CHARS`]），可被工具追加。
    summary: String,
    /// 模型可见投影（文本、截断标记、token 估算、续调参数）。
    model: ToolModelPayload,
    /// 大输出外置引用。**仅在 NoFold 极限模式且模型文本超过
    /// `CONTENT_STORE_THRESHOLD_BYTES`（10 MiB）时才会产生**——标准模式下
    /// 模型文本已被 [`TOOL_MODEL_MAX_CHARS`] 封顶（≤ 约 96 KiB），永远到不了
    /// 该阈值。因此它是传输保护阀，而非常规路径：前端应视为可选字段，
    /// 不要指望靠它拿"完整输出"。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    output_ref: Option<ContentRef>,
}

impl ToolResult {
    /// Estimate owned heap bytes and payload categories without serializing.
    pub fn memory_usage_estimate(&self) -> MemoryUsageEstimate {
        let mut usage = MemoryUsageEstimate {
            heap_estimate_bytes: std::mem::size_of::<Self>() as u64,
            item_count: 1,
            ..MemoryUsageEstimate::default()
        };
        usage.add_value(&self.data);
        usage.add_string(&self.summary, true);
        usage.add_string(&self.model.text, true);
        usage.add_vec_capacity::<ToolImage>(self.images.capacity());
        for image in &self.images {
            usage.heap_estimate_bytes = usage
                .heap_estimate_bytes
                .saturating_add(std::mem::size_of::<ToolImage>() as u64);
            usage.add_string(&image.mime_type, false);
            usage.add_image_string(&image.data);
        }
        if let Some(diff) = &self.diff {
            usage.add_string(diff, true);
        }
        if let Some(display) = &self.display {
            usage.heap_estimate_bytes = usage
                .heap_estimate_bytes
                .saturating_add(std::mem::size_of::<ToolResultDisplay>() as u64);
            if let Some(summary) = &display.summary {
                usage.add_string(summary, true);
            }
            if let Some(diff) = &display.diff {
                usage.add_string(diff, true);
            }
            if let Some(header) = &display.header {
                usage.heap_estimate_bytes = usage
                    .heap_estimate_bytes
                    .saturating_add(std::mem::size_of::<ToolResultDisplayHeader>() as u64);
                match header {
                    ToolResultDisplayHeader::Path { path, .. } => usage.add_string(path, false),
                    ToolResultDisplayHeader::Shell { command } => usage.add_string(command, false),
                    ToolResultDisplayHeader::Query { query, scope } => {
                        usage.add_string(query, false);
                        if let Some(scope) = scope {
                            usage.add_string(scope, false);
                        }
                    }
                    ToolResultDisplayHeader::Other { label } => usage.add_string(label, false),
                }
            }
            if let Some(body) = &display.body {
                usage.heap_estimate_bytes = usage
                    .heap_estimate_bytes
                    .saturating_add(std::mem::size_of::<ToolResultDisplayBody>() as u64);
                match body {
                    ToolResultDisplayBody::None | ToolResultDisplayBody::Unknown => {}
                    ToolResultDisplayBody::Text { text, .. } => usage.add_string(text, true),
                    ToolResultDisplayBody::Diff { unified, files } => {
                        usage.add_string(unified, true);
                        usage.add_vec_capacity::<String>(files.capacity());
                        for file in files {
                            usage.add_string(file, false);
                        }
                    }
                    ToolResultDisplayBody::Shell { output, .. } => usage.add_string(output, true),
                    ToolResultDisplayBody::Streams { stdout, stderr, .. } => {
                        usage.add_string(stdout, true);
                        usage.add_string(stderr, true);
                    }
                    ToolResultDisplayBody::Subagent { name, session_id } => {
                        usage.add_string(name, false);
                        usage.add_string(session_id, false);
                    }
                }
            }
            if display.outcome.is_some() {
                usage.heap_estimate_bytes = usage
                    .heap_estimate_bytes
                    .saturating_add(std::mem::size_of::<ToolResultDisplayOutcome>() as u64);
            }
        }
        if let Some(error) = &self.error {
            usage.heap_estimate_bytes = usage
                .heap_estimate_bytes
                .saturating_add(std::mem::size_of::<ToolError>() as u64);
            usage.add_string(&error.code, false);
            usage.add_string(&error.message, true);
            if let Some(hint) = &error.hint {
                usage.add_string(hint, true);
            }
        }
        if let Some(name) = &self.metrics.effective_tool_name {
            usage.add_string(name, false);
        }
        if let Some(continuation) = &self.model.continuation {
            usage.heap_estimate_bytes = usage
                .heap_estimate_bytes
                .saturating_add(std::mem::size_of::<ToolContinuation>() as u64);
            usage.add_string(&continuation.tool, false);
            usage.add_value(&continuation.args);
        }
        if let Some(reference) = &self.output_ref {
            usage.heap_estimate_bytes = usage
                .heap_estimate_bytes
                .saturating_add(std::mem::size_of::<ContentRef>() as u64);
            usage.add_string(&reference.content_id, false);
            usage.add_string(&reference.media_type, false);
            usage.add_string(&reference.sha256, false);
        }
        usage
    }

    pub fn ok(text: impl Into<String>) -> Self {
        Self::ok_with_limit(text.into(), Some(TOOL_MODEL_MAX_CHARS))
    }

    /// Create a success result with an explicit model-text cap.
    ///
    /// `limit = None` disables the model projection cap entirely (used by
    /// no-fold / extreme modes where the model itself controls context);
    /// `Some(n)` bounds the projected text to `n` characters like [`Self::ok`].
    pub fn ok_with_limit(text: String, limit: Option<usize>) -> Self {
        Self::text_with_limit(ToolStatus::Ok, text, limit)
    }

    pub fn partial(text: impl Into<String>) -> Self {
        let text = text.into();
        Self::with_error(
            ToolStatus::Partial,
            text.clone(),
            "partial",
            text,
            false,
            None,
        )
    }

    pub fn cancelled(text: impl Into<String>) -> Self {
        let text = text.into();
        Self::with_error(
            ToolStatus::Cancelled,
            text.clone(),
            "cancelled",
            text,
            false,
            None,
        )
    }

    pub fn backgrounded(text: impl Into<String>) -> Self {
        Self::text(ToolStatus::Backgrounded, text.into())
    }

    pub fn error(message: impl Into<String>) -> Self {
        let message = message.into();
        Self::error_with("tool_error", message, false, None)
    }

    pub fn error_with(
        code: impl Into<String>,
        message: impl Into<String>,
        retryable: bool,
        hint: Option<String>,
    ) -> Self {
        let code = code.into();
        let message = message.into();
        Self::with_error(
            ToolStatus::Error,
            message.clone(),
            code,
            message,
            retryable,
            hint,
        )
    }

    pub fn ok_data(data: serde_json::Value, text: impl Into<String>) -> Self {
        let text = text.into();
        let mut result = Self::text(ToolStatus::Ok, text);
        result.data = compact_data(data);
        result
    }

    pub fn error_data(
        code: impl Into<String>,
        message: impl Into<String>,
        retryable: bool,
        hint: Option<String>,
        data: serde_json::Value,
    ) -> Self {
        let mut result = Self::error_with(code, message, retryable, hint);
        result.data = compact_data(data);
        result
    }

    pub fn text(status: ToolStatus, text: String) -> Self {
        Self::text_with_limit(status, text, Some(TOOL_MODEL_MAX_CHARS))
    }

    fn text_with_limit(status: ToolStatus, text: String, model_limit: Option<usize>) -> Self {
        let model_text = match model_limit {
            Some(limit) => bounded_text(&text, limit),
            None => (text.clone(), false),
        };
        Self {
            status,
            summary: bounded_text(&text, TOOL_SUMMARY_MAX_CHARS).0,
            data: serde_json::Value::Object(Default::default()),
            model: ToolModelPayload {
                text: model_text.0,
                truncated: model_text.1,
                total_tokens: estimate_tokens(&text),
                continuation: None,
            },
            diff: None,
            display: None,
            output_ref: None,
            error: None,
            metrics: ToolResultMetrics::default(),
            images: Vec::new(),
        }
    }

    /// Attach display-plane diff text (never projected to the model).
    pub fn with_diff(mut self, diff: impl Into<String>) -> Self {
        self.diff = Some(diff.into());
        self
    }

    /// Attach the canonical display projection emitted by a typed tool.
    pub fn with_display(mut self, display: ToolResultDisplay) -> Self {
        self.display = Some(display);
        self
    }

    /// Canonical display projection, when the producing tool supplied one.
    pub fn display(&self) -> Option<&ToolResultDisplay> {
        self.display.as_ref()
    }

    /// Replace the display/model summary without changing the model payload.
    ///
    /// Typed tool adapters use this to keep the wire `summary` aligned with
    /// the human display projection while the model receives the canonical
    /// structured payload.
    pub fn with_summary(mut self, summary: impl Into<String>) -> Self {
        self.summary = bounded_text(&summary.into(), TOOL_SUMMARY_MAX_CHARS).0;
        self
    }

    /// Attach an image (mime + base64) to be appended to the tool message.
    pub fn with_image(mut self, mime_type: impl Into<String>, data: impl Into<String>) -> Self {
        self.images.push(ToolImage {
            mime_type: mime_type.into(),
            data: data.into(),
        });
        self
    }

    pub fn with_error(
        status: ToolStatus,
        summary: impl Into<String>,
        code: impl Into<String>,
        message: impl Into<String>,
        retryable: bool,
        hint: Option<String>,
    ) -> Self {
        let mut result = Self::text(status, summary.into());
        result.error = Some(ToolError {
            code: code.into(),
            message: bounded_text(&message.into(), TOOL_SUMMARY_MAX_CHARS).0,
            retryable,
            hint: hint.map(|value| bounded_text(&value, TOOL_SUMMARY_MAX_CHARS).0),
        });
        result
    }

    pub fn is_success(&self) -> bool {
        self.status.is_success()
    }

    pub fn model_text(&self) -> &str {
        &self.model.text
    }

    /// 展示/模型提示行（`summary` 投影，只读）。
    pub fn summary(&self) -> &str {
        &self.summary
    }

    /// 模型投影是否被截断。
    pub fn model_truncated(&self) -> bool {
        self.model.truncated
    }

    /// 大输出外置引用（通常见 [`Self::output_ref`] 字段说明：极少产生）。
    pub fn output_ref(&self) -> Option<&ContentRef> {
        self.output_ref.as_ref()
    }

    /// 往提示行追加一行（如 `pending_id=… confirm with confirm_apply …`），
    /// 并**重新按 [`TOOL_SUMMARY_MAX_CHARS`] 收敛**。
    ///
    /// 直接 `push_str` 会让 summary 突破 512 上限（历史上 `apply_patch` /
    /// `edit` 的 dry-run 分支正是这么写的，而 `validate()` 在生产路径上从不
    /// 被调用，于是契约被静默违反）。走这个方法就不会。
    pub fn push_hint(&mut self, hint: &str) {
        self.summary.push('\n');
        self.summary.push_str(hint);
        self.summary = bounded_text(&self.summary, TOOL_SUMMARY_MAX_CHARS).0;
    }

    /// 整体替换模型文本，并重算 summary 与 token 估算。
    ///
    /// 用于"占位符 → 真实内容"的修复（如 `amend_synthetic_repair`）：这类场景
    /// 若只改模型文本，summary 会停留在占位符上，正是投影漂移的典型形态。
    /// `truncated` 与 `output_ref` 不在本方法职责内，保持不变。
    pub fn rewrite_text(&mut self, text: impl Into<String>) {
        let text = text.into();
        self.summary = bounded_text(&text, TOOL_SUMMARY_MAX_CHARS).0;
        self.model.text = text;
        self.model.total_tokens = estimate_tokens(&self.model.text);
    }

    /// 套用工具侧折叠结果：替换模型文本并标记截断。
    ///
    /// 供 `tool_side_fold` 使用。summary **不随之改变**——它是全文的前 512
    /// 字符，而折叠只保留头部（命令输出限额 8K 远大于 512），两者天然一致。
    pub fn set_model_projection(&mut self, text: String, truncated: bool) {
        self.model.text = text;
        self.model.truncated = truncated;
        self.model.total_tokens = estimate_tokens(&self.model.text);
    }

    /// 大输出外置：一次性改写模型文本、截断标记、summary 与外置引用。
    ///
    /// 四项必须同时改写——历史上逐字段赋值导致 summary 与 `model.text` 语义
    /// 错乱（那时 summary 取的是 tail 的前 512 字符，等于输出中段，作为展示
    /// 行毫无意义）。`summary` 因此单独入参，由调用方给出**全文开头**。
    pub fn externalize_output(&mut self, retained: String, summary: String, reference: ContentRef) {
        self.summary = bounded_text(&summary, TOOL_SUMMARY_MAX_CHARS).0;
        self.model.text = retained;
        self.model.truncated = true;
        self.model.total_tokens = estimate_tokens(&self.model.text);
        self.output_ref = Some(reference);
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        if self.summary.chars().count() > TOOL_SUMMARY_MAX_CHARS {
            return Err("summary exceeds the Unicode character budget");
        }
        if self.status.is_failure() && self.error.is_none() {
            return Err("failure result must include error");
        }
        if self.status.is_success() && self.error.is_some() {
            return Err("successful result must not include error");
        }
        // Model-text budget is policy-driven (ToolResult::ok_with_limit):
        // no-fold / extreme modes intentionally allow results beyond the
        // default TOOL_MODEL_MAX_CHARS, so no hard check is applied here.
        Ok(())
    }

    /// 统一 XML 信封：发给模型的 tool result 文本形态。
    ///
    /// 设计要点：
    /// - 属性区承载全部信令（status/truncated/error_code/retryable），
    ///   正文零转义省 token（替代旧 JSON to_string 的 \n 转义税）；
    /// - 标签名纯字母数字（<qaqh_tool_result>），避开 <|...|> special-token
    ///   命名空间（ChatML/Llama/DeepSeek 全家都占用了竖线形态）；
    /// - 正文中字面闭合标签替换为 <\/...> 反斜杠变体防嵌套假闭合。
    pub fn render_xml_envelope(&self) -> String {
        let status = match self.status {
            ToolStatus::Ok => "ok",
            ToolStatus::Error => "error",
            ToolStatus::Partial => "partial",
            ToolStatus::Backgrounded => "backgrounded",
            ToolStatus::Cancelled => "cancelled",
        };
        let mut out = format!("<qaqh_tool_result status=\"{status}\"");
        if self.model.truncated {
            out.push_str(" truncated=\"true\"");
        }
        if let Some(error) = &self.error {
            out.push_str(&format!(" error_code=\"{}\"", error.code));
            if error.retryable {
                out.push_str(" retryable=\"true\"");
            }
        }
        out.push_str(">\n");
        let mut body = self.model.text.trim_end().to_string();
        if body.is_empty()
            && let Some(error) = &self.error
        {
            body = error.message.clone();
        }
        let body = body.replace("</qaqh_tool_result>", "<\\/qaqh_tool_result>");
        out.push_str(&body);
        out.push_str("\n</qaqh_tool_result>");
        out
    }

    /// Stable payload used by provider adapters and context accounting.
    pub fn project_for_model(&self) -> serde_json::Value {
        serde_json::json!({
            "status": self.status,
            "summary": self.summary,
            "data": self.data,
            "text": self.model.text,
            "truncated": self.model.truncated,
            "continuation": self.model.continuation,
        })
    }
}

fn compact_data(data: serde_json::Value) -> serde_json::Value {
    match data {
        serde_json::Value::Object(mut object) => {
            object.remove("stdout");
            object.remove("stderr");
            object.remove("output");
            object.remove("content");
            serde_json::Value::Object(object)
        }
        serde_json::Value::Null => serde_json::Value::Object(Default::default()),
        other => other,
    }
}

fn bounded_text(text: &str, max_chars: usize) -> (String, bool) {
    let mut chars = text.chars();
    let bounded: String = chars.by_ref().take(max_chars).collect();
    let truncated = chars.next().is_some();
    (bounded, truncated)
}

fn estimate_tokens(text: &str) -> u64 {
    (text.chars().count() as u64).div_ceil(4)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_status_is_the_only_failure_authority() {
        let result =
            ToolResult::error_with("not_found", "missing", false, Some("retry read".into()));
        assert_eq!(result.status, ToolStatus::Error);
        assert!(result.error.is_some());
        assert!(!result.is_success());
        result.validate().unwrap();
    }

    #[test]
    fn summary_budget_is_unicode_safe_and_model_projection_is_stable() {
        let result = ToolResult::ok("界".repeat(TOOL_MODEL_MAX_CHARS + 100));
        assert_eq!(result.summary.chars().count(), TOOL_SUMMARY_MAX_CHARS);
        assert!(result.model.truncated);
        assert!(result.project_for_model().get("success").is_none());
        result.validate().unwrap();
    }

    #[test]
    fn display_payload_is_optional_and_roundtrips() {
        let legacy = serde_json::to_value(ToolResult::ok("legacy")).expect("serialize");
        let restored: ToolResult = serde_json::from_value(legacy).expect("legacy JSON");
        assert!(restored.display().is_none());

        let result = ToolResult::ok("typed").with_display(ToolResultDisplay {
            summary: Some("typed summary".into()),
            diff: None,
            lines_added: 0,
            lines_removed: 0,
            header: Some(ToolResultDisplayHeader::Other {
                label: "typed".into(),
            }),
            body: Some(ToolResultDisplayBody::Text {
                text: "typed body".into(),
                truncated: false,
            }),
            outcome: Some(ToolResultDisplayOutcome {
                state: ToolResultDisplayOutcomeState::Succeeded,
                exit_code: Some(0),
                duration_ms: Some(12),
                output_bytes: Some(34),
                truncated: Some(false),
            }),
        });
        let value = serde_json::to_value(&result).expect("serialize display");
        assert_eq!(value["display"]["outcome"]["state"], "succeeded");
        assert_eq!(value["display"]["outcome"]["exit_code"], 0);
        let restored: ToolResult = serde_json::from_value(value).expect("deserialize display");
        assert_eq!(restored.display(), result.display());
    }

    #[test]
    fn streams_display_body_roundtrips_with_separate_output() {
        let body = ToolResultDisplayBody::Streams {
            stdout: "out\n".into(),
            stderr: "err\n".into(),
            exit_code: Some(1),
            truncated: false,
            interleaved: false,
        };
        let value = serde_json::to_value(&body).expect("serialize streams");
        assert_eq!(value["kind"], "streams");
        assert_eq!(value["stdout"], "out\n");
        assert_eq!(value["stderr"], "err\n");
        assert_eq!(value["exit_code"], 1);
        let restored: ToolResultDisplayBody =
            serde_json::from_value(value).expect("deserialize streams");
        assert_eq!(restored, body);
    }

    #[test]
    fn unknown_display_body_falls_back_to_unknown() {
        let body: ToolResultDisplayBody =
            serde_json::from_value(serde_json::json!({"kind": "hologram", "text": "future"}))
                .expect("unknown body must not reject the display");
        assert_eq!(body, ToolResultDisplayBody::Unknown);
    }

    #[test]
    fn unknown_display_outcome_state_is_forward_compatible() {
        let outcome: ToolResultDisplayOutcome = serde_json::from_value(serde_json::json!({
            "state": "paused",
            "exit_code": 7,
            "truncated": true
        }))
        .expect("unknown outcome state must not reject the block");
        assert_eq!(
            outcome.state,
            ToolResultDisplayOutcomeState::Unknown,
            "future states degrade to Unknown"
        );
        assert_eq!(outcome.exit_code, Some(7));
        assert_eq!(outcome.truncated, Some(true));
    }

    #[test]
    fn compact_data_drops_large_inline_output_fields() {
        let result = ToolResult::ok_data(
            serde_json::json!({"path":"a.rs", "stdout":"large", "output":"large"}),
            "done",
        );
        assert_eq!(result.data["path"], "a.rs");
        assert!(result.data.get("stdout").is_none());
        assert!(result.data.get("output").is_none());
    }

    #[test]
    fn ok_with_limit_none_preserves_full_text_and_validates() {
        // 极限模式（no-fold）：模型自己控制上下文，结果完整透传。
        let body = "x".repeat(TOOL_MODEL_MAX_CHARS * 4);
        let result = ToolResult::ok_with_limit(body.clone(), None);
        assert_eq!(result.model.text, body);
        assert!(!result.model.truncated);
        result.validate().unwrap();

        // Some(limit) 仍按预算截断（与 ok() 一致）。
        let capped = ToolResult::ok_with_limit(body, Some(100));
        assert_eq!(capped.model.text.chars().count(), 100);
        assert!(capped.model.truncated);
    }
}
