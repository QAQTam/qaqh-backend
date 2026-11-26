use std::collections::BTreeMap;

use serde_json::Value;

use crate::error::{Error, Result};
use crate::native_tool_stream::{CustomToolAccumulator, NativeToolIdentity};
use crate::types::{
    NativeToolCall, NativeToolCompletion, NativeToolKind, NativeToolPayload, ProviderState,
    ProviderStateFormat,
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
}
