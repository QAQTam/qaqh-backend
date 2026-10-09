//! Gemini `generateContent` transport built on the `mutil-ai` SDK.
//!
//! Wire facts that force this module to differ from the chat / responses /
//! messages siblings:
//! - the model is part of the request path (`/models/{model}:generateContent`,
//!   streaming sibling `:streamGenerateContent` + `alt=sse`), so the endpoint
//!   base URL is `scheme + host [+ version prefix]` and the path carries the
//!   `{model}` placeholder the SDK substitutes;
//! - authentication is the `key` query parameter, never a header;
//! - function calls carry no id, so one is synthesized per call and reused for
//!   both the progress event and the `Done` payload — the runtime's timeline
//!   block (`tool:<id>`) needs one stable target;
//! - tool results are declared by function *name*, which the harness stores
//!   only on the assistant turn, so it is resolved from history;
//! - assistant reasoning is replayed as a `thought: true` part. The opaque
//!   `thoughtSignature` is not persisted in history (the same accepted loss as
//!   Anthropic thinking signatures); the SDK falls back to the text form.
//!
//! Endpoint quirks that do not exist on this wire (`include_stream_usage`,
//! `tool_call_content`, `do_sample`, `prompt_cache_key`, `cache_field`) are
//! deliberately left unset — nothing is sent for them.

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use futures::StreamExt;
use mutil_ai::{
    AuthStyle, ChatRequest, EndpointAdapter, EndpointSpec, Error as SdkError, ImagePart,
    Message as WireMessage, ModelAdapter, Part, ProtocolSurface, Reasoning as WireReasoning,
    ReasoningConfig, ReasoningEffort, RequestOptions, Role, StreamEvent as SdkEvent,
    ToolCall as WireToolCall, ToolResult as WireToolResult, ToolSpec, Usage as WireUsage,
};
use reqwest::header::HeaderValue;

use crate::usage;
use qaqh_types::{ContentBlock, Message, ToolDef, UsageInfo};

use super::sdk_common::{
    CancelWatch, Outcome, RetryHub, StreamState, apply_management_headers, empty_stream_retry,
    sdk_error_detail, sdk_retry_policy, uuid_simple,
};
use super::transport::{
    RetryPolicy as GateRetryPolicy, block_on, is_cancelled, normalize_skill_envelope,
};
use super::types::{
    ProviderConfig, StreamEvent, clamp_effort_to_allowlist, normalize_reasoning_effort,
};

const MEDIA_ATTACHMENT_TEXT: &str = "Attached media from tool result:";

/// `:generateContent` → `:streamGenerateContent`. Applied to the (possibly
/// overridden) sync path so a custom prefix keeps working; a path without the
/// sync suffix is used as-is for both operations.
fn stream_path_for(path: &str) -> String {
    path.replace(":generateContent", ":streamGenerateContent")
}

/// Gemini declares tool results by function name, which the harness stores only
/// on the assistant `ToolUse` block. Resolve `id → name` from history so
/// `functionResponse.name` matches the original declaration.
fn tool_names_by_id(messages: &[Message]) -> HashMap<String, String> {
    let mut names = HashMap::new();
    for msg in messages {
        if msg.role != "assistant" {
            continue;
        }
        for block in &msg.content {
            if let ContentBlock::ToolUse { id, name, .. } = block {
                names.insert(id.clone(), name.clone());
            }
        }
    }
    names
}

