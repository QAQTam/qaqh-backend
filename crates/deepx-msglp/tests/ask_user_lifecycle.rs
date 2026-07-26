//! End-to-end ask_user lifecycle tests driven through UserInput and a mock
//! OpenAI-compatible SSE endpoint. These tests exercise the production new
//! Ring export and never use Ui2Agent::ToolCall as a lifecycle shortcut.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Once};
use std::thread;
use std::time::{Duration, Instant};

use deepx_msglp::state::agent::AgentState;
use deepx_msglp::ring::loop_core::Loop;
use deepx_proto::{Agent2Ui, AskAnswer, AskMode, AskResolution, Ui2Agent};
use serde_json::{Value, json};
use tiny_http::{Header, Response, Server};

static SESSION_INIT: Once = Once::new();
static TEST_LOCK: Mutex<()> = Mutex::new(());

struct MockServer {
    base_url: String,
    requests: Arc<AtomicUsize>,
    bodies: Arc<Mutex<Vec<String>>>,
    stop: Arc<Mutex<bool>>,
    handle: Option<thread::JoinHandle<()>>,
}

impl MockServer {
    fn sequential_with_delay(scenarios: Vec<Vec<String>>, response_delay: Duration) -> Self {
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
                let mut body = String::new();
                let _ = request.as_reader().read_to_string(&mut body);
                request_bodies.lock().expect("body lock").push(body);
                request_counter.fetch_add(1, Ordering::SeqCst);
                let scenario = scenarios
                    .lock()
                    .expect("scenario lock")
                    .pop_front()
                    .expect("unexpected extra gate request");
                let mut sse = String::new();
                for data in scenario {
                    sse.push_str("data: ");
                    sse.push_str(&data);
                    sse.push_str("\n\n");
                }
                let response = Response::from_string(sse).with_header(
                    "Content-Type: text/event-stream"
                        .parse::<Header>()
                        .expect("content-type header"),
                );
                if !response_delay.is_zero() {
                    thread::sleep(response_delay);
                }
                let _ = request.respond(response);
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

impl Drop for MockServer {
    fn drop(&mut self) {
        *self.stop.lock().expect("stop lock") = true;
        if let Some(handle) = self.handle.take() {
            handle.join().expect("mock server thread");
        }
    }
}

fn tool_round(calls: &[(&str, &str, Value)]) -> Vec<String> {
    let mut events = calls
        .iter()
        .enumerate()
        .map(|(index, (id, name, args))| {
            json!({
                "choices": [{
                    "index": 0,
                    "delta": {
                        "tool_calls": [{
                            "index": index,
                            "id": id,
                            "type": "function",
                            "function": {"name": name, "arguments": args.to_string()}
                        }]
                    }
                }]
            })
            .to_string()
        })
        .collect::<Vec<_>>();
    events.push(
        json!({
            "choices": [{"index": 0, "delta": {}, "finish_reason": "tool_calls"}],
            "usage": {"prompt_tokens": 10, "completion_tokens": 5, "total_tokens": 15}
        })
        .to_string(),
    );
    events.push("[DONE]".into());
    events
}

fn final_round(text: &str) -> Vec<String> {
    vec![
        json!({"choices": [{"index": 0, "delta": {"content": text}}]}).to_string(),
        json!({
            "choices": [{"index": 0, "delta": {}, "finish_reason": "stop"}],
            "usage": {"prompt_tokens": 12, "completion_tokens": 4, "total_tokens": 16}
        })
        .to_string(),
        "[DONE]".into(),
    ]
}

fn send(writer: &mut os_pipe::PipeWriter, command: Ui2Agent) {
    writeln!(writer, "{}", serde_json::to_string(&command).unwrap()).unwrap();
    writer.flush().unwrap();
}

fn expect_event(
    receiver: &std::sync::mpsc::Receiver<Agent2Ui>,
    timeout: Duration,
    predicate: impl Fn(&Agent2Ui) -> bool,
) -> Agent2Ui {
    let deadline = Instant::now() + timeout;
    let mut seen = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match receiver.recv_timeout(remaining) {
            Ok(event) if predicate(&event) => return event,
            Ok(Agent2Ui::Error { message }) => panic!("unexpected agent error: {message}"),
            Ok(event) => seen.push(format!("{event:?}")),
            Err(error) => panic!("event timeout/disconnect: {error}; seen={seen:#?}"),
        }
    }
}

fn expect_error(receiver: &std::sync::mpsc::Receiver<Agent2Ui>, timeout: Duration) -> String {
    let deadline = Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match receiver.recv_timeout(remaining) {
            Ok(Agent2Ui::Error { message }) => return message,
            Ok(_) => {}
            Err(error) => panic!("error event timeout/disconnect: {error}"),
        }
    }
}

