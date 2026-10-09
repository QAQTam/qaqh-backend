//! OpenAI Chat Completions transport built on the `mutil-ai` SDK (Phase 1
//! strangler). Same contract-preserving rules as `anthropic_sdk`; endpoint
//! quirks (thinking shapes, cache-token field, tool-call content encoding,
//! stream usage opt-in) are expressed as a declarative `ProviderProfile` +
//! `ProviderRequestOptions` instead of wire-level branches.
//!
//! Deliberate deltas from the retired hand-written client:
//! - mid-history `system` messages are hoisted to the top-level system by the
//!   SDK normalizer (they were inline before; only ordering changes);
//! - message-level `name` fields are not representable in the SDK's neutral
//!   model and are dropped (cosmetic; content is unchanged);
//! - DSML textual tool-call detection stays removed (2026-10-05 decision:
//!   structured `tool_calls` only).

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use futures::StreamExt;
use mutil_ai::{
    AuthStyle, ChatRequest, EndpointAdapter, EndpointSpec, Error as SdkError, ImagePart,
    MaxTokensSemantics, Message as WireMessage, ModelAdapter, Part, ProfileSelector,
    ProtocolSurface, ProviderProfile, ProviderRequestOptions, Reasoning as WireReasoning,
    ReasoningConfig, ReasoningMode, ReasoningReplayPolicy, RequestOptions, Role,
    StreamEvent as SdkEvent, ThinkingRequestProfile, ToolCall as WireToolCall,
    ToolResult as WireToolResult, ToolSpec, Usage as WireUsage,
};
use reqwest::header::HeaderValue;

use crate::usage;
use qaqh_types::{CacheTokenField, ContentBlock, Message, ThinkingParamMode, ToolDef, UsageInfo};

use super::sdk_common::{
    CancelWatch, Outcome, RetryHub, StreamState, apply_management_headers, empty_stream_retry,
    sdk_error_detail, sdk_retry_policy, split_url,
};
use super::transport::{
    RetryPolicy as GateRetryPolicy, block_on, is_cancelled, normalize_skill_envelope,
};
use super::types::{
    ProviderConfig, StreamEvent, clamp_effort_to_allowlist, normalize_reasoning_effort,
};

const MEDIA_ATTACHMENT_TEXT: &str = "Attached media from tool result:";

fn build_chat_url(base_url: &str, chat_path: Option<&str>) -> String {
    if let Some(path) = chat_path {
        if path.starts_with("http") {
            return path.to_string();
        }
        let base = base_url.trim_end_matches('/');
        return format!("{}{}", base, path);
    }
    let base = base_url.trim_end_matches('/');
    if base.ends_with("/chat/completions") {
        base.to_string()
    } else {
        format!("{}/chat/completions", base)
    }
}

/// Some compatible endpoints put reasoning inside `content` using think tags.
/// Split complete tags before events reach the frontend. The normal provider
/// fields remain the authoritative path; this is a compatibility guard.
fn split_inline_thinking(text: &str, in_thinking: &mut bool) -> Vec<(bool, String)> {
    let mut result = Vec::new();
    let mut rest = text;
    while !rest.is_empty() {
        let marker = if *in_thinking { "</think>" } else { "<think>" };
        match rest.find(marker) {
            Some(index) => {
                if index > 0 {
                    result.push((*in_thinking, rest[..index].to_string()));
                }
                *in_thinking = !*in_thinking;
                rest = &rest[index + marker.len()..];
            }
            None => {
                result.push((*in_thinking, rest.to_string()));
                break;
            }
        }
    }
    result
}