/// Project harness history onto the SDK's neutral model. Image handling
/// mirrors the chat wire: user images become `read_image` placeholders, tool
/// images become synthetic `user` media messages flushed after the whole tool
/// batch.
pub(crate) fn project_messages(
    messages: Vec<Message>,
    image_index_base: usize,
) -> Vec<WireMessage> {
    let names = tool_names_by_id(&messages);
    let mut out: Vec<WireMessage> = Vec::new();
    let mut pending_media: Vec<WireMessage> = Vec::new();
    let mut img_idx = image_index_base;

    for msg in messages {
        if msg.role != "tool" && !pending_media.is_empty() {
            out.append(&mut pending_media);
        }
        match msg.role.as_str() {
            "system" | "developer" => {
                // Gemini merges every instruction turn into `systemInstruction`
                // (SDK normalization); only the first text block is taken, as on
                // the chat wire.
                if let Some(tb) = msg.content.iter().find_map(|b| match b {
                    ContentBlock::Text { text } => Some(text.clone()),
                    _ => None,
                }) {
                    out.push(WireMessage::system(tb));
                }
            }
            "user" => {
                let mut text_parts: Vec<String> = Vec::new();
                let mut image_refs: Vec<String> = Vec::new();
                for block in &msg.content {
                    match block {
                        ContentBlock::Text { text: t } => text_parts.push(t.clone()),
                        ContentBlock::Image { mime_type, data } => {
                            image_refs.push(format!(
                                "[Image #{img_idx}: {mime_type}, ~{} bytes — call read_image(image_index={img_idx}) to view it yourself]",
                                data.len()
                            ));
                            img_idx += 1;
                        }
                        ContentBlock::ImageRef {
                            mime_type,
                            bytes_len,
                            ..
                        } => {
                            image_refs.push(format!(
                                "[Image #{img_idx}: {mime_type}, ~{bytes_len} bytes — call read_image(image_index={img_idx}) to view it yourself]"
                            ));
                            img_idx += 1;
                        }
                        _ => {}
                    }
                }
                let mut combined_text = text_parts.join("");
                if !image_refs.is_empty() {
                    if !combined_text.is_empty() {
                        combined_text.push('\n');
                    }
                    combined_text.push_str(&image_refs.join("\n"));
                }
                out.push(WireMessage::user(combined_text));
            }
            "assistant" => {
                let mut content = String::new();
                let mut reasoning = String::new();
                let mut calls: Vec<WireToolCall> = Vec::new();
                for block in &msg.content {
                    match block {
                        ContentBlock::Text { text } => content.push_str(text),
                        ContentBlock::Reasoning { reasoning: r } => reasoning.push_str(r),
                        ContentBlock::ToolUse { id, name, input } => {
                            let mut call = WireToolCall::new(name.clone(), input.clone());
                            call.id = Some(id.clone());
                            calls.push(call);
                        }
                        _ => {}
                    }
                }
                let mut parts: Vec<Part> = Vec::new();
                if !content.is_empty() {
                    parts.push(Part::text(content));
                } else if calls.is_empty() && !reasoning.is_empty() {
                    parts.push(Part::text("[Thinking complete]"));
                }
                if !reasoning.is_empty() {
                    // No provider state → the SDK emits `{"text":…, "thought":true}`.
                    parts.push(Part::Reasoning(WireReasoning::text(reasoning)));
                }
                parts.extend(calls.into_iter().map(Part::ToolCall));
                if !parts.is_empty() {
                    out.push(WireMessage::new(Role::Assistant, parts));
                }
            }
            "tool" => {
                let mut parts: Vec<Part> = Vec::new();
                for block in &msg.content {
                    if let ContentBlock::ToolResult {
                        tool_use_id,
                        result,
                    } = block
                    {
                        let name = msg
                            .name
                            .clone()
                            .filter(|n| !n.is_empty())
                            .or_else(|| names.get(tool_use_id).cloned())
                            .unwrap_or_else(|| tool_use_id.clone());
                        let mut tr = WireToolResult::new(
                            Some(tool_use_id.clone()),
                            name,
                            result.render_xml_envelope(),
                        );
                        tr.is_error = !result.is_success();
                        parts.push(Part::ToolResult(tr));
                    }
                    match block {
                        ContentBlock::Image { mime_type, data } => {
                            pending_media.push(media_message(mime_type, data));
                        }
                        ContentBlock::ImageRef {
                            sha256, mime_type, ..
                        } => match qaqh_types::image_store::load_image_b64(sha256, mime_type) {
                            Ok(data) => pending_media.push(media_message(mime_type, &data)),
                            Err(e) => log::warn!(
                                "[gate] image {sha256} load failed, dropped from gemini request: {e}"
                            ),
                        },
                        _ => {}
                    }
                }
                if !parts.is_empty() {
                    out.push(WireMessage::new(Role::Tool, parts));
                }
            }
            _ => {}
        }
    }
    out.append(&mut pending_media);
    out
}

