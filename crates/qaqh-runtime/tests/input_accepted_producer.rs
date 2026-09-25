//! Verifies that accepted conversation input reaches the canonical log.

use std::sync::mpsc;
use std::time::{Duration, Instant};

use qaqh_domain::{
    ControlCommand, ControlEvent, ConversationCommand, ConversationInputPurpose, SessionState,
};
use qaqh_ringing::{RingingCommand, RingingEvent, RingingWorkerCommandEnvelope};
use qaqh_runtime::agent::loop_core::{Loop, LoopChannels};
use qaqh_runtime::agent::state::agent::AgentState;
use qaqh_runtime::agent::types::{WorkerCommand, WriterEvent};
use qaqh_session::canonical::{CanonicalSessionIdentity, CommittedFactReader};
use qaqh_session::session_fact_v2::{FactPayload, InputKind, InputPurpose};

fn send_cmd(cmd_tx: &mpsc::SyncSender<WorkerCommand>, seed: &str, command: RingingCommand) {
    let env = RingingWorkerCommandEnvelope::new(seed, "input-accepted-test", command);
    cmd_tx
        .send(WorkerCommand {
            frame: env,
            causation: Some("input-accepted-test".into()),
        })
        .expect("send worker command");
}

fn expect(
    rx: &mpsc::Receiver<RingingEvent>,
    timeout: Duration,
    pred: impl Fn(&RingingEvent) -> bool,
) -> RingingEvent {
    let deadline = Instant::now() + timeout;
    loop {
        match rx.recv_timeout(deadline.saturating_duration_since(Instant::now())) {
            Ok(event) if pred(&event) => return event,
            Ok(_) => {}
            Err(error) => panic!("timeout waiting for event: {error}"),
        }
    }
}

#[test]
fn accepted_input_is_persisted_as_a_canonical_fact() {
    let tmp = tempfile::tempdir().expect("tempdir");
    let data = tmp.path().join("data");
    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(&data).expect("data dir");
    std::fs::create_dir_all(&ws).expect("workspace dir");
    unsafe {
        std::env::set_var("QAQH_DATA_DIR", &data);
    }
    qaqh_workspace::set_workspace(&ws.to_string_lossy());
    qaqh_session::SessionManager::init(qaqh_types::platform::data_dir());

    let mut agent = AgentState::init("input-accepted", qaqh_config::Config::default());
    agent.ephemeral = false;

    let channels = LoopChannels::new();
    let cmd_tx = channels.cmd_tx.clone();
    let (event_tx, event_rx) = mpsc::channel::<RingingEvent>();
    let reader = std::thread::spawn(move || {
        for event in channels.event_rx {
            if let WriterEvent::Ringing(env) = event
                && event_tx.send(env.event).is_err()
            {
                break;
            }
        }
    });

    let message_id = "msg_01J00000000000000000000001";
    let driver = std::thread::spawn(move || {
        send_cmd(
            &cmd_tx,
            "",
            RingingCommand::Control(ControlCommand::SessionCreate {
                close_current: false,
                cwd: None,
                tool_mode: None,
                custom_tools: Vec::new(),
            }),
        );
        let seed = match expect(&event_rx, Duration::from_secs(10), |event| {
            matches!(
                event,
                RingingEvent::Control(ControlEvent::SessionStateChanged {
                    state: SessionState::Created,
                    ..
                })
            )
        }) {
            RingingEvent::Control(ControlEvent::SessionStateChanged { seed, .. }) => seed,
            _ => unreachable!(),
        };

        send_cmd(
            &cmd_tx,
            &seed,
            RingingCommand::Conversation(ConversationCommand::ConversationSendMessage {
                text: "canonical input".into(),
                images: vec![],
                attachments: None,
                message_id: Some(message_id.into()),
                input_purpose: ConversationInputPurpose::QueueOnly,
                as_system: true,
                subagent_terminal: None,
            }),
        );
        send_cmd(
            &cmd_tx,
            &seed,
            RingingCommand::Control(ControlCommand::SessionShutdown),
        );
        seed
    });

    let mut lp = Loop::from_channels(
        agent,
        channels.cmd_rx,
        channels.event_tx,
        channels.cancel,
        channels.writer_dead,
        std::sync::Arc::new(qaqh_runtime::agent::liveness::WorkerLiveness::new()),
    );
    lp.run();

    let seed = driver.join().expect("driver thread");
    reader.join().expect("reader thread");

    let session_dir = qaqh_types::platform::sessions_dir().join(&seed);
    let identity = CanonicalSessionIdentity::open(&session_dir).expect("canonical identity");
    let facts = CommittedFactReader::open(
        &session_dir,
        identity.session_id.clone(),
        identity.log_id.clone(),
    )
    .expect("fact reader")
    .read_all()
    .expect("canonical facts");
    let accepted: Vec<_> = facts
        .iter()
        .filter_map(|fact| match &fact.payload {
            FactPayload::InputAccepted(payload) => Some(payload),
            _ => None,
        })
        .collect();

    assert_eq!(accepted.len(), 1, "accepted input must be written once");
    assert_eq!(accepted[0].input_kind, InputKind::System);
    assert_eq!(accepted[0].input_purpose, InputPurpose::QueueOnly);
    assert_eq!(accepted[0].inline_text.as_deref(), Some("canonical input"));
    assert_eq!(accepted[0].client_request_id.as_deref(), Some(message_id));
}
