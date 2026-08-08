//! Backend lifecycle tests — simulate frontend create-session + send-message
//! without daemon / WebSocket / Electron.
//!
//! M3：全部走 Ringing worker 线格式（`wire: "Ringing_domain_v1"`），
//! legacy `Ui2Agent` / `Agent2Ui` 已完全拆除。
//!
//! * create_session_emits_session_state — pure session creation
//! * send_message_triggers_turn_lifecycle — full frontend simulation:
//!   create session → send text → verify TurnStarted / RoundDelta / terminal
//! * ringing_send_is_not_dropped_during_a_session_switch

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Once};
use std::thread;
use std::time::{Duration, Instant};

use deepx_domain::{
    ControlCommand, ControlEvent, ConversationCommand, ConversationEvent, SessionState,
};
use deepx_msglp::ringing_v1::loop_core::Loop;
use deepx_msglp::state::agent::AgentState;
use deepx_ringing::{
    RingingCommand, RingingEvent, RingingWorkerCommandEnvelope, RingingWorkerEventEnvelope,
};
use serde_json::json;
use tiny_http::{Header, Response, Server};

static SESSION_INIT: Once = Once::new();
static SESSION_TEST_LOCK: Mutex<()> = Mutex::new(());

// ── Mock LLM server ────────────────────────────────────────────────────

struct MockServer {
    base_url: String,
    requests: Arc<AtomicUsize>,
    stop: Arc<AtomicBool>,
    handle: Option<thread::JoinHandle<()>>,
}

impl MockServer {
    fn single_response(events: Vec<String>) -> Self {
        let server = Server::http("127.0.0.1:0").expect("bind mock");
        let port = server.server_addr().to_ip().expect("addr").port();
        let requests = Arc::new(AtomicUsize::new(0));
        let stop = Arc::new(AtomicBool::new(false));
        let stop_flag = stop.clone();
        let req_count = requests.clone();
        let scenarios = Arc::new(Mutex::new(VecDeque::from([events])));
        let handle =
            thread::spawn(move || {
                loop {
                    if stop_flag.load(Ordering::SeqCst) {
                        break;
                    }
                    let mut request = match server.recv_timeout(Duration::from_millis(50)) {
                        Ok(Some(r)) => r,
                        Ok(None) => continue,
                        Err(_) => break,
                    };
                    let mut body = String::new();
                    let _ = request.as_reader().read_to_string(&mut body);
                    req_count.fetch_add(1, Ordering::SeqCst);
                    let scenario = scenarios
                        .lock()
                        .expect("lock")
                        .pop_front()
                        .expect("unexpected gate request");
                    let mut sse = String::new();
                    for data in scenario {
                        sse.push_str("data: ");
                        sse.push_str(&data);
                        sse.push_str("\n\n");
                    }
                    request
                        .respond(Response::from_string(sse).with_header(
                            "Content-Type: text/event-stream".parse::<Header>().unwrap(),
                        ))
                        .expect("respond");
                }
            });
        Self {
            base_url: format!("http://127.0.0.1:{port}"),
            requests,
            stop,
            handle: Some(handle),
        }
    }
}

impl Drop for MockServer {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        if let Some(h) = self.handle.take() {
            h.join().expect("mock server join");
        }
    }
}

// ── SSE scenario builders ──────────────────────────────────────────────

/// A single text-round response.
fn text_round(content: &str) -> Vec<String> {
    vec![
        json!({"choices":[{"index":0,"delta":{"content":content}}]}).to_string(),
        json!({"choices":[{"index":0,"delta":{},"finish_reason":"stop"}],
               "usage":{"prompt_tokens":10,"completion_tokens":3,"total_tokens":13}})
        .to_string(),
        "[DONE]".into(),
    ]
}

// ── helpers ────────────────────────────────────────────────────────────

fn next_command_id() -> u64 {
    static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(1);
    NEXT.fetch_add(1, Ordering::SeqCst)
}

fn send_cmd(w: &mut os_pipe::PipeWriter, seed: &str, command: RingingCommand) {
    let env = RingingWorkerCommandEnvelope::new(seed, format!("c{}", next_command_id()), command);
    writeln!(w, "{}", serde_json::to_string(&env).unwrap()).unwrap();
    w.flush().unwrap();
}

