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

mod messages_sdk;
mod gemini_sdk;
mod chat_sdk;
mod request_context;
mod responses_sdk;
#[cfg(test)]
mod rt_test;
mod sdk_common;
pub mod tool_parser;
mod transport;
mod types;
mod usage;
mod usage_semantics;

pub use anyhow::Error as GateError;
pub use request_context::RequestContext;
pub use transport::RetryPolicy;
pub use types::{ErrorKind, ProviderConfig, ProviderKind, ResponsesCompat, StreamEvent};
pub use qaqh_tool_core::model_tool_plan::ModelToolPlan;

use qaqh_types::{Message, ToolDef};
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

/// Stream Responses with an explicitly authorized, immutable native tool plan.
/// Namespace descriptions must be provided for every namespace in the plan.
/// Native declarations are supported only on the Responses wire; Chat,
/// Anthropic Messages and Gemini still reject them (`unsupported_tool_representation`).
#[allow(clippy::too_many_arguments)]
pub fn chat_stream_responses_with_plan(
    provider: &ProviderConfig,
    messages: Vec<Message>,
    plan: &ModelToolPlan,
    namespace_descriptions: &std::collections::HashMap<String, String>,
    max_tokens: u32,
    effort: Option<String>,
    user_id: Option<String>,
    cancel: Option<&Arc<AtomicBool>>,
    on_event: &mut dyn FnMut(StreamEvent),
) -> anyhow::Result<()> {
    anyhow::ensure!(matches!(provider.kind, ProviderKind::Responses), "native tool plans require Responses");
    responses_sdk::chat_stream_responses_plan(provider, messages, plan, namespace_descriptions, max_tokens, effort, user_id, cancel, on_event)
}

/// Send a chat completion request with SSE streaming.
///
/// `cancel` is an optional shared abort flag. When set to `true`, the
/// streaming read loop will return `Err("cancelled by user")` within
/// one 50ms polling interval, aborting the HTTP response promptly instead
/// of waiting for the server to finish.
#[allow(clippy::string_slice)]
#[allow(clippy::too_many_arguments)] // 参数面塑形另立项（PLAN D-5）
pub fn chat_stream(
    provider: &ProviderConfig,
    messages: Vec<Message>,
    tools: Option<Vec<ToolDef>>,
    max_tokens: u32,
    effort: Option<String>,
    user_id: Option<String>,
    cancel: Option<&Arc<AtomicBool>>,
    on_event: &mut dyn FnMut(StreamEvent),
) -> anyhow::Result<()> {
    match provider.kind {
        ProviderKind::Responses => responses_sdk::chat_stream_responses(
            provider,
            &provider.model,
            messages,
            tools,
            max_tokens,
            effort,
            user_id,
            cancel,
            on_event,
        ),
        ProviderKind::Messages => messages_sdk::chat_stream_messages(
            provider,
            &provider.model,
            messages,
            tools,
            max_tokens,
            effort,
            user_id,
            cancel,
            on_event,
        ),
        ProviderKind::Chat => chat_sdk::chat_stream_chat(
            provider,
            &provider.model,
            messages,
            tools,
            max_tokens,
            effort,
            user_id,
            cancel,
            on_event,
        ),
        ProviderKind::Gemini => gemini_sdk::chat_stream_gemini(
            provider,
            &provider.model,
            messages,
            tools,
            max_tokens,
            effort,
            user_id,
            cancel,
            on_event,
        ),
    }
}

/// Synchronous (non-streaming) chat for internal use (compact, etc.).
pub fn chat_sync(
    provider: &ProviderConfig,
    messages: Vec<Message>,
    max_tokens: u32,
) -> Result<String, String> {
    match provider.kind {
        ProviderKind::Responses => {
            responses_sdk::chat_sync_responses(provider, &provider.model, messages, max_tokens)
        }
        ProviderKind::Messages => {
            messages_sdk::chat_sync_messages(provider, &provider.model, messages, max_tokens)
        }
        ProviderKind::Chat => {
            chat_sdk::chat_sync_chat(provider, &provider.model, messages, max_tokens)
        }
        ProviderKind::Gemini => {
            gemini_sdk::chat_sync_gemini(provider, &provider.model, messages, max_tokens)
        }
    }
}
