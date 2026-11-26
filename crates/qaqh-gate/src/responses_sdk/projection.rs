//! Harness history → `mutil-ai` OpenAI Responses 消息投影。

use std::collections::{HashMap, HashSet};

use mutil_ai::{
    ImagePart, Message as WireMessage, NativeToolCall, NativeToolCompletion, NativeToolKind,
    NativeToolPayload, Part, ProviderState, ProviderStateFormat, Role, ToolCall as WireToolCall,
    ToolResult as WireToolResult,
};
use qaqh_types::{ContentBlock, Message};

use super::super::sdk_common::{
    STALE_IMAGE_STUB, stale_image_boundary, tool_result_name, wire_tool_input,
};
use super::super::types::ResponsesCompat;
use super::provider_function_name;

const MEDIA_ATTACHMENT_TEXT: &str = "Attached media from tool result:";

pub(super) fn provider_state_item(item: serde_json::Value) -> Part {
    Part::ProviderItem(mutil_ai::ServerToolItem::new(ProviderState::new(
        ProviderStateFormat::OpenAiResponses,
        item,
    )))
}

/// 回放前的 item 归一化：OpenAI Responses 端点对 `input` 里的
/// `reasoning` / `web_search_call` item 要求必填 `summary` 数组（缺失即
/// HTTP 400 `missing_required_parameter: 'input[N].summary'`）。原样回放的
/// provider item 自带该字段；但历史经过 timeline 重建等有损路径后只剩
/// 语义块，派生合成（或旧版本持久化的）item 可能没有 `summary`——
/// 统一在这里补空数组，语义等价（无摘要）且满足 schema。
fn replay_item(mut item: serde_json::Value) -> serde_json::Value {
    if let Some(obj) = item.as_object_mut() {
        let it = obj.get("type").and_then(|v| v.as_str()).unwrap_or("");
        if (it == "reasoning" || it == "web_search_call") && !obj.contains_key("summary") {
            obj.insert("summary".into(), serde_json::json!([]));
        }
        // id 卫生（对标 codex `prepare_response_items_for_request`，
        // codex-rs/core/src/client.rs:1087-1096 + `ResponseItemId::is_prefixed`）：
        // 端点只接受 `<prefix>_<suffix>` 形的 item id；有损历史留下的空 id /
        // 无前缀 id 会被 400，直接摘除让端点重新分配。
        let malformed_id = obj
            .get("id")
            .and_then(|v| v.as_str())
            .is_some_and(|id| !is_prefixed_item_id(id));
        if malformed_id {
            obj.remove("id");
        }
    }
    item
}

/// codex `ResponseItemId::is_prefixed` 的投影：`prefix_suffix` 且两段非空。
fn is_prefixed_item_id(id: &str) -> bool {
    id.split_once('_')
        .is_some_and(|(prefix, suffix)| !prefix.is_empty() && !suffix.is_empty())
}

