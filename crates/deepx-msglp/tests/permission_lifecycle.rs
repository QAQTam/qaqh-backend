//! End-to-end permission lifecycle tests driven through UserInput and a mock
//! OpenAI-compatible SSE endpoint. These tests exercise LLM-generated tool
//! calls; they deliberately do not use Ui2Agent::ToolCall.

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Once};
use std::thread;
use std::time::{Duration, Instant};

use deepx_msglp::state::agent::AgentState;
use deepx_msglp::ring::loop_core::Loop;
use deepx_proto::{Agent2Ui, PermissionRisk, Ui2Agent};
use serde_json::json;
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

impl Drop for MockServer {
    fn drop(&mut self) {
        *self.stop.lock().expect("stop lock") = true;
        if let Some(handle) = self.handle.take() {
            handle.join().expect("mock server thread");
        }
    }
}

fn tool_round(calls: &[(&str, &str, serde_json::Value)]) -> Vec<String> {
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

fn marker_exec_args(path: &std::path::Path) -> serde_json::Value {
    #[cfg(windows)]
    {
        let path = path.to_string_lossy().replace('\'', "''");
        json!({
            "argv": [
                "powershell",
                "-NoProfile",
                "-Command",
                format!("Set-Content -LiteralPath '{path}' -Value done"),
            ]
        })
    }
    #[cfg(not(windows))]
    {
        json!({
            "argv": ["sh", "-c", "printf done > \"$1\"", "sh", path.to_string_lossy()]
        })
    }
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
    loop {
        let remaining = deadline.saturating_duration_since(Instant::now());
        match receiver.recv_timeout(remaining) {
            Ok(event) if predicate(&event) => return event,
            Ok(Agent2Ui::Error { message }) => panic!("unexpected agent error: {message}"),
            Ok(_) => {}
            Err(error) => panic!("event timeout/disconnect: {error}"),
        }
    }
}

fn permission_id(receiver: &std::sync::mpsc::Receiver<Agent2Ui>) -> String {
    match expect_event(receiver, Duration::from_secs(5), |event| {
        matches!(event, Agent2Ui::PermissionRequest { .. })
    }) {
        Agent2Ui::PermissionRequest { tool_call_id, .. } => tool_call_id,
        _ => unreachable!(),
    }
}

fn assert_no_round_completion(receiver: &std::sync::mpsc::Receiver<Agent2Ui>) {
    let deadline = Instant::now() + Duration::from_millis(300);
    while Instant::now() < deadline {
        match receiver.recv_timeout(Duration::from_millis(25)) {
            Ok(Agent2Ui::ToolResults { .. } | Agent2Ui::TurnEnd { .. } | Agent2Ui::Done) => {
                panic!("suspended LLM turn completed prematurely")
            }
            Ok(Agent2Ui::Error { message }) => panic!("unexpected agent error: {message}"),
            Ok(_) | Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
            Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => {
                panic!("event channel disconnected")
            }
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

fn assert_single_completion(events: &[Agent2Ui], expected_results: usize) {
    let tool_events = events
        .iter()
        .filter_map(|event| match event {
            Agent2Ui::ToolResults {
                turn_id,
                round_num,
                results,
            } => Some((turn_id, round_num, results)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(tool_events.len(), 1, "tool results must be emitted once");
    assert!(tool_events[0].0.starts_with('t'));
    assert_eq!(*tool_events[0].1, 0);
    assert_eq!(tool_events[0].2.len(), expected_results);
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, Agent2Ui::TurnEnd { .. }))
            .count(),
        1,
        "TurnEnd must be emitted once",
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| matches!(event, Agent2Ui::Done))
            .count(),
        1,
        "Done must be emitted once",
    );
}

fn run_case(
    permission_level: u8,
    workspace: &std::path::Path,
    scenarios: Vec<Vec<String>>,
    expected_requests: usize,
    test: impl FnOnce(&mut os_pipe::PipeWriter, &std::sync::mpsc::Receiver<Agent2Ui>) + Send + 'static,
) -> Vec<String> {
    SESSION_INIT.call_once(|| {
        deepx_session::SessionManager::init(deepx_types::platform::data_dir(), false);
    });
    let mock = MockServer::sequential(scenarios);
    deepx_tools::set_workspace(&workspace.to_string_lossy());

    let mut agent = AgentState::init("permission-lifecycle-test");
    agent.ephemeral = true;
    agent.config.permission_level = permission_level;
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

    let workspace = workspace.to_path_buf();
    let driver = thread::spawn(move || {
        send(&mut input_writer, Ui2Agent::CreateSession);
        expect_event(&event_rx, Duration::from_secs(5), |event| {
            matches!(event, Agent2Ui::SessionCreated { .. })
        });
        // Session creation restores a persisted workspace. Tests need the
        // explicit workspace selection to occur after that lifecycle step.
        deepx_tools::set_workspace(&workspace.to_string_lossy());
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            test(&mut input_writer, &event_rx)
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
fn skill_activation_reaches_followup_round_and_next_user_turn() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let temp = tempfile::tempdir().unwrap();
    let skill_dir = temp.path().join(".agents/skills/sticky-skill");
    std::fs::create_dir_all(&skill_dir).unwrap();
    std::fs::write(
        skill_dir.join("SKILL.md"),
        "---\nname: sticky-skill\ndescription: Use for the lifecycle test.\n---\nSTICKY_SKILL_INSTRUCTION\n",
    )
    .unwrap();

    let bodies = run_case(
        1,
        temp.path(),
        vec![
            tool_round(&[(
                "activate-skill",
                "skills",
                json!({"action": "activate", "name": "sticky-skill"}),
            )]),
            final_round("first turn finished"),
            final_round("second turn finished"),
        ],
        3,
        move |writer, receiver| {
            send(
                writer,
                Ui2Agent::UserInput {
                    text: "use the matching skill".into(),
                },
            );
            assert_eq!(permission_id(receiver), "activate-skill");
            assert_no_round_completion(receiver);
            send(
                writer,
                Ui2Agent::PermissionResponse {
                    tool_call_id: "activate-skill".into(),
                    approved: true,
                    trust_folder: false,
                },
            );
            let first = collect_through_done(receiver);
            assert_single_completion(&first, 1);
            let tool_result = first.iter().find_map(|event| match event {
                Agent2Ui::ToolResults { results, .. } => results.first(),
                _ => None,
            });
            assert!(
                tool_result.is_some_and(|result| result.success),
                "skill tool must execute successfully: {tool_result:?}"
            );

            send(
                writer,
                Ui2Agent::UserInput {
                    text: "continue using it".into(),
                },
            );
            let second = collect_through_done(receiver);
            assert_eq!(
                second
                    .iter()
                    .filter(|event| matches!(event, Agent2Ui::Done))
                    .count(),
                1
            );
        },
    );

    assert!(!bodies[0].contains("STICKY_SKILL_INSTRUCTION"));
    assert!(
        bodies[1].contains("STICKY_SKILL_INSTRUCTION"),
        "follow-up body omitted activation: {}",
        bodies[1]
    );
    // Active state is owned by SkillContextManager and remains authoritative
    // across user turns until its three-successful-turn lease expires.
    assert!(
        bodies[2].contains("STICKY_SKILL_INSTRUCTION"),
        "next-turn should retain leased skill activation: {}",
        bodies[2]
    );
}

#[test]
fn llm_approval_resumes_original_turn_once() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("input.txt");
    std::fs::write(&path, "hello\n").unwrap();
    let call_path = path.clone();
    run_case(
        1,
        temp.path(),
        vec![
            tool_round(&[("llm-read", "read", json!({"path": path}))]),
            final_round("finished"),
        ],
        2,
        move |writer, receiver| {
            send(
                writer,
                Ui2Agent::UserInput {
                    text: "read it".into(),
                },
            );
            match expect_event(receiver, Duration::from_secs(5), |event| {
                matches!(event, Agent2Ui::PermissionRequest { .. })
            }) {
                Agent2Ui::PermissionRequest {
                    tool_call_id,
                    risk,
                    consequence,
                    ..
                } => {
                    assert_eq!(tool_call_id, "llm-read");
                    assert_eq!(risk, PermissionRisk::Low);
                    assert!(!consequence.is_empty());
                }
                _ => unreachable!(),
            }
            assert_no_round_completion(receiver);
            send(
                writer,
                Ui2Agent::PermissionResponse {
                    tool_call_id: "llm-read".into(),
                    approved: true,
                    trust_folder: false,
                },
            );
            let events = collect_through_done(receiver);
            assert_single_completion(&events, 1);
            let result = events.iter().find_map(|event| match event {
                Agent2Ui::ToolResults { results, .. } => results.first(),
                _ => None,
            });
            assert!(result.is_some_and(|result| result.success));
            assert!(call_path.exists());
        },
    );
}

#[test]
fn llm_rejection_resumes_with_original_failure() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let temp = tempfile::tempdir().unwrap();
    let path = temp.path().join("input.txt");
    std::fs::write(&path, "hello\n").unwrap();
    run_case(
        1,
        temp.path(),
        vec![
            tool_round(&[("llm-denied", "read", json!({"path": path}))]),
            final_round("handled denial"),
        ],
        2,
        move |writer, receiver| {
            send(
                writer,
                Ui2Agent::UserInput {
                    text: "read it".into(),
                },
            );
            assert_eq!(permission_id(receiver), "llm-denied");
            send(
                writer,
                Ui2Agent::PermissionResponse {
                    tool_call_id: "llm-denied".into(),
                    approved: false,
                    trust_folder: false,
                },
            );
            let events = collect_through_done(receiver);
            assert_single_completion(&events, 1);
            let result = events.iter().find_map(|event| match event {
                Agent2Ui::ToolResults { results, .. } => results.first(),
                _ => None,
            });
            assert!(result.is_some_and(|result| {
                !result.success
                    && result.tool_call_id == "llm-denied"
                    && result.output.contains("[DENIED]")
            }));
        },
    );
}

#[test]
fn llm_multiple_pending_waits_for_every_response() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let temp = tempfile::tempdir().unwrap();
    let first = temp.path().join("first.txt");
    let second = temp.path().join("second.txt");
    std::fs::write(&first, "one\n").unwrap();
    std::fs::write(&second, "two\n").unwrap();
    run_case(
        1,
        temp.path(),
        vec![
            tool_round(&[
                ("llm-first", "read", json!({"path": first})),
                ("llm-second", "read", json!({"path": second})),
            ]),
            final_round("both finished"),
        ],
        2,
        move |writer, receiver| {
            send(
                writer,
                Ui2Agent::UserInput {
                    text: "read both".into(),
                },
            );
            let mut ids = vec![permission_id(receiver), permission_id(receiver)];
            ids.sort();
            assert_eq!(ids, vec!["llm-first", "llm-second"]);
            send(
                writer,
                Ui2Agent::PermissionResponse {
                    tool_call_id: "llm-first".into(),
                    approved: true,
                    trust_folder: false,
                },
            );
            assert_no_round_completion(receiver);
            send(
                writer,
                Ui2Agent::PermissionResponse {
                    tool_call_id: "llm-second".into(),
                    approved: true,
                    trust_folder: false,
                },
            );
            let events = collect_through_done(receiver);
            assert_single_completion(&events, 2);
        },
    );
}

#[test]
fn llm_four_pending_execs_defer_execution_until_all_resolved() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let temp = tempfile::tempdir().unwrap();
    let markers = (1..=4)
        .map(|index| temp.path().join(format!("exec-{index}.txt")))
        .collect::<Vec<_>>();
    let calls = markers
        .iter()
        .enumerate()
        .map(|(index, path)| (format!("exec-{}", index + 1), marker_exec_args(path)))
        .collect::<Vec<_>>();
    let call_refs = calls
        .iter()
        .map(|(id, args)| (id.as_str(), "exec", args.clone()))
        .collect::<Vec<_>>();
    let expected_markers = markers.clone();

    run_case(
        1,
        temp.path(),
        vec![tool_round(&call_refs), final_round("all execs finished")],
        2,
        move |writer, receiver| {
            send(
                writer,
                Ui2Agent::UserInput {
                    text: "run four commands".into(),
                },
            );
            let mut ids = (0..4).map(|_| permission_id(receiver)).collect::<Vec<_>>();
            ids.sort();
            assert_eq!(ids, vec!["exec-1", "exec-2", "exec-3", "exec-4"]);

            for id in &ids[..3] {
                send(
                    writer,
                    Ui2Agent::PermissionResponse {
                        tool_call_id: id.clone(),
                        approved: true,
                        trust_folder: false,
                    },
                );
            }
            assert_no_round_completion(receiver);
            let execution_deadline = Instant::now() + Duration::from_secs(3);
            while Instant::now() < execution_deadline
                && expected_markers.iter().all(|path| !path.exists())
            {
                thread::sleep(Duration::from_millis(25));
            }
            assert!(
                expected_markers.iter().all(|path| !path.exists()),
                "approved execs must remain deferred until every decision is recorded",
            );

            send(
                writer,
                Ui2Agent::PermissionResponse {
                    tool_call_id: ids[3].clone(),
                    approved: true,
                    trust_folder: false,
                },
            );
            let completion_deadline = Instant::now() + Duration::from_secs(10);
            while Instant::now() < completion_deadline
                && expected_markers.iter().any(|path| !path.exists())
            {
                thread::sleep(Duration::from_millis(25));
            }
            assert!(
                expected_markers.iter().all(|path| path.exists()),
                "approved exec batch did not finish: {:?}",
                expected_markers
                    .iter()
                    .map(|path| path.exists())
                    .collect::<Vec<_>>(),
            );
            let events = collect_through_done(receiver);
            assert_single_completion(&events, 4);
        },
    );
}

#[test]
fn llm_mixed_auto_and_pending_emits_one_unified_result() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let temp = tempfile::tempdir().unwrap();
    let input = temp.path().join("input.txt");
    let output = temp.path().join("output.txt");
    std::fs::write(&input, "hello\n").unwrap();
    let expected_output = output.clone();
    run_case(
        2,
        temp.path(),
        vec![
            tool_round(&[
                ("llm-auto", "read", json!({"path": input})),
                (
                    "llm-pending",
                    "write",
                    json!({"path": output, "content": "created"}),
                ),
            ]),
            final_round("mixed finished"),
        ],
        2,
        move |writer, receiver| {
            send(
                writer,
                Ui2Agent::UserInput {
                    text: "read and write".into(),
                },
            );
            assert_eq!(permission_id(receiver), "llm-pending");
            assert_no_round_completion(receiver);
            send(
                writer,
                Ui2Agent::PermissionResponse {
                    tool_call_id: "llm-pending".into(),
                    approved: true,
                    trust_folder: false,
                },
            );
            let events = collect_through_done(receiver);
            assert_single_completion(&events, 2);
            assert_eq!(
                std::fs::read_to_string(&expected_output).unwrap(),
                "created"
            );
        },
    );
}

