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

mod projection;
mod reconcile;

pub(crate) use projection::project_responses_messages;
#[cfg(test)]
use projection::provider_state_item;
#[cfg(all(test, feature = "native-tool-plan"))]
use projection::native_call_from_item;
#[cfg(test)]
use reconcile::freeform_custom_blocks;
use reconcile::{assemble_terminal_blocks, interpret_provider_stream, reconcile_streamed_tool_calls};

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use futures::StreamExt;
use mutil_ai::{
    ChatRequest, CustomToolFormat, EndpointAdapter, EndpointSpec, Error as SdkError,
    Message as WireMessage, ModelAdapter, Part, ProfileSelector, ProtocolSurface, ProviderProfile,
    ProviderRequestOptions, ReasoningConfig, ReasoningEffort, ReasoningSummary, RequestOptions,
    Role, ScopedToolDeclaration, ServerTool, ServerToolState, StreamEvent as SdkEvent,
    ToolDeclaration, ToolSpec, TransportConfig, Usage as WireUsage,
};
#[cfg(test)]
use mutil_ai::ToolCall as WireToolCall;
#[cfg(feature = "native-tool-plan")]
use qaqh_tool_core::model_tool_plan::{
    CustomFormat, ModelToolDeclaration, ModelToolKind, ModelToolPlan, NativeToolCall as PlanCall,
    NativeToolCompletion as PlanCompletion, NativeToolPayload as PlanPayload,
};

use reqwest::header::HeaderValue;

use crate::usage;
use qaqh_types::{ContentBlock, Message, ToolDef, UsageInfo};

/// feature `native-tool-plan` 关闭时的占位类型：**不可构造**，因此流式路径上的
/// `plan: Option<&ModelToolPlan>` 恒为 `None`，原生工具调用一律 fail-closed
/// （见下面的 `adapt_native` stub）。
#[cfg(not(feature = "native-tool-plan"))]
#[derive(Debug)]
pub(super) enum ModelToolPlan {}

use super::sdk_common::{
    CancelWatch, Outcome, REQUEST_TOTAL_TIMEOUT, RetryHub, adopt_identity,
    apply_management_headers, empty_stream_backoff, insert_header, roomy_response_limits,
    sdk_error_detail, sdk_retry_policy, split_url,
};
use super::transport::{
    RetryPolicy as GateRetryPolicy, SseTrace, empty_request_noop_done_event, is_cancelled,
};
use super::types::{
    EFFORT_LADDER, ProviderConfig, ResponsesCompat, StreamEvent, normalize_reasoning_effort,
    stored_reasoning_kind,
};

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

pub(super) fn canonical_function_name(name: &str, compat: &ResponsesCompat) -> String {
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
        "ultra" => ReasoningEffort::Ultra,
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
        // 凭据头由 provider 决定（账号类凭据不是 Bearer）。
        provider.auth_style(),
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
    options.timeout = Some(REQUEST_TOTAL_TIMEOUT);
    options.response_limits = roomy_response_limits();
    let mut switches = ProviderRequestOptions::default();
    // plan 直连路由**要**下发 `prompt_cache_key`：官方 preview-limitations 的禁用
    // 清单只有 `prompt_cache_retention`（见 SIWC_PLAN_FORBIDDEN_EXTRA_FIELDS），
    // 没有 `prompt_cache_key`；codex 在同一路由上总是发它（codex-rs/core/src/
    // client.rs:1049）。此前按「同族」一并屏蔽是误判，直接后果是 store:false 的
    // 无状态请求每轮都拿不到前缀缓存。
    if let Some(ref pk) = provider.prompt_cache_key {
        switches.prompt_cache_key = Some(pk.clone());
    }
    options.provider_request = switches;
    apply_management_headers(provider, None, &mut options, false);
    // OAuth 账号面头（ChatgptPlan 凭据来源注入）：多账号/工作区按 id_token
    // claim 路由 + FedRAMP 标记。空 Vec = 普通端点，一个头都不加。
    for (name, value) in &provider.chatgpt_account_headers {
        insert_header(&mut options, name, value);
    }
    // DeepSeek OAuth 账号面头（DeepseekAccount 凭据来源注入）：会话粘性路由与
    // 缓存亲和头（x-deepseek-harness-session-id / x-deepseek-harness-user-id）。
    for (name, value) in &provider.deepseek_account_headers {
        insert_header(&mut options, name, value);
    }
    options
}

