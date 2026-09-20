//! P1 projection cursor, delivery and typed event contract.

use qaqh_domain::RingingChannel;
use qaqh_session::session_fact_v2::{
    AuditRef, ContentHash, ContentUnavailable, ContentUnavailableReason, ContentValue,
    ControlDelta, DeleteReason, Delivery, EventId, MetaDelta, ProjectionEvent, ProjectionPayload,
    ProjectionSlot, ReliableCursor, SessionFact, StreamKey, ValidationError,
};

const ENVELOPE_FIXTURE: &str = include_str!("fixtures/session_fact_v2/envelope.jsonl");

const GOLDEN_CONTROL: &str = r#"{"event_id":"01J00000000000000000000011","source_fact_seq":1,"source_event_id":"01J00000000000000000000001","stream_key":{"kind":"channel","data":"control"},"delivery":{"Reliable":{"cursor":{"log_id":"0198f1a0-0000-7000-8000-000000000002","fact_seq":1,"projection_index":2}}},"projection_slot":"control","projection_index":2,"payload":{"kind":"control_delta","data":{"kind":"session_created","data":{"revision":1,"session_id":"0198f1a0-0000-7000-8000-000000000001","cwd":"/workspace","model":"deepseek-v4.1-flash","schema_caps":["reliable_replay","interaction_replay"]}}}}"#;

const GOLDEN_INPUT: &str = r#"{"event_id":"01J00000000000000000000012","source_fact_seq":2,"source_event_id":"01J00000000000000000000002","stream_key":{"kind":"channel","data":"conversation"},"delivery":{"Reliable":{"cursor":{"log_id":"0198f1a0-0000-7000-8000-000000000002","fact_seq":2,"projection_index":0}}},"projection_slot":"conversation","projection_index":0,"payload":{"kind":"conversation_delta","data":{"kind":"input_accepted","data":{"revision":1,"input_id":"input_01J00000000000000000000000","input_kind":"user_text","input_purpose":"trigger_turn","content":{"kind":"inline","data":{"text":"hello"}},"attachments":[],"actor":{"kind":"user","id":"local"}}}}}"#;

const GOLDEN_TOOL_FINISHED: &str = r#"{"event_id":"01J00000000000000000000013","source_fact_seq":5,"source_event_id":"01J00000000000000000000005","stream_key":{"kind":"channel","data":"tool"},"delivery":{"Reliable":{"cursor":{"log_id":"0198f1a0-0000-7000-8000-000000000002","fact_seq":5,"projection_index":1}}},"projection_slot":"timeline","projection_index":1,"payload":{"kind":"timeline_delta","data":{"kind":"tool_result","data":{"revision":3,"call_id":"call_01J00000000000000000000000","terminal_status":"indeterminate","output":null,"error":{"code":"indeterminate_after_crash","message":"non-idempotent execution not replayed","retryable":false}}}}}"#;

fn cursor(fact_seq: u64, projection_index: u16) -> ReliableCursor {
    ReliableCursor {
        log_id: qaqh_session::session_fact_v2::LogId::new("0198f1a0-0000-7000-8000-000000000002"),
        fact_seq,
        projection_index,
    }
}

fn source_fact() -> SessionFact {
    serde_json::from_str(ENVELOPE_FIXTURE.trim()).expect("parse envelope fixture")
}

#[test]
fn reliable_cursor_json_shape_and_same_log_ordering() {
    let first = cursor(1, 2);
    assert_eq!(
        serde_json::to_value(&first).expect("serialize cursor"),
        serde_json::json!({
            "log_id": "0198f1a0-0000-7000-8000-000000000002",
            "fact_seq": 1,
            "projection_index": 2
        })
    );

    let same_fact_later_slot = cursor(1, 3);
    let next_fact_earlier_slot = cursor(2, 0);
    assert!(
        same_fact_later_slot
            .is_after(&first)
            .expect("same log comparison")
    );
    assert!(
        next_fact_earlier_slot
            .is_after(&same_fact_later_slot)
            .expect("fact sequence dominates projection index")
    );
    assert!(
        !first
            .is_after(&same_fact_later_slot)
            .expect("reverse comparison")
    );

    let mut other_log = first.clone();
    other_log.log_id =
        qaqh_session::session_fact_v2::LogId::new("0198f1a0-0000-7000-8000-000000000003");
    assert!(matches!(
        other_log.is_after(&first),
        Err(ValidationError::InvalidField {
            field: "log_id",
            ..
        })
    ));

    let mut end_of_fact = first.clone();
    end_of_fact.projection_index = qaqh_session::session_fact_v2::END_OF_FACT;
    assert!(matches!(
        end_of_fact.validate(),
        Err(ValidationError::InvalidField {
            field: "projection_index",
            ..
        })
    ));
}