/// 将不带 namespace 的回放 `function_call` item 转为类型化 `WireToolCall`。
/// `mutil-ai::normalize` 严禁 `Part::ProviderItem` 携带 `function_call` /
/// `custom_tool_call`，必须走类型化 `Part::ToolCall` / `Part::NativeToolCall`。
fn function_call_from_item(
    item: &serde_json::Value,
    compat: &ResponsesCompat,
) -> Option<WireToolCall> {
    let raw_name = item
        .get("name")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())?;
    let name = provider_function_name(raw_name, compat).to_string();
    let input = match item.get("arguments") {
        Some(serde_json::Value::String(s)) => match mutil_ai::lenient::parse(s) {
            Some(v) if v.is_object() => v,
            _ => serde_json::json!({}),
        },
        Some(v) if v.is_object() => v.clone(),
        _ => serde_json::json!({}),
    };
    let mut call = WireToolCall::new(name, input);
    call.id = item
        .get("call_id")
        .and_then(|v| v.as_str())
        .filter(|s| !s.is_empty())
        .map(str::to_string);
    Some(call)
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
    // mutil-ai 0.4.0 matches a tool result to its call by BOTH id and name, and
    // the projected call name is the provider-mapped function name — so resolve
    // result names from history against that same mapping (`Message::tool_result`
    // carries no name of its own).
    let names: HashMap<String, String> = messages
        .iter()
        .filter(|m| m.role == "assistant")
        .flat_map(|m| m.content.iter())
        .filter_map(|b| match b {
            ContentBlock::ToolUse { id, name, .. } => {
                Some((id.clone(), provider_function_name(name, compat).to_string()))
            }
            _ => None,
        })
        .collect();
    let mut out: Vec<WireMessage> = Vec::new();
    let mut pending_tool_attachments: Vec<WireMessage> = Vec::new();
    let mut top_taken = false;
    let mut img_idx: usize = 0;

    let stale_boundary = stale_image_boundary(messages);
    for (msg_index, msg) in messages.iter().enumerate() {
        if msg.role != "tool" && !pending_tool_attachments.is_empty() {
            out.extend(std::mem::take(&mut pending_tool_attachments));
        }
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
                // 调用项**必须**排在整条 message 的最后：DeepSeek OAuth 的
                // Responses 端点要求每个 `function_call` 后面紧跟它的
                // `function_call_output`，中间不许夹任何 item。
                let mut calls: Vec<Part> = Vec::new();
                let mut emitted_call_ids: HashSet<String> = HashSet::new();
                // 1) 回放 provider 原生 items（非调用项保持原样，调用项统一走类型化 Part）。
                for item in &raw_items {
                    let sanitized = replay_item((*item).clone());
                    let kind = item_type(&sanitized);
                    if kind == "reasoning" && !compat.echo_reasoning_content {
                        continue;
                    }
                    if let Some(call) = native_call_from_item(&sanitized) {
                        emitted_call_ids.insert(call.call_id.clone());
                        calls.push(Part::NativeToolCall(call));
                    } else if kind == "function_call" {
                        if let Some(call) = function_call_from_item(&sanitized, compat) {
                            if let Some(id) = &call.id {
                                emitted_call_ids.insert(id.clone());
                            }
                            calls.push(Part::ToolCall(call));
                        }
                    } else if matches!(
                        kind.as_str(),
                        "custom_tool_call" | "function_call_output" | "custom_tool_call_output"
                    ) {
                        // mutil-ai::normalize 硬拒绝 ProviderItem 携带这 4 类客户端工具项。
                        continue;
                    } else {
                        parts.push(provider_state_item(sanitized));
                    }
                }
                // 2) 派生 reasoning（无回放 item 且开关开启时合成旧形态）。
                if compat.echo_reasoning_content && !has_response_type("reasoning") {
                    for b in &msg.content {
                        if let ContentBlock::Reasoning { reasoning, .. } = b
                            && !reasoning.is_empty()
                        {
                            parts.push(provider_state_item(serde_json::json!({
                                "type": "reasoning",
                                "content": [{"type": "reasoning_text", "text": reasoning}],
                                "summary": [],
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
                // 4) 派生 function_call / freeform custom_tool_call（未被回放 item 覆盖的 call_id）。
                for b in &msg.content {
                    if let ContentBlock::ToolUse { id, name, input } = b {
                        if !id.is_empty() && emitted_call_ids.contains(id.as_str()) {
                            continue;
                        }
                        if compat.apply_patch_freeform && name == "apply_patch" && !id.is_empty() {
                            let raw = input
                                .get("input")
                                .or_else(|| input.get("patch"))
                                .and_then(|v| v.as_str())
                                .unwrap_or("");
                            calls.push(Part::NativeToolCall(NativeToolCall {
                                kind: NativeToolKind::Custom,
                                namespace: None,
                                name: "apply_patch".into(),
                                call_id: id.clone(),
                                item_id: None,
                                payload: NativeToolPayload::RawText(raw.to_string()),
                                completion: NativeToolCompletion::Completed,
                                provider_state: None,
                            }));
                            emitted_call_ids.insert(id.clone());
                            continue;
                        }
                        // Null input（上游参数不可还原）在 wire 层折叠成 {}，
                        // 端点不接受 "arguments": "null"（T-7-1）。
                        let mut call = WireToolCall::new(
                            provider_function_name(name, compat).to_string(),
                            wire_tool_input(input),
                        );
                        // 0.4.0 rejects an empty explicit id; leave it unset so the
                        // SDK's normalizer assigns a collision-free one.
                        call.id = if id.is_empty() {
                            None
                        } else {
                            Some(id.clone())
                        };
                        calls.push(Part::ToolCall(call));
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
                                "summary": [],
                            })));
                        }
                    }
                }
                if !parts.is_empty() || !calls.is_empty() {
                    parts.extend(calls);
                    out.push(WireMessage::new(Role::Assistant, parts));
                }
            }
            "tool" => {
                // 跨轮丢弃：位于最后一条 user 消息之前的工具图属于已消化回合，
                // 不再重发（见 sdk_common::stale_image_boundary）。
                let stale = stale_boundary.is_some_and(|boundary| msg_index < boundary);
                let mut dropped = 0usize;
                let mut run: Vec<Part> = Vec::new();
                let mut attachments: Vec<WireMessage> = Vec::new();
                for b in &msg.content {
                    match b {
                        ContentBlock::ToolResult {
                            tool_use_id,
                            result,
                        } => {
                            let name = tool_result_name(&names, msg.name.as_deref(), tool_use_id);
                            let mut tr = WireToolResult::new(
                                if tool_use_id.is_empty() {
                                    None
                                } else {
                                    Some(tool_use_id.clone())
                                },
                                name,
                                result.render_xml_envelope(),
                            );
                            tr.is_error = !result.is_success();
                            run.push(Part::ToolResult(tr));
                        }
                        ContentBlock::Image { mime_type, data } => {
                            if stale {
                                dropped += 1;
                                continue;
                            }
                            attachments.push(WireMessage::new(
                                Role::User,
                                response_media_parts(&mut img_idx, mime_type, data),
                            ));
                        }
                        ContentBlock::ImageRef {
                            sha256, mime_type, ..
                        } => match qaqh_types::image_store::load_image_b64(sha256, mime_type) {
                            _ if stale => {
                                dropped += 1;
                            }
                            Ok(data) => attachments.push(WireMessage::new(
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
                    if dropped > 0
                        && let Some(Part::ToolResult(tr)) = run.last_mut()
                    {
                        tr.content.push('\n');
                        tr.content.push_str(STALE_IMAGE_STUB);
                    }
                    out.push(WireMessage::new(Role::Tool, run));
                }
                // 连续多条并行 tool 结果必须全部紧邻 assistant 调用，
                // 图片附件暂存并在整组 tool 结束后统一追加。
                pending_tool_attachments.extend(attachments);
            }
            _ => {}
        }
    }
    if !pending_tool_attachments.is_empty() {
        out.extend(pending_tool_attachments);
    }
    out
}

pub(super) fn native_call_from_item(item: &serde_json::Value) -> Option<NativeToolCall> {
    if item
        .get("status")
        .and_then(serde_json::Value::as_str)
        .is_some_and(|status| status != "completed")
    {
        return None;
    }
    let typ = item.get("type")?.as_str()?;
    let (kind, payload) = match typ {
        "custom_tool_call" => (
            NativeToolKind::Custom,
            NativeToolPayload::RawText(item.get("input")?.as_str()?.to_owned()),
        ),
        "function_call"
            if item.get("namespace").is_some()
                || item.get("qaqh_native_call").and_then(|v| v.as_bool()) == Some(true) =>
        {
            (
                NativeToolKind::Function,
                NativeToolPayload::JsonArguments(item.get("arguments")?.as_str()?.to_owned()),
            )
        }
        _ => return None,
    };
    let call = NativeToolCall {
        kind,
        payload,
        namespace: item
            .get("namespace")
            .and_then(|v| v.as_str())
            .map(str::to_owned),
        name: item.get("name")?.as_str()?.to_owned(),
        call_id: item.get("call_id")?.as_str()?.to_owned(),
        item_id: item.get("id").and_then(|v| v.as_str()).map(str::to_owned),
        completion: NativeToolCompletion::Completed,
        provider_state: Some(ProviderState::new(
            ProviderStateFormat::OpenAiResponses,
            item.clone(),
        )),
    };
    call.validate_completed().ok()?;
    Some(call)
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