/// 「ChatGPT 套餐用量」直连路由**禁用**的请求字段
/// （`developers.openai.com/siwc/token-sharing-open-source/preview-limitations`）。
///
/// 之所以做成「无论谁塞进来都摘掉」而不是「记得别塞」：被禁字段一旦上线就是
/// 400 `subscription_sharing_unsupported_capability`，而该错误码按文档意味着
/// **我方发错了字段**——它不该靠人工 review 守住。
const SIWC_PLAN_FORBIDDEN_EXTRA_FIELDS: &[&str] = &[
    "background",
    "conversation",
    "max_output_tokens",
    "max_tool_calls",
    "metadata",
    "moderation",
    "multi_agent",
    "prompt",
    "prompt_cache_retention",
    "safety_identifier",
    "temperature",
    "top_logprobs",
    "top_p",
    "truncation",
    "previous_response_id",
    "user",
];

/// plan 模式的最终收口：清结构化字段 + 摘掉被禁成员 + 关闭 hosted 工具。
///
/// 放在 [`build_request`] 的**最后**一步：任何先于它的代码路径（现在的或将来加的）
/// 塞进来的被禁字段都会被摘掉，不依赖调用方自觉。
fn apply_siwc_plan(provider: &ProviderConfig, request: &mut ChatRequest) {
    if !provider.responses_compat.siwc_plan {
        return;
    }
    request.max_output_tokens = None;
    request.server_tools.clear();
    for field in SIWC_PLAN_FORBIDDEN_EXTRA_FIELDS {
        request.extra_body.remove(*field);
    }
    strip_reasoning_content_for_plan(request);
}

/// plan 直连路由上 `reasoning` 回放 item 的 `content` 数组是**禁写**字段：
/// schema 上限 0 条，非空即 HTTP 400 `Invalid 'input[N].content': array too
/// long. Expected an array with maximum length 0`（OAuth ChatGPT 线上实测，
/// 任何工具调用后的下一回合回放历史时触发）。`content` 的两条来源——
/// 原样回放的 provider item（模型产出的 reasoning text）与派生合成的旧形态
/// （`project_responses_messages` 显式构造）——都统一在这里摘除，不依赖投影侧
/// 自觉（与 extra_body 收口同一设计：被禁内容一旦上线就是 400）。
fn strip_reasoning_content_for_plan(request: &mut ChatRequest) {
    for msg in &mut request.messages {
        for part in &mut msg.parts {
            if let Part::ProviderItem(item) = part
                && item
                    .provider_state
                    .data
                    .get("type")
                    .and_then(|v| v.as_str())
                    == Some("reasoning")
                && let Some(obj) = item.provider_state.data.as_object_mut()
            {
                obj.remove("content");
            }
        }
    }
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
    let mut declarations: Vec<ToolDeclaration> = Vec::new();
    let specs: Vec<ToolSpec> = tools
        .unwrap_or_default()
        .into_iter()
        .filter_map(|td| {
            // freeform 收口：DeepSeek Responses 端点的 custom 工具**只认**
            // `{"type":"custom","name":"apply_patch"}`（其他名称 400），裸文本
            // 输入即 V4A 补丁。空 description = 最小形状（mutil-ai 契约）。
            if compat.apply_patch_freeform && td.function.name == "apply_patch" {
                declarations.push(ToolDeclaration::Tool {
                    tool: ScopedToolDeclaration::Custom {
                        name: td.function.name,
                        description: String::new(),
                        format: CustomToolFormat::Text,
                    },
                });
                return None;
            }
            Some(ToolSpec::new(
                provider_function_name(&td.function.name, compat).to_string(),
                td.function.description,
                sanitize_openai_schema(&td.function.parameters),
            ))
        })
        .collect();
    request.tools = specs;
    if !declarations.is_empty() {
        request.tool_declarations = declarations;
    }
    // 服务端内置搜索工具（模型可自主触发）：typed server_tools，非 wire 手拼。
    // plan 直连路由不提供 hosted 工具（web search 在该路由也受账户策略约束）→ 不发。
    if compat.web_search && !compat.siwc_plan {
        request.server_tools = vec![ServerTool::WebSearch];
    }
    // plan 直连路由禁用 `max_output_tokens`（见 SIWC_PLAN_FORBIDDEN_EXTRA_FIELDS）。
    if max_tokens > 0 && !compat.siwc_plan {
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
        // `user` 字段在 plan 直连路由被禁（不以 provider 的 supports_user 为准）。
        if compat.supports_user
            && !compat.siwc_plan
            && let Some(uid) = user_id.filter(|s| !s.is_empty())
        {
            request
                .extra_body
                .insert("user".into(), serde_json::json!(uid));
        }
    }
    // 收口放在最后：上面任何分支（含将来新增的）塞进来的被禁字段都在这被摘掉。
    apply_siwc_plan(provider, &mut request);
    request
}

