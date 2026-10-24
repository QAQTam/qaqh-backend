//! Exercise real gate requests through TurnEngine, including the second wire body.
use qaqh_runtime::agent::types::{
    CancelToken, Emitter, LoopPhase, Outcome, PendingState, RingContext, StatsCollector,
};
use qaqh_runtime::agent::{
    engine_tool::ToolEngine, engine_turn::TurnEngine, state::agent::AgentState,
};
use qaqh_types::{ContentBlock, CredentialSource, Wire};
use serde_json::{Value, json};
use std::sync::{Arc, atomic::AtomicBool};
use std::time::Duration;

struct Silent;
impl Emitter for Silent {}

fn event(value: Value) -> String {
    format!("data: {value}\n\n")
}
fn delta(text: &str) -> String {
    event(json!({"type":"response.output_text.delta","item_id":"m1","output_index":0,"delta":text}))
}
fn terminal(text: &str) -> String {
    event(
        json!({"type":"response.completed","response":{"id":"r1","model":"test","status":"completed","output":[{"type":"message","id":"m1","role":"assistant","status":"completed","content":[{"type":"output_text","text":text,"annotations":[]}]}]}}),
    )
}

#[test]
fn partial_context_is_replayed_without_user_continue_and_tools_fail_closed() {
    let scenarios = vec![
        delta("First part. "),
        delta("Second part.") + &terminal("Second part."),
        event(
            json!({"type":"response.output_item.added","output_index":0,"item":{"type":"function_call","id":"fc1","call_id":"call1","name":"read","arguments":""}}),
        ) + &event(
            json!({"type":"response.function_call_arguments.delta","output_index":0,"delta":"{\"path\":\"missing\"}"}),
        ),
        delta("a"),
        delta("b"),
        delta("c"),
        delta("d"),
    ];
    let server = tiny_http::Server::http("127.0.0.1:0").unwrap();
    let base_url = format!("http://{}", server.server_addr());
    let worker = std::thread::spawn(move || {
        let mut requests = Vec::new();
        for scenario in scenarios {
            let mut request = server
                .recv_timeout(Duration::from_secs(20))
                .unwrap()
                .expect("expected request");
            let mut body = String::new();
            request.as_reader().read_to_string(&mut body).unwrap();
            requests.push(serde_json::from_str::<Value>(&body).unwrap());
            let header =
                tiny_http::Header::from_bytes("Content-Type", "text/event-stream").unwrap();
            request
                .respond(tiny_http::Response::from_string(scenario).with_header(header))
                .unwrap();
        }
        requests
    });
    let config = qaqh_config::Config {
        wire: Wire::Responses,
        credential: CredentialSource::ApiKey,
        base_url,
        api_key: "test".into(),
        model: "test".into(),
        ..Default::default()
    };
    let mut agent = AgentState::init("stream-recovery", config);
    agent.ephemeral = true;
    agent.msg = qaqh_message::MessageStore::new_ephemeral("stream-recovery");
    agent.msg.push_user("Explain");
    let mut flow = qaqh_message::ContextFlow::new();
    qaqh_message::builtin::register_all(&mut flow);
    let mut phase = LoopPhase::Idle;
    let mut pending = PendingState::default();
    let dead = Arc::new(AtomicBool::new(false));
    let cancel = CancelToken::new();
    let mut stats = StatsCollector::new();
    let emitter = Silent;
    let mut turn = TurnEngine::new();
    let mut tools = ToolEngine::new();
    let mut ctx = RingContext {
        agent: &mut agent,
        emitter: &emitter,
        cancel: &cancel,
        phase: &mut phase,
        pending: &mut pending,
        writer_dead: &dead,
        stats: &mut stats,
        flow: &mut flow,
    };
    assert!(matches!(
        turn.run(&mut ctx, &mut tools, "t1".into(), 0, None),
        Outcome::ContinueTurn { .. }
    ));
    assert!(matches!(
        turn.run(&mut ctx, &mut tools, "t1".into(), 0, None),
        Outcome::TurnComplete { .. }
    ));
    let text: String = ctx
        .agent
        .msg
        .to_vec()
        .iter()
        .flat_map(|m| &m.content)
        .filter_map(|b| match b {
            ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect();
    assert!(text.contains("First part. Second part."), "{text}");
    ctx.agent.msg.push_user("Use a tool");
    assert!(matches!(
        turn.run(&mut ctx, &mut tools, "t2".into(), 0, None),
        Outcome::TurnFailed { .. }
    ));
    ctx.agent.msg.push_user("Exhaust recovery");
    for _ in 0..3 {
        assert!(matches!(
            turn.run(&mut ctx, &mut tools, "t3".into(), 0, None),
            Outcome::ContinueTurn { .. }
        ));
    }
    assert!(matches!(
        turn.run(&mut ctx, &mut tools, "t3".into(), 0, None),
        Outcome::TurnFailed { .. }
    ));
    let requests = worker.join().unwrap();
    assert_eq!(requests.len(), 7);
    let input = requests[1]["input"].as_array().unwrap();
    assert!(
        input
            .iter()
            .any(|item| item["role"] == "assistant" && item.to_string().contains("First part. "))
    );
    assert_eq!(
        input.iter().filter(|item| item["role"] == "user").count(),
        1
    );
    assert!(!requests[1].to_string().contains("继续补全"));
}