fn media_message(mime_type: &str, data: &str) -> WireMessage {
    WireMessage::new(
        Role::User,
        vec![
            Part::text(MEDIA_ATTACHMENT_TEXT),
            Part::Image {
                image: ImagePart::base64(mime_type, data),
            },
        ],
    )
}

fn convert_tools(tools: Option<Vec<ToolDef>>) -> Vec<ToolSpec> {
    tools
        .unwrap_or_default()
        .into_iter()
        .map(|td| {
            ToolSpec::new(
                td.function.name,
                td.function.description,
                td.function.parameters,
            )
        })
        .collect()
}

/// QAQ effort ladder → the SDK's neutral levels. Gemini exposes
/// MINIMAL/LOW/MEDIUM/HIGH only, so `xhigh` and `max` both land on the top tier
/// (the SDK folds XHigh/Max to `HIGH`). Unknown values send no effort level.
fn gemini_effort(effort: &str) -> Option<ReasoningEffort> {
    Some(match effort {
        "low" => ReasoningEffort::Low,
        "medium" => ReasoningEffort::Medium,
        "high" => ReasoningEffort::High,
        "xhigh" => ReasoningEffort::XHigh,
        "max" => ReasoningEffort::Max,
        _ => return None,
    })
}

fn build_adapter(
    provider: &ProviderConfig,
    model: &str,
    policy: &GateRetryPolicy,
) -> (EndpointAdapter, Arc<RetryHub>) {
    let path = provider
        .gemini_path
        .clone()
        .unwrap_or_else(|| "/models/{model}:generateContent".to_string());
    let mut spec = EndpointSpec::new(
        ProtocolSurface::GeminiGenerateContent,
        provider.base_url.clone(),
        path.clone(),
        AuthStyle::QueryKey {
            parameter: "key".to_string(),
        },
    );
    spec.stream_path = Some(stream_path_for(&path));
    let mut transport = mutil_ai::TransportConfig::default();
    transport.user_agent = HeaderValue::from_static(qaqh_types::QAQH_USER_AGENT);
    // SDK 建连前重试只经 audit sink 可见——转发为 harness 的 Retrying 事件。
    let hub = Arc::new(RetryHub::new(policy.max_retries));
    transport.audit_sink = Some(hub.clone());
    transport.audit_config = mutil_ai::AuditConfig {
        enabled: true,
        ..Default::default()
    };
    (
        EndpointAdapter::new(model, spec)
            .api_key(provider.api_key.clone())
            .retry_policy(sdk_retry_policy(policy))
            .transport(transport),
        hub,
    )
}

fn build_options(
    provider: &ProviderConfig,
    policy: &GateRetryPolicy,
    streaming: bool,
) -> RequestOptions {
    let mut options = RequestOptions::new();
    options.timeout = Some(Duration::from_secs(30 * 60));
    if streaming {
        options.idle_timeout = Some(policy.idle_timeout);
    }
    apply_management_headers(provider, None, &mut options, false);
    options
}

/// Thinking is requested only on the streaming path: `includeThoughts` lets
/// thought parts arrive as reasoning deltas, and the effort ladder is mapped
/// onto `thinkingLevel`. The sync path (compact / title) only wants the answer.
fn build_request(
    provider: &ProviderConfig,
    projected: Vec<WireMessage>,
    tools: Option<Vec<ToolDef>>,
    max_tokens: u32,
    effort: Option<String>,
    streaming: bool,
) -> ChatRequest {
    let mut request = ChatRequest::new(projected);
    request.tools = convert_tools(tools);
    request.max_output_tokens = Some(max_tokens);
    if provider.supports_thinking && streaming {
        let mut reasoning = ReasoningConfig::new().include_text(true);
        if provider.supports_reasoning_effort
            && let Some(level) = effort
                .as_deref()
                .and_then(|raw| normalize_reasoning_effort(Some(raw)))
                .map(|e| match &provider.effort_allowlist {
                    Some(list) => clamp_effort_to_allowlist(&e, list),
                    None => e,
                })
                .and_then(|e| gemini_effort(&e))
        {
            reasoning = reasoning.effort(level);
        }
        request.reasoning = Some(reasoning);
    }
    request
}