#[cfg(feature = "native-tool-plan")]
fn plan_declarations(
    plan: &ModelToolPlan,
    descriptions: &HashMap<String, String>,
) -> anyhow::Result<Vec<mutil_ai::ToolDeclaration>> {
    use mutil_ai::{CustomToolFormat, ScopedToolDeclaration as Scoped, ToolDeclaration};
    let mut declarations = Vec::new();
    for entry in plan.entries() {
        let name = entry.address.name.clone();
        let tool = match &entry.declaration {
            ModelToolDeclaration::Function {
                description,
                parameters,
                strict,
            } => Scoped::Function {
                tool: ToolSpec::new(name, description.clone(), parameters.clone()).strict(*strict),
            },
            ModelToolDeclaration::Custom {
                description,
                format,
            } => Scoped::Custom {
                name,
                description: description.clone(),
                format: match format {
                    CustomFormat::Text => CustomToolFormat::Text,
                    CustomFormat::Grammar { syntax, definition } => CustomToolFormat::Grammar {
                        syntax: syntax.clone(),
                        definition: definition.clone(),
                    },
                },
            },
        };
        if let Some(namespace) = &entry.address.namespace {
            if let Some(ToolDeclaration::Namespace { tools, .. }) = declarations
                .iter_mut()
                .find(|d| matches!(d, ToolDeclaration::Namespace { name, .. } if name == namespace))
            {
                tools.push(tool);
            } else {
                let description = descriptions
                    .get(namespace)
                    .filter(|s| !s.trim().is_empty())
                    .ok_or_else(|| anyhow::anyhow!("missing namespace description: {namespace}"))?;
                declarations.push(ToolDeclaration::Namespace {
                    name: namespace.clone(),
                    description: description.clone(),
                    tools: vec![tool],
                });
            }
        } else {
            declarations.push(ToolDeclaration::Tool { tool });
        }
    }
    mutil_ai::validate_tool_declarations(&declarations)?;
    Ok(declarations)
}