/// Project harness history onto the SDK's neutral model for Chat
/// Completions. Harness-level semantics preserved: `read_image` placeholders
/// with session-global numbering (continuing from `image_index_base`), and
/// tool-result images downgraded to synthetic `user` media messages that are
/// flushed only after the whole tool batch (an assistant message with
/// `tool_calls` must be followed by its tool messages, or strict endpoints
/// answer HTTP 400 — BUG-2026-09-16-01).
pub(crate) fn project_chat_messages(
    provider: &ProviderConfig,
    messages: Vec<Message>,
    image_index_base: usize,
) -> Vec<WireMessage> {
    let mut out: Vec<WireMessage> = Vec::new();
    let mut pending_media: Vec<WireMessage> = Vec::new();
    let mut img_idx = image_index_base;

    for msg in messages {
        if msg.role != "tool" && !pending_media.is_empty() {
            out.append(&mut pending_media);
        }
        match msg.role.as_str() {
            "system" | "developer" => {
                // `developer`（Responses 注入角色）在 Chat 协议降级为 system；
                // 只取首个 text 块，与退役前形态一致。
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
                if provider.supports_reasoning_content && !reasoning.is_empty() {
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
                    match block {
                        ContentBlock::ToolResult {
                            tool_use_id,
                            result,
                        } => {
                            let name = msg.name.clone().unwrap_or_else(|| tool_use_id.clone());
                            let mut tr = WireToolResult::new(
                                Some(tool_use_id.clone()),
                                name,
                                result.render_xml_envelope(),
                            );
                            tr.is_error = !result.is_success();
                            parts.push(Part::ToolResult(tr));
                        }
                        // 工具产出的图片（read_image）：OpenAI 兼容端点的 tool
                        // 消息只接受字符串 content，图片降级为合成 user 消息
                        // （opencode 同款策略），攒到整批 tool 消息之后再落盘。
                        ContentBlock::Image { mime_type, data } => {
                            pending_media.push(media_message(mime_type, data));
                        }
                        ContentBlock::ImageRef {
                            sha256, mime_type, ..
                        } => match qaqh_types::image_store::load_image_b64(sha256, mime_type) {
                            Ok(data) => pending_media.push(media_message(mime_type, &data)),
                            Err(e) => log::warn!(
                                "[gate] image {sha256} load failed, dropped from chat request: {e}"
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

/// 端点差异声明化：thinking 三形态、reasoning 字段别名与回放、max_tokens 语义。
/// MiniMax 的 `{"type":"adaptive"}` + `reasoning_split` 是静态字段，落在
/// `build_chat_request` 的 `extra_body`（受 supports_thinking 门控）。
fn chat_profile(provider: &ProviderConfig) -> Arc<ProviderProfile> {
    let mut p = ProviderProfile::new("qaqh-chat-compatible");
    p.reasoning.aliases.response_text = vec![
        "reasoning_content".to_string(),
        "reasoning".to_string(),
        "thinking".to_string(),
        "analysis_content".to_string(),
    ];
    p.reasoning.replay = if provider.supports_reasoning_content {
        ReasoningReplayPolicy::SameProvider
    } else {
        ReasoningReplayPolicy::Never
    };
    // 首条 system 提顶为前导项，运行时动态注入（skills catalog/envelope）
    // 留在历史原位——保住端点侧 prefix-cache 只从注入点向后失效。
    p.normalize.system_placement = mutil_ai::SystemPlacement::FirstToTopRestInPlace;
    p.reasoning.thinking = if !provider.supports_thinking {
        ThinkingRequestProfile::None
    } else {
        match provider.thinking_mode {
            ThinkingParamMode::OpenAi => ThinkingRequestProfile::ThinkingObject,
            ThinkingParamMode::QwenEnableThinking => {
                ThinkingRequestProfile::EnabledFlag("enable_thinking".to_string())
            }
            ThinkingParamMode::MiniMaxAdaptive => ThinkingRequestProfile::None,
        }
    };
    p.max_tokens_semantics = MaxTokensSemantics::MaxTokens;
    Arc::new(p)
}

fn build_adapter(
    provider: &ProviderConfig,
    model: &str,
    policy: &GateRetryPolicy,
) -> (EndpointAdapter, Arc<RetryHub>) {
    let full = build_chat_url(&provider.base_url, provider.chat_path.as_deref());
    let (base, path) = split_url(&full);
    let mut spec = EndpointSpec::new(ProtocolSurface::OpenAiChat, base, path, AuthStyle::Bearer);
    spec.profile = ProfileSelector::Custom(chat_profile(provider));
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
    let switches = ProviderRequestOptions {
        tool_call_content: Some(if provider.tool_call_content_null {
            mutil_ai::ToolCallContentMode::Null
        } else {
            mutil_ai::ToolCallContentMode::Omit
        }),
        include_stream_usage: streaming.then_some(provider.include_stream_usage),
        do_sample: streaming.then_some(provider.do_sample).flatten(),
        ..Default::default()
    };
    options.provider_request = switches;
    apply_management_headers(provider, None, &mut options, false);
    options
}

/// 流式请求体：thinking 门控、reasoning_effort（含 allowlist 吸附）、
/// OpenRouter `provider.require_parameters`、`user_id` 透传——
/// 全部经 profile/typed-switch/extra_body 表达，无运营商名判断。
fn build_chat_request(
    provider: &ProviderConfig,
    projected: Vec<WireMessage>,
    tools: Option<Vec<ToolDef>>,
    max_tokens: u32,
    effort: Option<String>,
    user_id: Option<String>,
    streaming: bool,
) -> ChatRequest {
    let mut request = ChatRequest::new(projected);
    request.tools = convert_tools(tools);
    request.max_output_tokens = Some(max_tokens);
    if provider.supports_thinking {
        request.reasoning = Some(ReasoningConfig::new().mode(ReasoningMode::Enabled));
        if matches!(provider.thinking_mode, ThinkingParamMode::MiniMaxAdaptive) {
            request
                .extra_body
                .insert("thinking".into(), serde_json::json!({"type": "adaptive"}));
            request
                .extra_body
                .insert("reasoning_split".into(), serde_json::json!(true));
        }
    }
    if streaming {
        if provider.supports_reasoning_effort
            && let Some(raw) = effort
        {
            let e = normalize_reasoning_effort(Some(raw.as_str())).unwrap_or(raw);
            let e = match &provider.effort_allowlist {
                Some(list) => clamp_effort_to_allowlist(&e, list),
                None => e,
            };
            request
                .extra_body
                .insert("reasoning_effort".into(), serde_json::json!(e));
        }
        if provider.require_provider_parameters && !request.tools.is_empty() {
            request.extra_body.insert(
                "provider".into(),
                serde_json::json!({"require_parameters": true}),
            );
        }
        if let Some(uid) = user_id
            && provider.user_id_mode.is_some()
            && !uid.is_empty()
        {
            request
                .extra_body
                .insert("user_id".into(), serde_json::json!(uid));
        }
    }
    request
}

/// Chat-completions usage：缓存 token 字段名按端点声明选取（`CacheTokenField`），
/// 从 SDK 保留的原生 usage JSON 读取。字段名差异留在本函数，口径与 `extras`
/// 交给 [`usage::normalize`]。
fn usage_to_info_chat(cache_field: &CacheTokenField, u: &WireUsage) -> UsageInfo {
    let raw = u.raw.as_ref();
    let get = |key: &str| -> Option<u32> {
        raw.and_then(|r| r.get(key))
            .and_then(|v| v.as_u64())
            .map(|v| v as u32)
    };
    let pt = get("prompt_tokens")
        .or(u.input_tokens.map(|v| v as u32))
        .unwrap_or(0);
    let ct = get("completion_tokens")
        .or(u.output_tokens.map(|v| v as u32))
        .unwrap_or(0);
    let (hit, miss, reported) = match cache_field {
        CacheTokenField::PromptCacheHitTokens => {
            let hit_value = raw.and_then(|r| r.get("prompt_cache_hit_tokens"));
            let miss_value = raw.and_then(|r| r.get("prompt_cache_miss_tokens"));
            (
                hit_value.and_then(|v| v.as_u64()).unwrap_or(0) as u32,
                miss_value.and_then(|v| v.as_u64()).unwrap_or(0) as u32,
                hit_value.is_some() || miss_value.is_some(),
            )
        }
        CacheTokenField::PromptDetailsCached => {
            let cached_value = raw
                .and_then(|r| r.get("prompt_tokens_details"))
                .and_then(|d| d.get("cached_tokens"));
            let cached = cached_value
                .and_then(|v| v.as_u64())
                .or(u.cache_read_tokens)
                .unwrap_or(0) as u32;
            (cached, pt.saturating_sub(cached), cached_value.is_some())
        }
        CacheTokenField::UsageCachedTokens => {
            let cached_value = raw.and_then(|r| r.get("cached_tokens"));
            let cached = cached_value.and_then(|v| v.as_u64()).unwrap_or(0) as u32;
            (cached, pt.saturating_sub(cached), cached_value.is_some())
        }
        CacheTokenField::None => (0, 0, false),
    };
    let rt = raw
        .and_then(|r| r.get("completion_tokens_details"))
        .and_then(|d| d.get("reasoning_tokens"))
        .and_then(|v| v.as_u64())
        .or(u.reasoning_tokens)
        .unwrap_or(0) as u32;
    usage::normalize(
        usage::UsageSeed {
            prompt_tokens: pt,
            completion_tokens: ct,
            // chat 端点自报的 total 历史上没被采用（一律 prompt + completion），
            // 保持原口径不动。
            total_tokens: None,
            cache_hit_tokens: hit,
            cache_miss_tokens: miss,
            cache_reported: reported,
            reasoning_tokens: rt,
        },
        raw,
    )
}

#[allow(clippy::too_many_arguments)] // strangler 内部管道函数（PLAN D-5 同口径）
async fn run_sdk_stream(
    adapter: &EndpointAdapter,
    request: &ChatRequest,
    options: &RequestOptions,
    api_key: &str,
    cache_field: &CacheTokenField,
    hub: &RetryHub,
    cancel: Option<&Arc<AtomicBool>>,
    on_event: &mut dyn FnMut(StreamEvent),
) -> Outcome {
    let watch = CancelWatch::start(cancel);
    let mut options = options.clone();
    options.cancellation = Some(watch.token().clone());
    consume(
        adapter,
        request,
        &options,
        api_key,
        cache_field,
        hub,
        cancel,
        on_event,
    )
    .await
}

#[allow(clippy::too_many_arguments)] // 与 consume 同形（strangler 内部函数）
async fn consume(
    adapter: &EndpointAdapter,
    request: &ChatRequest,
    options: &RequestOptions,
    api_key: &str,
    cache_field: &CacheTokenField,
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
            let (msg, full) = sdk_error_detail("OpenAI", &e, api_key);
            on_event(StreamEvent::Error {
                kind,
                message: full,
            });
            return Outcome::Fatal(anyhow::anyhow!("{msg}"));
        }
    };
    hub.drain(&mut *on_event);

    let mut state = StreamState::new();
    let mut inline_thinking = false;
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
                // inline <think> 切分：正文里夹带的思考标签不进 content。
                for (is_reasoning, t) in split_inline_thinking(&text, &mut inline_thinking) {
                    if t.is_empty() {
                        continue;
                    }
                    if is_reasoning {
                        state.reasoning.push_str(&t);
                        on_event(StreamEvent::ReasoningDelta(t));
                    } else {
                        state.text.push_str(&t);
                        on_event(StreamEvent::ContentDelta(t));
                    }
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
                // id/name 只在首帧采集（与退役前 or_insert 语义一致），
                // 仅参数增量触发进度事件；无参数的首帧不发帧（旧实现同）。
                let entry = state.tools.entry(index).or_insert_with(|| {
                    (
                        id.unwrap_or_default(),
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
                let u = usage_to_info_chat(cache_field, &usage);
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
                // 旧实现在 [DONE] 收口时 usage 已由末帧事件覆盖；此处兜底
                // 一次确保 Done 载荷非空（部分端点只在末帧带 usage）。
                if state.usage.is_none()
                    && let Some(usage) = response.usage.as_ref()
                {
                    state.usage = Some(usage_to_info_chat(cache_field, usage));
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
        log::warn!("OpenAI SSE interrupted mid-stream, keeping partial output: {why}");
        state.finalize(false, on_event);
        return Outcome::Completed;
    }
    if let Some(why) = interrupted {
        log::warn!("OpenAI stream ended with no content (will retry): {why}");
    }
    Outcome::EmptyStream
}

#[allow(clippy::too_many_arguments)] // 参数面塑形另立项（PLAN D-5）
pub fn chat_stream_openai(
    provider: &ProviderConfig,
    model: &str,
    messages: Vec<Message>,
    tools: Option<Vec<ToolDef>>,
    max_tokens: u32,
    effort: Option<String>,
    user_id: Option<String>,
    cancel: Option<&Arc<AtomicBool>>,
    on_event: &mut dyn FnMut(StreamEvent),
) -> anyhow::Result<()> {
    let messages = normalize_skill_envelope(provider, messages).map_err(anyhow::Error::msg)?;
    // 历史一律全量发送：gate 侧不做增量过滤。
    let request = build_chat_request(
        provider,
        project_chat_messages(provider, messages, 0),
        tools,
        max_tokens,
        effort,
        user_id,
        true,
    );
    let policy = GateRetryPolicy::from_spec(provider.retry.as_ref());
    let (adapter, hub) = build_adapter(provider, model, &policy);
    let options = build_options(provider, &policy, true);

    let cache_field = provider.cache_field.clone();
    let api_key = provider.api_key.as_str();
    empty_stream_retry(
        &policy,
        "upstream closed stream before any content",
        cancel,
        on_event,
        |traced| {
            block_on(run_sdk_stream(
                &adapter,
                &request,
                &options,
                api_key,
                &cache_field,
                &hub,
                cancel,
                traced,
            ))
        },
    )
}

pub fn chat_sync_openai(
    provider: &ProviderConfig,
    model: &str,
    messages: Vec<Message>,
    max_tokens: u32,
) -> Result<String, String> {
    let messages = normalize_skill_envelope(provider, messages)?;
    // 同流式路径：全量历史，无增量过滤。
    let request = build_chat_request(
        provider,
        project_chat_messages(provider, messages, 0),
        None,
        max_tokens,
        None,
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
                return Err("compact: no content in response".to_string());
            }
            Ok(out)
        }
        Err(e) => {
            let (_msg, full) = sdk_error_detail("OpenAI", &e, &provider.api_key);
            Err(format!("compact request failed: {full}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider() -> ProviderConfig {
        ProviderConfig::openai(
            "https://example.com/v1",
            "sk-test",
            "m",
            None,
            None,
            ThinkingParamMode::OpenAi,
            CacheTokenField::PromptCacheHitTokens,
            false,
            None,
        )
    }

    fn qa_msg(role: &str, content: Vec<ContentBlock>) -> Message {
        Message {
            msg_id: None,
            role: role.into(),
            name: None,
            content,
        }
    }

    #[test]
    fn chat_url_builder_follows_legacy_shape() {
        assert_eq!(
            build_chat_url("https://api.openai.com/v1", None),
            "https://api.openai.com/v1/chat/completions"
        );
        assert_eq!(
            build_chat_url("https://x.com/v1/chat/completions", None),
            "https://x.com/v1/chat/completions"
        );
        assert_eq!(
            build_chat_url("https://x.com/v1", Some("/proxy/chat")),
            "https://x.com/v1/proxy/chat"
        );
        assert_eq!(
            build_chat_url("ignored", Some("http://127.0.0.1:8317/v1/chat/completions")),
            "http://127.0.0.1:8317/v1/chat/completions"
        );
        let (base, path) = split_url(&build_chat_url("https://api.deepseek.com/v1", None));
        assert_eq!(
            (base.as_str(), path.as_str()),
            ("https://api.deepseek.com", "/v1/chat/completions")
        );
    }

    #[test]
    fn inline_think_tags_split_across_chunks() {
        let mut in_think = false;
        let mut reasoning = String::new();
        let mut text = String::new();
        for chunk in ["lead <think>deep", " thought</think> tail"] {
            for (is_r, t) in split_inline_thinking(chunk, &mut in_think) {
                if is_r {
                    reasoning.push_str(&t);
                } else {
                    text.push_str(&t);
                }
            }
        }
        assert_eq!(reasoning, "deep thought");
        assert_eq!(text, "lead  tail");
    }

    /// 复现 BUG-2026-09-16-01：合成 media user 消息不得插在两条 tool 消息之间。
    #[test]
    fn media_flushed_after_whole_tool_batch() {
        let assistant = qa_msg(
            "assistant",
            vec![
                ContentBlock::ToolUse {
                    id: "call-1".into(),
                    name: "read_image".into(),
                    input: serde_json::json!({"i":1}),
                },
                ContentBlock::ToolUse {
                    id: "call-2".into(),
                    name: "read_image".into(),
                    input: serde_json::json!({"i":2}),
                },
            ],
        );
        let tool = |id: &str, img: bool| {
            let mut content = vec![ContentBlock::ToolResult {
                tool_use_id: id.into(),
                result: qaqh_types::ToolResult::ok("ran"),
            }];
            if img {
                content.push(ContentBlock::image("image/png", "Zm9v"));
            }
            qa_msg("tool", content)
        };
        let msgs = vec![
            Message::user("go"),
            assistant,
            tool("call-1", true),
            tool("call-2", true),
            Message::user("after"),
        ];
        let out = project_chat_messages(&provider(), msgs, 0);
        let roles: Vec<&str> = out
            .iter()
            .map(|m| match &m.role {
                Role::User => "user",
                Role::Assistant => "assistant",
                Role::Tool => "tool",
                Role::System => "system",
                Role::Developer => "developer",
                Role::Custom(_) => "other",
            })
            .collect();
        // user, assistant, tool, tool, user(media), user(media), user(after)
        assert_eq!(
            roles,
            vec!["user", "assistant", "tool", "tool", "user", "user", "user"],
            "{roles:?}"
        );
        let Part::ToolResult(_) = &out[2].parts[0] else {
            panic!()
        };
        let has_image = matches!(&out[4].parts[1], Part::Image { .. });
        assert!(has_image, "media 消息须带 Image part");
    }

    #[test]
    fn reasoning_only_assistant_gets_thinking_complete_fallback() {
        let msgs = vec![
            Message::user("q"),
            qa_msg(
                "assistant",
                vec![ContentBlock::Reasoning {
                    reasoning: "hm".into(),
                }],
            ),
        ];
        let out = project_chat_messages(&provider(), msgs, 0);
        let assistant = &out[1];
        assert!(
            matches!(&assistant.parts[0], Part::Text { text, .. } if text == "[Thinking complete]")
        );
        // supports_reasoning_content=true（openai() 构造缺省）→ reasoning part 在
        assert!(matches!(&assistant.parts[1], Part::Reasoning(_)));

        let mut no_replay = provider();
        no_replay.supports_reasoning_content = false;
        let out = project_chat_messages(
            &no_replay,
            vec![
                Message::user("q"),
                qa_msg(
                    "assistant",
                    vec![ContentBlock::Reasoning {
                        reasoning: "hm".into(),
                    }],
                ),
            ],
            0,
        );
        assert_eq!(out[1].parts.len(), 1, "关闭回放时不得带 reasoning part");
    }

    #[test]
    fn empty_assistant_is_skipped() {
        let out = project_chat_messages(
            &provider(),
            vec![Message::user("q"), qa_msg("assistant", vec![])],
            0,
        );
        assert_eq!(out.len(), 1);
    }

    #[test]
    fn profile_maps_thinking_modes_and_replay() {
        let mut p = provider();
        p.supports_thinking = true;
        let prof = chat_profile(&p);
        assert_eq!(
            prof.reasoning.thinking,
            ThinkingRequestProfile::ThinkingObject
        );
        assert_eq!(prof.reasoning.replay, ReasoningReplayPolicy::SameProvider);

        p.thinking_mode = ThinkingParamMode::QwenEnableThinking;
        assert_eq!(
            chat_profile(&p).reasoning.thinking,
            ThinkingRequestProfile::EnabledFlag("enable_thinking".to_string())
        );
        p.thinking_mode = ThinkingParamMode::MiniMaxAdaptive;
        assert_eq!(
            chat_profile(&p).reasoning.thinking,
            ThinkingRequestProfile::None
        );

        p.supports_thinking = false;
        p.supports_reasoning_content = false;
        let prof = chat_profile(&p);
        assert_eq!(prof.reasoning.thinking, ThinkingRequestProfile::None);
        assert_eq!(prof.reasoning.replay, ReasoningReplayPolicy::Never);
    }

    #[test]
    fn minimax_static_thinking_lands_in_extra_body() {
        let mut p = provider();
        p.supports_thinking = true;
        p.thinking_mode = ThinkingParamMode::MiniMaxAdaptive;
        let req = build_chat_request(
            &p,
            vec![WireMessage::user("hi")],
            None,
            100,
            None,
            None,
            true,
        );
        assert_eq!(
            req.extra_body["thinking"],
            serde_json::json!({"type": "adaptive"})
        );
        assert_eq!(req.extra_body["reasoning_split"], serde_json::json!(true));
    }

    #[test]
    fn effort_is_normalized_clamped_and_sent_via_extra_body() {
        let mut p = provider();
        p.supports_reasoning_effort = true;
        p.effort_allowlist = Some(vec!["low".into(), "high".into(), "max".into()]);
        let req = build_chat_request(
            &p,
            vec![WireMessage::user("hi")],
            None,
            100,
            Some("medium".into()),
            None,
            true,
        );
        assert_eq!(
            req.extra_body["reasoning_effort"],
            serde_json::json!("high")
        );
        // none/minimal 类关闭值被提升为 low（QAQ 永远思考）
        let req = build_chat_request(
            &p,
            vec![WireMessage::user("hi")],
            None,
            100,
            Some("none".into()),
            None,
            true,
        );
        assert_eq!(req.extra_body["reasoning_effort"], serde_json::json!("low"));
    }

    #[test]
    fn openrouter_require_parameters_object_is_preserved() {
        let mut p = provider();
        p.require_provider_parameters = true;
        let tools = vec![ToolDef {
            call_type: "function".into(),
            function: qaqh_types::ToolFunction {
                name: "exec".into(),
                description: "d".into(),
                parameters: serde_json::json!({"type":"object"}),
            },
        }];
        let req = build_chat_request(
            &p,
            vec![WireMessage::user("hi")],
            Some(tools),
            100,
            None,
            None,
            true,
        );
        assert_eq!(
            req.extra_body["provider"],
            serde_json::json!({"require_parameters": true})
        );
        // 无工具时不发
        let req = build_chat_request(
            &p,
            vec![WireMessage::user("hi")],
            None,
            100,
            None,
            None,
            true,
        );
        assert!(!req.extra_body.contains_key("provider"));
    }

    #[test]
    fn cache_field_selection_follows_endpoint_declaration() {
        let raw = serde_json::json!({
            "prompt_tokens": 1000,
            "completion_tokens": 50,
            "prompt_cache_hit_tokens": 800,
            "prompt_cache_miss_tokens": 200,
            "prompt_tokens_details": {"cached_tokens": 700},
            "completion_tokens_details": {"reasoning_tokens": 20}
        });
        let u = WireUsage {
            input_tokens: None,
            output_tokens: None,
            total_tokens: None,
            cache_read_tokens: None,
            cache_write_tokens: None,
            reasoning_tokens: None,
            raw: Some(raw.clone()),
        };
        let hit_miss = usage_to_info_chat(&CacheTokenField::PromptCacheHitTokens, &u);
        assert_eq!(
            (
                hit_miss.prompt_cache_hit_tokens,
                hit_miss.prompt_cache_miss_tokens
            ),
            (800, 200)
        );
        assert_eq!(hit_miss.reasoning_tokens, 20);

        let details = usage_to_info_chat(&CacheTokenField::PromptDetailsCached, &u);
        assert_eq!(
            (
                details.prompt_cache_hit_tokens,
                details.prompt_cache_miss_tokens
            ),
            (700, 300)
        );

        let none = usage_to_info_chat(&CacheTokenField::None, &u);
        assert_eq!(
            (none.prompt_cache_hit_tokens, none.prompt_cache_miss_tokens),
            (0, 0)
        );
        assert_eq!(none.cache_usage_reported, Some(false));
        assert_eq!(none.total_tokens, 1050);
    }

    #[test]
    fn user_id_only_sent_when_mode_declared() {
        let mut p = provider();
        let req = build_chat_request(
            &p,
            vec![WireMessage::user("hi")],
            None,
            100,
            None,
            Some("u1".into()),
            true,
        );
        assert!(!req.extra_body.contains_key("user_id"));
        p.user_id_mode = Some(qaqh_types::UserSendMode::Body);
        let req = build_chat_request(
            &p,
            vec![WireMessage::user("hi")],
            None,
            100,
            None,
            Some("u1".into()),
            true,
        );
        assert_eq!(req.extra_body["user_id"], serde_json::json!("u1"));
    }
}
