use std::collections::{BTreeMap, HashMap, HashSet};

use serde_json::{Value, json};

use super::openai_responses::{
    ResponsesPartAccumulator, from_responses_response, parse_json_or_string,
    responses_reasoning_part, responses_server_tool_item,
};
use crate::error::{Error, Result};
use crate::native_tool_stream::{CustomToolAccumulator, NativeToolIdentity};
use crate::types::{
    ChatResponse, Message, NativeToolCall, NativeToolCompletion, NativeToolKind,
    NativeToolPayload, Part, ProviderState, ProviderStateFormat, Reasoning, ReasoningKind, Role,
    ToolCall,
};

pub(super) fn find_custom_call_key(
    custom_calls: &BTreeMap<usize, CustomToolAccumulator>,
    item: &Value,
) -> Option<usize> {
    if let Some(call_id) = item
        .get("call_id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        && let Some((&key, _)) = custom_calls
            .iter()
            .find(|(_, call)| call.call_id() == Some(call_id))
    {
        return Some(key);
    }
    if let Some(item_id) = item
        .get("id")
        .and_then(Value::as_str)
        .filter(|s| !s.is_empty())
        && let Some((&key, _)) = custom_calls
            .iter()
            .find(|(_, call)| call.item_id() == Some(item_id))
    {
        return Some(key);
    }
    None
}

/// Some compatible endpoints (notably DeepSeek) document the custom tool-call
/// output item without an `input` field. Match accumulated stream calls by
/// `call_id` / `item_id` rather than `output` array position so reordered or
/// sparse output items still recover their streamed input.
pub(super) fn backfill_custom_inputs(
    custom_calls: &BTreeMap<usize, CustomToolAccumulator>,
    response: &mut Value,
) {
    let Some(output) = response.get_mut("output").and_then(Value::as_array_mut) else {
        return;
    };
    for item in output.iter_mut() {
        if item.get("type").and_then(Value::as_str) != Some("custom_tool_call") {
            continue;
        }
        let has_input = item
            .get("input")
            .and_then(Value::as_str)
            .is_some_and(|input| !input.is_empty());
        if has_input {
            continue;
        }
        let Some(key) = find_custom_call_key(custom_calls, item) else {
            continue;
        };
        let Some(streamed) = custom_calls
            .get(&key)
            .map(|call| call.input())
            .filter(|input| !input.is_empty())
        else {
            continue;
        };
        if let Some(object) = item.as_object_mut() {
            object.insert("input".to_owned(), Value::String(streamed.to_owned()));
        }
    }
}

pub(super) fn reconcile_custom_output_items(
    custom_calls: &mut BTreeMap<usize, CustomToolAccumulator>,
    response: &Value,
) -> Result<()> {
    if let Some(output) = response.get("output").and_then(Value::as_array) {
        for item in output {
            if item.get("type").and_then(Value::as_str) != Some("custom_tool_call") {
                continue;
            }
            let call = parse_custom_item(item)?;
            let key = find_custom_call_key(custom_calls, item).unwrap_or_else(|| {
                (0usize..)
                    .find(|candidate| !custom_calls.contains_key(candidate))
                    .expect("unbounded range")
            });
            let accumulator = custom_calls.entry(key).or_default();
            accumulator.merge_identity(&custom_identity(item))?;
            let NativeToolPayload::RawText(input) = &call.payload else {
                unreachable!()
            };
            accumulator.finish_item(input)?;
        }
    }
    if custom_calls.values().any(|call| !call.is_completed()) {
        return Err(Error::StreamProtocol("tool_call_incomplete".into()));
    }
    Ok(())
}

pub(super) fn parse_custom_item(item: &Value) -> Result<NativeToolCall> {
    if item
        .get("status")
        .and_then(Value::as_str)
        .is_some_and(|status| status != "completed")
    {
        return Err(Error::StreamProtocol("tool_call_incomplete".into()));
    }
    let required = |key: &str| {
        item.get(key)
            .and_then(Value::as_str)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .ok_or_else(|| Error::StreamProtocol("tool_identity_conflict".into()))
    };
    let input = item
        .get("input")
        .and_then(Value::as_str)
        .ok_or_else(|| Error::StreamProtocol("tool_payload_conflict".into()))?;
    let call = NativeToolCall {
        kind: NativeToolKind::Custom,
        namespace: item
            .get("namespace")
            .and_then(Value::as_str)
            .map(str::to_string),
        name: required("name")?,
        call_id: required("call_id")?,
        item_id: item.get("id").and_then(Value::as_str).map(str::to_string),
        payload: NativeToolPayload::RawText(input.to_owned()),
        completion: NativeToolCompletion::Completed,
        provider_state: Some(ProviderState::new(
            ProviderStateFormat::OpenAiResponses,
            item.clone(),
        )),
    };
    call.validate_completed()?;
    Ok(call)
}

/// Parse a completed custom item, allowing a streamed snapshot to stand in
/// when the terminal item omits `input` (DeepSeek's documented item shape).
pub(super) fn parse_custom_item_with_fallback(
    item: &Value,
    fallback_input: Option<&str>,
) -> Result<NativeToolCall> {
    let has_input = item
        .get("input")
        .and_then(Value::as_str)
        .is_some_and(|input| !input.is_empty());
    if has_input || fallback_input.is_none_or(str::is_empty) {
        return parse_custom_item(item);
    }
    let mut patched = item.clone();
    if let Some(object) = patched.as_object_mut() {
        object.insert(
            "input".to_owned(),
            Value::String(fallback_input.unwrap_or_default().to_owned()),
        );
    }
    parse_custom_item(&patched)
}

pub(super) fn custom_identity(item: &Value) -> NativeToolIdentity {
    let get = |key: &str| item.get(key).and_then(Value::as_str).map(str::to_string);
    NativeToolIdentity {
        namespace: get("namespace"),
        name: get("name"),
        call_id: get("call_id"),
        item_id: get("id"),
    }
}

pub(super) fn custom_event_index(event: &Value) -> Result<usize> {
    event
        .get("output_index")
        .and_then(Value::as_u64)
        .and_then(|value| usize::try_from(value).ok())
        .ok_or_else(|| Error::StreamProtocol("tool_identity_conflict: missing output_index".into()))
}

pub(super) fn accumulator_to_part(part: ResponsesPartAccumulator) -> Option<Part> {
    match part {
        ResponsesPartAccumulator::Text(text) => (!text.is_empty()).then(|| Part::text(text)),
        ResponsesPartAccumulator::Reasoning {
            summary,
            text,
            item,
        } => Some(if let Some(item) = item {
            Part::reasoning(responses_reasoning_part(&item))
        } else {
            Part::reasoning(Reasoning {
                kind: if text.is_empty() {
                    ReasoningKind::Summary
                } else {
                    ReasoningKind::Text
                },
                summary: (!summary.is_empty()).then_some(summary),
                text: (!text.is_empty()).then_some(text),
                state: None,
            })
        }),
        ResponsesPartAccumulator::ToolCall {
            call_id,
            name,
            arguments,
            item,
        } => {
            let arguments = if arguments.trim().is_empty() {
                json!({})
            } else {
                parse_json_or_string(&arguments)
            };
            Some(Part::ToolCall(ToolCall {
                id: call_id.or_else(|| {
                    item.as_ref()
                        .and_then(|item| item.get("id"))
                        .and_then(Value::as_str)
                        .map(str::to_string)
                }),
                name,
                arguments,
                provider_state: None,
            }))
        }
        ResponsesPartAccumulator::ProviderItem(item) => {
            Some(Part::ProviderItem(responses_server_tool_item(&item)))
        }
    }
}

fn part_call_id(part: &Part) -> Option<&str> {
    match part {
        Part::NativeToolCall(call) => Some(call.call_id.as_str()),
        Part::ToolCall(call) => call.id.as_deref(),
        _ => None,
    }
}

fn accumulator_part_for_complete(acc: ResponsesPartAccumulator) -> Result<Option<Part>> {
    if let ResponsesPartAccumulator::ToolCall {
        item: Some(ref item),
        ..
    } = acc
        && item
            .get("status")
            .and_then(Value::as_str)
            .is_none_or(|status| status == "completed")
    {
        let mut extra = from_responses_response(json!({ "output": [item] }))?;
        if let Some(part) = extra.message.parts.drain(..).next() {
            return Ok(Some(part));
        }
    }
    Ok(accumulator_to_part(acc))
}

/// Validates streamed `custom_tool_call` records against terminal parts and
/// backfills any omitted streamed items in `output_index` order.
pub(super) fn reconcile_and_backfill_streamed_calls(
    normalized: &mut ChatResponse,
    parts: &mut BTreeMap<usize, ResponsesPartAccumulator>,
    custom_calls: &BTreeMap<usize, CustomToolAccumulator>,
    terminal_ids: &mut HashSet<String>,
) -> Result<()> {
    let mut streamed_ids = HashSet::new();
    let mut validated_custom = Vec::with_capacity(custom_calls.len());
    for (&output_index, call) in custom_calls {
        let call = call.completed_call()?;
        if !streamed_ids.insert(call.call_id.clone()) {
            return Err(Error::StreamProtocol(
                "tool_identity_conflict: duplicate call_id".into(),
            ));
        }
        if terminal_ids.contains(&call.call_id)
            && !normalized.message.parts.iter().any(|part| {
                matches!(
                    part,
                    Part::NativeToolCall(other)
                        if other.call_id == call.call_id && other.kind == NativeToolKind::Custom
                )
            })
        {
            return Err(Error::StreamProtocol(
                "tool_identity_conflict: duplicate call_id".into(),
            ));
        }
        if let Some(other) = normalized.message.parts.iter().find_map(|part| match part {
            Part::NativeToolCall(other) if other.call_id == call.call_id => Some(other),
            _ => None,
        }) && (other.namespace != call.namespace
            || other.name != call.name
            || other.item_id != call.item_id
            || other.payload != call.payload)
        {
            return Err(Error::StreamProtocol(
                "tool_identity_conflict: terminal call differs from stream".into(),
            ));
        }
        validated_custom.push((output_index, call));
    }

    if normalized.message.parts.is_empty() && (!parts.is_empty() || !validated_custom.is_empty()) {
        // Compatible endpoints that omit the authoritative `output` array on
        // `response.completed` still streamed their items through
        // `output_item.done`; merge standard parts and custom calls by
        // `output_index` so interleaved order is preserved.
        let mut merged: BTreeMap<usize, Part> = BTreeMap::new();
        for (idx, acc) in std::mem::take(parts) {
            if let Some(part) = accumulator_part_for_complete(acc)? {
                if let Some(id) = part_call_id(&part)
                    && !terminal_ids.insert(id.to_owned())
                {
                    return Err(Error::StreamProtocol(
                        "tool_identity_conflict: duplicate call_id".into(),
                    ));
                }
                merged.insert(idx, part);
            }
        }
        for (idx, call) in validated_custom {
            if !terminal_ids.insert(call.call_id.clone()) {
                return Err(Error::StreamProtocol(
                    "tool_identity_conflict: duplicate call_id".into(),
                ));
            }
            if merged.insert(idx, Part::NativeToolCall(call)).is_some() {
                return Err(Error::StreamProtocol(
                    "tool_identity_conflict: output index reused".into(),
                ));
            }
        }
        normalized.message = Message::new(Role::Assistant, merged.into_values().collect());
        return Ok(());
    }

    let mut streamed_index_by_call_id: HashMap<String, usize> = HashMap::new();
    for (&idx, acc) in parts.iter() {
        if let ResponsesPartAccumulator::ToolCall { call_id, item, .. } = acc {
            let id = call_id.as_deref().or_else(|| {
                item.as_ref()
                    .and_then(|item| item.get("call_id"))
                    .and_then(Value::as_str)
            });
            if let Some(id) = id.filter(|s| !s.is_empty()) {
                streamed_index_by_call_id.insert(id.to_owned(), idx);
            }
        }
    }
    for (idx, call) in &validated_custom {
        streamed_index_by_call_id.insert(call.call_id.clone(), *idx);
    }

    let mut missing_by_index: BTreeMap<usize, Part> = BTreeMap::new();
    for (&idx, acc) in parts.iter() {
        let ResponsesPartAccumulator::ToolCall {
            item: Some(item), ..
        } = acc
        else {
            continue;
        };
        if item
            .get("status")
            .and_then(Value::as_str)
            .is_some_and(|status| status != "completed")
        {
            continue;
        }
        let Some(call_id) = item.get("call_id").and_then(Value::as_str) else {
            continue;
        };
        if terminal_ids.contains(call_id) {
            continue;
        }
        let mut extra = from_responses_response(json!({ "output": [item] }))?;
        for part in extra.message.parts.drain(..) {
            let id = part_call_id(&part).map(str::to_owned);
            if let Some(id) = id
                && terminal_ids.insert(id)
            {
                missing_by_index.insert(idx, part);
            }
        }
    }
    for (idx, call) in validated_custom {
        if !terminal_ids.contains(&call.call_id) {
            terminal_ids.insert(call.call_id.clone());
            missing_by_index.insert(idx, Part::NativeToolCall(call));
        }
    }

    for (missing_index, part) in missing_by_index {
        let insert_at = normalized.message.parts.iter().position(|existing| {
            part_call_id(existing)
                .and_then(|id| streamed_index_by_call_id.get(id))
                .is_some_and(|&idx| idx > missing_index)
        });
        match insert_at {
            Some(pos) => normalized.message.parts.insert(pos, part),
            None => normalized.message.parts.push(part),
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use reqwest::header::HeaderMap;
    use serde_json::{Value, json};

    use super::super::openai_responses::{ResponsesStreamMapper, from_responses_response};
    use crate::error::Result;
    use crate::sse::SseMessage;
    use crate::stream::{SseMapper, StreamEvent};
    use crate::types::{NativeToolPayload, Part, ResponseMetadata};

    #[test]
    fn custom_tool_matches_by_identity_when_output_index_differs_from_array_position() {
        fn send(mapper: &mut ResponsesStreamMapper, event: Value) -> Result<Vec<StreamEvent>> {
            mapper.map(SseMessage {
                event: None,
                data: event.to_string(),
                id: None,
            })
        }
        let patch = "*** Begin Patch\n*** Add File: b.txt\n+ok\n*** End Patch";
        let mut mapper = ResponsesStreamMapper::new(
            "deepseek-v4-pro".into(),
            ResponseMetadata {
                status: 200,
                request_id: None,
                headers: HeaderMap::new(),
            },
        );
        send(
            &mut mapper,
            json!({
                "type": "response.output_item.added",
                "output_index": 2,
                "item": {
                    "type": "custom_tool_call",
                    "id": "ctc_2",
                    "call_id": "call_2",
                    "name": "apply_patch",
                    "input": "",
                    "status": "in_progress"
                }
            }),
        )
        .unwrap();
        send(
            &mut mapper,
            json!({
                "type": "response.custom_tool_call_input.delta",
                "output_index": 2,
                "item_id": "ctc_2",
                "delta": patch
            }),
        )
        .unwrap();
        send(
            &mut mapper,
            json!({
                "type": "response.custom_tool_call_input.done",
                "output_index": 2,
                "item_id": "ctc_2",
                "input": patch
            }),
        )
        .unwrap();
        send(
            &mut mapper,
            json!({
                "type": "response.output_item.done",
                "output_index": 2,
                "item": {
                    "type": "custom_tool_call",
                    "id": "ctc_2",
                    "call_id": "call_2",
                    "name": "apply_patch",
                    "status": "completed"
                }
            }),
        )
        .unwrap();
        let terminal = send(
            &mut mapper,
            json!({
                "type": "response.completed",
                "response": {
                    "status": "completed",
                    "output": [
                        {"type": "message", "content": [{"text": "done"}]},
                        {"type": "custom_tool_call", "id": "ctc_2", "call_id": "call_2", "name": "apply_patch", "status": "completed"}
                    ]
                }
            }),
        )
        .expect("sparse output array must match custom_tool_call by identity");
        let response = terminal
            .iter()
            .find_map(|event| match event {
                StreamEvent::Done { response, .. } => Some(response),
                _ => None,
            })
            .expect("done");
        let call = response
            .message
            .parts
            .iter()
            .find_map(|p| match p {
                Part::NativeToolCall(c) => Some(c),
                _ => None,
            })
            .expect("custom call present");
        assert!(matches!(&call.payload, NativeToolPayload::RawText(t) if t == patch));
    }

    #[test]
    fn custom_item_accepts_omitted_status_and_rejects_in_progress() {
        let patch = "*** Begin Patch\n*** End Patch";
        let ok = from_responses_response(json!({
            "output": [{
                "type": "custom_tool_call",
                "id": "ctc_1",
                "call_id": "call_1",
                "name": "apply_patch",
                "input": patch
            }]
        }))
        .expect("omitted status must be accepted");
        assert!(matches!(
            ok.message.parts.first(),
            Some(Part::NativeToolCall(c)) if c.call_id == "call_1"
        ));

        let err = from_responses_response(json!({
            "output": [{
                "type": "custom_tool_call",
                "id": "ctc_1",
                "call_id": "call_1",
                "name": "apply_patch",
                "input": patch,
                "status": "in_progress"
            }]
        }));
        assert!(err.is_err());
    }

    #[test]
    fn custom_input_done_without_delta_emits_native_tool_progress() {
        let patch = "*** Begin Patch\n*** Add File: c.txt\n+hi\n*** End Patch";
        let mut mapper = ResponsesStreamMapper::new(
            "deepseek-v4-pro".into(),
            ResponseMetadata {
                status: 200,
                request_id: None,
                headers: HeaderMap::new(),
            },
        );
        mapper
            .map(SseMessage {
                event: None,
                data: json!({
                    "type": "response.output_item.added",
                    "output_index": 0,
                    "item": {
                        "type": "custom_tool_call",
                        "id": "ctc_1",
                        "call_id": "call_1",
                        "name": "apply_patch",
                        "input": "",
                        "status": "in_progress"
                    }
                })
                .to_string(),
                id: None,
            })
            .unwrap();
        let events = mapper
            .map(SseMessage {
                event: None,
                data: json!({
                    "type": "response.custom_tool_call_input.done",
                    "output_index": 0,
                    "item_id": "ctc_1",
                    "input": patch
                })
                .to_string(),
                id: None,
            })
            .unwrap();
        assert!(events.iter().any(|e| matches!(
            e,
            StreamEvent::NativeToolProgress {
                call_id,
                name,
                delta,
                input_so_far,
                ..
            } if call_id.as_deref() == Some("call_1")
                && name.as_deref() == Some("apply_patch")
                && delta == patch
                && input_so_far == patch
        )));
    }

    fn send_event(mapper: &mut ResponsesStreamMapper, event: Value) -> Result<Vec<StreamEvent>> {
        mapper.map(SseMessage {
            event: None,
            data: event.to_string(),
            id: None,
        })
    }

    fn new_mapper() -> ResponsesStreamMapper {
        ResponsesStreamMapper::new(
            "deepseek-v4-pro".into(),
            ResponseMetadata {
                status: 200,
                request_id: None,
                headers: HeaderMap::new(),
            },
        )
    }

    fn emit_custom_then_function(
        mapper: &mut ResponsesStreamMapper,
        include_text_at_0: bool,
    ) {
        let mut idx = 0usize;
        if include_text_at_0 {
            send_event(
                mapper,
                json!({
                    "type": "response.output_text.delta",
                    "output_index": 0,
                    "content_index": 0,
                    "delta": "patching first"
                }),
            )
            .unwrap();
            idx = 1;
        }
        let patch = "*** Begin Patch\n*** Add File: a.txt\n+ok\n*** End Patch";
        send_event(
            mapper,
            json!({
                "type": "response.output_item.added",
                "output_index": idx,
                "item": {
                    "type": "custom_tool_call",
                    "id": "ctc_1",
                    "call_id": "call_patch",
                    "name": "apply_patch",
                    "input": "",
                    "status": "in_progress"
                }
            }),
        )
        .unwrap();
        send_event(
            mapper,
            json!({
                "type": "response.custom_tool_call_input.done",
                "output_index": idx,
                "item_id": "ctc_1",
                "input": patch
            }),
        )
        .unwrap();
        send_event(
            mapper,
            json!({
                "type": "response.output_item.done",
                "output_index": idx,
                "item": {
                    "type": "custom_tool_call",
                    "id": "ctc_1",
                    "call_id": "call_patch",
                    "name": "apply_patch",
                    "status": "completed"
                }
            }),
        )
        .unwrap();

        let fn_idx = idx + 1;
        send_event(
            mapper,
            json!({
                "type": "response.output_item.added",
                "output_index": fn_idx,
                "item": {
                    "type": "function_call",
                    "id": "fc_1",
                    "call_id": "call_read",
                    "name": "read",
                    "arguments": "",
                    "status": "in_progress"
                }
            }),
        )
        .unwrap();
        send_event(
            mapper,
            json!({
                "type": "response.function_call_arguments.delta",
                "output_index": fn_idx,
                "item_id": "fc_1",
                "delta": "{\"path\":\"a.txt\"}"
            }),
        )
        .unwrap();
        send_event(
            mapper,
            json!({
                "type": "response.output_item.done",
                "output_index": fn_idx,
                "item": {
                    "type": "function_call",
                    "id": "fc_1",
                    "call_id": "call_read",
                    "name": "read",
                    "arguments": "{\"path\":\"a.txt\"}",
                    "status": "completed"
                }
            }),
        )
        .unwrap();
    }

    fn part_summary(part: &Part) -> String {
        match part {
            Part::Text { .. } => "text".to_string(),
            Part::NativeToolCall(c) => format!("native:{}", c.call_id),
            Part::ToolCall(c) => format!("tool:{}", c.id.as_deref().unwrap_or("")),
            other => format!("other:{other:?}"),
        }
    }

    #[test]
    fn backfill_preserves_interleaved_order_when_completed_output_is_empty() {
        let mut mapper = new_mapper();
        emit_custom_then_function(&mut mapper, false);
        let terminal = send_event(
            &mut mapper,
            json!({
                "type": "response.completed",
                "response": { "status": "completed", "output": [] }
            }),
        )
        .unwrap();
        let response = terminal
            .iter()
            .find_map(|e| match e {
                StreamEvent::Done { response, .. } => Some(response),
                _ => None,
            })
            .expect("done");
        let summary: Vec<String> = response.message.parts.iter().map(part_summary).collect();
        assert_eq!(summary, vec!["native:call_patch", "tool:call_read"]);
    }

    #[test]
    fn backfill_preserves_interleaved_order_when_completed_output_omits_tool_calls() {
        let mut mapper = new_mapper();
        emit_custom_then_function(&mut mapper, true);
        let terminal = send_event(
            &mut mapper,
            json!({
                "type": "response.completed",
                "response": {
                    "status": "completed",
                    "output": [
                        {"type": "message", "content": [{"type": "output_text", "text": "patching first"}]}
                    ]
                }
            }),
        )
        .unwrap();
        let response = terminal
            .iter()
            .find_map(|e| match e {
                StreamEvent::Done { response, .. } => Some(response),
                _ => None,
            })
            .expect("done");
        let summary: Vec<String> = response.message.parts.iter().map(part_summary).collect();
        assert_eq!(summary, vec!["text", "native:call_patch", "tool:call_read"]);
    }

    #[test]
    fn backfill_inserts_omitted_custom_call_before_later_terminal_function_call() {
        let mut mapper = new_mapper();
        emit_custom_then_function(&mut mapper, true);
        let terminal = send_event(
            &mut mapper,
            json!({
                "type": "response.completed",
                "response": {
                    "status": "completed",
                    "output": [
                        {"type": "message", "content": [{"type": "output_text", "text": "patching first"}]},
                        {"type": "function_call", "id": "fc_1", "call_id": "call_read", "name": "read", "arguments": "{\"path\":\"a.txt\"}", "status": "completed"}
                    ]
                }
            }),
        )
        .unwrap();
        let response = terminal
            .iter()
            .find_map(|e| match e {
                StreamEvent::Done { response, .. } => Some(response),
                _ => None,
            })
            .expect("done");
        let summary: Vec<String> = response.message.parts.iter().map(part_summary).collect();
        assert_eq!(summary, vec!["text", "native:call_patch", "tool:call_read"]);
    }
}