fn collect_through_done(receiver: &std::sync::mpsc::Receiver<Agent2Ui>) -> Vec<Agent2Ui> {
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut events = Vec::new();
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        let event = receiver
            .recv_timeout(remaining)
            .expect("resumed turn did not complete");
        let done = matches!(event, Agent2Ui::Done);
        if let Agent2Ui::Error { ref message } = event {
            panic!("unexpected agent error: {message}");
        }
        events.push(event);
        if done {
            return events;
        }
    }
}

fn assert_no_terminal_event(receiver: &std::sync::mpsc::Receiver<Agent2Ui>, duration: Duration) {
    let deadline = Instant::now() + duration;
    while Instant::now() < deadline {
        match receiver.recv_timeout(Duration::from_millis(25)) {
            Ok(event)
                if matches!(
                    event,
                    Agent2Ui::Cancelled | Agent2Ui::TurnEnd { .. } | Agent2Ui::Done
                ) =>
            {
                panic!("duplicate terminal event after Done: {event:?}")
            }
            Ok(_) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
        }
    }
}

fn find_tool_result_content(value: &Value, call_id: &str) -> Option<String> {
    match value {
        Value::Object(object) => {
            if object
                .get("tool_call_id")
                .and_then(Value::as_str)
                .is_some_and(|id| id == call_id)
                && object.get("role").and_then(Value::as_str) == Some("tool")
            {
                return object
                    .get("content")
                    .and_then(Value::as_str)
                    .map(str::to_string);
            }
            object
                .values()
                .find_map(|child| find_tool_result_content(child, call_id))
        }
        Value::Array(array) => array
            .iter()
            .find_map(|child| find_tool_result_content(child, call_id)),
        _ => None,
    }
}

fn run_case(
    scenarios: Vec<Vec<String>>,
    expected_requests: usize,
    test: impl FnOnce(
        &mut os_pipe::PipeWriter,
        &std::sync::mpsc::Receiver<Agent2Ui>,
        Arc<AtomicUsize>,
        String,
    ) + Send
    + 'static,
) -> Vec<String> {
    run_case_with_delay(scenarios, Duration::ZERO, expected_requests, test)
}

fn run_case_with_delay(
    scenarios: Vec<Vec<String>>,
    response_delay: Duration,
    expected_requests: usize,
    test: impl FnOnce(
        &mut os_pipe::PipeWriter,
        &std::sync::mpsc::Receiver<Agent2Ui>,
        Arc<AtomicUsize>,
        String,
    ) + Send
    + 'static,
) -> Vec<String> {
    SESSION_INIT.call_once(|| {
        deepx_session::SessionManager::init(deepx_types::platform::data_dir(), false);
    });
    let temp = tempfile::tempdir().unwrap();
    std::fs::write(
        temp.path().join("input.txt"),
        "hello from permission test\n",
    )
    .unwrap();
    std::fs::write(temp.path().join("input2.txt"), "second permission input\n").unwrap();
    let mock = MockServer::sequential_with_delay(scenarios, response_delay);
    let request_count = mock.requests.clone();
    deepx_tools::set_workspace(&temp.path().to_string_lossy());

    let mut agent = AgentState::init("ask-lifecycle-test");
    agent.ephemeral = true;
    agent.config.permission_level = 1;
    agent.config.base_url = mock.base_url.clone();
    agent.config.api_key = "sk-test".into();
    agent.config.model = "test-model".into();
    agent.config.provider_id.clear();
    agent.config.endpoint.clear();
    agent.config.compliance_enabled = false;

    let (input_reader, mut input_writer) = os_pipe::pipe().unwrap();
    let (output_reader, output_writer) = os_pipe::pipe().unwrap();
    let mut agent_loop = Loop::new_ipc(agent, BufReader::new(input_reader), output_writer);
    let (event_tx, event_rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        for line in BufReader::new(output_reader).lines().map_while(Result::ok) {
            if let Ok(event) = serde_json::from_str::<Agent2Ui>(&line) {
                if event_tx.send(event).is_err() {
                    break;
                }
            }
        }
    });

    let workspace = temp.path().to_path_buf();
    let driver = thread::spawn(move || {
        send(&mut input_writer, Ui2Agent::CreateSession);
        let seed = match expect_event(&event_rx, Duration::from_secs(5), |event| {
            matches!(event, Agent2Ui::SessionCreated { .. })
        }) {
            Agent2Ui::SessionCreated { seed } => seed,
            _ => unreachable!(),
        };
        deepx_tools::set_workspace(&workspace.to_string_lossy());
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            test(&mut input_writer, &event_rx, request_count, seed)
        }));
        send(&mut input_writer, Ui2Agent::Shutdown);
        if let Err(payload) = outcome {
            std::panic::resume_unwind(payload);
        }
    });

    agent_loop.run();
    driver.join().expect("test driver");
    assert_eq!(mock.requests.load(Ordering::SeqCst), expected_requests);
    mock.bodies.lock().expect("body lock").clone()
}

