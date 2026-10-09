//! Anthropic Messages transport built on the `mutil-ai` SDK (Phase 1
//! strangler). Replaces the hand-rolled reqwest/SSE/retry stack while keeping
//! the `chat_stream` / `chat_sync` contract: same `StreamEvent` sequence,
//! same partial-keep semantics on mid-stream interruption, same dual-auth /
//! OpenCode / session header compatibility via `RequestOptions`.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use futures::StreamExt;
use mutil_ai::{
    Anthropic, ChatRequest, Error as SdkError, ImagePart, Message as WireMessage, ModelAdapter,
    Part, ProviderState, ProviderStateFormat, Reasoning as WireReasoning, ReasoningConfig,
    ReasoningMode, RequestOptions, StreamEvent as SdkEvent, ToolCall as WireToolCall,
    ToolResult as WireToolResult, ToolResultPart, ToolSpec, TransportConfig, Usage as WireUsage,
};
use reqwest::header::{HeaderName, HeaderValue};

use crate::usage;
use qaqh_types::{ContentBlock, Message, ToolDef, UsageInfo};

use super::sdk_common::{
    CancelWatch, Outcome, RetryHub, StreamState, apply_management_headers, empty_stream_retry,
    insert_header, sdk_error_detail, sdk_retry_policy,
};
use super::transport::{
    RetryPolicy as GateRetryPolicy, block_on, is_cancelled, normalize_skill_envelope,
};
use super::types::{ProviderConfig, StreamEvent};

/// Same shape as the retired `message_api::build_anthropic_url`; the SDK
/// adapter appends `/messages` itself, so the result is stripped back into
/// `AnthropicMessages::base_url`.
fn build_anthropic_url(base_url: &str, anthropic_path: Option<&str>) -> String {
    if let Some(path) = anthropic_path {
        if path.starts_with("http") {
            return path.to_string();
        }
        let base = base_url.trim_end_matches('/');
        return format!("{}{}", base, path);
    }
    let base = base_url.trim_end_matches('/');
    if base.ends_with("/v1/messages") || base.ends_with("/api/anthropic/v1/messages") {
        base.to_string()
    } else {
        format!("{}/v1/messages", base)
    }
}

fn sdk_base_url(full_url: &str) -> String {
    full_url
        .trim_end()
        .trim_end_matches("/messages")
        .to_string()
}

fn map_anthropic_stop_reason(s: &str) -> String {
    match s {
        "end_turn" => "stop".to_string(),
        "tool_use" => "tool_calls".to_string(),
        "max_tokens" => "length".to_string(),
        _ => s.to_string(),
    }
}

/// 大上下文模型（`thinking_budget_large`，如 zcode GLM-5.3 1M 窗口）放宽到
/// 16k-96k；默认档保持 1k-16k。档位表与退役前的 `message_api` 一致。
fn thinking_budget(effort: &str, large: bool) -> u32 {
    if large {
        match effort {
            "low" => 16384,
            "medium" => 32768,
            "high" => 65536,
            "xhigh" => 81920,
            "max" => 96000,
            _ => 32768,
        }
    } else {
        match effort {
            "low" => 1024,
            "medium" => 2048,
            "high" => 4096,
            "xhigh" => 8192,
            "max" => 16384,
            _ => 4096,
        }
    }
}

fn extract_text(content: &[ContentBlock]) -> Option<String> {
    let mut out = String::new();
    for b in content {
        if let ContentBlock::Text { text } = b {
            out.push_str(text);
        }
    }
    if out.is_empty() { None } else { Some(out) }
}

