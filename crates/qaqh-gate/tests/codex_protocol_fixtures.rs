//! Synthetic protocol targets plus an opt-in native round-trip over HTTP.
//! This does not replace real-provider OAuth probes.

use std::collections::HashSet;

use serde_json::{Value, json};
mod common;

#[test]
fn default_namespace_calls_without_namespace_roundtrip_over_http() {
    use common::mock_server::{MockServer, SseChunk};
    use qaqh_tool_core::model_tool_plan::{
        CustomFormat, InputAdapter, ModelToolAddress, ModelToolDeclaration, ModelToolEntry,
        ModelToolKind, ModelToolPlan, TextInputAdapter,
    };
    use qaqh_types::{ContentBlock, Message, ToolResult};
    use std::collections::HashMap;

    for kind in [ModelToolKind::Function, ModelToolKind::Custom] {
        let (name, item, declaration, adapter, expected) = match kind {
            ModelToolKind::Function => (
                "read",
                json!({"type":"function_call", "id":"fc_1", "call_id":"call_1",
                    "name":"read", "arguments":"{\"path\":\"a.txt\"}", "status":"completed"}),
                ModelToolDeclaration::Function {
                    description: "read file".into(),
                    parameters: json!({"type":"object", "properties":{"path":{"type":"string"}}}),
                    strict: false,
                },
                InputAdapter::FunctionJson,
                json!({"path":"a.txt"}),
            ),
            ModelToolKind::Custom => (
                "apply_patch",
                json!({"type":"custom_tool_call", "id":"ctc_1", "call_id":"call_1",
                    "name":"apply_patch", "input":"*** Begin Patch\n*** End Patch", "status":"completed"}),
                ModelToolDeclaration::Custom {
                    description: "patch".into(),
                    format: CustomFormat::Text,
                },
                InputAdapter::TextInput(
                    TextInputAdapter::custom_raw("patch")
                        .with_defaults(json!({"dry_run": false})),
                ),
                json!({"patch":"*** Begin Patch\n*** End Patch", "dry_run":false}),
            ),
        };
        let server = MockServer::new(vec![
            SseChunk::Data(
                json!({"type":"response.output_item.done", "output_index":0, "item":item}),
            ),
            SseChunk::Data(json!({"type":"response.completed",
                "response":{"id":"resp_1", "status":"completed", "output":[item]}})),
        ]);
        let plan = ModelToolPlan::new(
            "1",
            "siwc",
            vec![ModelToolEntry {
                address: ModelToolAddress {
                    kind,
                    namespace: Some("functions".into()),
                    name: name.into(),
                },
                declaration,
                internal_tool_key: name.into(),
                adapter,
            }],
        )
        .expect("plan");
        let descriptions = HashMap::from([("functions".into(), "workspace".into())]);
        let mut provider =
            qaqh_gate::ProviderConfig::responses(&server.base_url(), "key", "model", None);
        provider.responses_compat.siwc_plan = true;
        let mut done = None;
        qaqh_gate::chat_stream_responses_with_plan(
            &provider,
            vec![Message::user("use tool")],
            &plan,
            &descriptions,
            0,
            None,
            None,
            None,
            &mut |event| {
                if let qaqh_gate::StreamEvent::Done { raw_message, .. } = event {
                    done = Some(raw_message);
                }
            },
        )
        .expect("default namespace call must be admitted");
        let message = done.expect("done");
        assert_eq!(
            message
                .content
                .iter()
                .filter(|block| matches!(block,
                    ContentBlock::ToolUse { id, name: tool, input }
                        if id == "call_1" && tool == name && input == &expected
                ))
                .count(),
            1
        );
        assert!(
            message.content.iter().any(|block| matches!(block,
                ContentBlock::ResponseOutputItem { item: raw }
                    if raw["name"] == name && raw.get("namespace").is_none()
            )),
            "retain the provider's original namespace representation"
        );
        let request = server.last_request_json().expect("request");
        assert_eq!(request["tools"][0]["type"], "namespace");
        assert_eq!(request["tools"][0]["name"], "functions");

        let replay_server = MockServer::new(vec![SseChunk::Data(json!({
            "type":"response.completed", "response":{"id":"resp_2", "status":"completed", "output":[]}
        }))]);
        provider.base_url = replay_server.base_url();
        qaqh_gate::chat_stream_responses_with_plan(
            &provider,
            vec![
                Message::user("use tool"),
                message,
                Message::tool_result("call_1", ToolResult::ok("done")),
            ],
            &plan,
            &descriptions,
            0,
            None,
            None,
            None,
            &mut |_| {},
        )
        .expect("replay");
        let replay = replay_server.last_request_json().expect("replay request");
        let input = replay["input"].as_array().expect("input");
        let calls: Vec<_> = input
            .iter()
            .filter(|value| value["type"] == item["type"])
            .collect();
        assert_eq!(calls.len(), 1);
        assert_eq!(calls[0]["call_id"], "call_1");
        assert!(calls[0].get("namespace").is_none());
        let output_type = if kind == ModelToolKind::Custom {
            "custom_tool_call_output"
        } else {
            "function_call_output"
        };
        assert_eq!(
            input
                .iter()
                .filter(|value| value["type"] == output_type && value["call_id"] == "call_1")
                .count(),
            1
        );
    }
}