#[test]
fn batch_ask_waits_for_every_answer_and_writes_one_exact_result() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let bodies = run_case(
        vec![
            tool_round(&[(
                "ask-batch",
                "ask_user",
                json!({
                    "questions": [
                        {"id":"q1", "question":"First?", "options":["A","B"], "allow_custom":false},
                        {"id":"q2", "question":"Second?", "options":["C","D"], "allow_custom":false}
                    ]
                }),
            )]),
            final_round("finished"),
        ],
        2,
        |writer, receiver, request_count, _seed| {
            send(
                writer,
                Ui2Agent::UserInput {
                    text: "ask me".into(),
                },
            );
            let ask = expect_event(receiver, Duration::from_secs(5), |event| {
                matches!(
                    event,
                    Agent2Ui::AskUser {
                        ask_id,
                        mode: AskMode::Batch,
                        questions,
                        ..
                    } if ask_id == "ask-batch" && questions.len() == 2
                )
            });
            assert!(matches!(
                ask,
                Agent2Ui::AskUser {
                    turn_id,
                    round_num: 0,
                    ..
                } if !turn_id.is_empty()
            ));

            send(
                writer,
                Ui2Agent::AskResponse {
                    ask_id: "ask-batch".into(),
                    answers: vec![AskAnswer {
                        question_id: "q1".into(),
                        answer: "A".into(),
                    }],
                },
            );
            expect_event(receiver, Duration::from_secs(5), |event| {
                matches!(
                    event,
                    Agent2Ui::AskRejected { ask_id, .. } if ask_id == "ask-batch"
                )
            });
            assert_eq!(request_count.load(Ordering::SeqCst), 1);

            send(
                writer,
                Ui2Agent::AskResponse {
                    ask_id: "ask-batch".into(),
                    answers: vec![
                        AskAnswer {
                            question_id: "q1".into(),
                            answer: "A".into(),
                        },
                        AskAnswer {
                            question_id: "q2".into(),
                            answer: "D".into(),
                        },
                    ],
                },
            );
            expect_event(receiver, Duration::from_secs(5), |event| {
                matches!(
                    event,
                    Agent2Ui::AskResolved {
                        ask_id,
                        resolution: AskResolution::Answered
                    } if ask_id == "ask-batch"
                )
            });
            let events = collect_through_done(receiver);
            let tool_results = events
                .iter()
                .filter_map(|event| match event {
                    Agent2Ui::ToolResults { results, .. } => Some(results),
                    _ => None,
                })
                .collect::<Vec<_>>();
            assert_eq!(tool_results.len(), 1);
            assert_eq!(tool_results[0].len(), 1);
            assert_eq!(tool_results[0][0].tool_call_id, "ask-batch");
            assert_eq!(
                events
                    .iter()
                    .filter(|event| matches!(event, Agent2Ui::TurnEnd { .. }))
                    .count(),
                1
            );
            assert_eq!(
                events
                    .iter()
                    .filter(|event| matches!(event, Agent2Ui::Done))
                    .count(),
                1
            );
            assert_no_terminal_event(receiver, Duration::from_millis(250));
        },
    );

    let second_request: Value = serde_json::from_str(&bodies[1]).unwrap();
    let content = find_tool_result_content(&second_request, "ask-batch")
        .expect("second request must include ask result");
    let result: Value = serde_json::from_str(&content).expect("ask result is structured JSON");
    assert_eq!(
        result,
        json!({
            "status": "answered",
            "answers": [
                {"question_id":"q1", "answer":"A"},
                {"question_id":"q2", "answer":"D"}
            ]
        })
    );
}