/// Project harness history onto the SDK's neutral model. Wire-level rules
/// (same-role alternation, tool_result placement, orphan pairing) are left to
/// the SDK normalization layer; only harness semantics live here: system
/// joining, `read_image` placeholders, tool-image dedup (BUG-2026-09-16-03),
/// empty-user drop (BUG-2026-09-13-27), and bare-thinking replay via
/// `ProviderState` (the SDK only replays Anthropic `thinking` blocks that
/// carry provider state).
pub(crate) fn project_messages(
    messages: Vec<Message>,
    image_index_base: usize,
) -> Vec<WireMessage> {
    let mut system_parts: Vec<String> = Vec::new();
    let mut projected: Vec<WireMessage> = Vec::new();
    let mut img_idx = image_index_base;

    for msg in messages {
        match msg.role.as_str() {
            "system" | "developer" => {
                if let Some(t) = extract_text(&msg.content) {
                    system_parts.push(t);
                }
            }
            "user" => {
                let mut parts: Vec<Part> = Vec::new();
                for block in &msg.content {
                    match block {
                        ContentBlock::Text { text } => {
                            if !text.is_empty() {
                                parts.push(Part::text(text));
                            }
                        }
                        ContentBlock::Image { mime_type, data } => {
                            // Harness image placeholder — let model call `read_image`.
                            parts.push(Part::text(format!(
                                "[Image #{}: {}, ~{} bytes — call read_image(image_index={}) to view it yourself]",
                                img_idx, mime_type, data.len(), img_idx
                            )));
                            img_idx += 1;
                        }
                        ContentBlock::ImageRef {
                            mime_type,
                            bytes_len,
                            ..
                        } => {
                            // A-2 L0：外置图片仅持索引，占位符用 bytes_len 显示。
                            parts.push(Part::text(format!(
                                "[Image #{}: {}, ~{bytes_len} bytes — call read_image(image_index={}) to view it yourself]",
                                img_idx, mime_type, img_idx
                            )));
                            img_idx += 1;
                        }
                        _ => {}
                    }
                }
                if parts.is_empty() {
                    // BUG-2026-09-13-27：零信息 user 消息丢弃，避免不可重试的 400。
                    log::warn!("anthropic: dropping user message with no convertible content");
                    continue;
                }
                projected.push(WireMessage::new(mutil_ai::Role::User, parts));
            }
            "assistant" => {
                let mut parts: Vec<Part> = Vec::new();
                for block in &msg.content {
                    match block {
                        ContentBlock::Text { text } if !text.is_empty() => {
                            parts.push(Part::text(text));
                        }
                        ContentBlock::Reasoning { reasoning } if !reasoning.is_empty() => {
                            let mut r = WireReasoning::text(reasoning);
                            r.state = Some(ProviderState::new(
                                ProviderStateFormat::AnthropicMessages,
                                serde_json::json!({"type": "thinking", "thinking": reasoning}),
                            ));
                            parts.push(Part::Reasoning(r));
                        }
                        ContentBlock::ToolUse { id, name, input } => {
                            let mut call = WireToolCall::new(name.clone(), input.clone());
                            call.id = Some(id.clone());
                            parts.push(Part::ToolCall(call));
                        }
                        _ => {}
                    }
                }
                if !parts.is_empty() {
                    projected.push(WireMessage::new(mutil_ai::Role::Assistant, parts));
                }
            }
            "tool" => {
                // 去重：工具图在存储层是「内联 result.images + 兄弟 ImageRef」
                // 双份（BUG-2026-09-16-03），同一份字节只投影一次。
                let ref_count = msg
                    .content
                    .iter()
                    .filter(|b| matches!(b, ContentBlock::ImageRef { .. }))
                    .count();
                let mut skip_inline = ref_count;
                let mut parts: Vec<Part> = Vec::new();
                let mut last_result: Option<usize> = None;
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
                            for img in &result.images {
                                if skip_inline > 0 {
                                    skip_inline -= 1;
                                    continue;
                                }
                                tr.parts.push(ToolResultPart::Image {
                                    image: ImagePart::base64(&img.mime_type, &img.data),
                                });
                            }
                            parts.push(Part::ToolResult(tr));
                            last_result = Some(parts.len() - 1);
                        }
                        ContentBlock::Image { mime_type, data } => {
                            if skip_inline > 0 {
                                skip_inline -= 1;
                                continue;
                            }
                            if let Some(idx) = last_result {
                                attach_image(&mut parts, idx, mime_type, data);
                            } else {
                                log::warn!(
                                    "[gate] orphan image block in tool message, dropped from anthropic request"
                                );
                            }
                        }
                        ContentBlock::ImageRef {
                            sha256, mime_type, ..
                        } => {
                            // A-2 L0：按需读盘后走同一 base64 路径。
                            match qaqh_types::image_store::load_image_b64(sha256, mime_type) {
                                Ok(data) => {
                                    if let Some(idx) = last_result {
                                        attach_image(&mut parts, idx, mime_type, &data);
                                    } else {
                                        log::warn!(
                                            "[gate] image ref without owning tool result, dropped"
                                        );
                                    }
                                }
                                Err(e) => log::warn!(
                                    "[gate] image {sha256} load failed, dropped from anthropic request: {e}"
                                ),
                            }
                        }
                        _ => {}
                    }
                }
                if !parts.is_empty() {
                    projected.push(WireMessage::new(mutil_ai::Role::Tool, parts));
                }
            }
            _ => {}
        }
    }

    let mut out: Vec<WireMessage> = Vec::new();
    if !system_parts.is_empty() {
        out.push(WireMessage::system(system_parts.join("\n")));
    }
    out.extend(projected);
    out
}