/// Gemini's `promptTokenCount` is the **total** input (cached included), so the
/// cached count is the hit and the remainder the miss.
fn usage_to_info(u: &WireUsage) -> UsageInfo {
    let prompt = u.input_tokens.unwrap_or(0) as u32;
    let completion = u.output_tokens.unwrap_or(0) as u32;
    let hit = u.cache_read_tokens.unwrap_or(0) as u32;
    usage::normalize(
        usage::UsageSeed {
            prompt_tokens: prompt,
            completion_tokens: completion,
            total_tokens: u.total_tokens.map(|v| v as u32),
            cache_hit_tokens: hit,
            cache_miss_tokens: prompt.saturating_sub(hit),
            cache_reported: u.cache_read_tokens.is_some(),
            reasoning_tokens: u.reasoning_tokens.unwrap_or(0) as u32,
        },
        u.raw.as_ref(),
    )
}

async fn run_sdk_stream(
    adapter: &EndpointAdapter,
    request: &ChatRequest,
    options: &RequestOptions,
    api_key: &str,
    hub: &RetryHub,
    cancel: Option<&Arc<AtomicBool>>,
    on_event: &mut dyn FnMut(StreamEvent),
) -> Outcome {
    let watch = CancelWatch::start(cancel);
    let mut options = options.clone();
    options.cancellation = Some(watch.token().clone());
    consume(adapter, request, &options, api_key, hub, cancel, on_event).await
}

#[allow(clippy::too_many_arguments)] // 与 consume 同形（strangler 内部函数）
async fn consume(
    adapter: &EndpointAdapter,
    request: &ChatRequest,
    options: &RequestOptions,
    api_key: &str,
    hub: &RetryHub,
    cancel: Option<&Arc<AtomicBool>>,
    on_event: &mut dyn FnMut(StreamEvent),
) -> Outcome {
    let mut stream = match adapter.stream_with(request, options).await {
        Ok(s) => s,
        Err(e) => {
            hub.drain(&mut *on_event);
            if matches!(e, SdkError::Cancelled) {
                return Outcome::Fatal(anyhow::anyhow!("cancelled by user"));
            }
            let kind = e.kind();
            let (msg, full) = sdk_error_detail("Gemini", &e, api_key);
            on_event(StreamEvent::Error {
                kind,
                message: full,
            });
            return Outcome::Fatal(anyhow::anyhow!("{msg}"));
        }
    };
    hub.drain(&mut *on_event);

    let mut state = StreamState::new();
    let mut interrupted: Option<String> = None;

    loop {
        if is_cancelled(cancel) {
            return Outcome::Fatal(anyhow::anyhow!("cancelled by user"));
        }
        let Some(next) = stream.next().await else {
            break;
        };
        hub.drain(&mut *on_event);
        match next {
            Ok(SdkEvent::Start { .. }) => {}
            Ok(SdkEvent::TextDelta { text }) => {
                if !text.is_empty() {
                    state.text.push_str(&text);
                    on_event(StreamEvent::ContentDelta(text));
                }
            }
            Ok(SdkEvent::ReasoningDelta { text, .. }) => {
                if !text.is_empty() {
                    state.reasoning.push_str(&text);
                    on_event(StreamEvent::ReasoningDelta(text));
                }
            }
            Ok(SdkEvent::ToolCallDelta {
                index,
                id,
                name,
                arguments_delta,
            }) => {
                // 一次调用一帧完整参数；id 在首帧合成，进度事件与 Done 载荷
                // 共用同一值（runtime 的 timeline block 以 `tool:<id>` 为键）。
                let entry = state.tools.entry(index).or_insert_with(|| {
                    (
                        id.filter(|s| !s.is_empty())
                            .unwrap_or_else(|| format!("toolu_{}", uuid_simple())),
                        name.unwrap_or_default(),
                        String::new(),
                    )
                });
                if !arguments_delta.is_empty() {
                    entry.2.push_str(&arguments_delta);
                    on_event(StreamEvent::ToolCallProgress {
                        index,
                        id: entry.0.clone(),
                        name: entry.1.clone(),
                        args_chunk: arguments_delta,
                    });
                }
            }
            Ok(SdkEvent::ToolCallProgress { .. }) => {}
            Ok(SdkEvent::ServerToolStatus { .. }) => {}
            Ok(SdkEvent::Usage { usage }) => {
                let u = usage_to_info(&usage);
                state.usage = Some(u.clone());
                on_event(StreamEvent::UsageUpdate(u));
            }
            Ok(SdkEvent::Retry { .. }) => {}
            Ok(SdkEvent::Retrying {
                attempt,
                max_attempts,
                delay,
                reason,
            }) => {
                on_event(StreamEvent::Retrying {
                    attempt,
                    max_retries: max_attempts,
                    delay_secs: delay.as_secs(),
                    error: reason,
                });
            }
            Ok(SdkEvent::Error { error }) => {
                on_event(StreamEvent::Error {
                    kind: error.kind,
                    message: error.message.clone(),
                });
                interrupted = Some(error.message);
                break;
            }
            Ok(SdkEvent::Done {
                finish_reason,
                response,
                ..
            }) => {
                state.stop = finish_reason;
                if state.usage.is_none()
                    && let Some(usage) = response.usage.as_ref()
                {
                    state.usage = Some(usage_to_info(usage));
                }
                state.responded = true;
                break;
            }
            Err(e) => {
                if matches!(e, SdkError::Cancelled) {
                    return Outcome::Fatal(anyhow::anyhow!("cancelled by user"));
                }
                interrupted = Some(e.to_string());
                break;
            }
        }
    }

    if state.responded {
        state.finalize(false, on_event);
        return Outcome::Completed;
    }
    if state.had_output() {
        let why = interrupted.unwrap_or_else(|| "upstream closed stream mid-content".to_string());
        log::warn!("Gemini SSE interrupted mid-stream, keeping partial output: {why}");
        state.finalize(false, on_event);
        return Outcome::Completed;
    }
    if let Some(why) = interrupted {
        log::warn!("Gemini stream ended with no content (will retry): {why}");
    }
    Outcome::EmptyStream
}