fn cmd_session_create() -> RingingCommand {
    RingingCommand::Control(ControlCommand::SessionCreate {
        close_current: false,
    })
}

fn cmd_session_resume(seed: &str) -> RingingCommand {
    RingingCommand::Control(ControlCommand::SessionResume { seed: seed.into() })
}

fn cmd_session_shutdown() -> RingingCommand {
    RingingCommand::Control(ControlCommand::SessionShutdown)
}

fn cmd_user_input(text: &str) -> RingingCommand {
    RingingCommand::Conversation(ConversationCommand::ConversationSendMessage {
        text: text.into(),
        images: vec![],
        attachments: None,
    })
}

fn expect(
    rx: &std::sync::mpsc::Receiver<RingingEvent>,
    timeout: Duration,
    pred: impl Fn(&RingingEvent) -> bool,
) -> RingingEvent {
    let dl = Instant::now() + timeout;
    loop {
        match rx.recv_timeout(dl.saturating_duration_since(Instant::now())) {
            Ok(e) if pred(&e) => return e,
            Ok(e) => {
                eprintln!("skipped event: {e:?}");
            }
            Err(e) => panic!("timeout/disconnect: {e}"),
        }
    }
}

/// 事件收集线程：解析 Ringing worker 事件帧并转发。
fn spawn_event_reader(oread: os_pipe::PipeReader) -> std::sync::mpsc::Receiver<RingingEvent> {
    let (tx, rx) = std::sync::mpsc::channel::<RingingEvent>();
    thread::spawn(move || {
        for line in BufReader::new(oread).lines().map_while(Result::ok) {
            if let Ok(env) = serde_json::from_str::<RingingWorkerEventEnvelope>(&line) {
                if tx.send(env.event).is_err() {
                    break;
                }
            }
        }
    });
    rx
}

// ── tests ──────────────────────────────────────────────────────────────

#[test]
fn create_session_emits_session_state() {
    let _test_lock = SESSION_TEST_LOCK.lock().unwrap();
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    deepx_workspace::set_workspace(&ws.to_string_lossy());
    SESSION_INIT
        .call_once(|| deepx_session::SessionManager::init(deepx_types::platform::data_dir()));

    let mut agent = AgentState::init("test");
    agent.ephemeral = true;

    let (ir, mut iw) = os_pipe::pipe().unwrap();
    let (oread, owrite) = os_pipe::pipe().unwrap();
    let mut lp = Loop::new_ipc(agent, BufReader::new(ir), owrite);
    let rx = spawn_event_reader(oread);

    let drv = thread::spawn(move || {
        send_cmd(&mut iw, "", cmd_session_create());
        let seed = match expect(&rx, Duration::from_secs(10), |e| {
            matches!(
                e,
                RingingEvent::Control(ControlEvent::SessionStateChanged {
                    state: SessionState::Created,
                    ..
                })
            )
        }) {
            RingingEvent::Control(ControlEvent::SessionStateChanged { seed, .. }) => seed,
            other => panic!("expected SessionStateChanged(Created), got {other:?}"),
        };
        assert!(!seed.is_empty());
        send_cmd(&mut iw, &seed, cmd_session_shutdown());
    });
    lp.run();
    drv.join().unwrap();
}

