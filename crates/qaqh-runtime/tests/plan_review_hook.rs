//! Regression test for the deterministic TUI plan-review hook.
//!
//! This intentionally runs through the production `Loop` and worker channels:
//! the hook must yield a real `PlanReviewRequested`, then both approve and
//! reject must resume through `handle_plan_response` into the next provider
//! round.

mod common;

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Once};
use std::thread;
use std::time::{Duration, Instant};

use qaqh_domain::{
    ControlCommand, ControlEvent, ConversationCommand, ConversationEvent, SessionState,
    TimelineBlockKind, TimelineIntent, TimelineToolState,
};
use qaqh_ringing::{
    RingingCommand, RingingEvent, RingingTimelineIntentEnvelope, RingingWorkerCommandEnvelope,
    RingingWorkerEventEnvelope,
};
use qaqh_runtime::agent::state::agent::AgentState;
use serde_json::json;
use tiny_http::{Header, Response, Server};

static SESSION_INIT: Once = Once::new();

struct MockProvider {
    base_url: String,
    requests: Arc<AtomicUsize>,
    bodies: Arc<Mutex<Vec<serde_json::Value>>>,
    stop: Arc<Mutex<bool>>,
    handle: Option<thread::JoinHandle<()>>,
}

impl MockProvider {
    fn sequential(scenarios: Vec<Vec<String>>) -> Self {
        let server = Server::http("127.0.0.1:0").expect("bind mock server");
        let port = server.server_addr().to_ip().expect("mock address").port();
        let requests = Arc::new(AtomicUsize::new(0));
        let bodies = Arc::new(Mutex::new(Vec::new()));
        let stop = Arc::new(Mutex::new(false));
        let scenarios = Arc::new(Mutex::new(VecDeque::from(scenarios)));
        let request_counter = requests.clone();
        let request_bodies = bodies.clone();
        let stop_flag = stop.clone();
        let handle = thread::spawn(move || {
            loop {
                if *stop_flag.lock().expect("stop lock") {
                    break;
                }
                let mut request = match server.recv_timeout(Duration::from_millis(50)) {
                    Ok(Some(request)) => request,
                    Ok(None) => continue,
                    Err(_) => break,
                };
                request_counter.fetch_add(1, Ordering::SeqCst);
                let mut request_body = String::new();
                request
                    .as_reader()
                    .read_to_string(&mut request_body)
                    .expect("read provider request");
                request_bodies
                    .lock()
                    .expect("request bodies lock")
                    .push(serde_json::from_str(&request_body).expect("provider request json"));
                let scenario = scenarios
                    .lock()
                    .expect("scenario lock")
                    .pop_front()
                    .expect("unexpected extra provider request");
                let mut body = String::new();
                for data in scenario {
                    body.push_str("data: ");
                    body.push_str(&data);
                    body.push_str("\n\n");
                }
                let response = Response::from_string(body).with_header(
                    "Content-Type: text/event-stream"
                        .parse::<Header>()
                        .expect("content-type header"),
                );
                request.respond(response).expect("mock response");
            }
        });
        Self {
            base_url: format!("http://127.0.0.1:{port}"),
            requests,
            bodies,
            stop,
            handle: Some(handle),
        }
    }
}

impl Drop for MockProvider {
    fn drop(&mut self) {
        *self.stop.lock().expect("stop lock") = true;
        if let Some(handle) = self.handle.take() {
            handle.join().expect("mock server thread");
        }
    }
}

struct EnvGuard;

impl Drop for EnvGuard {
    fn drop(&mut self) {
        unsafe {
            std::env::remove_var("QAQH_TEST_PLAN_REVIEW");
            std::env::remove_var("QAQH_TEST_PLAN_REVIEW_CONTENT");
        }
    }
}

fn next_command_id() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::SeqCst)
}

fn send_cmd(writer: &mut os_pipe::PipeWriter, session_id: &str, command: RingingCommand) {
    let env =
        RingingWorkerCommandEnvelope::new(session_id, format!("c{}", next_command_id()), command);
    writeln!(
        writer,
        "{}",
        serde_json::to_string(&env).expect("serialize envelope")
    )
    .expect("write frame");
    writer.flush().expect("flush pipe");
}

fn cmd_session_create() -> RingingCommand {
    RingingCommand::Control(ControlCommand::SessionCreate {
        close_current: false,
        cwd: None,
        tool_mode: None,
        custom_tools: Vec::new(),
    })
}

fn cmd_user_input(text: &str) -> RingingCommand {
    RingingCommand::Conversation(ConversationCommand::ConversationSendMessage {
        text: text.into(),
        images: vec![],
        attachments: None,
        message_id: None,
        input_purpose: qaqh_domain::ConversationInputPurpose::TriggerTurn,
        as_system: false,
        inter_agent: None,
        subagent_terminal: None,
    })
}

