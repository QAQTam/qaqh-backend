//! qaqh-gate: LLM API gateway — HTTP streaming + message format conversion.
//!
//! Supports OpenAI Chat Completions, Responses API, and Anthropic Messages protocols.
//!
//! # Note: string slices
//!
//! All string slices in this crate use indices from `find()` on ASCII
//! patterns (`<`, `>`, `\n`, `"data: "`, etc.), always on valid UTF-8
//! boundaries.  The clippy `string_slice` lint is allowed at the crate
//! level (see Cargo.toml).

#![cfg_attr(not(test), forbid(unsafe_code))]
mod chat_sdk;
mod gemini_sdk;
mod messages_sdk;
mod request_context;
mod responses_sdk;
mod sdk_common;
pub mod tool_parser;
mod transport;
mod types;
mod usage;
mod usage_semantics;

pub use anyhow::Error as GateError;
#[cfg(feature = "native-tool-plan")]
pub use qaqh_tool_core::model_tool_plan::ModelToolPlan;
pub use request_context::RequestContext;
pub use transport::RetryPolicy;
pub use types::{ErrorKind, ProviderConfig, ProviderKind, ResponsesCompat, StreamEvent};

use qaqh_types::{Message, ToolDef};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

/// Stream Responses with an explicitly authorized, immutable native tool plan.
/// Namespace descriptions must be provided for every namespace in the plan.
/// Native declarations are supported only on the Responses wire; Chat,
/// Anthropic Messages and Gemini still reject them (`unsupported_tool_representation`).
///
/// feature `native-tool-plan` 关闭时（默认）本入口不编译：生产路径走
/// [`chat_stream`]，它不下发原生工具声明。
#[cfg(feature = "native-tool-plan")]
#[allow(clippy::too_many_arguments)]
pub async fn chat_stream_responses_with_plan(
    provider: &ProviderConfig,
    messages: Vec<Message>,
    plan: &ModelToolPlan,
    namespace_descriptions: &std::collections::HashMap<String, String>,
    max_tokens: u32,
    effort: Option<String>,
    user_id: Option<String>,
    cancel: Option<&Arc<AtomicBool>>,
    on_event: &mut (dyn FnMut(StreamEvent) + Send),
) -> anyhow::Result<()> {
    anyhow::ensure!(
        matches!(provider.kind, ProviderKind::Responses),
        "native tool plans require Responses"
    );
    responses_sdk::chat_stream_responses_plan(
        provider,
        messages,
        plan,
        namespace_descriptions,
        max_tokens,
        effort,
        user_id,
        cancel,
        on_event,
    )
    .await
}

/// Send a chat completion request with SSE streaming.
///
/// 原生 `async fn`（P0-1）：直接在调用方的 Tokio Runtime 上驱动 HTTP/SSE，
/// gate 不再持有任何 crate 级 runtime。`cancel` is an optional shared abort
/// flag. When set to `true`, the streaming read loop will return
/// `Err("cancelled by user")` within one 50ms polling interval, aborting the
/// HTTP response promptly instead of waiting for the server to finish.
/// 回调契约要求 `Send`：宿主多线程 runtime 的任务可在 worker 间迁移。
#[allow(clippy::string_slice)]
#[allow(clippy::too_many_arguments)] // 参数面塑形另立项（PLAN D-5）
pub async fn chat_stream(
    provider: &ProviderConfig,
    messages: Vec<Message>,
    tools: Option<Vec<ToolDef>>,
    max_tokens: u32,
    effort: Option<String>,
    user_id: Option<String>,
    cancel: Option<&Arc<AtomicBool>>,
    on_event: &mut (dyn FnMut(StreamEvent) + Send),
) -> anyhow::Result<()> {
    match provider.kind {
        ProviderKind::Responses => {
            responses_sdk::chat_stream_responses(
                provider,
                &provider.model,
                messages,
                tools,
                max_tokens,
                effort,
                user_id,
                cancel,
                on_event,
            )
            .await
        }
        ProviderKind::Messages => {
            messages_sdk::chat_stream_messages(
                provider,
                &provider.model,
                messages,
                tools,
                max_tokens,
                effort,
                user_id,
                cancel,
                on_event,
            )
            .await
        }
        ProviderKind::Chat => {
            chat_sdk::chat_stream_chat(
                provider,
                &provider.model,
                messages,
                tools,
                max_tokens,
                effort,
                user_id,
                cancel,
                on_event,
            )
            .await
        }
        ProviderKind::Gemini => {
            gemini_sdk::chat_stream_gemini(
                provider,
                &provider.model,
                messages,
                tools,
                max_tokens,
                effort,
                user_id,
                cancel,
                on_event,
            )
            .await
        }
    }
}
