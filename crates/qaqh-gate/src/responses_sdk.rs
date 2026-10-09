//! OpenAI Responses API transport built on the `mutil-ai` SDK (Phase 1
//! strangler). Replaces `responses_api.rs`; endpoint quirks become a
//! declarative `ProviderProfile` + typed request switches:
//! - `ResponsesCompat` knobs → `server_tools`(web_search), `include_encrypted`,
//!   `prompt_cache_key`/`user` switches, effort clamp (pure ladder math, no
//!   model-name specials);
//! - provider output items (`reasoning`/`web_search_call`/`message`) are
//!   replayed verbatim through `Part::ProviderItem` (OpenAiResponses state);
//! - `response.incomplete` keeps the legacy terminal contract (SDK surfaces it
//!   as a stream error; the bridge re-reads it as a normal Done carrying the
//!   truncation reason and the authoritative output items).

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

use futures::StreamExt;
use mutil_ai::{
    AuthStyle, ChatRequest, EndpointAdapter, EndpointSpec, Error as SdkError, ImagePart,
    Message as WireMessage, ModelAdapter, Part, ProfileSelector, ProtocolSurface, ProviderProfile,
    ProviderRequestOptions, ReasoningConfig, ReasoningEffort, ReasoningSummary, RequestOptions,
    Role, ServerTool, ServerToolItem, ServerToolState, StreamEvent as SdkEvent,
    ToolCall as WireToolCall, ToolResult as WireToolResult, ToolSpec, TransportConfig,
    Usage as WireUsage,
};
use reqwest::header::HeaderValue;

use crate::usage;
use qaqh_types::{ContentBlock, Message, ToolDef, UsageInfo};

use super::sdk_common::{
    CancelWatch, Outcome, RetryHub, apply_management_headers, empty_stream_retry, sdk_error_detail,
    sdk_retry_policy, split_url,
};
use super::transport::{
    RetryPolicy as GateRetryPolicy, block_on, empty_request_noop_done_event,
    empty_request_sync_error, is_cancelled, normalize_skill_envelope,
};
use super::types::{
    EFFORT_LADDER, ProviderConfig, ResponsesCompat, StreamEvent, normalize_reasoning_effort,
};

const MEDIA_ATTACHMENT_TEXT: &str = "Attached media from tool result:";

fn build_responses_url(base_url: &str, responses_path: Option<&str>) -> String {
    if let Some(path) = responses_path {
        if path.starts_with("http") {
            return path.to_string();
        }
        let base = base_url.trim_end_matches('/');
        return format!("{}{}", base, path);
    }
    let base = base_url.trim_end_matches('/');
    if base.ends_with("/responses") {
        base.to_string()
    } else {
        format!("{}/responses", base)
    }
}

/// QAQ 稳定工具名 → 端点名（`search` 在部分 Responses 端点是保留名）。
/// 映射只允许 confinement 在本适配器：授权、执行、事件与持久化历史继续用
/// canonical 名，出/入站各反转一次（与退役前一致）。
fn provider_function_name<'a>(name: &'a str, compat: &'a ResponsesCompat) -> &'a str {
    if name != "search" {
        return name;
    }
    compat
        .search_function_alias
        .as_deref()
        .filter(|alias| !alias.is_empty())
        .unwrap_or(name)
}

fn canonical_function_name(name: &str, compat: &ResponsesCompat) -> String {
    match compat.search_function_alias.as_deref() {
        Some(alias) if !alias.is_empty() && name == alias => "search".into(),
        _ => name.to_string(),
    }
}

/// Clamp a requested reasoning effort to the endpoint's declared upper bound
/// (pure ladder math on `ResponsesCompat::effort_max`; no model-name specials
/// — the muse `xhigh` bound now flows through the endpoint config).
fn clamp_effort(effort: Option<String>, max: &str) -> ReasoningEffort {
    let requested = effort
        .as_deref()
        .and_then(|e| normalize_reasoning_effort(Some(e)))
        .unwrap_or_else(|| "medium".into());
    let max_idx = EFFORT_LADDER.iter().position(|&v| v == max).unwrap_or(4);
    let idx = EFFORT_LADDER
        .iter()
        .position(|&v| v == requested)
        .unwrap_or(max_idx)
        .min(max_idx);
    match EFFORT_LADDER[idx] {
        "low" => ReasoningEffort::Low,
        "high" => ReasoningEffort::High,
        "xhigh" => ReasoningEffort::XHigh,
        "max" => ReasoningEffort::Max,
        _ => ReasoningEffort::Medium,
    }
}