#[test]
fn multiple_ask_calls_are_presented_sequentially_before_one_resume() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let bodies = run_case(
        vec![
            tool_round(&[
                (
                    "ask-1",
                    "ask_user",
                    json!({"question":"First?", "options":["A"], "allow_custom":false}),
                ),
                (
                    "ask-2",
                    "ask_user",
                    json!({"question":"Second?", "options":["B"], "allow_custom":false}),
                ),
            ]),
            final_round("finished"),
        ],
        2,
        |writer, receiver, request_count, _seed| {
            send(
                writer,
                Ui2Agent::UserInput {
                    text: "ask twice".into(),
                },
            );
            expect_event(
                receiver,
                Duration::from_secs(5),
                |event| matches!(event, Agent2Ui::AskUser { ask_id, .. } if ask_id == "ask-1"),
            );
            send(
                writer,
                Ui2Agent::AskResponse {
                    ask_id: "ask-1".into(),
                    answers: vec![AskAnswer {
                        question_id: "q1".into(),
                        answer: "A".into(),
                    }],
                },
            );
            expect_event(receiver, Duration::from_secs(5), |event| {
                matches!(
                    event,
                    Agent2Ui::AskResolved {
                        ask_id,
                        resolution: AskResolution::Answered
                    } if ask_id == "ask-1"
                )
            });
            expect_event(
                receiver,
                Duration::from_secs(5),
                |event| matches!(event, Agent2Ui::AskUser { ask_id, .. } if ask_id == "ask-2"),
            );
            assert_eq!(request_count.load(Ordering::SeqCst), 1);

            send(
                writer,
                Ui2Agent::AskResponse {
                    ask_id: "ask-2".into(),
                    answers: vec![AskAnswer {
                        question_id: "q1".into(),
                        answer: "B".into(),
                    }],
                },
            );
            expect_event(receiver, Duration::from_secs(5), |event| {
                matches!(
                    event,
                    Agent2Ui::AskResolved {
                        ask_id,
                        resolution: AskResolution::Answered
                    } if ask_id == "ask-2"
                )
            });
            let events = collect_through_done(receiver);
            let results = events.iter().find_map(|event| match event {
                Agent2Ui::ToolResults { results, .. } => Some(results),
                _ => None,
            });
            let results = results.expect("one unified tool result event");
            assert_eq!(results.len(), 2);
            assert_eq!(results[0].tool_call_id, "ask-1");
            assert_eq!(results[1].tool_call_id, "ask-2");
        },
    );

    for (call_id, expected) in [("ask-1", "A"), ("ask-2", "B")] {
        let second_request: Value = serde_json::from_str(&bodies[1]).unwrap();
        let content = find_tool_result_content(&second_request, call_id)
            .expect("second request must include each ask result");
        let result: Value = serde_json::from_str(&content).unwrap();
        assert_eq!(result["answers"][0]["answer"], expected);
    }
}