#[cfg(feature = "native-tool-plan")]
pub(super) fn adapt_native(
    call: &mutil_ai::NativeToolCall,
    plan: Option<&ModelToolPlan>,
) -> anyhow::Result<Vec<ContentBlock>> {
    call.validate_completed()?;
    let plan =
        plan.ok_or_else(|| anyhow::anyhow!("native tool call requires an explicit tool plan"))?;
    let payload = match &call.payload {
        mutil_ai::NativeToolPayload::RawText(text) => PlanPayload::RawText(text.clone()),
        mutil_ai::NativeToolPayload::JsonArguments(text) => {
            PlanPayload::JsonArguments(serde_json::from_str(text)?)
        }
    };
    let mut address = qaqh_tool_core::model_tool_plan::ModelToolAddress {
        kind: match call.kind {
            mutil_ai::NativeToolKind::Function => ModelToolKind::Function,
            mutil_ai::NativeToolKind::Custom => ModelToolKind::Custom,
        },
        namespace: call.namespace.clone(),
        name: call.name.clone(),
    };
    // Responses may omit the default `functions` namespace for both function
    // and custom calls (Codex's ToolName::with_default_namespace). Resolve only
    // that default against the frozen declarations; an exact flat declaration
    // wins, and explicit/other namespaces are never guessed. Keep `call` intact
    // so history and replay retain the provider's original representation.
    if address.namespace.is_none() && !plan.entries().iter().any(|e| e.address == address) {
        let default_address = qaqh_tool_core::model_tool_plan::ModelToolAddress {
            namespace: Some("functions".into()),
            ..address.clone()
        };
        if plan.entries().iter().any(|e| e.address == default_address) {
            address = default_address;
        }
    }
    let adapted = plan.adapt(PlanCall {
        address,
        call_id: call.call_id.clone(),
        item_id: call.item_id.clone(),
        payload,
        completion: PlanCompletion::Completed,
        provider_state: call.provider_state.as_ref().map(|s| s.data.clone()),
    })?;
    let mut item = call
        .provider_state
        .as_ref()
        .map(|state| state.data.clone())
        .filter(serde_json::Value::is_object)
        .unwrap_or_else(|| serde_json::json!({}));
    item["type"] = match call.kind {
        mutil_ai::NativeToolKind::Function => "function_call",
        mutil_ai::NativeToolKind::Custom => "custom_tool_call",
    }
    .into();
    item["name"] = call.name.clone().into();
    item["call_id"] = call.call_id.clone().into();
    item["status"] = "completed".into();
    if let Some(object) = item.as_object_mut() {
        object.remove("input");
        object.remove("arguments");
        object.remove("qaqh_native_call");
    }
    if call.kind == mutil_ai::NativeToolKind::Function && call.namespace.is_none() {
        item["qaqh_native_call"] = true.into();
    }
    if let Some(id) = &call.item_id {
        item["id"] = id.clone().into();
    }
    if let Some(ns) = &call.namespace {
        item["namespace"] = ns.clone().into();
    }
    match &call.payload {
        mutil_ai::NativeToolPayload::RawText(text) => item["input"] = text.clone().into(),
        mutil_ai::NativeToolPayload::JsonArguments(text) => item["arguments"] = text.clone().into(),
    }
    Ok(vec![
        ContentBlock::ToolUse {
            id: adapted.canonical.call_id,
            name: adapted.canonical.internal_tool_key,
            input: adapted.canonical.args,
        },
        ContentBlock::ResponseOutputItem { item },
    ])
}