#[test]
fn llm_session_switch_invalidates_suspended_turn() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    let temp = tempfile::tempdir().unwrap();
    let output = temp.path().join("must-not-exist.txt");
    let stale_output = output.clone();
    run_case(
        1,
        temp.path(),
        vec![
            tool_round(&[(
                "llm-stale",
                "write",
                json!({"path": output, "content": "unsafe"}),
            )]),
            final_round("new session works"),
        ],
        2,
        move |writer, receiver| {
            send(
                writer,
                Ui2Agent::UserInput {
                    text: "write it".into(),
                },
            );
            assert_eq!(permission_id(receiver), "llm-stale");
            send(writer, Ui2Agent::NewSession);
            expect_event(receiver, Duration::from_secs(5), |event| {
                matches!(event, Agent2Ui::SessionCreated { .. })
            });
            send(
                writer,
                Ui2Agent::PermissionResponse {
                    tool_call_id: "llm-stale".into(),
                    approved: true,
                    trust_folder: false,
                },
            );
            assert!(
                !stale_output.exists(),
                "stale approval executed after switch"
            );
            send(
                writer,
                Ui2Agent::UserInput {
                    text: "continue in new session".into(),
                },
            );
            let events = collect_through_done(receiver);
            assert!(
                events
                    .iter()
                    .any(|event| matches!(event, Agent2Ui::TurnEnd { .. }))
            );
            assert!(!stale_output.exists());
        },
    );
}