#[allow(clippy::too_many_arguments)] // 参数面塑形另立项（PLAN D-5）
pub fn chat_stream_gemini(
    provider: &ProviderConfig,
    model: &str,
    messages: Vec<Message>,
    tools: Option<Vec<ToolDef>>,
    max_tokens: u32,
    effort: Option<String>,
    _user_id: Option<String>,
    cancel: Option<&Arc<AtomicBool>>,
    on_event: &mut dyn FnMut(StreamEvent),
) -> anyhow::Result<()> {
    let messages = normalize_skill_envelope(provider, messages).map_err(anyhow::Error::msg)?;
    let request = build_request(
        provider,
        project_messages(messages, 0),
        tools,
        max_tokens,
        effort,
        true,
    );
    let policy = GateRetryPolicy::from_spec(provider.retry.as_ref());
    let (adapter, hub) = build_adapter(provider, model, &policy);
    let options = build_options(provider, &policy, true);
    let api_key = provider.api_key.as_str();

    empty_stream_retry(
        &policy,
        "upstream closed stream before any content",
        cancel,
        on_event,
        |traced| {
            block_on(run_sdk_stream(
                &adapter, &request, &options, api_key, &hub, cancel, traced,
            ))
        },
    )
}

pub fn chat_sync_gemini(
    provider: &ProviderConfig,
    model: &str,
    messages: Vec<Message>,
    max_tokens: u32,
) -> Result<String, String> {
    let messages = normalize_skill_envelope(provider, messages)?;
    let request = build_request(
        provider,
        project_messages(messages, 0),
        None,
        max_tokens,
        None,
        false,
    );
    let policy = GateRetryPolicy::from_spec(provider.retry.as_ref());
    let (adapter, _hub) = build_adapter(provider, model, &policy);
    let options = build_options(provider, &policy, false);
    match block_on(adapter.complete_with(&request, &options)) {
        Ok(response) => {
            let out: String = response
                .message
                .parts
                .iter()
                .filter_map(|p| match p {
                    Part::Text { text, .. } => Some(text.as_str()),
                    _ => None,
                })
                .collect::<Vec<_>>()
                .join("");
            if out.is_empty() {
                return Err("compact: no content in gemini response".to_string());
            }
            Ok(out)
        }
        Err(e) => {
            let (_msg, full) = sdk_error_detail("Gemini", &e, &provider.api_key);
            Err(format!("compact request failed: {full}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qaqh_types::ToolResult;

    fn gemini_provider() -> ProviderConfig {
        ProviderConfig::gemini(
            "https://example.com/v1beta",
            "sk-test",
            "gemini-3-flash",
            None,
        )
    }

    #[test]
    fn stream_path_switches_the_sync_suffix_and_keeps_custom_prefixes() {
        assert_eq!(
            stream_path_for("/models/{model}:generateContent"),
            "/models/{model}:streamGenerateContent"
        );
        assert_eq!(
            stream_path_for("/v1beta/models/{model}:generateContent"),
            "/v1beta/models/{model}:streamGenerateContent"
        );
        // 无同步后缀的覆写原样保留（同步/流式同路径）。
        assert_eq!(stream_path_for("/custom/gemini"), "/custom/gemini");
    }

    #[test]
    fn effort_ladder_maps_onto_the_sdk_levels() {
        assert_eq!(gemini_effort("low"), Some(ReasoningEffort::Low));
        assert_eq!(gemini_effort("medium"), Some(ReasoningEffort::Medium));
        assert_eq!(gemini_effort("high"), Some(ReasoningEffort::High));
        assert_eq!(gemini_effort("xhigh"), Some(ReasoningEffort::XHigh));
        assert_eq!(gemini_effort("max"), Some(ReasoningEffort::Max));
        assert_eq!(gemini_effort("ultra"), None);
    }

    #[test]
    fn thinking_is_requested_only_on_the_streaming_path() {
        let provider = gemini_provider();
        let streamed = build_request(
            &provider,
            Vec::new(),
            None,
            1024,
            Some("high".to_string()),
            true,
        );
        let reasoning = streamed.reasoning.expect("streaming requests reasoning");
        assert_eq!(reasoning.effort, Some(ReasoningEffort::High));
        assert_eq!(reasoning.include_text, Some(true));

        let sync = build_request(
            &provider,
            Vec::new(),
            None,
            1024,
            Some("high".to_string()),
            false,
        );
        assert!(
            sync.reasoning.is_none(),
            "sync path must not force thinking"
        );
    }

    #[test]
    fn tool_result_name_is_resolved_from_the_assistant_turn() {
        let history = vec![
            Message {
                msg_id: None,
                role: "assistant".into(),
                name: None,
                content: vec![ContentBlock::ToolUse {
                    id: "toolu_abc".into(),
                    name: "exec".into(),
                    input: serde_json::json!({"cmd": "ls"}),
                }],
            },
            Message::tool_result("toolu_abc", ToolResult::ok("done")),
        ];
        let projected = project_messages(history, 0);
        let tool = projected
            .iter()
            .find(|m| m.role == Role::Tool)
            .expect("tool message projected");
        let Part::ToolResult(tr) = &tool.parts[0] else {
            panic!("expected tool result part");
        };
        assert_eq!(tr.name, "exec", "functionResponse.name must match the call");
        assert_eq!(tr.call_id.as_deref(), Some("toolu_abc"));
    }

    #[test]
    fn assistant_reasoning_replays_as_a_thought_part_without_state() {
        let history = vec![Message {
            msg_id: None,
            role: "assistant".into(),
            name: None,
            content: vec![ContentBlock::Reasoning {
                reasoning: "先规划。".into(),
            }],
        }];
        let projected = project_messages(history, 0);
        let Part::Reasoning(reasoning) = projected[0]
            .parts
            .iter()
            .find(|p| matches!(p, Part::Reasoning(_)))
            .expect("reasoning part projected")
        else {
            unreachable!("find() matched a reasoning part");
        };
        assert_eq!(reasoning.text.as_deref(), Some("先规划。"));
        assert!(
            reasoning.state.is_none(),
            "signature is not persisted in history"
        );
    }
}