/// feature 关闭时的 fail-closed 实现：不解析原生调用，也不放行——与「provider 回来
/// 了原生调用但没有人给过 plan」的错误语义完全一致。
#[cfg(not(feature = "native-tool-plan"))]
pub(super) fn adapt_native(
    _call: &mutil_ai::NativeToolCall,
    _plan: Option<&ModelToolPlan>,
) -> anyhow::Result<Vec<ContentBlock>> {
    Err(anyhow::anyhow!(
        "native tool call requires an explicit tool plan"
    ))
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

#[derive(Default)]
struct Resps {
    text: String,
    reasoning: String,
    /// 思考类别（**首帧可展示类别胜出**，规则同 `sdk_common::StreamState`）。
    reasoning_kind: Option<qaqh_types::ReasoningKind>,
    tools: HashMap<usize, (String, String, String)>,
    /// Display-only byte watermark. Never use item ids as tool call ids and
    /// never replay a cumulative snapshot as another argument delta.
    tool_progress: HashMap<usize, usize>,
    had_native_output: bool,
    usage: Option<UsageInfo>,
}

impl Resps {
    fn emit_tool_progress(&mut self, index: usize, on_event: &mut (dyn FnMut(StreamEvent) + Send)) {
        let Some((id, name, args)) = self.tools.get(&index) else {
            return;
        };
        if id.is_empty() || name.is_empty() {
            return;
        }
        let previous = self.tool_progress.get(&index).copied();
        let sent = previous.unwrap_or(0);
        if previous.is_some() && args.len() <= sent {
            return;
        }
        // An empty first frame opens the card before a long write starts.
        on_event(StreamEvent::ToolCallProgress {
            index,
            id: id.clone(),
            name: name.clone(),
            args_chunk: args.get(sent..).unwrap_or_default().to_string(),
        });
        self.tool_progress.insert(index, args.len());
    }

    fn had_output(&self) -> bool {
        !self.text.is_empty()
            || !self.reasoning.is_empty()
            || !self.tools.is_empty()
            || self.had_native_output
    }
}

async fn consume(
    adapter: &EndpointAdapter,
    request: &ChatRequest,
    options: &RequestOptions,
    provider: &ProviderConfig,
    hub: &RetryHub,
    cancel: Option<&Arc<AtomicBool>>,
    on_event: &mut (dyn FnMut(StreamEvent) + Send),
    plan: Option<&ModelToolPlan>,
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
    let mut native_calls = HashMap::new();

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
            Ok(SdkEvent::ReasoningDelta { kind, text }) => {
                let kind = stored_reasoning_kind(kind);
                if !text.is_empty() && kind.is_displayable() {
                    if resp.reasoning_kind.is_none() {
                        resp.reasoning_kind = Some(kind);
                    }
                    resp.reasoning.push_str(&text);
                    on_event(StreamEvent::ReasoningDelta { kind, text });
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
                // **参数增量帧的 `id` 是 item id（`fc_1`），不是 `call_id`**：
                // 只有同时带函数名的块起始帧才配当身份来源。谁先到算谁的旧写法
                // 会把 item id 当 call id 收进缓冲——下一轮回放的
                // `function_call_output` 就再也对不上这次调用。
                let named = name.as_deref().is_some_and(|n| !n.is_empty());
                if named {
                    adopt_identity(&mut entry.0, id.as_deref());
                    let canonical = name
                        .as_deref()
                        .map(|n| {
                            if plan.is_some() {
                                n.to_string()
                            } else {
                                canonical_function_name(n, &compat)
                            }
                        })
                        .unwrap_or_default();
                    adopt_identity(&mut entry.1, Some(canonical.as_str()));
                }
                entry.2.push_str(&arguments_delta);
                resp.emit_tool_progress(index, on_event);
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
                // 累计帧的 id/name 来自 SDK 累计器里的 `call_id`/函数名，权威。
                adopt_identity(&mut entry.0, id.as_deref());
                let canonical = name
                    .as_deref()
                    .map(|n| {
                        if plan.is_some() {
                            n.to_string()
                        } else {
                            canonical_function_name(n, &compat)
                        }
                    })
                    .unwrap_or_default();
                adopt_identity(&mut entry.1, Some(canonical.as_str()));
                // 字节水位守卫（与 messages_sdk 同款不变量）：累计快照只增不回退。
                // 网关/SDK 的 `arguments_so_far` 可能滞后于已 append 的 delta——
                // 无守卫的赋值会把缓冲**截短**，拼出非法 JSON；下游一旦解析失败，
                // 旧代码会伪造 `{}` 当参数，模型就看到「missing field `patch`」
                // 这种假错（参数明明发过）。这里宁可用已累积的更长缓冲。
                if arguments_so_far.len() > entry.2.len() {
                    entry.2 = arguments_so_far;
                }
                resp.emit_tool_progress(index, on_event);
            }
            Ok(SdkEvent::NativeToolProgress {
                index,
                item_id,
                call_id,
                name,
                delta,
                input_so_far,
            }) => {
                resp.had_native_output = true;
                on_event(StreamEvent::NativeToolProgress {
                    index,
                    item_id,
                    call_id,
                    name,
                    delta,
                    input_so_far,
                });
            }
            Ok(SdkEvent::NativeToolCompleted { index, call }) => {
                resp.had_native_output = true;
                native_calls.insert(index, call);
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
                // ADR-0018：raw = SSE chunk 数组（而非 response 对象）= SDK
                // 在 EOF 的合成收口，response.completed 从未到达 → 上游掐流
                // 按失败处理，不得洗成完成。真 completed 的 raw 是 response
                // 对象；其 finish_reason 只携带 incomplete_details.reason
                // （正常完成缺席 → 下方映射 "stop"）。判别式依赖 retain_raw
                // 恒 true（roomy_response_limits 钉死）。
                if response.raw.is_array() {
                    if plan.is_some() {
                        return Outcome::Fatal(anyhow::anyhow!(
                            "native tool plan stream lacks a terminal response"
                        ));
                    }
                    interrupted = Some("upstream closed stream before terminal event".to_string());
                    break;
                }
                // Terminal-only providers still need a card. Streamed calls
                // already emitted their arguments; never feed them twice.
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
                    if let Some((slot, (_, _, streamed_args))) = resp
                        .tools
                        .iter()
                        .find(|(_, (known_id, _, _))| known_id == &id)
                    {
                        if let Some(sent) = resp.tool_progress.get(slot) {
                            // Some gateways send added + done, without any
                            // argument delta. The empty opening frame isn't
                            // proof that arguments have already been delivered.
                            if *sent == 0 && streamed_args.is_empty() {
                                on_event(StreamEvent::ToolCallProgress {
                                    index: *slot,
                                    id,
                                    name,
                                    args_chunk,
                                });
                            }
                            continue;
                        }
                    }
                    on_event(StreamEvent::ToolCallProgress {
                        index,
                        id,
                        name,
                        args_chunk,
                    });
                }
                // completed → stop "stop"；incomplete_details.reason 原样
                // 上抛（runtime 分类为 Length/Incomplete）。合成收口已在
                // 上方按掐流失败处理，不会走到这里（ADR-0018）。
                let stop = match finish_reason {
                    Some(fr) => Some(fr),
                    None => Some("stop".to_string()),
                };
                if resp.usage.is_none()
                    && let Some(usage) = response.usage.as_ref()
                {
                    resp.usage = Some(usage_to_info_responses(usage));
                }
                let blocks = match assemble_terminal_blocks(
                    &response,
                    &resp.tools,
                    native_calls,
                    &compat,
                    plan,
                ) {
                    Ok(blocks) => blocks,
                    Err(e) => return Outcome::Fatal(e),
                };
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
                    && let Some(term) = interpret_provider_stream(raw, &compat)
                {
                    if plan.is_some() {
                        return Outcome::Fatal(anyhow::anyhow!(
                            "native tool plan response terminated incompletely"
                        ));
                    }
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
                            kind: resp.reasoning_kind,
                        });
                    }
                    if !resp.text.is_empty() {
                        let text = std::mem::take(&mut resp.text);
                        blocks.push(ContentBlock::text(&text));
                    }
                    blocks.extend(term.raw_message.content);
                    // 截断终态同样可能不带 function_call 条目：按流补齐。
                    reconcile_streamed_tool_calls(&mut blocks, &resp.tools);
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
                if !matches!(
                    e.kind(),
                    mutil_ai::ErrorKind::Connection | mutil_ai::ErrorKind::Timeout
                ) {
                    let (msg, full) = sdk_error_detail("Responses", &e, &provider.api_key);
                    on_event(StreamEvent::Error {
                        kind: e.kind(),
                        message: full,
                    });
                    return Outcome::Fatal(anyhow::anyhow!(msg));
                }
                // Connection/Timeout 容忍路径此前不带类别上抛;turn 级重发
                // 判定需要 ErrorKind,与上方立即 Fatal 分支同律补发事件。
                on_event(StreamEvent::Error {
                    kind: e.kind(),
                    message: e.to_string(),
                });
                interrupted = Some(e.to_string());
                break;
            }
        }
    }

    if resp.had_output() {
        // ADR-0018：掐流就是失败，不得伪造 Done 把半截输出洗成完成。
        // 原生工具 plan 流本就 fail-closed；其余 wire 同律带真实原因上抛。
        if plan.is_some() {
            return Outcome::Fatal(anyhow::anyhow!("native tool plan stream interrupted"));
        }
        let why = interrupted.unwrap_or_else(|| "upstream closed stream mid-content".to_string());
        log::warn!("Responses SSE interrupted mid-stream: {why}");
        return Outcome::Fatal(anyhow::anyhow!("{why}"));
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
    on_event: &mut (dyn FnMut(StreamEvent) + Send),
    plan: Option<&ModelToolPlan>,
) -> Outcome {
    let watch = CancelWatch::start(cancel);
    let mut options = options.clone();
    options.cancellation = Some(watch.token().clone());
    consume(
        adapter, request, &options, provider, hub, cancel, on_event, plan,
    )
    .await
}

