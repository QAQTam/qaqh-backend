//! Backend lifecycle tests — simulate frontend create-session + send-message
//! without daemon / WebSocket / Electron.
//!
//! * create_session_emits_session_created_and_ready — pure session creation
//! * send_message_triggers_turn_lifecycle — full frontend simulation:
//!   create session → send text → verify TurnStart / answer / Done

use std::collections::VecDeque;
use std::io::{BufRead, BufReader, Write};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, Once};
use std::thread;
use std::time::{Duration, Instant};

use deepx_msglp::state::agent::AgentState;
use deepx_msglp::ring::loop_core::Loop;
use deepx_proto::{Agent2Ui, Ui2Agent};
use serde_json::json;
use tiny_http::{Header, Response, Server};

static SESSION_INIT: Once = Once::new();

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
        let handle = thread::spawn(move || loop {
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
            let scenario = scenarios.lock().expect("lock").pop_front()
                .expect("unexpected gate request");
            let mut sse = String::new();
            for data in scenario {
                sse.push_str("data: ");
                sse.push_str(&data);
                sse.push_str("\n\n");
            }
            request.respond(
                Response::from_string(sse)
                    .with_header("Content-Type: text/event-stream".parse::<Header>().unwrap()),
            ).expect("respond");
        });
        Self { base_url: format!("http://127.0.0.1:{port}"), requests, stop, handle: Some(handle) }
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

fn send(w: &mut os_pipe::PipeWriter, cmd: Ui2Agent) {
    writeln!(w, "{}", serde_json::to_string(&cmd).unwrap()).unwrap();
    w.flush().unwrap();
}

fn expect(rx: &std::sync::mpsc::Receiver<Agent2Ui>, timeout: Duration, pred: impl Fn(&Agent2Ui) -> bool) -> Agent2Ui {
    let dl = Instant::now() + timeout;
    loop {
        match rx.recv_timeout(dl.saturating_duration_since(Instant::now())) {
            Ok(e) if pred(&e) => return e,
            Ok(Agent2Ui::Error { message }) => panic!("agent error: {message}"),
            Ok(_) => {}
            Err(e) => panic!("timeout/disconnect: {e}"),
        }
    }
}

// ── tests ──────────────────────────────────────────────────────────────

#[test]
fn create_session_emits_session_created_and_ready() {
    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    deepx_tools::set_workspace(&ws.to_string_lossy());
    SESSION_INIT.call_once(|| deepx_session::SessionManager::init(deepx_types::platform::data_dir(), false));

    let mut agent = AgentState::init("test");
    agent.ephemeral = true;

    let (ir, mut iw) = os_pipe::pipe().unwrap();
    let (oread, owrite) = os_pipe::pipe().unwrap();
    let mut lp = Loop::new_ipc(agent, BufReader::new(ir), owrite);
    let (tx, rx) = std::sync::mpsc::channel::<Agent2Ui>();
    thread::spawn(move || {
        for line in BufReader::new(oread).lines().map_while(Result::ok) {
            if let Ok(ev) = serde_json::from_str::<Agent2Ui>(&line) { if tx.send(ev).is_err() { break; } }
        }
    });

    let drv = thread::spawn(move || {
        send(&mut iw, Ui2Agent::CreateSession);
        let seed = match expect(&rx, Duration::from_secs(10), |e| matches!(e, Agent2Ui::SessionCreated { .. })) {
            Agent2Ui::SessionCreated { seed } => seed,
            _ => unreachable!(),
        };
        assert!(!seed.is_empty());
        expect(&rx, Duration::from_secs(10), |e| matches!(e, Agent2Ui::Ready));
        send(&mut iw, Ui2Agent::Shutdown);
    });
    lp.run();
    drv.join().unwrap();
}

#[test]
fn send_message_triggers_turn_lifecycle() {
    let mock = MockServer::single_response(text_round("Hello from DeepX"));

    let tmp = tempfile::tempdir().unwrap();
    let ws = tmp.path().join("ws");
    std::fs::create_dir(&ws).unwrap();
    deepx_tools::set_workspace(&ws.to_string_lossy());

    SESSION_INIT.call_once(|| deepx_session::SessionManager::init(deepx_types::platform::data_dir(), false));

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
    let (tx, rx) = std::sync::mpsc::channel::<Agent2Ui>();
    thread::spawn(move || {
        for line in BufReader::new(oread).lines().map_while(Result::ok) {
            if let Ok(ev) = serde_json::from_str::<Agent2Ui>(&line) { if tx.send(ev).is_err() { break; } }
        }
    });

    let drv = thread::spawn(move || {
        // Step 1: create session
        send(&mut iw, Ui2Agent::CreateSession);
        expect(&rx, Duration::from_secs(10), |e| matches!(e, Agent2Ui::SessionCreated { .. }));
        expect(&rx, Duration::from_secs(10), |e| matches!(e, Agent2Ui::Ready));

        // Step 2: send a user message (this is what the frontend does)
        send(&mut iw, Ui2Agent::UserInput { text: "Hi!".into(), images: vec![] });

        // Step 3: verify the full turn lifecycle
        expect(&rx, Duration::from_secs(15), |e| matches!(e, Agent2Ui::TurnStart { .. }));
        expect(&rx, Duration::from_secs(15), |e| {
            matches!(e, Agent2Ui::RoundDelta { kind: deepx_proto::RoundDeltaKind::Answering, .. })
        });
        expect(&rx, Duration::from_secs(15), |e| matches!(e, Agent2Ui::RoundComplete { .. }));
        expect(&rx, Duration::from_secs(15), |e| matches!(e, Agent2Ui::TurnEnd { .. }));
        expect(&rx, Duration::from_secs(15), |e| matches!(e, Agent2Ui::Done));

        send(&mut iw, Ui2Agent::Shutdown);
    });
    lp.run();
    drv.join().unwrap();

    assert_eq!(mock.requests.load(Ordering::SeqCst), 1, "expected exactly 1 LLM request");
}