#[test]
fn native_custom_stream_roundtrip_over_http() {
    use common::mock_server::{MockServer, SseChunk};
    use qaqh_tool_core::model_tool_plan::{
        CustomFormat, InputAdapter, ModelToolAddress, ModelToolDeclaration, ModelToolEntry,
        ModelToolKind, ModelToolPlan, TextInputAdapter,
    };
    use qaqh_types::{ContentBlock, Message, ToolResult};
    use std::collections::HashMap;
    let item = json!({"type":"custom_tool_call","id":"ctc_1","call_id":"call_1","namespace":"functions","name":"apply_patch","input":"*** Begin Patch\n*** End Patch","status":"completed"});
    let server = MockServer::new(vec![
        SseChunk::Data(
            json!({"type":"response.output_item.added","output_index":0,"item":{"type":"custom_tool_call","id":"ctc_1","call_id":"call_1","namespace":"functions","name":"apply_patch","input":"","status":"in_progress"}}),
        ),
        SseChunk::Data(
            json!({"type":"response.custom_tool_call_input.delta","output_index":0,"item_id":"ctc_1","delta":"*** Begin Patch\n*** End Patch"}),
        ),
        SseChunk::Data(json!({"type":"response.output_item.done","output_index":0,"item":item})),
        SseChunk::Data(
            json!({"type":"response.completed","response":{"id":"resp_1","status":"completed","output":[item]}}),
        ),
    ]);
    let plan = ModelToolPlan::new(
        "1",
        "test",
        vec![ModelToolEntry {
            address: ModelToolAddress {
                kind: ModelToolKind::Custom,
                namespace: Some("functions".into()),
                name: "apply_patch".into(),
            },
            declaration: ModelToolDeclaration::Custom {
                description: "patch".into(),
                format: CustomFormat::Text,
            },
            internal_tool_key: "apply_patch".into(),
            adapter: InputAdapter::TextInput(
                TextInputAdapter::custom_raw("patch")
                    .with_defaults(json!({"dry_run": false})),
            ),
        }],
    )
    .expect("plan");
    let provider = qaqh_gate::ProviderConfig::responses(&server.base_url(), "key", "model", None);
    let descriptions = HashMap::from([("functions".to_string(), "workspace".to_string())]);
    let mut done = None;
    let mut native_progress = None;
    qaqh_gate::chat_stream_responses_with_plan(
        &provider,
        vec![Message::user("patch")],
        &plan,
        &descriptions,
        0,
        None,
        None,
        None,
        &mut |event| match event {
            qaqh_gate::StreamEvent::Done { raw_message, .. } => done = Some(raw_message),
            qaqh_gate::StreamEvent::NativeToolProgress {
                call_id,
                name,
                delta,
                input_so_far,
                ..
            } => native_progress = Some((call_id, name, delta, input_so_far)),
            _ => {}
        },
    )
    .expect("stream");
    assert_eq!(
        native_progress,
        Some((
            Some("call_1".into()),
            Some("apply_patch".into()),
            "*** Begin Patch\n*** End Patch".into(),
            "*** Begin Patch\n*** End Patch".into(),
        )),
        "custom delta must reach the gate with canonical identity"
    );
    let message = done.expect("done");
    assert!(message.content.iter().any(|b| matches!(b, ContentBlock::ToolUse { id, input, .. } if id == "call_1" && input["patch"] == "*** Begin Patch\n*** End Patch")));
    let request = server.last_request_json().expect("request");
    assert_eq!(request["tools"][0]["type"], "namespace");
    assert_eq!(request["tools"][0]["tools"][0]["type"], "custom");
    let server2 = MockServer::new(vec![SseChunk::Data(
        json!({"type":"response.completed","response":{"id":"resp_2","status":"completed","output":[]}}),
    )]);
    let provider2 = qaqh_gate::ProviderConfig::responses(&server2.base_url(), "key", "model", None);
    qaqh_gate::chat_stream_responses_with_plan(
        &provider2,
        vec![
            Message::user("patch"),
            message,
            Message::tool_result("call_1", ToolResult::ok("done")),
        ],
        &plan,
        &descriptions,
        0,
        None,
        None,
        None,
        &mut |_| {},
    )
    .expect("replay");
    let request = server2.last_request_json().expect("replay request");
    let input = request["input"].as_array().expect("input");
    assert_eq!(
        input
            .iter()
            .filter(|v| v["type"] == "custom_tool_call")
            .count(),
        1
    );
    assert!(
        input
            .iter()
            .any(|v| v["type"] == "custom_tool_call_output" && v["call_id"] == "call_1")
    );
}