#[test]
fn delivery_json_shapes_are_distinguishable() {
    assert_eq!(
        serde_json::to_value(Delivery::Reliable {
            cursor: cursor(1, 2)
        })
        .expect("serialize reliable"),
        serde_json::json!({
            "Reliable": {
                "cursor": {
                    "log_id": "0198f1a0-0000-7000-8000-000000000002",
                    "fact_seq": 1,
                    "projection_index": 2
                }
            }
        })
    );
    assert_eq!(
        serde_json::to_value(Delivery::Replaceable { revision: 7 }).expect("serialize replaceable"),
        serde_json::json!({"Replaceable": {"revision": 7}})
    );
    assert_eq!(
        serde_json::to_value(Delivery::Ephemeral).expect("serialize ephemeral"),
        serde_json::json!("Ephemeral")
    );
}

#[test]
fn projection_event_constructors_populate_and_validate_source_identity() {
    let fact = source_fact();
    let reliable = ProjectionEvent::reliable(
        EventId::new("01J00000000000000000000021"),
        &fact,
        StreamKey::Channel(RingingChannel::Control),
        ProjectionSlot::Control,
        ProjectionPayload::ControlDelta(ControlDelta::SessionCreated {
            revision: 1,
            session_id: fact.session_id.clone(),
            cwd: "/workspace".into(),
            model: "deepseek-v4.1-flash".into(),
            schema_caps: vec!["reliable_replay".into()],
        }),
    )
    .expect("construct reliable projection event");
    assert_eq!(reliable.source_fact_seq, fact.fact_seq);
    assert_eq!(reliable.source_event_id, fact.event_id);
    assert_eq!(
        reliable.projection_index,
        Some(ProjectionSlot::Control.as_u16())
    );
    assert_eq!(
        reliable
            .delivery
            .reliable_cursor()
            .expect("reliable cursor")
            .log_id,
        fact.log_id
    );
    reliable.validate().expect("reliable event validates");

    let replaceable = ProjectionEvent::replaceable(
        EventId::new("01J00000000000000000000022"),
        &fact,
        StreamKey::Channel(RingingChannel::Control),
        7,
        ProjectionPayload::AuditRef(AuditRef {
            audit_seq: 1,
            audit_hash: ContentHash::new(
                "sha256:5555555555555555555555555555555555555555555555555555555555555555",
            ),
        }),
    )
    .expect("construct replaceable projection event");
    assert!(matches!(
        replaceable.delivery,
        Delivery::Replaceable { revision: 7 }
    ));
    assert_eq!(replaceable.projection_slot, None);
    assert_eq!(replaceable.projection_index, None);

    let ephemeral = ProjectionEvent::ephemeral(
        EventId::new("01J00000000000000000000023"),
        &fact,
        StreamKey::Channel(RingingChannel::Control),
        ProjectionPayload::AuditRef(AuditRef {
            audit_seq: 2,
            audit_hash: ContentHash::new(
                "sha256:6666666666666666666666666666666666666666666666666666666666666666",
            ),
        }),
    )
    .expect("construct ephemeral projection event");
    assert!(matches!(ephemeral.delivery, Delivery::Ephemeral));
    assert_eq!(ephemeral.projection_slot, None);
    assert_eq!(ephemeral.projection_index, None);
}

#[test]
fn projection_event_golden_examples_roundtrip_and_validate() {
    for golden in [GOLDEN_CONTROL, GOLDEN_INPUT, GOLDEN_TOOL_FINISHED] {
        let event: ProjectionEvent =
            serde_json::from_str(golden).expect("deserialize golden projection event");
        event.validate().expect("golden event must validate");
        assert_eq!(
            serde_json::to_value(event).expect("serialize projection event"),
            serde_json::from_str::<serde_json::Value>(golden).expect("parse golden JSON")
        );
    }
}