fn cmd_plan_respond(id: &str, approved: bool, message: Option<&str>) -> RingingCommand {
    RingingCommand::Control(ControlCommand::PlanReviewRespond {
        interaction_id: id.into(),
        approved,
        message: message.map(str::to_string),
        autonomous: false,
    })
}

fn cmd_session_shutdown() -> RingingCommand {
    RingingCommand::Control(ControlCommand::SessionShutdown)
}

fn expect_event(
    receiver: &std::sync::mpsc::Receiver<RingingEvent>,
    timeout: Duration,
    predicate: impl Fn(&RingingEvent) -> bool,
) -> RingingEvent {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match receiver.recv_timeout(remaining) {
            Ok(event) if predicate(&event) => return event,
            Ok(_) => {}
            Err(error) => panic!("event timeout/disconnect: {error}"),
        }
    }
}

fn expect_session_created(receiver: &std::sync::mpsc::Receiver<RingingEvent>) -> String {
    match expect_event(receiver, Duration::from_secs(5), |event| {
        matches!(
            event,
            RingingEvent::Control(ControlEvent::SessionStateChanged {
                state: SessionState::Created,
                ..
            })
        )
    }) {
        RingingEvent::Control(ControlEvent::SessionStateChanged { session_id, .. }) => session_id,
        other => panic!("expected SessionStateChanged(Created), got {other:?}"),
    }
}

fn expect_plan_review(
    receiver: &std::sync::mpsc::Receiver<RingingEvent>,
) -> (String, String, String) {
    match expect_event(receiver, Duration::from_secs(5), |event| {
        matches!(
            event,
            RingingEvent::Control(ControlEvent::PlanReviewRequested { .. })
        )
    }) {
        RingingEvent::Control(ControlEvent::PlanReviewRequested {
            interaction_id,
            turn_id,
            plan_content,
            review_type,
            ..
        }) => {
            assert_eq!(review_type, "plan");
            assert!(
                interaction_id.starts_with("test-plan-review-"),
                "unexpected interaction id: {interaction_id}"
            );
            assert!(!turn_id.is_empty());
            assert!(!plan_content.is_empty());
            (interaction_id, turn_id, plan_content)
        }
        other => panic!("expected PlanReviewRequested, got {other:?}"),
    }
}

fn expect_turn_completed(receiver: &std::sync::mpsc::Receiver<RingingEvent>) {
    expect_event(receiver, Duration::from_secs(10), |event| {
        matches!(
            event,
            RingingEvent::Conversation(ConversationEvent::TurnCompleted { .. })
        )
    });
}