/// Port of the retired schema sanitizer: OpenAI-compatible endpoints reject
/// or silently misbehave on JSON-schema members outside this allowlist
/// (`format`, `default`, custom keywords …), so the wire copy is reduced to
/// the structural subset before sending.
fn sanitize_openai_schema(value: &serde_json::Value) -> serde_json::Value {
    use serde_json::Value;
    const TYPES: &[&str] = &[
        "string", "number", "boolean", "integer", "object", "array", "null",
    ];
    const COMPOSITION_KEYS: &[&str] = &["anyOf", "oneOf", "allOf"];
    match value {
        Value::Bool(_) => serde_json::json!({"type": "string"}),
        Value::Array(arr) => Value::Array(arr.iter().map(sanitize_openai_schema).collect()),
        Value::Object(map) => {
            let mut result = serde_json::Map::new();
            if let Some(Value::String(s)) = map.get("$ref") {
                result.insert("$ref".into(), Value::String(s.clone()));
            }
            if let Some(Value::String(s)) = map.get("description") {
                result.insert("description".into(), Value::String(s.clone()));
            }
            if map.contains_key("const") {
                if let Some(v) = map.get("const") {
                    result.insert("enum".into(), Value::Array(vec![sanitize_openai_schema(v)]));
                }
            } else if let Some(Value::Array(arr)) = map.get("enum") {
                result.insert("enum".into(), Value::Array(arr.clone()));
            }
            if let Some(Value::Object(props)) = map.get("properties") {
                let mut new_props = serde_json::Map::new();
                for (k, v) in props {
                    new_props.insert(k.clone(), sanitize_openai_schema(v));
                }
                result.insert("properties".into(), Value::Object(new_props));
            }
            if let Some(Value::Array(req)) = map.get("required") {
                let filtered: Vec<Value> = req.iter().filter(|v| v.is_string()).cloned().collect();
                result.insert("required".into(), Value::Array(filtered));
            }
            if map.contains_key("items")
                && let Some(v) = map.get("items")
            {
                result.insert("items".into(), sanitize_openai_schema(v));
            }
            if map.contains_key("additionalProperties")
                && let Some(v) = map.get("additionalProperties")
            {
                let sanitized = if v.is_boolean() {
                    v.clone()
                } else {
                    sanitize_openai_schema(v)
                };
                result.insert("additionalProperties".into(), sanitized);
            }
            for key in COMPOSITION_KEYS {
                if let Some(Value::Array(arr)) = map.get(*key) {
                    let sanitized: Vec<Value> = arr.iter().map(sanitize_openai_schema).collect();
                    result.insert((*key).into(), Value::Array(sanitized));
                }
            }
            for key in ["$defs", "definitions"] {
                if let Some(Value::Object(obj)) = map.get(key) {
                    let mut new_defs = serde_json::Map::new();
                    for (k, v) in obj {
                        new_defs.insert(k.clone(), sanitize_openai_schema(v));
                    }
                    result.insert(key.into(), Value::Object(new_defs));
                }
            }
            let mut schema_types: Vec<String> = Vec::new();
            if let Some(t) = map.get("type") {
                match t {
                    Value::String(s) => {
                        if TYPES.contains(&s.as_str()) {
                            schema_types.push(s.clone());
                        }
                    }
                    Value::Array(arr) => {
                        for v in arr {
                            if let Value::String(s) = v
                                && TYPES.contains(&s.as_str())
                            {
                                schema_types.push(s.clone());
                            }
                        }
                    }
                    _ => {}
                }
            }
            if schema_types.is_empty()
                && (result.contains_key("$ref")
                    || COMPOSITION_KEYS.iter().any(|k| result.contains_key(*k)))
            {
                return Value::Object(result);
            }
            let inferred: Vec<String> = if !schema_types.is_empty() {
                schema_types.clone()
            } else if ["properties", "required", "additionalProperties"]
                .iter()
                .any(|k| map.contains_key(*k))
            {
                vec!["object".into()]
            } else if ["items", "prefixItems"]
                .iter()
                .any(|k| map.contains_key(*k))
            {
                vec!["array".into()]
            } else if result.contains_key("enum") || map.contains_key("format") {
                vec!["string".into()]
            } else if [
                "minimum",
                "maximum",
                "exclusiveMinimum",
                "exclusiveMaximum",
                "multipleOf",
            ]
            .iter()
            .any(|k| map.contains_key(*k))
            {
                vec!["number".into()]
            } else {
                vec![]
            };
            if inferred.is_empty() {
                return Value::Object(serde_json::Map::new());
            }
            if inferred.len() == 1 {
                result.insert("type".into(), Value::String(inferred[0].clone()));
            } else {
                result.insert(
                    "type".into(),
                    Value::Array(inferred.iter().map(|s| Value::String(s.clone())).collect()),
                );
            }
            if inferred.contains(&"object".to_string()) && !result.contains_key("properties") {
                result.insert("properties".into(), Value::Object(serde_json::Map::new()));
            }
            if inferred.contains(&"array".to_string()) && !result.contains_key("items") {
                result.insert("items".into(), serde_json::json!({"type": "string"}));
            }
            Value::Object(result)
        }
        _ => value.clone(),
    }
}

fn provider_state_item(item: serde_json::Value) -> Part {
    Part::ProviderItem(ServerToolItem::new(mutil_ai::ProviderState::new(
        mutil_ai::ProviderStateFormat::OpenAiResponses,
        item,
    )))
}