#[allow(clippy::too_many_arguments)] // 参数面塑形另立项（PLAN D-5）
pub async fn chat_stream_responses(
    provider: &ProviderConfig,
    model: &str,
    messages: Vec<Message>,
    tools: Option<Vec<ToolDef>>,
    max_tokens: u32,
    effort: Option<String>,
    user_id: Option<String>,
    cancel: Option<&Arc<AtomicBool>>,
    on_event: &mut (dyn FnMut(StreamEvent) + Send),
) -> anyhow::Result<()> {
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
    // SDK 负责 HTTP 层重试；驱动只补空流掐断的整请求重试（同其余三面）。
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        if is_cancelled(cancel) {
            return Err(anyhow::anyhow!("cancelled by user"));
        }
        let mut trace = SseTrace::from_env();
        let mut traced = |event: StreamEvent| {
            trace.record(&event);
            on_event(event)
        };
        match run_sdk_stream(
            &adapter, &request, &options, provider, &hub, cancel, &mut traced, None,
        )
        .await
        {
            Outcome::Completed => return Ok(()),
            Outcome::Fatal(e) => return Err(e),
            Outcome::EmptyStream => {
                empty_stream_backoff(
                    attempt,
                    &policy,
                    "upstream closed stream before any content",
                    cancel,
                    on_event,
                )
                .await?;
            }
        }
    }
}