#[test]
fn invalid_or_stale_responses_do_not_consume_the_active_ask() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    run_case(
        vec![
            tool_round(&[(
                "active-ask",
                "ask_user",
                json!({"question":"Pick A", "options":["A"], "allow_custom":false}),
            )]),
            final_round("finished"),
        ],
        2,
        |writer, receiver, request_count, _seed| {
            send(
                writer,
                Ui2Agent::UserInput {
                    text: "validate identity".into(),
                },
            );
            expect_event(
                receiver,
                Duration::from_secs(5),
                |event| matches!(event, Agent2Ui::AskUser { ask_id, .. } if ask_id == "active-ask"),
            );

            let invalid = [
                Ui2Agent::AskResponse {
                    ask_id: "stale-ask".into(),
                    answers: vec![AskAnswer {
                        question_id: "q1".into(),
                        answer: "A".into(),
                    }],
                },
                Ui2Agent::AskResponse {
                    ask_id: "active-ask".into(),
                    answers: vec![
                        AskAnswer {
                            question_id: "q1".into(),
                            answer: "A".into(),
                        },
                        AskAnswer {
                            question_id: "q1".into(),
                            answer: "A".into(),
                        },
                    ],
                },
                Ui2Agent::AskResponse {
                    ask_id: "active-ask".into(),
                    answers: vec![AskAnswer {
                        question_id: "unknown".into(),
                        answer: "A".into(),
                    }],
                },
                Ui2Agent::AskResponse {
                    ask_id: "active-ask".into(),
                    answers: vec![AskAnswer {
                        question_id: "q1".into(),
                        answer: "B".into(),
                    }],
                },
            ];

            for command in invalid {
                let rejected_id = match &command {
                    Ui2Agent::AskResponse { ask_id, .. } => ask_id.clone(),
                    _ => unreachable!(),
                };
                send(writer, command);
                expect_event(receiver, Duration::from_secs(5), |event| {
                    matches!(
                        event,
                        Agent2Ui::AskRejected { ask_id, .. } if ask_id == &rejected_id
                    )
                });
                assert_eq!(request_count.load(Ordering::SeqCst), 1);
            }

            send(
                writer,
                Ui2Agent::AskResponse {
                    ask_id: "active-ask".into(),
                    answers: vec![AskAnswer {
                        question_id: "q1".into(),
                        answer: "A".into(),
                    }],
                },
            );
            expect_event(
                receiver,
                Duration::from_secs(5),
                |event| matches!(event, Agent2Ui::AskResolved { ask_id, .. } if ask_id == "active-ask"),
            );
            collect_through_done(receiver);
        },
    );
}

#[test]
fn dismiss_validates_identity_and_does_not_swallow_the_next_user_input() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    run_case(
        vec![
            tool_round(&[(
                "dismiss-ask",
                "ask_user",
                json!({"question":"Continue?", "options":["yes"], "allow_custom":false}),
            )]),
            final_round("fresh turn finished"),
        ],
        2,
        |writer, receiver, request_count, _seed| {
            send(
                writer,
                Ui2Agent::UserInput {
                    text: "start dismiss case".into(),
                },
            );
            expect_event(
                receiver,
                Duration::from_secs(5),
                |event| matches!(event, Agent2Ui::AskUser { ask_id, .. } if ask_id == "dismiss-ask"),
            );

            send(
                writer,
                Ui2Agent::AskDismiss {
                    ask_id: "stale-dismiss".into(),
                },
            );
            expect_event(receiver, Duration::from_secs(5), |event| {
                matches!(
                    event,
                    Agent2Ui::AskRejected { ask_id, .. } if ask_id == "stale-dismiss"
                )
            });
            assert_eq!(request_count.load(Ordering::SeqCst), 1);

            send(
                writer,
                Ui2Agent::AskDismiss {
                    ask_id: "dismiss-ask".into(),
                },
            );
            expect_event(receiver, Duration::from_secs(5), |event| {
                matches!(
                    event,
                    Agent2Ui::AskResolved {
                        ask_id,
                        resolution: AskResolution::Dismissed
                    } if ask_id == "dismiss-ask"
                )
            });
            let aborted = collect_through_done(receiver);
            assert_eq!(
                aborted
                    .iter()
                    .filter(|event| matches!(event, Agent2Ui::Cancelled))
                    .count(),
                1
            );
            assert_eq!(
                aborted
                    .iter()
                    .filter(|event| matches!(event, Agent2Ui::TurnEnd { .. }))
                    .count(),
                1
            );

            send(
                writer,
                Ui2Agent::UserInput {
                    text: "fresh input".into(),
                },
            );
            let fresh = collect_through_done(receiver);
            assert!(fresh.iter().any(|event| matches!(
                event,
                Agent2Ui::RoundComplete { answer: Some(answer), .. }
                    if answer == "fresh turn finished"
            )));
        },
    );
}