#[test]
fn projection_event_validation_enforces_slot_and_cursor_consistency() {
    let mut event: ProjectionEvent =
        serde_json::from_str(GOLDEN_CONTROL).expect("deserialize golden event");

    event.projection_slot = None;
    assert!(matches!(
        event.validate(),
        Err(ValidationError::InvalidField {
            field: "projection_slot",
            ..
        })
    ));

    event.projection_slot = Some(ProjectionSlot::Control);
    event.projection_index = Some(3);
    assert!(matches!(
        event.validate(),
        Err(ValidationError::InvalidField {
            field: "projection_index",
            ..
        })
    ));

    event.projection_index = Some(2);
    if let Delivery::Reliable { cursor } = &mut event.delivery {
        cursor.projection_index = 3;
    }
    assert!(matches!(
        event.validate(),
        Err(ValidationError::EnvelopeMismatch {
            field: "delivery.cursor.projection_index"
        })
    ));

    event.delivery = Delivery::Replaceable { revision: 7 };
    assert!(matches!(
        event.validate(),
        Err(ValidationError::InvalidField {
            field: "projection_index",
            ..
        })
    ));

    event.projection_slot = None;
    event.projection_index = None;
    event.source_fact_seq = 0;
    assert!(matches!(
        event.validate(),
        Err(ValidationError::InvalidFactSeq { fact_seq: 0 })
    ));
}

#[test]
fn reliable_payload_family_and_audit_ref_are_closed() {
    let mut event: ProjectionEvent =
        serde_json::from_str(GOLDEN_CONTROL).expect("deserialize golden event");

    event.payload = ProjectionPayload::MetaDelta(MetaDelta::Deleted {
        revision: 1,
        tombstone_at_ms: 1_789_830_000_001,
        reason: DeleteReason::User,
        purge_after_ms: None,
    });
    assert!(matches!(
        event.validate(),
        Err(ValidationError::InvalidField {
            field: "projection_slot",
            ..
        })
    ));

    event.payload = ProjectionPayload::AuditRef(AuditRef {
        audit_seq: 9,
        audit_hash: ContentHash::new(
            "sha256:4444444444444444444444444444444444444444444444444444444444444444",
        ),
    });
    assert!(matches!(
        event.validate(),
        Err(ValidationError::InvalidField {
            field: "payload",
            ..
        })
    ));
}

#[test]
fn content_unavailable_and_resource_stream_key_roundtrip() {
    let content = ContentValue::Unavailable(ContentUnavailable {
        content_ref: qaqh_session::session_fact_v2::ContentRef::new(ContentHash::new(
            "sha256:3333333333333333333333333333333333333333333333333333333333333333",
        )),
        reason: ContentUnavailableReason::GarbageCollected,
        observed_at_logical_ms: 1_789_830_000_000,
        source_fact_seq: Some(41),
        gc_event_seq: Some(7),
    });
    assert_eq!(
        serde_json::to_value(&content).expect("serialize content value"),
        serde_json::json!({
            "kind": "unavailable",
            "data": {
                "content_ref": "sha256:3333333333333333333333333333333333333333333333333333333333333333",
                "reason": "garbage_collected",
                "observed_at_logical_ms": 1_789_830_000_000_u64,
                "source_fact_seq": 41,
                "gc_event_seq": 7
            }
        })
    );

    let stream_key = StreamKey::Resource {
        kind: qaqh_session::session_fact_v2::ResourceKind::Todo,
        id: qaqh_session::session_fact_v2::ResourceId::new("todo-1"),
    };
    assert_eq!(
        serde_json::to_value(&stream_key).expect("serialize resource stream key"),
        serde_json::json!({
            "kind": "resource",
            "data": {
                "kind": "todo",
                "id": "todo-1"
            }
        })
    );

    let payload = ProjectionPayload::MetaDelta(qaqh_session::session_fact_v2::MetaDelta::Deleted {
        revision: 3,
        tombstone_at_ms: 1_789_830_000_001,
        reason: qaqh_session::session_fact_v2::DeleteReason::User,
        purge_after_ms: None,
    });
    let roundtrip: ProjectionPayload =
        serde_json::from_value(serde_json::to_value(payload).expect("serialize payload"))
            .expect("deserialize payload");
    assert!(matches!(
        roundtrip,
        ProjectionPayload::MetaDelta(qaqh_session::session_fact_v2::MetaDelta::Deleted {
            revision: 3,
            purge_after_ms: None,
            ..
        })
    ));
}

#[test]
fn stream_key_channel_is_closed_to_ringing_channels() {
    let key = StreamKey::Channel(RingingChannel::Tool);
    assert_eq!(
        serde_json::to_value(key).expect("serialize channel stream key"),
        serde_json::json!({"kind": "channel", "data": "tool"})
    );
}