fn final_round(text: &str) -> Vec<String> {
    vec![
        json!({"choices": [{"index": 0, "delta": {"content": text}}]}).to_string(),
        json!({"choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}]}).to_string(),
        "[DONE]".into(),
    ]
}

fn assert_plan_tool_block_opened(intents: &Arc<Mutex<Vec<TimelineIntent>>>, call_id: &str) {
    let block_id = format!("tool:{call_id}");
    let found = intents
        .lock()
        .expect("timeline intents lock")
        .iter()
        .any(|intent| {
            matches!(
                intent,
                TimelineIntent::BlockOpened {
                    block_id: actual,
                    kind: TimelineBlockKind::Tool,
                    tool: Some(tool),
                    ..
                } if actual == &block_id
                    && tool.name == "plan_submit"
                    && tool.state == TimelineToolState::Prepared
            )
        });
    assert!(found, "plan tool block was not opened for {call_id}");
}

fn assert_plan_tool_terminal(
    intents: &Arc<Mutex<Vec<TimelineIntent>>>,
    call_id: &str,
    expected_state: TimelineToolState,
) {
    let block_id = format!("tool:{call_id}");
    let found = intents
        .lock()
        .expect("timeline intents lock")
        .iter()
        .any(|intent| {
            matches!(
                intent,
                TimelineIntent::ToolUpdated {
                    block_id: actual,
                    tool,
                    ..
                } if actual == &block_id && tool.state == expected_state
            )
        });
    assert!(
        found,
        "plan tool block did not reach {expected_state:?} for {call_id}"
    );
}

fn assert_provider_plan_pair(body: &serde_json::Value, expected_result: &str) {
    let messages = body
        .get("messages")
        .and_then(serde_json::Value::as_array)
        .expect("provider request messages");
    let tool = messages
        .iter()
        .rev()
        .find(|message| message["role"] == "tool")
        .expect("provider request tool result");
    let tool_json = serde_json::to_string(tool).expect("serialize tool result");
    assert!(
        tool_json.contains(expected_result),
        "missing expected plan result {expected_result:?}: {tool_json}"
    );
    let assistant = messages
        .iter()
        .rev()
        .find(|message| message["role"] == "assistant" && message.get("tool_calls").is_some())
        .expect("provider request assistant tool call");
    let call_id = assistant["tool_calls"][0]["id"]
        .as_str()
        .expect("assistant tool call id");
    assert_eq!(
        tool["tool_call_id"].as_str(),
        Some(call_id),
        "plan result must pair with the assistant tool call"
    );
}

#[test]
fn plan_review_hook_yields_then_resumes_on_approve_and_reject() {
    SESSION_INIT.call_once(|| {
        qaqh_session::SessionManager::init(qaqh_types::platform::data_dir());
    });

    unsafe {
        std::env::set_var("QAQH_TEST_PLAN_REVIEW", "1");
        std::env::set_var("QAQH_TEST_PLAN_REVIEW_CONTENT", "hook test plan");
    }
    let _env_guard = EnvGuard;

    let mock = MockProvider::sequential(vec![
        final_round("approved follow-up"),
        final_round("rejected follow-up"),
    ]);
    let temp = tempfile::tempdir().expect("tempdir");
    qaqh_workspace::set_workspace(&temp.path().to_string_lossy());

    let mut agent = AgentState::init("plan-review-hook-test", qaqh_config::Config::default());
    agent.ephemeral = true;
    agent.config.permission_level = 1;
    agent.config.base_url = mock.base_url.clone();
    agent.config.api_key = "sk-test".into();
    agent.config.model = "test-model".into();
    agent.config.provider_id.clear();
    agent.config.endpoint.clear();
    agent.config.compliance_enabled = false;

    let (input_reader, mut input_writer) = os_pipe::pipe().expect("os pipe");
    let (output_reader, output_writer) = os_pipe::pipe().expect("os pipe");
    let mut agent_loop =
        common::spawn_pipe_loop(agent, BufReader::new(input_reader), output_writer);
    let (event_tx, event_rx) = std::sync::mpsc::channel();
    let timeline_intents = Arc::new(Mutex::new(Vec::new()));
    let timeline_sink = timeline_intents.clone();
    thread::spawn(move || {
        for line in BufReader::new(output_reader).lines().map_while(Result::ok) {
            if let Ok(env) = serde_json::from_str::<RingingWorkerEventEnvelope>(&line) {
                if event_tx.send(env.event).is_err() {
                    break;
                }
            } else if let Ok(env) = serde_json::from_str::<RingingTimelineIntentEnvelope>(&line) {
                timeline_sink
                    .lock()
                    .expect("timeline intents lock")
                    .push(env.intent);
            }
        }
    });

    let requests = mock.requests.clone();
    let request_bodies = mock.bodies.clone();
    let timeline_assertions = timeline_intents.clone();
    let driver = thread::spawn(move || {
        send_cmd(&mut input_writer, "", cmd_session_create());
        let session_id = expect_session_created(&event_rx);

        send_cmd(&mut input_writer, &session_id, cmd_user_input("first"));
        let (first_id, _, first_content) = expect_plan_review(&event_rx);
        assert_eq!(first_content, "hook test plan");
        assert_plan_tool_block_opened(&timeline_assertions, &first_id);
        assert_eq!(
            requests.load(Ordering::SeqCst),
            0,
            "plan review must be emitted before the provider call"
        );
        send_cmd(
            &mut input_writer,
            &session_id,
            cmd_plan_respond(&first_id, true, None),
        );
        expect_turn_completed(&event_rx);
        assert_plan_tool_terminal(
            &timeline_assertions,
            &first_id,
            TimelineToolState::Succeeded,
        );

        send_cmd(&mut input_writer, &session_id, cmd_user_input("second"));
        let (second_id, _, second_content) = expect_plan_review(&event_rx);
        assert_eq!(second_content, "hook test plan");
        assert_plan_tool_block_opened(&timeline_assertions, &second_id);
        send_cmd(
            &mut input_writer,
            &session_id,
            cmd_plan_respond(&second_id, false, Some("not this time")),
        );
        expect_turn_completed(&event_rx);
        assert_plan_tool_terminal(&timeline_assertions, &second_id, TimelineToolState::Failed);

        send_cmd(&mut input_writer, &session_id, cmd_session_shutdown());
    });

    agent_loop.run();
    driver.join().expect("test driver");
    assert_eq!(mock.requests.load(Ordering::SeqCst), 2);
    let bodies = request_bodies.lock().expect("request bodies lock");
    assert_eq!(bodies.len(), 2);
    assert_provider_plan_pair(&bodies[0], "Plan approved.");
    assert_provider_plan_pair(&bodies[1], "Plan rejected: not this time");
}