#[test]
fn send_message_triggers_turn_lifecycle() {
    let _test_lock = SESSION_TEST_LOCK.lock().unwrap();
    let mock = MockServer::single_response(text_round("Hello from DeepX"));

    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    deepx_workspace::set_workspace(&ws.to_string_lossy());

    SESSION_INIT
        .call_once(|| deepx_session::SessionManager::init(deepx_types::platform::data_dir()));

    let mut agent = AgentState::init("test");
    agent.ephemeral = true;
    agent.config.base_url = mock.base_url.clone();
    agent.config.api_key = "sk-test".into();
    agent.config.model = "test-model".into();
    agent.config.provider_id.clear();
    agent.config.endpoint.clear();
    agent.config.compliance_enabled = false;

    let (ir, mut iw) = os_pipe::pipe().unwrap();
    let (oread, owrite) = os_pipe::pipe().unwrap();
    let mut lp = Loop::new_ipc(agent, BufReader::new(ir), owrite);
    let rx = spawn_event_reader(oread);

    let drv = thread::spawn(move || {
        // Step 1: create session
        send_cmd(&mut iw, "", cmd_session_create());
        let seed = match expect(&rx, Duration::from_secs(10), |e| {
            matches!(
                e,
                RingingEvent::Control(ControlEvent::SessionStateChanged {
                    state: SessionState::Created,
                    ..
                })
            )
        }) {
            RingingEvent::Control(ControlEvent::SessionStateChanged { seed, .. }) => seed,
            other => panic!("expected SessionStateChanged(Created), got {other:?}"),
        };

        // Step 2: send a user message (this is what the frontend does)
        send_cmd(&mut iw, &seed, cmd_user_input("Hi!"));

        // Step 3: verify the full turn lifecycle
        expect(&rx, Duration::from_secs(15), |e| {
            matches!(
                e,
                RingingEvent::Conversation(ConversationEvent::TurnStarted { .. })
            )
        });
        expect(&rx, Duration::from_secs(15), |e| {
            matches!(
                e,
                RingingEvent::Conversation(ConversationEvent::RoundDelta {
                    kind: deepx_domain::RoundDeltaKind::Answering,
                    ..
                })
            )
        });
        expect(&rx, Duration::from_secs(15), |e| {
            matches!(
                e,
                RingingEvent::Conversation(ConversationEvent::RoundCompleted { .. })
            )
        });
        expect(&rx, Duration::from_secs(15), |e| {
            matches!(
                e,
                RingingEvent::Conversation(ConversationEvent::TurnCompleted { .. })
            )
        });

        send_cmd(&mut iw, &seed, cmd_session_shutdown());
    });
    lp.run();
    drv.join().unwrap();

    assert_eq!(
        mock.requests.load(Ordering::SeqCst),
        1,
        "expected exactly 1 LLM request"
    );
}

#[test]
fn ringing_send_is_not_dropped_during_a_session_switch() {
    let _test_lock = SESSION_TEST_LOCK.lock().unwrap();
    let mock = MockServer::single_response(text_round("queued send"));
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    deepx_workspace::set_workspace(&ws.to_string_lossy());
    SESSION_INIT
        .call_once(|| deepx_session::SessionManager::init(deepx_types::platform::data_dir()));

    let mut agent = AgentState::init("test");
    agent.ephemeral = true;
    agent.config.base_url = mock.base_url.clone();
    agent.config.api_key = "sk-test".into();
    agent.config.model = "test-model".into();
    agent.config.provider_id.clear();
    agent.config.endpoint.clear();
    agent.config.compliance_enabled = false;

    let (ir, mut iw) = os_pipe::pipe().unwrap();
    let (oread, owrite) = os_pipe::pipe().unwrap();
    let mut lp = Loop::new_ipc(agent, BufReader::new(ir), owrite);
    let rx = spawn_event_reader(oread);

    let drv = thread::spawn(move || {
        send_cmd(&mut iw, "", cmd_session_create());
        let seed = match expect(&rx, Duration::from_secs(10), |e| {
            matches!(
                e,
                RingingEvent::Control(ControlEvent::SessionStateChanged {
                    state: SessionState::Created,
                    ..
                })
            )
        }) {
            RingingEvent::Control(ControlEvent::SessionStateChanged { seed, .. }) => seed,
            other => panic!("expected SessionStateChanged(Created), got {other:?}"),
        };

        // Session switch + 紧接的 Ringing send：切换完成后 send 必须到达 provider
        // （Ringing 命令经 deferred 队列保留，不丢）。
        send_cmd(&mut iw, &seed, cmd_session_resume(&seed));
        send_cmd(&mut iw, &seed, cmd_user_input("Hi after resume"));

        let deadline = Instant::now() + Duration::from_secs(15);
        while mock.requests.load(Ordering::SeqCst) == 0 && Instant::now() < deadline {
            thread::sleep(Duration::from_millis(25));
        }
        assert_eq!(
            mock.requests.load(Ordering::SeqCst),
            1,
            "queued send must reach the provider"
        );
        send_cmd(&mut iw, &seed, cmd_session_shutdown());
    });
    lp.run();
    drv.join().unwrap();
}