fn attach_image(parts: &mut [Part], idx: usize, mime_type: &str, data: &str) {
    if let Some(Part::ToolResult(tr)) = parts.get_mut(idx) {
        tr.parts.push(ToolResultPart::Image {
            image: ImagePart::base64(mime_type, data),
        });
    }
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

fn build_adapter(
    provider: &ProviderConfig,
    model: &str,
    policy: &GateRetryPolicy,
) -> (mutil_ai::AnthropicMessages, Arc<RetryHub>) {
    let full = build_anthropic_url(&provider.base_url, provider.anthropic_path.as_deref());
    let mut transport = TransportConfig::default();
    transport.user_agent = HeaderValue::from_static(qaqh_types::QAQH_USER_AGENT);
    // 兼容层要求 x-api-key 之外同时携带 `Authorization: Bearer`（zcode/bun 网关），
    // SDK 默认保护该头——显式放行，除此之外不放宽任何受保护头。
    transport.header_policy =
        mutil_ai::HeaderPolicy::new().allow_override(HeaderName::from_static("authorization"));
    // SDK 的建连前重试（429/5xx/transport）只在 audit sink 上可见——转发为
    // harness 的 Retrying 事件（旧 run_with_retry 的可观测性不能丢）。
    let hub = Arc::new(RetryHub::new(policy.max_retries));
    transport.audit_sink = Some(hub.clone());
    transport.audit_config = mutil_ai::AuditConfig {
        enabled: true,
        ..Default::default()
    };
    (
        Anthropic::messages(model)
            .api_key(provider.api_key.clone())
            .base_url(sdk_base_url(&full))
            .retry_policy(sdk_retry_policy(policy))
            .transport(transport),
        hub,
    )
}

/// Dual-auth (`x-api-key` from the SDK + legacy `Authorization: Bearer`),
/// session passthrough, and OpenCode gateway management headers — same set
/// the retired hand-written client attached.
fn build_options(
    provider: &ProviderConfig,
    user_id: Option<&str>,
    policy: &GateRetryPolicy,
) -> RequestOptions {
    let mut options = RequestOptions::new();
    options.idle_timeout = Some(policy.idle_timeout);
    options.timeout = Some(Duration::from_secs(30 * 60));
    insert_header(
        &mut options,
        "Authorization",
        &format!("Bearer {}", provider.api_key),
    );
    apply_management_headers(provider, user_id, &mut options, true);
    options
}

/// Anthropic 语义：`input_tokens` **不含**缓存读写，全量输入 =
/// input + cache_read + cache_creation。只取 `input_tokens` 会把上下文用量
/// 报成零头（BUG-2026-09-16-02）——从 SDK 保留的原生 usage 里重算。
fn usage_to_info(u: &WireUsage) -> UsageInfo {
    let raw = u.raw.as_ref();
    let pick = |key: &str| -> Option<u32> {
        raw.and_then(|r| r.get(key))
            .and_then(|v| v.as_u64())
            .map(|v| v as u32)
    };
    let uncached = pick("input_tokens")
        .or(u.input_tokens.map(|v| v as u32))
        .unwrap_or(0);
    let cached = pick("cache_read_input_tokens")
        .or(u.cache_read_tokens.map(|v| v as u32))
        .unwrap_or(0);
    let created = pick("cache_creation_input_tokens")
        .or(u.cache_write_tokens.map(|v| v as u32))
        .unwrap_or(0);
    let completion = pick("output_tokens")
        .or(u.output_tokens.map(|v| v as u32))
        .unwrap_or(0);
    let pt = uncached.saturating_add(cached).saturating_add(created);
    // created 不计入 hit，单算写入（miss 侧）。
    let reported = cached != 0
        || created != 0
        || raw.is_some_and(|r| {
            r.get("cache_read_input_tokens").is_some()
                || r.get("cache_creation_input_tokens").is_some()
        })
        || u.cache_read_tokens.is_some()
        || u.cache_write_tokens.is_some();
    usage::normalize(
        usage::UsageSeed {
            prompt_tokens: pt,
            completion_tokens: completion,
            total_tokens: None,
            cache_hit_tokens: cached,
            cache_miss_tokens: uncached.saturating_add(created),
            cache_reported: reported,
            reasoning_tokens: u.reasoning_tokens.unwrap_or(0) as u32,
        },
        raw,
    )
}

async fn run_sdk_stream(
    adapter: &mutil_ai::AnthropicMessages,
    request: &ChatRequest,
    options: &RequestOptions,
    api_key: &str,
    hub: &RetryHub,
    cancel: Option<&Arc<AtomicBool>>,
    on_event: &mut dyn FnMut(StreamEvent),
) -> Outcome {
    // SDK 的退避睡眠/流读取只观察 CancellationToken；CancelWatch 用 50ms
    // 轮询把 harness 的 AtomicBool 桥接进去（对齐旧实现的取消粒度）。
    let watch = CancelWatch::start(cancel);
    let mut options = options.clone();
    options.cancellation = Some(watch.token().clone());
    consume(adapter, request, &options, api_key, hub, cancel, on_event).await
}

async fn consume(
    adapter: &mutil_ai::AnthropicMessages,
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
            let (msg, full) = sdk_error_detail("Anthropic", &e, api_key);
            on_event(StreamEvent::Error {
                kind,
                message: full,
            });
            return Outcome::Fatal(anyhow::anyhow!("{msg}"));
        }
    };
    hub.drain(&mut *on_event);

    let mut state = StreamState::new();
    // 帧内错误/流中断标记：有产出则走 partial-keep 收口（stop=None，
    // runtime 续写接管），零产出则 EmptyStream 整请求重试。
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
                let entry = state
                    .tools
                    .entry(index)
                    .or_insert_with(|| (String::new(), String::new(), String::new()));
                if (id.is_some() || name.is_some()) && entry.0.is_empty() && entry.1.is_empty() {
                    // 块起始：先按旧契约发一帧空参数（对齐 content_block_start）。
                    entry.0 = id.clone().unwrap_or_default();
                    entry.1 = name.clone().unwrap_or_default();
                    on_event(StreamEvent::ToolCallProgress {
                        index,
                        id: entry.0.clone(),
                        name: entry.1.clone(),
                        args_chunk: String::new(),
                    });
                }
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
            Ok(SdkEvent::ToolCallProgress { .. }) => {
                // 累计形态与 ToolCallDelta 重复，缓冲已覆盖。
            }
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
                response,
                finish_reason,
            }) => {
                if let Some(fr) = finish_reason {
                    state.stop = Some(map_anthropic_stop_reason(&fr));
                }
                // 旧实现在收口时兜底一次 usage（同一 UsageUpdate 语义）。
                if let Some(usage) = response.usage.as_ref() {
                    let u = usage_to_info(usage);
                    state.usage = Some(u);
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
        state.finalize(true, on_event);
        return Outcome::Completed;
    }
    if state.had_output() {
        let why = interrupted.unwrap_or_else(|| "upstream closed stream mid-content".to_string());
        log::warn!("Anthropic SSE interrupted mid-stream, keeping partial output: {why}");
        state.finalize(true, on_event);
        return Outcome::Completed;
    }
    if let Some(why) = interrupted {
        log::warn!("Anthropic stream ended with no content (will retry): {why}");
    }
    Outcome::EmptyStream
}