/// Project harness history onto the SDK's neutral Responses model.
/// 第一条 system → `instructions`（提顶）；后续 system/developer → 原位
/// `developer` item（与退役前逐字节同形态，依赖 SystemPlacement 策略）。
/// assistant 消息里持久化的 provider output items 原样回放
/// （`ResponseOutputItem`），QAQ 自己派生的 reasoning/text/function_call/
/// web_search 只在没有对应回放 item 时合成，防双份。
pub(crate) fn project_responses_messages(
    messages: &[Message],
    compat: &ResponsesCompat,
) -> Vec<WireMessage> {
    let mut out: Vec<WireMessage> = Vec::new();
    let mut top_taken = false;
    let mut img_idx: usize = 0;

    for msg in messages {
        match msg.role.as_str() {
            "system" => {
                let text = first_text(&msg.content);
                if !top_taken {
                    top_taken = true;
                    out.push(WireMessage::system(text));
                } else {
                    // 动态注入：原位 developer item（追加语义）。
                    out.push(WireMessage::new(Role::Developer, vec![Part::text(text)]));
                }
            }
            "developer" => {
                let text = first_text(&msg.content);
                out.push(WireMessage::new(Role::Developer, vec![Part::text(text)]));
            }
            "user" => {
                let mut parts: Vec<Part> = Vec::new();
                for b in &msg.content {
                    match b {
                        ContentBlock::Text { text } => parts.push(Part::text(text)),
                        ContentBlock::Image { mime_type, data } => {
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
                    parts.push(Part::text(""));
                }
                out.push(WireMessage::new(Role::User, parts));
            }
            "assistant" => {
                let raw_items: Vec<&serde_json::Value> = msg
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::ResponseOutputItem { item } => Some(item),
                        _ => None,
                    })
                    .collect();
                let item_type = |item: &serde_json::Value| -> String {
                    item.get("type")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string()
                };
                let has_response_type =
                    |expected: &str| raw_items.iter().any(|item| item_type(item) == expected);
                let mut parts: Vec<Part> = Vec::new();
                // 1) 回放 provider 原生 items（顺序保持原样）。
                for item in &raw_items {
                    let is_reasoning = item_type(item) == "reasoning";
                    if is_reasoning && !compat.echo_reasoning_content {
                        continue;
                    }
                    parts.push(provider_state_item((*item).clone()));
                }
                // 2) 派生 reasoning（无回放 item 且开关开启时合成旧形态）。
                if compat.echo_reasoning_content && !has_response_type("reasoning") {
                    for b in &msg.content {
                        if let ContentBlock::Reasoning { reasoning } = b
                            && !reasoning.is_empty()
                        {
                            parts.push(provider_state_item(serde_json::json!({
                                "type": "reasoning",
                                "content": [{"type": "reasoning_text", "text": reasoning}],
                            })));
                        }
                    }
                }
                // 3) 派生正文。
                let text_parts: Vec<String> = msg
                    .content
                    .iter()
                    .filter_map(|b| match b {
                        ContentBlock::Text { text } if !text.is_empty() => Some(text.clone()),
                        _ => None,
                    })
                    .collect();
                if !text_parts.is_empty() && !has_response_type("message") {
                    for t in text_parts {
                        parts.push(Part::text(t));
                    }
                }
                // 4) 派生 function_call（未被回放 item 覆盖的 call_id）。
                for b in &msg.content {
                    if let ContentBlock::ToolUse { id, name, input } = b {
                        let already = raw_items.iter().any(|item| {
                            item_type(item) == "function_call"
                                && item.get("call_id").and_then(|v| v.as_str()) == Some(id.as_str())
                        });
                        if already {
                            continue;
                        }
                        let mut call = WireToolCall::new(
                            provider_function_name(name, compat).to_string(),
                            input.clone(),
                        );
                        call.id = Some(id.clone());
                        parts.push(Part::ToolCall(call));
                    }
                }
                // 5) 派生 web_search_call 回放（未被覆盖的 id）。
                if compat.echo_web_search_call {
                    for b in &msg.content {
                        if let ContentBlock::WebSearchCall { id, action } = b {
                            let already = raw_items.iter().any(|item| {
                                item_type(item) == "web_search_call"
                                    && item.get("id").and_then(|v| v.as_str()) == Some(id.as_str())
                            });
                            if already {
                                continue;
                            }
                            parts.push(provider_state_item(serde_json::json!({
                                "type": "web_search_call",
                                "id": id,
                                "action": action,
                            })));
                        }
                    }
                }
                if !parts.is_empty() {
                    out.push(WireMessage::new(Role::Assistant, parts));
                }
            }
            "tool" => {
                let mut run: Vec<Part> = Vec::new();
                for b in &msg.content {
                    match b {
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
                            run.push(Part::ToolResult(tr));
                        }
                        // 工具图：紧随其后的合成 user message（input_image data URL）。
                        // Responses 协议不要求 assistant(tool_calls) 后必须连续
                        // tool item，原位发出即可（与退役前一致）。
                        ContentBlock::Image { mime_type, data } => {
                            out.push(WireMessage::new(
                                Role::User,
                                response_media_parts(&mut img_idx, mime_type, data),
                            ));
                        }
                        ContentBlock::ImageRef {
                            sha256, mime_type, ..
                        } => match qaqh_types::image_store::load_image_b64(sha256, mime_type) {
                            Ok(data) => out.push(WireMessage::new(
                                Role::User,
                                response_media_parts(&mut img_idx, mime_type, &data),
                            )),
                            Err(e) => log::warn!(
                                "[gate] image {sha256} load failed, dropped from responses request: {e}"
                            ),
                        },
                        _ => {}
                    }
                }
                if !run.is_empty() {
                    out.push(WireMessage::new(Role::Tool, run));
                }
            }
            _ => {}
        }
    }
    out
}

fn response_media_parts(_img_idx: &mut usize, mime_type: &str, data: &str) -> Vec<Part> {
    vec![
        Part::text(MEDIA_ATTACHMENT_TEXT),
        Part::Image {
            image: ImagePart::base64(mime_type, data),
        },
    ]
}

fn first_text(blocks: &[ContentBlock]) -> String {
    for b in blocks {
        if let ContentBlock::Text { text } = b {
            return text.clone();
        }
    }
    String::new()
}

fn responses_profile() -> Arc<ProviderProfile> {
    let mut p = ProviderProfile::new("qaqh-responses-compatible");
    // instructions 首条提顶 + 后续注入原位 developer item（PR#1 策略）。
    p.normalize.system_placement = mutil_ai::SystemPlacement::FirstToTopRestInPlace;
    // 内置 web_search 是服务端执行工具——Responses 端点族的这一能力由开关声明，
    // compat.web_search=false 的端点只是不塞 server_tools，能力位保持开不影响 wire。
    p.capabilities.server_side_state = true;
    Arc::new(p)
}