#[test]
fn permission_then_ask_resolves_the_same_tool_round_once() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let bodies = run_case(
        vec![
            tool_round(&[
                ("read-call", "read", json!({"path":"input.txt"})),
                (
                    "ask-after-read",
                    "ask_user",
                    json!({"question":"Continue?", "options":["yes"], "allow_custom":false}),
                ),
            ]),
            final_round("finished"),
        ],
        2,
        |writer, receiver, request_count, _seed| {
            send(
                writer,
                Ui2Agent::UserInput {
                    text: "read then ask".into(),
                },
            );
            expect_event(receiver, Duration::from_secs(5), |event| {
                matches!(
                    event,
                    Agent2Ui::PermissionRequest { tool_call_id, .. }
                        if tool_call_id == "read-call"
                )
            });
            assert_eq!(request_count.load(Ordering::SeqCst), 1);

            send(
                writer,
                Ui2Agent::PermissionResponse {
                    tool_call_id: "read-call".into(),
                    approved: true,
                    trust_folder: false,
                },
            );
            expect_event(
                receiver,
                Duration::from_secs(5),
                |event| matches!(event, Agent2Ui::AskUser { ask_id, .. } if ask_id == "ask-after-read"),
            );
            assert_eq!(request_count.load(Ordering::SeqCst), 1);

            send(
                writer,
                Ui2Agent::AskResponse {
                    ask_id: "ask-after-read".into(),
                    answers: vec![AskAnswer {
                        question_id: "q1".into(),
                        answer: "yes".into(),
                    }],
                },
            );
            let events = collect_through_done(receiver);
            let results = events.iter().find_map(|event| match event {
                Agent2Ui::ToolResults { results, .. } => Some(results),
                _ => None,
            });
            let results = results.expect("one unified tool result event");
            assert_eq!(results.len(), 2);
            assert_eq!(results[0].tool_call_id, "read-call");
            assert_eq!(results[1].tool_call_id, "ask-after-read");
        },
    );

    let second_request: Value = serde_json::from_str(&bodies[1]).unwrap();
    assert!(find_tool_result_content(&second_request, "read-call").is_some());
    assert!(find_tool_result_content(&second_request, "ask-after-read").is_some());
}

#[test]
fn every_permission_resolves_before_the_queued_ask_is_presented() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    run_case(
        vec![
            tool_round(&[
                ("read-one", "read", json!({"path":"input.txt"})),
                ("read-two", "read", json!({"path":"input2.txt"})),
                (
                    "ask-after-two-reads",
                    "ask_user",
                    json!({"question":"Continue?", "options":["yes"], "allow_custom":false}),
                ),
            ]),
            final_round("finished"),
        ],
        2,
        |writer, receiver, request_count, _seed| {
            send(
                writer,
                Ui2Agent::UserInput {
                    text: "approve both then ask".into(),
                },
            );
            for expected in ["read-one", "read-two"] {
                expect_event(receiver, Duration::from_secs(5), |event| {
                    matches!(
                        event,
                        Agent2Ui::PermissionRequest { tool_call_id, .. }
                            if tool_call_id == expected
                    )
                });
            }

            send(
                writer,
                Ui2Agent::PermissionResponse {
                    tool_call_id: "read-one".into(),
                    approved: true,
                    trust_folder: false,
                },
            );
            let deadline = Instant::now() + Duration::from_millis(250);
            while Instant::now() < deadline {
                match receiver.recv_timeout(Duration::from_millis(25)) {
                    Ok(event)
                        if matches!(
                            event,
                            Agent2Ui::AskUser { .. }
                                | Agent2Ui::ToolResults { .. }
                                | Agent2Ui::TurnEnd { .. }
                                | Agent2Ui::Done
                        ) =>
                    {
                        panic!("turn advanced before all permissions resolved: {event:?}")
                    }
                    Ok(Agent2Ui::Error { message }) => panic!("unexpected error: {message}"),
                    Ok(_) | Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                    Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
                }
            }
            assert_eq!(request_count.load(Ordering::SeqCst), 1);

            send(
                writer,
                Ui2Agent::PermissionResponse {
                    tool_call_id: "read-two".into(),
                    approved: true,
                    trust_folder: false,
                },
            );
            expect_event(receiver, Duration::from_secs(5), |event| {
                matches!(
                    event,
                    Agent2Ui::AskUser { ask_id, .. } if ask_id == "ask-after-two-reads"
                )
            });
            send(
                writer,
                Ui2Agent::AskResponse {
                    ask_id: "ask-after-two-reads".into(),
                    answers: vec![AskAnswer {
                        question_id: "q1".into(),
                        answer: "yes".into(),
                    }],
                },
            );
            let events = collect_through_done(receiver);
            let results = events.iter().find_map(|event| match event {
                Agent2Ui::ToolResults { results, .. } => Some(results),
                _ => None,
            });
            assert_eq!(results.expect("unified tool results").len(), 3);
        },
    );
}