#[allow(clippy::too_many_arguments)] // 参数面塑形另立项（PLAN D-5）
pub fn chat_stream_anthropic(
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
    // 历史一律全量发送：gate 侧不做增量过滤，wire 级修补由 SDK normalize 负责。
    let mut request = ChatRequest::new(project_messages(messages, 0));
    request.tools = convert_tools(tools);
    let mut max_toks = if max_tokens == 0 { 8192 } else { max_tokens };
    let effort_norm = crate::types::normalize_reasoning_effort(effort.as_deref());
    // Thinking budget: only when provider explicitly supports it. If effort
    // is None we do not force thinking — let the provider default.
    if provider.supports_thinking
        && let Some(e) = effort_norm.as_deref()
    {
        let budget = thinking_budget(e, provider.thinking_budget_large);
        max_toks = max_toks.max(budget + 1024);
        request.reasoning = Some(
            ReasoningConfig::new()
                .mode(ReasoningMode::Enabled)
                .budget_tokens(budget),
        );
    }
    request.max_output_tokens = Some(max_toks);

    let policy = GateRetryPolicy::from_spec(provider.retry.as_ref());
    let (adapter, hub) = build_adapter(provider, model, &policy);
    let options = build_options(provider, user_id.as_deref(), &policy);

    // SDK 负责 HTTP 层重试（429/5xx/transport、Retry-After、Retrying 事件）；
    // 驱动只补它没覆盖的一类：2xx 后空流掐断，整请求重试无损。
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
                &provider.api_key,
                &hub,
                cancel,
                traced,
            ))
        },
    )
}