fn build_adapter(
    provider: &ProviderConfig,
    model: &str,
    policy: &GateRetryPolicy,
) -> (EndpointAdapter, Arc<RetryHub>) {
    let full = build_responses_url(&provider.base_url, provider.responses_path.as_deref());
    let (base, path) = split_url(&full);
    let mut spec = EndpointSpec::new(
        ProtocolSurface::OpenAiResponses,
        base,
        path,
        AuthStyle::Bearer,
    );
    spec.profile = ProfileSelector::Custom(responses_profile());
    let mut transport = TransportConfig::default();
    transport.user_agent = HeaderValue::from_static(qaqh_types::QAQH_USER_AGENT);
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

fn build_options(provider: &ProviderConfig, policy: &GateRetryPolicy) -> RequestOptions {
    let mut options = RequestOptions::new();
    options.idle_timeout = Some(policy.idle_timeout);
    options.timeout = Some(Duration::from_secs(30 * 60));
    let mut switches = ProviderRequestOptions::default();
    if let Some(ref pk) = provider.prompt_cache_key {
        switches.prompt_cache_key = Some(pk.clone());
    }
    options.provider_request = switches;
    apply_management_headers(provider, None, &mut options, false);
    options
}

fn build_request(
    provider: &ProviderConfig,
    projected: Vec<WireMessage>,
    tools: Option<Vec<ToolDef>>,
    max_tokens: u32,
    effort: Option<String>,
    user_id: Option<String>,
    streaming: bool,
) -> ChatRequest {
    let compat = &provider.responses_compat;
    let mut request = ChatRequest::new(projected);
    let specs: Vec<ToolSpec> = tools
        .unwrap_or_default()
        .into_iter()
        .map(|td| {
            ToolSpec::new(
                provider_function_name(&td.function.name, compat).to_string(),
                td.function.description,
                sanitize_openai_schema(&td.function.parameters),
            )
        })
        .collect();
    request.tools = specs;
    // 服务端内置搜索工具（模型可自主触发）：typed server_tools，非 wire 手拼。
    if compat.web_search {
        request.server_tools = vec![ServerTool::WebSearch];
    }
    if max_tokens > 0 {
        request.max_output_tokens = Some(max_tokens);
    }
    request.reasoning = Some(
        ReasoningConfig::new()
            .effort(clamp_effort(effort, compat.effort_max.as_str()))
            .summary(ReasoningSummary::Auto),
    );
    if compat.send_include {
        request.reasoning = request
            .reasoning
            .take()
            .map(|r| r.include_encrypted(true))
            .or_else(|| Some(ReasoningConfig::new().include_encrypted(true)));
    }
    // 旧 wire 契约保留成员：store=false（无服务端会话）、并行工具调用。
    request
        .extra_body
        .insert("store".into(), serde_json::json!(false));
    if streaming {
        request
            .extra_body
            .insert("parallel_tool_calls".into(), serde_json::json!(true));
        if compat.supports_user
            && let Some(uid) = user_id.filter(|s| !s.is_empty())
        {
            request
                .extra_body
                .insert("user".into(), serde_json::json!(uid));
        }
    }
    request
}

/// Responses usage：`input_tokens`/`output_tokens` +
/// `input_tokens_details.cached_tokens` +
/// `output_tokens_details.reasoning_tokens`（DeepSeek 语境口径），
/// cache miss 仅在端点上报 cached 时给出（与退役前一致）。
fn usage_to_info_responses(u: &WireUsage) -> UsageInfo {
    let input = u.input_tokens.unwrap_or(0) as u32;
    let output = u.output_tokens.unwrap_or(0) as u32;
    let cached_hit = u.cache_read_tokens.map(|v| v as u32);
    let reported = cached_hit.is_some();
    usage::normalize(
        usage::UsageSeed {
            prompt_tokens: input,
            completion_tokens: output,
            total_tokens: u.total_tokens.map(|v| v as u32),
            cache_hit_tokens: cached_hit.unwrap_or(0),
            cache_miss_tokens: if reported {
                input.saturating_sub(cached_hit.unwrap_or(0))
            } else {
                0
            },
            cache_reported: reported,
            reasoning_tokens: u.reasoning_tokens.unwrap_or(0) as u32,
        },
        u.raw.as_ref(),
    )
}

fn server_tool_status_name(state: ServerToolState) -> &'static str {
    match state {
        ServerToolState::InProgress => "in_progress",
        ServerToolState::Completed => "completed",
        ServerToolState::Failed => "failed",
        ServerToolState::Unknown => "in_progress",
    }
}

/// `response.incomplete` 等价收口：SDK 把它当流错误抛出（事件原文在
/// `ProviderStream(json)`），旧契约里它是**带截断原因的正常终态**。这里把
/// 事件里的 response 对象重新取用：output items → Done 块，reason → stop。
struct IncompleteTerminal {
    stop_reason: Option<String>,
    usage: Option<UsageInfo>,
    raw_message: Message,
    is_failed: bool,
    failure_message: String,
}

