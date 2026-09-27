//! Verifies that accepted conversation input reaches the canonical log.

use std::sync::{Arc, mpsc};
use std::time::{Duration, Instant};

use qaqh_domain::{
    ControlCommand, ControlEvent, ConversationCommand, ConversationInputPurpose,
    InterAgentDelivery, InterAgentEnvelope, SessionState,
};
use qaqh_ringing::{RingingCommand, RingingEvent, RingingWorkerCommandEnvelope};
use qaqh_runtime::agent::loop_core::{Loop, LoopChannels};
use qaqh_runtime::agent::state::agent::AgentState;
use qaqh_runtime::agent::types::{WorkerCommand, WriterEvent};
use qaqh_session::canonical::{CanonicalSessionIdentity, CommittedFactReader};
use qaqh_session::projection::{MailboxProjection, Projection};
use qaqh_session::session_fact_v2::{
    ActorKind, FactPayload, InputKind, InputPurpose, InterAgentContent, MailboxMessageState,
};

fn send_cmd(cmd_tx: &mpsc::SyncSender<WorkerCommand>, session_id: &str, command: RingingCommand) {
    let env = RingingWorkerCommandEnvelope::new(session_id, "input-accepted-test", command);
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
    let inter_agent_message_id = "msg_01J00000000000000000000002";
    let oversized_inter_agent = "inter-agent input ".repeat(600);
    let oversized_inter_agent_for_thread = oversized_inter_agent.clone();
    let hub = Arc::new(qaqh_runtime::RingingHub::with_persistence(
        "input-accepted-content-epoch",
        tmp.path().join("ringing"),
    ));
    let hub_for_loop = Arc::clone(&hub);
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
        let session_id = match expect(&event_rx, Duration::from_secs(10), |event| {
            matches!(
                event,
                RingingEvent::Control(ControlEvent::SessionStateChanged {
                    state: SessionState::Created,
                    ..
                })
            )
        }) {
            RingingEvent::Control(ControlEvent::SessionStateChanged { session_id, .. }) => {
                session_id
            }
            _ => unreachable!(),
        };

        send_cmd(
            &cmd_tx,
            &session_id,
            RingingCommand::Conversation(ConversationCommand::ConversationSendMessage {
                text: "canonical input".into(),
                images: vec![],
                attachments: None,
                message_id: Some(message_id.into()),
                input_purpose: ConversationInputPurpose::QueueOnly,
                as_system: true,
                inter_agent: None,
                subagent_terminal: None,
            }),
        );
        send_cmd(
            &cmd_tx,
            &session_id,
            RingingCommand::Conversation(ConversationCommand::ConversationSendMessage {
                text: oversized_inter_agent_for_thread,
                images: vec![],
                attachments: None,
                message_id: Some(inter_agent_message_id.into()),
                input_purpose: ConversationInputPurpose::QueueOnly,
                as_system: false,
                inter_agent: Some(InterAgentEnvelope {
                    message_id: inter_agent_message_id.into(),
                    root_session_id: session_id.clone(),
                    author: "/root".into(),
                    recipient: "/root/review".into(),
                    other_recipients: vec![],
                    task_id: None,
                    reply_to: None,
                    causation_id: None,
                    delivery: InterAgentDelivery::Queue,
                    created_at_ms: 1_789_830_000_000,
                }),
                subagent_terminal: None,
            }),
        );
        send_cmd(
            &cmd_tx,
            &session_id,
            RingingCommand::Control(ControlCommand::SessionShutdown),
        );
        session_id
    });

    let mut lp = Loop::from_channels(
        agent,
        channels.cmd_rx,
        channels.event_tx,
        channels.cancel,
        channels.writer_dead,
        std::sync::Arc::new(qaqh_runtime::agent::liveness::WorkerLiveness::new()),
        Some(hub_for_loop),
    );
    lp.run();

    let session_id = driver.join().expect("driver thread");
    reader.join().expect("reader thread");

    let session_dir = qaqh_types::platform::sessions_dir().join(&session_id);
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
    let communications: Vec<_> = facts
        .iter()
        .filter_map(|fact| match &fact.payload {
            FactPayload::InterAgentCommunication(payload) => Some(payload),
            _ => None,
        })
        .collect();

    assert_eq!(accepted.len(), 2, "both inputs must be accepted once");
    let normal = accepted
        .iter()
        .find(|payload| payload.client_request_id.as_deref() == Some(message_id))
        .expect("normal input accepted");
    assert_eq!(normal.input_kind, InputKind::System);
    assert_eq!(normal.input_purpose, InputPurpose::QueueOnly);
    assert_eq!(normal.inline_text.as_deref(), Some("canonical input"));

    assert_eq!(communications.len(), 1);
    assert_eq!(
        communications[0].message_id.as_str(),
        inter_agent_message_id
    );
    let communication_ref = match &communications[0].content {
        InterAgentContent::ContentRef { content_ref } => content_ref,
        InterAgentContent::Inline { text } => {
            panic!(
                "oversized inter-agent message stayed inline: {} bytes",
                text.len()
            )
        }
    };
    let communication_store_id = communication_ref
        .hash()
        .as_str()
        .strip_prefix("sha256:")
        .expect("canonical content refs carry the sha256 prefix");
    assert_eq!(
        hub.get_content_any(communication_store_id)
            .expect("inter-agent content resolves")
            .bytes,
        oversized_inter_agent.as_bytes()
    );

    let inter_agent = accepted
        .iter()
        .find(|payload| payload.client_request_id.as_deref() == Some(inter_agent_message_id))
        .expect("inter-agent input accepted");
    assert_eq!(inter_agent.input_kind, InputKind::UserText);
    assert_eq!(inter_agent.input_purpose, InputPurpose::QueueOnly);
    assert!(inter_agent.inline_text.is_none());
    let input_ref = inter_agent
        .content_ref
        .as_ref()
        .expect("oversized InputAccepted carries content_ref");
    assert_eq!(input_ref, communication_ref);
    assert_eq!(inter_agent.actor.kind, ActorKind::Subagent);
    assert_eq!(inter_agent.actor.id, "/root");

    let mut mailbox = MailboxProjection::default();
    for fact in &facts {
        mailbox.apply(fact);
    }
    assert_eq!(mailbox.pending_count(), 0);
    assert_eq!(
        mailbox.snapshot().messages[0].state,
        MailboxMessageState::Delivered
    );
}