#[cfg(feature = "native-tool-plan")]
#[allow(clippy::too_many_arguments)]
pub(crate) async fn chat_stream_responses_plan(
    provider: &ProviderConfig,
    messages: Vec<Message>,
    plan: &ModelToolPlan,
    descriptions: &HashMap<String, String>,
    max_tokens: u32,
    effort: Option<String>,
    user_id: Option<String>,
    cancel: Option<&Arc<AtomicBool>>,
    on_event: &mut (dyn FnMut(StreamEvent) + Send),
) -> anyhow::Result<()> {
    let declarations = plan_declarations(plan, descriptions)?;
    let projected = project_responses_messages(&messages, &provider.responses_compat);
    if projected.iter().all(|m| m.role == Role::System) {
        on_event(empty_request_noop_done_event());
        return Ok(());
    }
    let mut request = build_request(provider, projected, None, max_tokens, effort, user_id, true);
    request.tool_declarations = declarations;
    let policy = GateRetryPolicy::from_spec(provider.retry.as_ref());
    let (adapter, hub) = build_adapter(provider, &provider.model, &policy);
    let options = build_options(provider, &policy);
    let mut attempt = 0u32;
    loop {
        attempt += 1;
        if is_cancelled(cancel) {
            return Err(anyhow::anyhow!("cancelled by user"));
        }
        let mut trace = SseTrace::from_env();
        let mut traced = |event: StreamEvent| {
            trace.record(&event);
            on_event(event)
        };
        match run_sdk_stream(
            &adapter,
            &request,
            &options,
            provider,
            &hub,
            cancel,
            &mut traced,
            Some(plan),
        )
        .await
        {
            Outcome::Completed => return Ok(()),
            Outcome::Fatal(e) => return Err(e),
            Outcome::EmptyStream => {
                empty_stream_backoff(
                    attempt,
                    &policy,
                    "upstream closed stream before any content",
                    cancel,
                    on_event,
                )
                .await?;
            }
        }
    }
}

#[cfg(test)]
mod tests;
