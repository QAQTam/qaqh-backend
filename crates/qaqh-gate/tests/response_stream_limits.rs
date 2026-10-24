mod common;
use common::mock_server::{MockServer, SseChunk};
use qaqh_gate::{ProviderConfig, StreamEvent};
use qaqh_types::Message;
use serde_json::json;
use std::sync::atomic::Ordering;

fn run(mock: &MockServer) -> (anyhow::Result<()>, Vec<StreamEvent>) {
    let provider = ProviderConfig::responses(&mock.base_url(), "test", "test-model", None);
    let mut events = Vec::new();
    let result = qaqh_gate::chat_stream(
        &provider,
        vec![Message::user("inspect")],
        None,
        4096,
        None,
        None,
        None,
        &mut |event| events.push(event),
    );
    (result, events)
}

fn event(value: serde_json::Value) -> SseChunk {
    SseChunk::Raw(format!("data: {value}\n\n"))
}

#[test]
fn malformed_response_after_text_is_fatal_not_a_continuation() {
    let mock = MockServer::new(vec![
        event(json!({"type":"response.output_text.delta","item_id":"m1","delta":"partial"})),
        SseChunk::Raw("data: {not-json}\n\n".into()),
    ]);
    let (result, events) = run(&mock);
    assert!(
        result.is_err(),
        "protocol errors must not be salvaged as successful EOF"
    );
    assert!(
        events
            .iter()
            .any(|ev| matches!(ev, StreamEvent::Error { .. }))
    );
    assert!(
        !events
            .iter()
            .any(|ev| matches!(ev, StreamEvent::Done { .. }))
    );
    assert_eq!(mock.request_count.load(Ordering::SeqCst), 1);
}

#[test]
fn responses_large_terminal_event_completes_in_one_request() {
    let text = "x".repeat(128 * 1024);
    let mock = MockServer::new(vec![
        event(
            json!({"type":"response.output_text.delta","item_id":"m1","output_index":0,"delta":text}),
        ),
        event(
            json!({"type":"response.completed","response":{"id":"r1","status":"completed","model":"test-model","output":[{"type":"message","id":"m1","role":"assistant","status":"completed","content":[{"type":"output_text","text":text,"annotations":[]}]}]}}),
        ),
    ]);
    let (result, events) = run(&mock);
    result.unwrap();
    assert!(events.iter().any(|ev| matches!(
        ev,
        StreamEvent::Done {
            stop_reason: Some(_),
            ..
        }
    )));
    assert_eq!(mock.request_count.load(Ordering::SeqCst), 1);
}
