use qaqh_session::projection::{MailboxProjection, Projection, ProjectionSet};
use qaqh_session::session_fact_v2::{
    ActorKind, ActorRef, AgentPath, EventId, FactPayload, FactSchema, InputAccepted, InputId,
    InputKind, InputPurpose, InterAgentCommunication, InterAgentContent, InterAgentDelivery, LogId,
    MailboxDelta, MailboxMessageState, MessageId, ProjectionPayload, ProjectionSlot, SessionFact,
    SessionId,
};

const NOW_MS: i64 = 1_789_830_000_100;

fn session_id() -> SessionId {
    SessionId::new("0198f1a0-0000-7000-8000-000000000001")
}

fn log_id() -> LogId {
    LogId::new("0198f1a0-0000-7000-8000-000000000002")
}

fn message_id() -> MessageId {
    MessageId::new("msg_01J00000000000000000000000")
}

fn path(raw: &str) -> AgentPath {
    AgentPath::parse_absolute(raw).expect("valid path")
}

fn communication(delivery: InterAgentDelivery) -> InterAgentCommunication {
    InterAgentCommunication {
        message_id: message_id(),
        root_session_id: session_id(),
        author: path("/root"),
        recipient: path("/root/review"),
        other_recipients: vec![],
        task_id: Some("review".to_string()),
        content: InterAgentContent::Inline {
            text: "review this change".to_string(),
        },
        reply_to: None,
        causation_id: None,
        delivery,
        created_at_ms: NOW_MS,
    }
}

fn fact(fact_seq: u64, payload: FactPayload) -> SessionFact {
    SessionFact {
        schema: FactSchema::v2(),
        session_id: session_id(),
        log_id: log_id(),
        fact_seq,
        event_id: EventId::new(format!("01J00000000000000000000{fact_seq:04}")),
        ts_ms: NOW_MS,
        causation_id: None,
        turn_id: None,
        call_id: None,
        interaction_id: None,
        payload,
    }
}

fn input_accepted(fact_seq: u64) -> SessionFact {
    fact(
        fact_seq,
        FactPayload::InputAccepted(InputAccepted {
            input_id: InputId::new("input_01J00000000000000000000000"),
            input_kind: InputKind::System,
            input_purpose: InputPurpose::TriggerTurn,
            content_ref: None,
            inline_text: Some("review this change".to_string()),
            attachments: vec![],
            actor: ActorRef {
                kind: ActorKind::Subagent,
                id: "/root".to_string(),
                display_name: None,
            },
            client_request_id: Some(message_id().as_str().to_string()),
        }),
    )
}

#[test]
fn queue_delivery_stays_pending_until_matching_input_is_accepted() {
    let mut mailbox = MailboxProjection::default();
    let queued = mailbox
        .apply(&fact(
            1,
            FactPayload::InterAgentCommunication(communication(InterAgentDelivery::Queue)),
        ))
        .expect("queued delta");

    assert!(matches!(queued, MailboxDelta::Queued { .. }));
    assert_eq!(mailbox.pending_count(), 1);
    assert_eq!(mailbox.last_activity_fact_seq(), 1);

    let delivered = mailbox.apply(&input_accepted(2)).expect("delivered delta");
    assert!(matches!(
        delivered,
        MailboxDelta::Delivered {
            delivered_fact_seq: 2,
            ..
        }
    ));
    assert_eq!(mailbox.pending_count(), 0);
    let snapshot = mailbox.snapshot();
    assert_eq!(snapshot.messages[0].state, MailboxMessageState::Delivered);
    assert_eq!(snapshot.messages[0].delivered_fact_seq, Some(2));
    assert_eq!(snapshot.last_activity_fact_seq, 2);
}

#[test]
fn pending_for_filters_by_primary_and_other_recipients() {
    let mut mailbox = MailboxProjection::default();
    let mut payload = communication(InterAgentDelivery::Queue);
    payload.other_recipients = vec![path("/root/review/tests")];
    mailbox.apply(&fact(1, FactPayload::InterAgentCommunication(payload)));

    assert_eq!(
        mailbox.pending_for(&path("/root/review")).count(),
        1,
        "primary recipient"
    );
    assert_eq!(
        mailbox.pending_for(&path("/root/review/tests")).count(),
        1,
        "other recipient"
    );
    assert_eq!(mailbox.pending_for(&path("/root/explore")).count(), 0);
}

#[test]
fn duplicate_communication_is_idempotent() {
    let mut mailbox = MailboxProjection::default();
    let payload = communication(InterAgentDelivery::Trigger);
    assert!(
        mailbox
            .apply(&fact(
                1,
                FactPayload::InterAgentCommunication(payload.clone())
            ))
            .is_some()
    );
    assert!(
        mailbox
            .apply(&fact(2, FactPayload::InterAgentCommunication(payload)))
            .is_none()
    );
    assert_eq!(mailbox.pending_count(), 1);
}

#[test]
fn delivery_policy_is_explicit() {
    assert!(!InterAgentDelivery::Queue.triggers_idle_turn());
    assert!(!InterAgentDelivery::Queue.interrupts_turn());
    assert!(InterAgentDelivery::Trigger.triggers_idle_turn());
    assert!(!InterAgentDelivery::Trigger.interrupts_turn());
    assert!(InterAgentDelivery::Interrupt.triggers_idle_turn());
    assert!(InterAgentDelivery::Interrupt.interrupts_turn());
}

#[test]
fn projection_set_exposes_mailbox_slot_and_snapshot() {
    let mut projections = ProjectionSet::default();
    let deltas = projections.apply(&fact(
        1,
        FactPayload::InterAgentCommunication(communication(InterAgentDelivery::Queue)),
    ));
    let mailbox = deltas
        .iter()
        .find(|delta| delta.slot == ProjectionSlot::Mailbox)
        .expect("mailbox delta");
    assert!(matches!(
        mailbox.payload,
        ProjectionPayload::MailboxDelta(MailboxDelta::Queued { .. })
    ));
    assert_eq!(projections.snapshot().mailbox.messages.len(), 1);
}

#[test]
fn communication_validation_rejects_cross_namespace_and_empty_content() {
    let mut invalid = communication(InterAgentDelivery::Queue);
    invalid.recipient = path("/morpheus/task");
    assert!(
        fact(1, FactPayload::InterAgentCommunication(invalid))
            .validate()
            .is_err()
    );

    let mut invalid = communication(InterAgentDelivery::Queue);
    invalid.content = InterAgentContent::Inline {
        text: String::new(),
    };
    assert!(
        fact(1, FactPayload::InterAgentCommunication(invalid))
            .validate()
            .is_err()
    );
}