pub fn chat_sync_anthropic(
    provider: &ProviderConfig,
    model: &str,
    messages: Vec<Message>,
    max_tokens: u32,
) -> Result<String, String> {
    let messages = normalize_skill_envelope(provider, messages)?;
    // 同流式路径：全量历史，无增量过滤。
    let mut request = ChatRequest::new(project_messages(messages, 0));
    let mut max_toks = if max_tokens == 0 { 4096 } else { max_tokens };
    if provider.supports_thinking {
        // Keep a minimal budget headroom for sync thinking paths.
        max_toks = max_toks.max(4096);
    }
    request.max_output_tokens = Some(max_toks);

    let policy = GateRetryPolicy::from_spec(provider.retry.as_ref());
    let (adapter, _hub) = build_adapter(provider, model, &policy);
    let options = build_options(provider, None, &policy);
    let result = block_on(adapter.complete_with(&request, &options));
    match result {
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
                return Err("compact: no content in anthropic response".to_string());
            }
            Ok(out)
        }
        Err(e) => {
            let (_msg, full) = sdk_error_detail("Anthropic", &e, &provider.api_key);
            // sync 路径（compact/title）沿用旧文案：重试已由 SDK 内部完成，
            // 终态错误统一冠以 "compact request failed"。
            Err(format!("compact request failed: {full}"))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mutil_ai::{Protocol, normalize};

    fn qa_msg(role: &str, content: Vec<ContentBlock>) -> Message {
        Message {
            msg_id: None,
            role: role.into(),
            name: None,
            content,
        }
    }

    fn normalized(request: &ChatRequest) -> mutil_ai::NormalizedChat {
        normalize(request, Protocol::AnthropicMessages)
            .expect("normalize")
            .0
    }

    #[test]
    fn url_and_base_join_follow_legacy_shape() {
        let full = build_anthropic_url("https://api.anthropic.com", None);
        assert_eq!(full, "https://api.anthropic.com/v1/messages");
        assert_eq!(
            sdk_base_url(&full),
            "https://api.anthropic.com/v1",
            "SDK adapter appends /messages"
        );
        let zcode = build_anthropic_url(
            "https://open.bigmodel.cn",
            Some("/api/anthropic/v1/messages"),
        );
        assert_eq!(zcode, "https://open.bigmodel.cn/api/anthropic/v1/messages");
        assert_eq!(
            build_anthropic_url("https://open.bigmodel.cn/api/anthropic/v1/messages", None),
            "https://open.bigmodel.cn/api/anthropic/v1/messages"
        );
        let abs = build_anthropic_url("ignored", Some("http://localhost:8080/v1/messages"));
        assert_eq!(abs, "http://localhost:8080/v1/messages");
    }

    #[test]
    fn thinking_budget_tiers_match_legacy_table() {
        assert_eq!(thinking_budget("low", false), 1024);
        assert_eq!(thinking_budget("max", false), 16384);
        assert_eq!(thinking_budget("low", true), 16384);
        assert_eq!(thinking_budget("xhigh", true), 81920);
        assert_eq!(thinking_budget("max", true), 96000);
    }

    /// 回归 BUG-2026-09-16-02：全量输入 = input + cache_read + cache_creation。
    #[test]
    fn usage_counts_cached_input_from_raw() {
        let u = WireUsage {
            input_tokens: Some(481),
            output_tokens: Some(0),
            total_tokens: None,
            cache_read_tokens: Some(413_000),
            cache_write_tokens: Some(50),
            reasoning_tokens: None,
            raw: Some(serde_json::json!({
                "input_tokens": 481,
                "output_tokens": 0,
                "cache_creation_input_tokens": 50,
                "cache_read_input_tokens": 413_000
            })),
        };
        let info = usage_to_info(&u);
        assert_eq!(info.prompt_tokens, 481 + 413_000 + 50);
        assert_eq!(info.prompt_cache_hit_tokens, 413_000);
        assert_eq!(info.prompt_cache_miss_tokens, 481 + 50);
        assert_eq!(info.total_tokens, info.prompt_tokens);
        assert_eq!(info.cache_usage_reported, Some(true));
    }

    /// 回归 BUG-2026-09-16-03：内联图 + 兄弟 ImageRef 是同一份字节，只投影一次。
    #[test]
    fn tool_image_inline_and_ref_are_projected_once() {
        let b64 = "Zm9vYmFy";
        let sha =
            qaqh_types::image_store::store_image_b64(b64, "image/png").expect("store image b64");
        let result = qaqh_types::ToolResult::ok("image attached").with_image("image/png", b64);
        let msgs = vec![
            Message::user("look"),
            qa_msg(
                "assistant",
                vec![ContentBlock::ToolUse {
                    id: "toolu_1".into(),
                    name: "read_image".into(),
                    input: serde_json::json!({"path": "a.png"}),
                }],
            ),
            qa_msg(
                "tool",
                vec![
                    ContentBlock::ToolResult {
                        tool_use_id: "toolu_1".into(),
                        result,
                    },
                    ContentBlock::ImageRef {
                        sha256: sha,
                        mime_type: "image/png".into(),
                        bytes_len: b64.len(),
                    },
                ],
            ),
        ];
        let request = ChatRequest::new(project_messages(msgs, 0));
        let norm = normalized(&request);
        let tool_msg = norm
            .messages
            .iter()
            .find(|m| m.role == mutil_ai::ExternalRole::Tool)
            .expect("tool message");
        let Part::ToolResult(tr) = &tool_msg.parts[0] else {
            panic!("expected tool result part");
        };
        let images: Vec<_> = tr
            .parts
            .iter()
            .filter(|p| matches!(p, ToolResultPart::Image { .. }))
            .collect();
        assert_eq!(images.len(), 1, "同一张图只应投影一次");
    }

    /// 回归 BUG-2026-09-13-27：零信息 user 消息必须丢弃而不是兜底空 text。
    #[test]
    fn empty_user_message_is_dropped() {
        let msgs = vec![
            qa_msg(
                "user",
                vec![ContentBlock::Text {
                    text: String::new(),
                }],
            ),
            Message::user("real"),
        ];
        let projected = project_messages(msgs, 0);
        assert_eq!(projected.len(), 1);
    }

    #[test]
    fn system_is_top_level_joined_and_developer_included() {
        let msgs = vec![
            Message::system("base"),
            Message::developer("catalog"),
            Message::user("hi"),
        ];
        let request = ChatRequest::new(project_messages(msgs, 0));
        let norm = normalized(&request);
        assert_eq!(norm.system.as_deref(), Some("base\ncatalog"));
        assert!(
            !norm
                .messages
                .iter()
                .any(|m| m.role == mutil_ai::ExternalRole::System)
        );
    }

    #[test]
    fn assistant_reasoning_carries_anthropic_replay_state() {
        let msgs = vec![
            Message::user("think"),
            qa_msg(
                "assistant",
                vec![
                    ContentBlock::Reasoning {
                        reasoning: "hm hm".into(),
                    },
                    ContentBlock::text("done"),
                ],
            ),
        ];
        let projected = project_messages(msgs, 0);
        let assistant = &projected[1];
        let Part::Reasoning(r) = &assistant.parts[0] else {
            panic!("expected reasoning part");
        };
        let state = r.state.as_ref().expect("replay state");
        assert_eq!(state.format, ProviderStateFormat::AnthropicMessages);
        assert_eq!(state.data["type"], "thinking");
        assert_eq!(state.data["thinking"], "hm hm");
    }

    #[test]
    fn tool_results_paired_and_alternation_holds_after_normalize() {
        let msgs = vec![
            Message::user("run two commands"),
            qa_msg(
                "assistant",
                vec![
                    ContentBlock::ToolUse {
                        id: "toolu_1".into(),
                        name: "exec".into(),
                        input: serde_json::json!({ "command": "x" }),
                    },
                    ContentBlock::ToolUse {
                        id: "toolu_2".into(),
                        name: "exec".into(),
                        input: serde_json::json!({ "command": "y" }),
                    },
                ],
            ),
            qa_msg(
                "tool",
                vec![ContentBlock::ToolResult {
                    tool_use_id: "toolu_1".into(),
                    result: qaqh_types::ToolResult::ok("ran1"),
                }],
            ),
            qa_msg(
                "tool",
                vec![ContentBlock::ToolResult {
                    tool_use_id: "toolu_2".into(),
                    result: qaqh_types::ToolResult::ok("ran2"),
                }],
            ),
            qa_msg(
                "user",
                vec![ContentBlock::text(
                    "[workspace-changes] app.py is now empty",
                )],
            ),
        ];
        let request = ChatRequest::new(project_messages(msgs, 0));
        let norm = normalized(&request);
        // 每条 tool_result 配对完整；同角色相邻由 SDK 出口合并，
        // wire 上不会出现连续 user。
        let trs: Vec<_> = norm
            .messages
            .iter()
            .flat_map(|m| m.parts.iter())
            .filter(|p| matches!(p, Part::ToolResult(_)))
            .collect();
        assert_eq!(trs.len(), 2);
    }

    #[test]
    fn retry_policy_maps_gate_knobs_to_sdk() {
        let gate = GateRetryPolicy::default();
        let sdk = sdk_retry_policy(&gate);
        assert_eq!(sdk.max_attempts, gate.max_retries);
        assert!(
            !sdk.require_idempotency_key,
            "harness 请求无 idempotency key"
        );
        assert_eq!(sdk.max_retry_after, gate.max_delay.saturating_mul(5));
    }
}