#[test]
fn cancel_aborts_one_suspended_turn_and_invalidates_its_ask_id() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    run_case(
        vec![tool_round(&[(
            "cancel-ask",
            "ask_user",
            json!({"question":"Wait?", "options":["yes"], "allow_custom":false}),
        )])],
        1,
        |writer, receiver, request_count, _seed| {
            send(
                writer,
                Ui2Agent::UserInput {
                    text: "start cancel case".into(),
                },
            );
            expect_event(
                receiver,
                Duration::from_secs(5),
                |event| matches!(event, Agent2Ui::AskUser { ask_id, .. } if ask_id == "cancel-ask"),
            );

            send(writer, Ui2Agent::Cancel);
            let aborted = collect_through_done(receiver);
            assert_eq!(
                aborted
                    .iter()
                    .filter(|event| matches!(event, Agent2Ui::Cancelled))
                    .count(),
                1
            );
            assert_eq!(
                aborted
                    .iter()
                    .filter(|event| matches!(event, Agent2Ui::TurnEnd { .. }))
                    .count(),
                1
            );
            assert_eq!(
                aborted
                    .iter()
                    .filter(|event| matches!(event, Agent2Ui::Done))
                    .count(),
                1
            );

            send(
                writer,
                Ui2Agent::AskResponse {
                    ask_id: "cancel-ask".into(),
                    answers: vec![AskAnswer {
                        question_id: "q1".into(),
                        answer: "yes".into(),
                    }],
                },
            );
            expect_event(
                receiver,
                Duration::from_secs(5),
                |event| matches!(event, Agent2Ui::AskRejected { ask_id, .. } if ask_id == "cancel-ask"),
            );
            assert_eq!(request_count.load(Ordering::SeqCst), 1);
        },
    );
}

#[test]
fn new_session_invalidates_the_suspended_ask() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    run_case(
        vec![
            tool_round(&[(
                "new-session-ask",
                "ask_user",
                json!({"question":"Switch?", "options":["yes"], "allow_custom":false}),
            )]),
            final_round("stale answer was consumed"),
        ],
        1,
        |writer, receiver, request_count, _seed| {
            send(
                writer,
                Ui2Agent::UserInput {
                    text: "start new-session case".into(),
                },
            );
            expect_event(
                receiver,
                Duration::from_secs(5),
                |event| matches!(event, Agent2Ui::AskUser { ask_id, .. } if ask_id == "new-session-ask"),
            );
            send(writer, Ui2Agent::NewSession);
            expect_event(receiver, Duration::from_secs(5), |event| {
                matches!(event, Agent2Ui::SessionCreated { .. })
            });
            send(
                writer,
                Ui2Agent::AskResponse {
                    ask_id: "new-session-ask".into(),
                    answers: vec![AskAnswer {
                        question_id: "q1".into(),
                        answer: "yes".into(),
                    }],
                },
            );
            expect_event(
                receiver,
                Duration::from_secs(5),
                |event| matches!(event, Agent2Ui::AskRejected { ask_id, .. } if ask_id == "new-session-ask"),
            );
            assert_eq!(request_count.load(Ordering::SeqCst), 1);
        },
    );
}

#[test]
fn resume_session_invalidates_the_suspended_ask() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    run_case(
        vec![
            tool_round(&[(
                "resume-session-ask",
                "ask_user",
                json!({"question":"Resume?", "options":["yes"], "allow_custom":false}),
            )]),
            final_round("stale answer was consumed"),
        ],
        1,
        |writer, receiver, request_count, seed| {
            send(
                writer,
                Ui2Agent::UserInput {
                    text: "start resume-session case".into(),
                },
            );
            expect_event(
                receiver,
                Duration::from_secs(5),
                |event| matches!(event, Agent2Ui::AskUser { ask_id, .. } if ask_id == "resume-session-ask"),
            );
            send(writer, Ui2Agent::ResumeSession { seed });
            expect_event(receiver, Duration::from_secs(5), |event| {
                matches!(event, Agent2Ui::SessionRestored { .. })
            });
            send(
                writer,
                Ui2Agent::AskResponse {
                    ask_id: "resume-session-ask".into(),
                    answers: vec![AskAnswer {
                        question_id: "q1".into(),
                        answer: "yes".into(),
                    }],
                },
            );
            expect_event(
                receiver,
                Duration::from_secs(5),
                |event| matches!(event, Agent2Ui::AskRejected { ask_id, .. } if ask_id == "resume-session-ask"),
            );
            assert_eq!(request_count.load(Ordering::SeqCst), 1);
        },
    );
}