#[test]
fn deepseek_minimal_custom_declaration_is_type_name_only() {
    use common::mock_server::{MockServer, SseChunk};
    use qaqh_tool_core::model_tool_plan::{
        CustomFormat, InputAdapter, ModelToolAddress, ModelToolDeclaration, ModelToolEntry,
        ModelToolKind, ModelToolPlan, TextInputAdapter,
    };
    use qaqh_types::Message;
    use std::collections::HashMap;

    let server = MockServer::new(vec![SseChunk::Data(json!({
        "type":"response.completed",
        "response":{"id":"resp_min","status":"completed","output":[]}
    }))]);
    let plan = ModelToolPlan::new(
        "1",
        "deepseek-minimal",
        vec![ModelToolEntry {
            address: ModelToolAddress {
                kind: ModelToolKind::Custom,
                namespace: None,
                name: "apply_patch".into(),
            },
            declaration: ModelToolDeclaration::Custom {
                description: String::new(),
                format: CustomFormat::Text,
            },
            internal_tool_key: "apply_patch".into(),
            adapter: InputAdapter::TextInput(
                TextInputAdapter::custom_raw("patch")
                    .with_defaults(json!({"dry_run": false})),
            ),
        }],
    )
    .expect("minimal plan");
    let provider = qaqh_gate::ProviderConfig::responses(&server.base_url(), "key", "model", None);
    qaqh_gate::chat_stream_responses_with_plan(
        &provider,
        vec![Message::user("patch")],
        &plan,
        &HashMap::new(),
        0,
        None,
        None,
        None,
        &mut |_| {},
    )
    .expect("stream");
    let request = server.last_request_json().expect("request");
    assert_eq!(
        request["tools"][0],
        json!({"type":"custom", "name":"apply_patch"}),
        "DeepSeek must receive exactly the documented custom shape"
    );
    assert_eq!(
        request["tools"]
            .as_array()
            .expect("tools")
            .iter()
            .filter(|tool| tool["type"] == "custom")
            .count(),
        1,
        "OAuth must expose exactly one custom tool"
    );
}

const FIXTURES: &str = include_str!("../../../docs/codex-tools对齐/fixtures/tool-protocol.json");

fn fixtures() -> Value {
    serde_json::from_str(FIXTURES).expect("protocol fixture must be valid JSON")
}

#[test]
fn codex_fixture_matrix_covers_four_layout_and_input_combinations() {
    let fixtures = fixtures();
    assert_eq!(fixtures["version"], 1);
    assert_eq!(fixtures["synthetic"], true);
    assert_eq!(fixtures["endpoint_verified"], false);
    let cases = fixtures["cases"].as_array().expect("cases");
    let ids: HashSet<_> = cases.iter().map(|case| case["id"].as_str()).collect();
    assert_eq!(cases.len(), 4);
    assert_eq!(ids.len(), 4);
    for id in [
        "flat_function",
        "flat_custom_mixed",
        "namespace_function",
        "namespace_custom_mixed",
    ] {
        assert!(ids.contains(&Some(id)));
    }
}