fn interpret_provider_stream(raw: &str) -> Option<IncompleteTerminal> {
    let event: serde_json::Value = serde_json::from_str(raw).ok()?;
    let typ = event.get("type").and_then(|v| v.as_str()).unwrap_or("");
    let response = event.get("response")?;
    if typ == "response.failed" || typ == "error" {
        let message = response
            .get("error")
            .and_then(|e| e.get("message"))
            .and_then(|m| m.as_str())
            .unwrap_or("response failed")
            .to_string();
        return Some(IncompleteTerminal {
            stop_reason: None,
            usage: None,
            raw_message: empty_assistant(),
            is_failed: true,
            failure_message: message,
        });
    }
    if typ != "response.incomplete" {
        return None;
    }
    let stop_reason = response
        .get("incomplete_details")
        .and_then(|d| d.get("reason"))
        .and_then(|v| v.as_str())
        .map(String::from);
    let mut blocks: Vec<ContentBlock> = Vec::new();
    if let Some(output) = response.get("output").and_then(|o| o.as_array()) {
        for item in output {
            let it = item.get("type").and_then(|v| v.as_str()).unwrap_or("");
            match it {
                // message/reasoning item：只作 ResponseOutputItem 回存；正文/思考
                // 文本来自 delta 缓冲（旧 emit_done 同款分工）。
                "message" | "reasoning" => {}
                "function_call" => {
                    let id = item
                        .get("call_id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let name = item
                        .get("name")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let input = item
                        .get("arguments")
                        .and_then(|v| v.as_str())
                        .and_then(|a| serde_json::from_str(a).ok())
                        .unwrap_or(serde_json::Value::Null);
                    if !id.is_empty() && !name.is_empty() {
                        blocks.push(ContentBlock::ToolUse { id, name, input });
                    }
                }
                "web_search_call" => {
                    let id = item
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let action = item
                        .get("action")
                        .cloned()
                        .unwrap_or_else(|| serde_json::json!({"type": "search"}));
                    blocks.push(ContentBlock::WebSearchCall { id, action });
                }
                _ => {}
            }
            blocks.push(ContentBlock::ResponseOutputItem { item: item.clone() });
        }
    }
    let usage = response.get("usage").map(|u| {
        let input_tokens = u.get("input_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        let output_tokens = u.get("output_tokens").and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        let cached_value = u
            .get("input_tokens_details")
            .and_then(|d| d.get("cached_tokens"));
        let cached = cached_value.and_then(|v| v.as_u64()).unwrap_or(0) as u32;
        let reported = cached_value.and_then(|v| v.as_u64()).is_some();
        usage::normalize(
            usage::UsageSeed {
                prompt_tokens: input_tokens,
                completion_tokens: output_tokens,
                total_tokens: u.get("total_tokens").and_then(|v| v.as_u64()).map(|v| v as u32),
                cache_hit_tokens: cached,
                cache_miss_tokens: if reported {
                    input_tokens.saturating_sub(cached)
                } else {
                    0
                },
                cache_reported: reported,
                reasoning_tokens: u
                    .get("output_tokens_details")
                    .and_then(|d| d.get("reasoning_tokens"))
                    .and_then(|v| v.as_u64())
                    .unwrap_or(0) as u32,
            },
            Some(u),
        )
    });
    Some(IncompleteTerminal {
        stop_reason,
        usage,
        raw_message: Message {
            msg_id: None,
            role: "assistant".into(),
            name: None,
            content: blocks,
        },
        is_failed: false,
        failure_message: String::new(),
    })
}

fn empty_assistant() -> Message {
    Message {
        msg_id: None,
        role: "assistant".into(),
        name: None,
        content: Vec::new(),
    }
}

/// Done 收口的 raw_message：由 SDK 组装好的 assistant message parts 映射回
/// harness 内容块（ProviderItem→ResponseOutputItem；web_search_call 额外派生
/// WebSearchCall 块；工具名反转 alias）。
fn response_message_to_blocks(msg: &WireMessage, compat: &ResponsesCompat) -> Vec<ContentBlock> {
    let mut blocks: Vec<ContentBlock> = Vec::new();
    for part in &msg.parts {
        match part {
            Part::Text { text, .. } => {
                if !text.is_empty() {
                    blocks.push(ContentBlock::text(text));
                }
            }
            Part::Reasoning(r) => {
                let t = r
                    .text
                    .clone()
                    .or_else(|| r.summary.clone())
                    .filter(|s| !s.is_empty());
                if let Some(reasoning) = t {
                    blocks.push(ContentBlock::Reasoning { reasoning });
                }
            }
            Part::ToolCall(call) => {
                let id = call.id.clone().unwrap_or_default();
                let name = canonical_function_name(&call.name, compat);
                blocks.push(ContentBlock::ToolUse {
                    id,
                    name,
                    input: call.arguments.clone(),
                });
            }
            Part::ProviderItem(item) => {
                let data = item.provider_state.data.clone();
                if data.get("type").and_then(|v| v.as_str()) == Some("web_search_call") {
                    let id = data
                        .get("id")
                        .and_then(|v| v.as_str())
                        .unwrap_or("")
                        .to_string();
                    let action = data
                        .get("action")
                        .cloned()
                        .unwrap_or_else(|| serde_json::json!({"type": "search"}));
                    blocks.push(ContentBlock::WebSearchCall { id, action });
                }
                blocks.push(ContentBlock::ResponseOutputItem { item: data });
            }
            Part::ToolResult(result) => {
                blocks.push(ContentBlock::ToolResult {
                    tool_use_id: result
                        .call_id
                        .clone()
                        .unwrap_or_else(|| result.name.clone()),
                    result: qaqh_types::ToolResult::ok(result.content.clone()),
                });
            }
            _ => {}
        }
    }
    blocks
}

#[derive(Default)]
struct Resps {
    text: String,
    reasoning: String,
    tools: HashMap<usize, (String, String, String)>,
    usage: Option<UsageInfo>,
}

impl Resps {
    fn had_output(&self) -> bool {
        !self.text.is_empty() || !self.reasoning.is_empty() || !self.tools.is_empty()
    }

    fn partial_done(self, on_event: &mut dyn FnMut(StreamEvent)) {
        let mut blocks: Vec<ContentBlock> = Vec::new();
        if !self.reasoning.is_empty() {
            blocks.push(ContentBlock::Reasoning {
                reasoning: self.reasoning,
            });
        }
        if !self.text.is_empty() {
            blocks.push(ContentBlock::text(&self.text));
        }
        let mut sorted: Vec<(usize, String, String, String)> = self
            .tools
            .into_iter()
            .map(|(i, t)| (i, t.0, t.1, t.2))
            .collect();
        sorted.sort_by_key(|(i, _, _, _)| *i);
        for (_i, id, name, args) in sorted {
            let input = serde_json::from_str(&args).unwrap_or(serde_json::Value::Null);
            blocks.push(ContentBlock::ToolUse { id, name, input });
        }
        on_event(StreamEvent::Done {
            raw_message: Message {
                msg_id: None,
                role: "assistant".into(),
                name: None,
                content: blocks,
            },
            usage: self.usage,
            stop_reason: None,
        });
    }
}

async fn consume(
    adapter: &EndpointAdapter,
    request: &ChatRequest,
    options: &RequestOptions,
    provider: &ProviderConfig,
    hub: &RetryHub,
    cancel: Option<&Arc<AtomicBool>>,
    on_event: &mut dyn FnMut(StreamEvent),
) -> Outcome {
    let compat = provider.responses_compat.clone();
    let mut stream = match adapter.stream_with(request, options).await {
        Ok(s) => s,
        Err(e) => {
            hub.drain(&mut *on_event);
            if matches!(e, SdkError::Cancelled) {
                return Outcome::Fatal(anyhow::anyhow!("cancelled by user"));
            }
            let kind = e.kind();
            let (msg, full) = sdk_error_detail("Responses", &e, &provider.api_key);
            on_event(StreamEvent::Error {
                kind,
                message: full,
            });
            return Outcome::Fatal(anyhow::anyhow!("{msg}"));
        }
    };
    hub.drain(&mut *on_event);

    let mut resp = Resps::default();
    let mut interrupted: Option<String> = None;

    loop {
        if is_cancelled(cancel) {
            return Outcome::Fatal(anyhow::anyhow!("cancelled by user"));
        }
        let Some(next) = stream.next().await else {
            break;
        };
        match next {
            Ok(SdkEvent::Start { .. }) => {}
            Ok(SdkEvent::TextDelta { text }) => {
                if !text.is_empty() {
                    resp.text.push_str(&text);
                    on_event(StreamEvent::ContentDelta(text));
                }
            }
            Ok(SdkEvent::ReasoningDelta { text, .. }) => {
                if !text.is_empty() {
                    resp.reasoning.push_str(&text);
                    on_event(StreamEvent::ReasoningDelta(text));
                }
            }
            Ok(SdkEvent::ToolCallDelta {
                index,
                id,
                name,
                arguments_delta,
            }) => {
                let entry = resp
                    .tools
                    .entry(index)
                    .or_insert_with(|| (String::new(), String::new(), String::new()));
                if entry.0.is_empty() && id.is_some() {
                    entry.0 = id.clone().unwrap_or_default();
                }
                if entry.1.is_empty() && name.is_some() {
                    entry.1 = name
                        .clone()
                        .map(|n| canonical_function_name(&n, &compat))
                        .unwrap_or_default();
                }
                entry.2.push_str(&arguments_delta);
            }
            Ok(SdkEvent::ToolCallProgress {
                index,
                id,
                name,
                arguments_so_far,
            }) => {
                let entry = resp
                    .tools
                    .entry(index)
                    .or_insert_with(|| (String::new(), String::new(), String::new()));
                if entry.0.is_empty() {
                    entry.0 = id.unwrap_or_default();
                }
                if entry.1.is_empty() {
                    entry.1 = name
                        .map(|n| canonical_function_name(&n, &compat))
                        .unwrap_or_default();
                }
                if !arguments_so_far.is_empty() {
                    entry.2 = arguments_so_far;
                }
            }
            Ok(SdkEvent::ServerToolStatus { state, .. }) => {
                on_event(StreamEvent::WebSearchStatus(
                    server_tool_status_name(state).to_string(),
                ));
            }
            Ok(SdkEvent::Usage { usage }) => {
                let u = usage_to_info_responses(&usage);
                resp.usage = Some(u.clone());
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
                // 旧契约的收口帧：每个 function call 在 item 完成时一次给全量
                // 参数（delta 只进缓冲不发帧）。先快照再遍历，避免借用冲突。
                let frames: Vec<(usize, String, String, String)> = response
                    .message
                    .parts
                    .iter()
                    .enumerate()
                    .filter_map(|(i, part)| match part {
                        Part::ToolCall(call) if !call.name.is_empty() => Some((
                            i,
                            call.id.clone().unwrap_or_default(),
                            canonical_function_name(&call.name, &compat),
                            serde_json::to_string(&call.arguments).unwrap_or_default(),
                        )),
                        _ => None,
                    })
                    .collect();
                for (index, id, name, args_chunk) in frames {
                    on_event(StreamEvent::ToolCallProgress {
                        index,
                        id,
                        name,
                        args_chunk,
                    });
                }
                // 旧契约：completed → stop "stop"；截断（incomplete_details.reason）
                // 经 ProviderStream 路径处理（见下）。无终止事件的合成收口
                // （raw = SSE chunks 数组而非 response 对象）→ stop None，
                // 让 runtime 的续写机制接管——与退役前 T7 契约一致。
                let stop = match finish_reason {
                    Some(fr) => Some(fr),
                    None if response.raw.is_array() => None,
                    None => Some("stop".to_string()),
                };
                if resp.usage.is_none()
                    && let Some(usage) = response.usage.as_ref()
                {
                    resp.usage = Some(usage_to_info_responses(usage));
                }
                let blocks = response_message_to_blocks(&response.message, &compat);
                on_event(StreamEvent::Done {
                    raw_message: Message {
                        msg_id: None,
                        role: "assistant".into(),
                        name: None,
                        content: blocks,
                    },
                    usage: resp.usage.clone(),
                    stop_reason: stop,
                });
                return Outcome::Completed;
            }
            Err(e) => {
                if matches!(e, SdkError::Cancelled) {
                    return Outcome::Fatal(anyhow::anyhow!("cancelled by user"));
                }
                // response.incomplete / response.failed 以事件原文形式落到这里：
                // 前者是带截断原因的正常终态，后者维持旧 Fatal 语义。
                if let SdkError::ProviderStream(raw) = &e
                    && let Some(term) = interpret_provider_stream(raw)
                {
                    if term.is_failed {
                        // 带内失败原文交给 SDK 分类（response.failed 的 error.code /
                        // 文案），不再由 gate 自己认字。
                        on_event(StreamEvent::Error {
                            kind: e.kind(),
                            message: term.failure_message.clone(),
                        });
                        return Outcome::Fatal(anyhow::anyhow!(term.failure_message));
                    }
                    if resp.usage.is_none() {
                        resp.usage = term.usage.clone();
                    }
                    if let Some(u) = term.usage.clone() {
                        on_event(StreamEvent::UsageUpdate(u));
                    }
                    // 旧 emit_done 顺序：reasoning 缓冲 → text 缓冲 → items 派生块
                    let mut blocks: Vec<ContentBlock> = Vec::new();
                    if !resp.reasoning.is_empty() {
                        blocks.push(ContentBlock::Reasoning {
                            reasoning: std::mem::take(&mut resp.reasoning),
                        });
                    }
                    if !resp.text.is_empty() {
                        let text = std::mem::take(&mut resp.text);
                        blocks.push(ContentBlock::text(&text));
                    }
                    blocks.extend(term.raw_message.content);
                    on_event(StreamEvent::Done {
                        raw_message: Message {
                            msg_id: None,
                            role: "assistant".into(),
                            name: None,
                            content: blocks,
                        },
                        usage: resp.usage.clone(),
                        stop_reason: term.stop_reason,
                    });
                    return Outcome::Completed;
                }
                interrupted = Some(e.to_string());
                break;
            }
        }
    }

    if resp.had_output() {
        let why = interrupted.unwrap_or_else(|| "upstream closed stream mid-content".to_string());
        log::warn!("Responses SSE interrupted mid-stream, keeping partial output: {why}");
        resp.partial_done(on_event);
        return Outcome::Completed;
    }
    if let Some(why) = interrupted {
        log::warn!("Responses stream ended with no content (will retry): {why}");
    }
    Outcome::EmptyStream
}

async fn run_sdk_stream(
    adapter: &EndpointAdapter,
    request: &ChatRequest,
    options: &RequestOptions,
    provider: &ProviderConfig,
    hub: &RetryHub,
    cancel: Option<&Arc<AtomicBool>>,
    on_event: &mut dyn FnMut(StreamEvent),
) -> Outcome {
    let watch = CancelWatch::start(cancel);
    let mut options = options.clone();
    options.cancellation = Some(watch.token().clone());
    consume(adapter, request, &options, provider, hub, cancel, on_event).await
}

#[allow(clippy::too_many_arguments)] // 参数面塑形另立项（PLAN D-5）
pub fn chat_stream_responses(
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
    let projected = project_responses_messages(&messages, &provider.responses_compat);
    // 空输入保护：`input: []` 会被上游判 400 且不可重试 → 整回合 Fatal。
    // 与退役前同语义：本地收口，零 HTTP 请求（instructions 单条不算 input）。
    if projected.iter().all(|m| m.role == Role::System) {
        log::warn!("responses: input 为空（无任何可投影消息），本地短路，不发请求");
        on_event(empty_request_noop_done_event());
        return Ok(());
    }
    let request = build_request(
        provider, projected, tools, max_tokens, effort, user_id, true,
    );
    let policy = GateRetryPolicy::from_spec(provider.retry.as_ref());
    let (adapter, hub) = build_adapter(provider, model, &policy);
    let options = build_options(provider, &policy);
    empty_stream_retry(
        &policy,
        "upstream closed stream before any content",
        cancel,
        on_event,
        |traced| {
            block_on(run_sdk_stream(
                &adapter, &request, &options, provider, &hub, cancel, traced,
            ))
        },
    )
}

pub fn chat_sync_responses(
    provider: &ProviderConfig,
    model: &str,
    messages: Vec<Message>,
    max_tokens: u32,
) -> Result<String, String> {
    let messages = normalize_skill_envelope(provider, messages)?;
    let projected = project_responses_messages(&messages, &provider.responses_compat);
    if projected.iter().all(|m| m.role == Role::System) {
        log::warn!("responses: input 为空（无任何可投影消息），sync 短路，不发请求");
        return Err(empty_request_sync_error());
    }
    let request = build_request(provider, projected, None, max_tokens, None, None, false);
    let policy = GateRetryPolicy::from_spec(provider.retry.as_ref());
    let (adapter, _hub) = build_adapter(provider, model, &policy);
    let options = build_options(provider, &policy);
    match block_on(adapter.complete_with(&request, &options)) {
        Ok(response) => {
            let out = mutil_ai::ChatResponse::text(&response);
            // BUG-2026-09-13-20：模型只输出 reasoning/工具调用时不得把空
            // 摘要当成功（compact 会污染压缩后上下文）。
            if out.trim().is_empty() {
                return Err("compact: no content in responses response".to_string());
            }
            Ok(out)
        }
        Err(e) => {
            let (_msg, full) = sdk_error_detail("Responses", &e, &provider.api_key);
            Err(full)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn provider() -> ProviderConfig {
        ProviderConfig::responses(
            "https://example.com/v1",
            "sk-test",
            "m",
            Some("/responses".into()),
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
    fn responses_url_and_split() {
        assert_eq!(
            build_responses_url("https://api.openai.com/v1", None),
            "https://api.openai.com/v1/responses"
        );
        let (base, path) = split_url(&build_responses_url("https://x.com/api/v1", None));
        assert_eq!(
            (base.as_str(), path.as_str()),
            ("https://x.com", "/api/v1/responses")
        );
    }

    #[test]
    fn first_system_goes_top_later_system_becomes_developer() {
        let msgs = vec![
            Message::system("base"),
            Message::system("catalog"),
            Message::developer("envelope"),
            Message::user("hi"),
        ];
        let out = project_responses_messages(&msgs, &provider().responses_compat);
        assert_eq!(out[0].role, Role::System);
        assert_eq!(out[1].role, Role::Developer);
        assert_eq!(out[2].role, Role::Developer);
        assert_eq!(out[3].role, Role::User);
    }

    #[test]
    fn assistant_replays_raw_items_and_skips_duplicate_derivations() {
        let mut compat = provider().responses_compat.clone();
        compat.echo_reasoning_content = true;
        let raw_call = serde_json::json!({
            "type": "function_call", "call_id": "c1", "name": "exec", "arguments": "{\"a\":1}"
        });
        let msgs = vec![
            Message::user("go"),
            qa_msg(
                "assistant",
                vec![
                    ContentBlock::ResponseOutputItem { item: raw_call },
                    ContentBlock::ToolUse {
                        id: "c1".into(),
                        name: "exec".into(),
                        input: serde_json::json!({"a": 1}),
                    },
                    ContentBlock::ToolUse {
                        id: "c2".into(),
                        name: "read".into(),
                        input: serde_json::json!({"p": "x"}),
                    },
                ],
            ),
        ];
        let out = project_responses_messages(&msgs, &compat);
        let assistant = &out[1];
        let provider_items = assistant
            .parts
            .iter()
            .filter(|p| matches!(p, Part::ProviderItem(_)))
            .count();
        let calls: Vec<&WireToolCall> = assistant
            .parts
            .iter()
            .filter_map(|p| match p {
                Part::ToolCall(c) => Some(c),
                _ => None,
            })
            .collect();
        assert_eq!(provider_items, 1, "回放 item 原样保留");
        assert_eq!(
            calls.len(),
            1,
            "call_id 已被回放覆盖的派生 ToolUse 不再合成（防双份）"
        );
        assert_eq!(calls[0].id.as_deref(), Some("c2"));
    }

    #[test]
    fn echo_gating_drops_reasoning_items_and_bodies() {
        let mut compat = provider().responses_compat.clone();
        compat.echo_reasoning_content = false;
        let msgs = vec![
            Message::user("go"),
            qa_msg(
                "assistant",
                vec![
                    ContentBlock::ResponseOutputItem {
                        item: serde_json::json!({"type": "reasoning", "summary": []}),
                    },
                    ContentBlock::Reasoning {
                        reasoning: "text".into(),
                    },
                ],
            ),
        ];
        let out = project_responses_messages(&msgs, &compat);
        // 关闭回放后该 assistant 无任何可发内容——整条跳过（旧行为同）。
        assert_eq!(out.len(), 1, "reasoning-only assistant 应被丢弃：{out:?}");
    }

    #[test]
    fn search_alias_roundtrip() {
        let mut p = provider();
        p.responses_compat.search_function_alias = Some("web_search_tool".into());
        let compat = p.responses_compat.clone();
        let td = ToolDef {
            call_type: "function".into(),
            function: qaqh_types::ToolFunction {
                name: "search".into(),
                description: "d".into(),
                parameters: serde_json::json!({"type": "object"}),
            },
        };
        let req = build_request(
            &p,
            vec![WireMessage::user("hi")],
            Some(vec![td]),
            100,
            None,
            None,
            true,
        );
        assert_eq!(req.tools[0].name, "web_search_tool");
        assert_eq!(
            canonical_function_name("web_search_tool", &compat),
            "search"
        );
    }

    #[test]
    fn effort_clamps_to_declared_bound() {
        // 端点声明 effort_max=high（OpenAI 缺省）：xhigh/max 都被夹到 high
        assert_eq!(
            clamp_effort(Some("max".into()), "high"),
            ReasoningEffort::High
        );
        // 端点声明 xhigh（muse 类网关经 responses_effort_max 配置，无模型名特判）
        assert_eq!(
            clamp_effort(Some("max".into()), "xhigh"),
            ReasoningEffort::XHigh
        );
        // 关闭思考的值被提升为 low（QAQ 永远思考）；未知值取端点上限
        assert_eq!(
            clamp_effort(Some("none".into()), "high"),
            ReasoningEffort::Low
        );
        assert_eq!(
            clamp_effort(Some("turbo".into()), "max"),
            ReasoningEffort::Max
        );
        assert_eq!(clamp_effort(None, "high"), ReasoningEffort::Medium);
    }

    #[test]
    fn typed_switches_and_reserved_body_members() {
        let mut p = provider();
        p.responses_compat.supports_user = true;
        p.responses_compat.web_search = true;
        p.prompt_cache_key = Some("seed-1".into());
        let req = build_request(
            &p,
            vec![WireMessage::user("hi")],
            None,
            0,
            None,
            Some("u42".into()),
            true,
        );
        assert_eq!(req.server_tools, vec![ServerTool::WebSearch]);
        assert_eq!(req.extra_body["store"], serde_json::json!(false));
        assert_eq!(
            req.extra_body["parallel_tool_calls"],
            serde_json::json!(true)
        );
        assert_eq!(req.extra_body["user"], serde_json::json!("u42"));
        assert!(
            req.max_output_tokens.is_none(),
            "0 = 不发 max_output_tokens"
        );
        let opts = build_options(&p, &GateRetryPolicy::default());
        assert_eq!(
            opts.provider_request.prompt_cache_key.as_deref(),
            Some("seed-1")
        );
    }

    #[test]
    fn usage_maps_deepseek_cache_and_reasoning() {
        let u = WireUsage {
            input_tokens: Some(1000),
            output_tokens: Some(50),
            total_tokens: Some(1050),
            cache_read_tokens: Some(700),
            cache_write_tokens: None,
            reasoning_tokens: Some(20),
            raw: None,
        };
        let info = usage_to_info_responses(&u);
        assert_eq!(info.prompt_cache_hit_tokens, 700);
        assert_eq!(info.prompt_cache_miss_tokens, 300);
        assert_eq!(info.reasoning_tokens, 20);
        assert_eq!(info.cache_usage_reported, Some(true));
        let silent = WireUsage {
            input_tokens: Some(10),
            output_tokens: Some(1),
            total_tokens: Some(11),
            cache_read_tokens: None,
            cache_write_tokens: None,
            reasoning_tokens: None,
            raw: None,
        };
        let info = usage_to_info_responses(&silent);
        assert_eq!(
            info.prompt_cache_miss_tokens, 0,
            "端点未上报 cached 时 miss=0（旧语义）"
        );
        assert_eq!(info.cache_usage_reported, Some(false));
    }

    #[test]
    fn incomplete_event_reads_as_terminal_with_reason_and_items() {
        let event = serde_json::json!({
            "type": "response.incomplete",
            "response": {
                "incomplete_details": {"reason": "max_output_tokens"},
                "output": [
                    {"type": "message", "role": "assistant", "content": [
                        {"type": "output_text", "text": "half a story"}]},
                    {"type": "function_call", "call_id": "c1", "name": "exec",
                     "arguments": "{\"command\":\"x\"}"}
                ],
                "usage": {"input_tokens": 9, "output_tokens": 3, "total_tokens": 12}
            }
        });
        let term = interpret_provider_stream(&event.to_string()).expect("incomplete terminal");
        assert!(!term.is_failed);
        assert_eq!(term.stop_reason.as_deref(), Some("max_output_tokens"));
        // message item 只回存；function_call → ToolUse + 回存（旧 preserve 形态）
        assert_eq!(
            term.raw_message.content.len(),
            3,
            "{:?}",
            term.raw_message.content
        );
        assert!(matches!(
            &term.raw_message.content[0],
            ContentBlock::ResponseOutputItem { .. }
        ));
        assert!(matches!(
            &term.raw_message.content[1],
            ContentBlock::ToolUse { id, .. } if id == "c1"
        ));
        assert!(term.usage.is_some());
    }

    #[test]
    fn failed_event_reads_as_fatal() {
        let event = serde_json::json!({
            "type": "response.failed",
            "response": {"error": {"message": "server is overloaded"}}
        });
        let term = interpret_provider_stream(&event.to_string()).expect("failed terminal");
        assert!(term.is_failed);
        assert_eq!(term.failure_message, "server is overloaded");
    }

    #[test]
    fn sanitizer_strips_nonstructural_keys() {
        let schema = serde_json::json!({
            "type": "object",
            "properties": {
                "cmd": {"type": ["string", "null"], "format": "shell", "default": "ls"},
                "n": {"minimum": 1}
            },
            "required": ["cmd", 5]
        });
        let cleaned = sanitize_openai_schema(&schema);
        // 多类型保留为数组；format/default 等非结构键剥离
        assert_eq!(
            cleaned["properties"]["cmd"]["type"],
            serde_json::json!(["string", "null"])
        );
        assert!(cleaned["properties"]["cmd"].get("format").is_none());
        assert_eq!(cleaned["properties"]["n"]["type"], "number");
        assert_eq!(cleaned["required"], serde_json::json!(["cmd"]));
    }
}