#[test]
fn undo_invalidates_the_suspended_ask() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    run_case(
        vec![tool_round(&[(
            "undo-ask",
            "ask_user",
            json!({"question":"Undo?", "options":["yes"], "allow_custom":false}),
        )])],
        1,
        |writer, receiver, request_count, _seed| {
            send(
                writer,
                Ui2Agent::UserInput {
                    text: "start undo case".into(),
                },
            );
            let turn_id = match expect_event(
                receiver,
                Duration::from_secs(5),
                |event| matches!(event, Agent2Ui::AskUser { ask_id, .. } if ask_id == "undo-ask"),
            ) {
                Agent2Ui::AskUser { turn_id, .. } => turn_id,
                _ => unreachable!(),
            };
            send(writer, Ui2Agent::UndoTurn { turn_id });
            expect_event(receiver, Duration::from_secs(5), |event| {
                matches!(event, Agent2Ui::SessionRestored { .. })
            });
            send(
                writer,
                Ui2Agent::AskResponse {
                    ask_id: "undo-ask".into(),
                    answers: vec![AskAnswer {
                        question_id: "q1".into(),
                        answer: "yes".into(),
                    }],
                },
            );
            expect_event(
                receiver,
                Duration::from_secs(5),
                |event| matches!(event, Agent2Ui::AskRejected { ask_id, .. } if ask_id == "undo-ask"),
            );
            assert_eq!(request_count.load(Ordering::SeqCst), 1);
        },
    );
}

#[test]
fn cancel_during_gate_emits_one_complete_terminal_transaction() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    run_case_with_delay(
        vec![final_round("too late")],
        Duration::from_millis(300),
        1,
        |writer, receiver, request_count, _seed| {
            send(
                writer,
                Ui2Agent::UserInput {
                    text: "cancel during gate".into(),
                },
            );
            expect_event(receiver, Duration::from_secs(5), |event| {
                matches!(event, Agent2Ui::TurnStart { .. })
            });
            let deadline = Instant::now() + Duration::from_secs(5);
            while request_count.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
                thread::sleep(Duration::from_millis(10));
            }
            assert_eq!(request_count.load(Ordering::SeqCst), 1);
            send(writer, Ui2Agent::Cancel);
            let events = collect_through_done(receiver);
            assert_eq!(
                events
                    .iter()
                    .filter(|event| matches!(event, Agent2Ui::Cancelled))
                    .count(),
                1
            );
            assert_eq!(
                events
                    .iter()
                    .filter(|event| matches!(event, Agent2Ui::TurnEnd { .. }))
                    .count(),
                1
            );
            assert_eq!(
                events
                    .iter()
                    .filter(|event| matches!(event, Agent2Ui::Done))
                    .count(),
                1
            );
        },
    );
}

#[test]
fn stale_undo_does_not_consume_the_active_ask() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    run_case(
        vec![
            tool_round(&[(
                "undo-identity-ask",
                "ask_user",
                json!({"question":"Continue?", "options":["yes"], "allow_custom":false}),
            )]),
            final_round("finished"),
        ],
        2,
        |writer, receiver, request_count, _seed| {
            send(
                writer,
                Ui2Agent::UserInput {
                    text: "validate undo identity".into(),
                },
            );
            expect_event(
                receiver,
                Duration::from_secs(5),
                |event| matches!(event, Agent2Ui::AskUser { ask_id, .. } if ask_id == "undo-identity-ask"),
            );
            send(
                writer,
                Ui2Agent::UndoTurn {
                    turn_id: "stale-turn".into(),
                },
            );
            assert!(expect_error(receiver, Duration::from_secs(5)).contains("active turn"));
            assert_eq!(request_count.load(Ordering::SeqCst), 1);

            send(
                writer,
                Ui2Agent::AskResponse {
                    ask_id: "undo-identity-ask".into(),
                    answers: vec![AskAnswer {
                        question_id: "q1".into(),
                        answer: "yes".into(),
                    }],
                },
            );
            expect_event(
                receiver,
                Duration::from_secs(5),
                |event| matches!(event, Agent2Ui::AskResolved { ask_id, .. } if ask_id == "undo-identity-ask"),
            );
            collect_through_done(receiver);
        },
    );
}