#[test]
fn codex_fixture_declarations_and_replay_keep_native_identity() {
    let fixtures = fixtures();
    for case in fixtures["cases"].as_array().expect("cases") {
        let tools = case["request"]["tools"].as_array().expect("tools");
        let namespaced = case["layout"] == "functions_namespace";
        let declarations = if namespaced {
            assert_eq!(tools.len(), 1);
            assert_eq!(tools[0]["type"], "namespace");
            assert_eq!(tools[0]["name"], "functions");
            assert_eq!(case["request"]["store"], false);
            assert_eq!(case["request"]["stream"], true);
            tools[0]["tools"].as_array().expect("namespace children")
        } else {
            tools
        };
        let mut names = HashSet::new();
        for tool in declarations {
            assert!(names.insert(tool["name"].as_str().expect("tool name")));
            match tool["type"].as_str().expect("tool kind") {
                "function" => assert!(tool["parameters"].is_object()),
                "custom" => {
                    assert_eq!(tool["name"], "apply_patch");
                    assert!(tool.get("parameters").is_none());
                }
                other => panic!("unexpected declaration kind: {other}"),
            }
        }

        let calls = case["response"]["output"].as_array().expect("output");
        let next = case["next_input"].as_array().expect("next input");
        assert_eq!(case["response"]["status"], "completed");
        assert_eq!(next.len(), calls.len() * 2);
        let mut ids = HashSet::new();
        for call in calls {
            let id = call["call_id"].as_str().expect("call id");
            assert!(!id.is_empty());
            assert!(ids.insert(id));
            assert_ne!(call["id"], call["call_id"]);
            assert_eq!(call["status"], "completed");
            if namespaced {
                assert_eq!(call["namespace"], "functions");
            } else {
                assert!(call.get("namespace").is_none());
            }
            let kind = match call["type"].as_str().expect("call kind") {
                "function_call" => "function",
                "custom_tool_call" => "custom",
                other => panic!("unexpected call kind: {other}"),
            };
            assert!(
                declarations
                    .iter()
                    .any(|tool| { tool["name"] == call["name"] && tool["type"] == kind })
            );
            // Raw replay is present exactly once, never replaced with a function.
            assert_eq!(next.iter().filter(|item| *item == call).count(), 1);
            let outputs: Vec<_> = next
                .iter()
                .filter(|item| {
                    item["call_id"] == id
                        && item["type"]
                            .as_str()
                            .is_some_and(|t| t.ends_with("_output"))
                })
                .collect();
            assert_eq!(outputs.len(), 1);
            let expected = if kind == "custom" {
                "custom_tool_call_output"
            } else {
                "function_call_output"
            };
            assert_eq!(outputs[0]["type"], expected);
            assert!(outputs[0]["output"].is_string());
        }
    }
}

#[test]
fn codex_fixture_canonical_targets_preserve_patch_bytes() {
    let fixtures = fixtures();
    let mut patches = Vec::new();
    for case in fixtures["cases"].as_array().expect("cases") {
        let canonical = case["canonical"].as_array().expect("canonical targets");
        let calls = case["response"]["output"].as_array().expect("output");
        assert_eq!(canonical.len(), calls.len());
        for (call, target) in calls.iter().zip(canonical) {
            assert_eq!(call["call_id"], target["call_id"]);
            assert_eq!(call["name"], target["internal_tool_key"]);
            assert!(target["args"].is_object());
            let expected = if call["type"] == "custom_tool_call" {
                json!({"patch": call["input"], "dry_run": false})
            } else {
                serde_json::from_str::<Value>(call["arguments"].as_str().expect("arguments"))
                    .expect("function arguments JSON")
            };
            assert_eq!(target["args"], expected);
            if target["internal_tool_key"] == "apply_patch" {
                patches.push(target["args"]["patch"].as_str().expect("patch"));
            }
        }
    }
    assert_eq!(patches.len(), 4);
    assert!(patches.iter().all(|patch| patch == &patches[0]));
    assert!(patches[0].contains("你好 \"Codex\" \\ path\n+\n"));
}

#[test]
fn codex_fixture_custom_deltas_and_snapshots_describe_the_same_input() {
    let fixtures = fixtures();
    let mut checked = 0;
    for case in fixtures["cases"].as_array().expect("cases") {
        let Some(events) = case["custom_events"].as_array() else {
            continue;
        };
        checked += 1;
        let call = &case["response"]["output"][0];
        let added = &events[0]["item"];
        assert_eq!(added["status"], "in_progress");
        assert_eq!(added["input"], "");
        for key in ["type", "id", "call_id", "name", "namespace"] {
            assert_eq!(added[key], call[key]);
        }
        let mut accumulated = String::new();
        let mut input_done = 0;
        let mut item_done = 0;
        for event in events {
            assert_eq!(event["output_index"], 0);
            match event["type"].as_str().expect("event kind") {
                "response.output_item.added" => {}
                "response.custom_tool_call_input.delta" => {
                    assert_eq!(event["item_id"], call["id"]);
                    accumulated.push_str(event["delta"].as_str().expect("raw delta"));
                }
                "response.custom_tool_call_input.done" => {
                    input_done += 1;
                    assert_eq!(event["item_id"], call["id"]);
                    assert_eq!(event["input"], accumulated);
                    assert_eq!(event["input"], call["input"]);
                }
                "response.output_item.done" => {
                    item_done += 1;
                    assert_eq!(event["item"], *call);
                }
                other => panic!("unexpected event kind: {other}"),
            }
        }
        assert_eq!(input_done, 1);
        assert_eq!(item_done, 1);
        assert_eq!(accumulated, call["input"]);
    }
    assert_eq!(checked, 2);
}

#[test]
fn flat_custom_declaration_is_sent_and_decoded() {
    use common::mock_server::{MockServer, SseChunk};
    use qaqh_tool_core::model_tool_plan::{
        CustomFormat, InputAdapter, ModelToolAddress, ModelToolDeclaration, ModelToolEntry,
        ModelToolKind, ModelToolPlan, TextInputAdapter,
    };
    use qaqh_types::{ContentBlock, Message};
    use std::collections::HashMap;

    let patch = "*** Begin Patch\n*** Add File: 示例.txt\n+你好\n*** End Patch\n";
    let item = json!({
        "type":"custom_tool_call", "id":"ctc_flat", "call_id":"call_flat",
        "name":"apply_patch", "input":patch, "status":"completed"
    });
    let server = MockServer::new(vec![
        SseChunk::Data(json!({
            "type":"response.output_item.added", "output_index":0,
            "item":{"type":"custom_tool_call", "id":"ctc_flat", "call_id":"call_flat",
                    "name":"apply_patch", "input":"", "status":"in_progress"}
        })),
        SseChunk::Data(json!({
            "type":"response.custom_tool_call_input.delta", "output_index":0,
            "item_id":"ctc_flat", "delta":patch
        })),
        SseChunk::Data(json!({
            "type":"response.output_item.done", "output_index":0, "item":item
        })),
        SseChunk::Data(json!({
            "type":"response.completed",
            "response":{"id":"resp_flat", "status":"completed", "output":[item]}
        })),
    ]);
    let plan = ModelToolPlan::new(
        "1",
        "deepseek-flat",
        vec![ModelToolEntry {
            address: ModelToolAddress {
                kind: ModelToolKind::Custom,
                namespace: None,
                name: "apply_patch".into(),
            },
            declaration: ModelToolDeclaration::Custom {
                description: "patch".into(),
                format: CustomFormat::Text,
            },
            internal_tool_key: "apply_patch".into(),
            adapter: InputAdapter::TextInput(
                TextInputAdapter::custom_raw("patch")
                    .with_defaults(json!({"dry_run": false})),
            ),
        }],
    )
    .expect("plan");
    let provider = qaqh_gate::ProviderConfig::responses(&server.base_url(), "key", "model", None);
    let mut done = None;
    qaqh_gate::chat_stream_responses_with_plan(
        &provider,
        vec![Message::user("patch")],
        &plan,
        &HashMap::new(),
        0,
        None,
        None,
        None,
        &mut |event| {
            if let qaqh_gate::StreamEvent::Done { raw_message, .. } = event {
                done = Some(raw_message);
            }
        },
    )
    .expect("stream");
    let message = done.expect("done");
    assert!(message.content.iter().any(|block| {
        matches!(block, ContentBlock::ToolUse { id, input, .. }
            if id == "call_flat" && input["patch"] == patch && input["dry_run"] == false)
    }));
    let request = server.last_request_json().expect("request");
    assert_eq!(request["tools"][0]["type"], "custom");
    assert_eq!(request["tools"][0]["name"], "apply_patch");
    assert!(
        request["tools"]
            .as_array()
            .expect("tools")
            .iter()
            .any(|tool| tool["type"] == "custom" && tool["name"] == "apply_patch")
    );
}
