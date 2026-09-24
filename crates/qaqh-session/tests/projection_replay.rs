//! ProjectionEvent publishing and reliable replay contract.

use qaqh_domain::RingingChannel;
use qaqh_session::projection::{
    ProjectionSet, ReplayOutcome, ReplayWindow, projection_event_id, projection_events_for_fact,
    projection_replaceable_events_for_fact, projection_stream_key, replaceable_identity,
    replay_reliable,
};
use qaqh_session::session_fact_v2::{
    AuditRef, ContentHash, Delivery, END_OF_FACT, EventId, FactPayload, LogId, ProjectionEvent,
    ProjectionPayload, ProjectionSlot, ReliableCursor, ResetReason, SessionFact, StreamKey,
    projection_slots,
};

const ENVELOPE_FIXTURE: &str = include_str!("fixtures/session_fact_v2/envelope.jsonl");
const PAYLOAD_FIXTURES: &str = include_str!("fixtures/session_fact_v2/payloads.jsonl");

fn fact_for(payload: FactPayload, ordinal: u64) -> SessionFact {
    let mut fact: SessionFact =
        serde_json::from_str(ENVELOPE_FIXTURE.trim()).expect("parse envelope fixture");
    fact.fact_seq = ordinal;
    fact.event_id = EventId::new(format!("01J{ordinal:023}"));
    fact.payload = payload;
    fact
}

fn payload_facts() -> Vec<SessionFact> {
    PAYLOAD_FIXTURES
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .enumerate()
        .map(|(index, line)| {
            let payload: FactPayload = serde_json::from_str(line).expect("parse payload fixture");
            fact_for(payload, index as u64 + 1)
        })
        .collect()
}

fn input_fact() -> SessionFact {
    payload_facts()
        .into_iter()
        .find(|fact| matches!(&fact.payload, FactPayload::InputAccepted(_)))
        .expect("input_accepted fixture")
}

fn tool_finished_fact() -> SessionFact {
    payload_facts()
        .into_iter()
        .find(|fact| matches!(&fact.payload, FactPayload::ToolFinished(_)))
        .expect("tool_finished fixture")
}

fn cursor(log_id: LogId, fact_seq: u64, projection_index: u16) -> ReliableCursor {
    ReliableCursor {
        log_id,
        fact_seq,
        projection_index,
    }
}

#[test]
fn all_projection_set_deltas_become_valid_stable_events() {
    let mut set = ProjectionSet::default();
    let mut total_events = 0;

    for fact in payload_facts() {
        let deltas = set.apply(&fact);
        let events = projection_events_for_fact(&fact, &deltas).expect("build events");
        let expected_slots = projection_slots(&fact.payload);

        assert_eq!(events.len(), expected_slots.len());
        for (event, expected_slot) in events.iter().zip(expected_slots) {
            event.validate().expect("event validates");
            assert_eq!(event.projection_slot, Some(*expected_slot));
            assert_eq!(event.projection_index, Some(expected_slot.as_u16()));
            assert_eq!(event.source_event_id, fact.event_id);
            assert_eq!(event.source_fact_seq, fact.fact_seq);
            assert_eq!(event.event_id, projection_event_id(&fact, *expected_slot));
            assert_eq!(
                event.stream_key,
                projection_stream_key(&fact, *expected_slot)
            );
            let cursor = event
                .delivery
                .reliable_cursor()
                .expect("reliable event cursor");
            assert_eq!(cursor.log_id, fact.log_id);
            assert_eq!(cursor.fact_seq, fact.fact_seq);
            assert_eq!(cursor.projection_index, expected_slot.as_u16());
            total_events += 1;
        }
    }

    assert_eq!(total_events, 33);
}

#[test]
fn replaceable_events_cover_current_state_only() {
    let fact = payload_facts()
        .into_iter()
        .find(|fact| matches!(&fact.payload, FactPayload::WorkspaceResourceChanged(_)))
        .expect("workspace_resource_changed fixture");
    let mut set = ProjectionSet::default();
    let deltas = set.apply(&fact);
    let events =
        projection_replaceable_events_for_fact(&fact, &deltas).expect("replaceable events");

    assert_eq!(events.len(), 1);
    let event = &events[0];
    assert!(matches!(
        &event.delivery,
        Delivery::Replaceable { revision: 1 }
    ));
    assert_eq!(event.payload.revision(), Some(1));
    assert_eq!(
        replaceable_identity(&event.payload).as_deref(),
        Some("resource:workspace:file:res_01J00000000000000000000000")
    );
    assert_eq!(event.source_fact_seq, fact.fact_seq);

    let input = input_fact();
    let mut input_set = ProjectionSet::default();
    let input_deltas = input_set.apply(&input);
    assert!(
        projection_replaceable_events_for_fact(&input, &input_deltas)
            .expect("input replaceable events")
            .is_empty(),
        "conversation/timeline history stays reliable-only"
    );
}

#[test]
fn stream_keys_follow_the_frozen_fact_slot_table() {
    let tool_finished = tool_finished_fact();
    assert_eq!(
        projection_stream_key(&tool_finished, ProjectionSlot::Conversation),
        StreamKey::Channel(RingingChannel::Conversation)
    );
    assert_eq!(
        projection_stream_key(&tool_finished, ProjectionSlot::Timeline),
        StreamKey::Channel(RingingChannel::Tool)
    );

    let input = input_fact();
    assert_eq!(
        projection_stream_key(&input, ProjectionSlot::Timeline),
        StreamKey::Channel(RingingChannel::Conversation)
    );
}

#[test]
fn reliable_replay_sorts_filters_and_deduplicates_by_full_cursor() {
    let fact = input_fact();
    let mut set = ProjectionSet::default();
    let deltas = set.apply(&fact);
    let mut events = projection_events_for_fact(&fact, &deltas).expect("build events");
    events.reverse();
    events.push(events[1].clone());

    let window = ReplayWindow {
        log_id: fact.log_id.clone(),
        earliest_available_fact_seq: 1,
        snapshot_cursor: None,
    };

    let all = replay_reliable(events.clone(), None, &window).expect("replay all");
    let ReplayOutcome::Delivered { events: all } = all else {
        panic!("expected delivered events");
    };
    assert_eq!(all.len(), 2);
    assert_eq!(
        all.iter()
            .map(|event| event.projection_index.expect("projection index"))
            .collect::<Vec<_>>(),
        vec![0, 1]
    );

    let since = cursor(
        fact.log_id.clone(),
        fact.fact_seq,
        ProjectionSlot::Conversation.as_u16(),
    );
    let tail = replay_reliable(events, Some(&since), &window).expect("replay tail");
    let ReplayOutcome::Delivered { events: tail } = tail else {
        panic!("expected delivered tail");
    };
    assert_eq!(tail.len(), 1);
    assert_eq!(
        tail[0].projection_index,
        Some(ProjectionSlot::Timeline.as_u16())
    );
}

#[test]
fn replay_ignores_replaceable_and_ephemeral_events() {
    let fact = input_fact();
    let mut set = ProjectionSet::default();
    let deltas = set.apply(&fact);
    let reliable = projection_events_for_fact(&fact, &deltas).expect("build events");

    let replaceable = ProjectionEvent::replaceable(
        EventId::new("01J00000000000000000000090"),
        &fact,
        StreamKey::Channel(RingingChannel::Control),
        7,
        ProjectionPayload::AuditRef(AuditRef {
            audit_seq: 1,
            audit_hash: ContentHash::new(
                "sha256:aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa",
            ),
        }),
    )
    .expect("replaceable event");
    let ephemeral = ProjectionEvent::ephemeral(
        EventId::new("01J00000000000000000000091"),
        &fact,
        StreamKey::Channel(RingingChannel::Control),
        ProjectionPayload::AuditRef(AuditRef {
            audit_seq: 2,
            audit_hash: ContentHash::new(
                "sha256:bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb",
            ),
        }),
    )
    .expect("ephemeral event");

    let window = ReplayWindow {
        log_id: fact.log_id.clone(),
        earliest_available_fact_seq: 1,
        snapshot_cursor: None,
    };
    let outcome = replay_reliable(
        [replaceable, ephemeral].into_iter().chain(reliable),
        None,
        &window,
    )
    .expect("replay");
    let ReplayOutcome::Delivered { events } = outcome else {
        panic!("expected delivered events");
    };
    assert_eq!(events.len(), 2);
    assert!(
        events
            .iter()
            .all(|event| matches!(&event.delivery, Delivery::Reliable { .. }))
    );
}

#[test]
fn replay_resets_on_log_mismatch() {
    let fact = input_fact();
    let window = ReplayWindow {
        log_id: fact.log_id.clone(),
        earliest_available_fact_seq: 1,
        snapshot_cursor: None,
    };
    let since = cursor(
        LogId::new("0198f1a0-0000-7000-8000-000000000099"),
        fact.fact_seq,
        ProjectionSlot::Conversation.as_u16(),
    );

    let outcome = replay_reliable(Vec::new(), Some(&since), &window).expect("replay");
    let ReplayOutcome::ResetRequired(reset) = outcome else {
        panic!("expected reset");
    };
    assert_eq!(reset.log_id, fact.log_id);
    assert_eq!(reset.reason, ResetReason::LogIdMismatch);
    assert_eq!(reset.snapshot_cursor, None);
}

#[test]
fn replay_resets_for_expired_cursor_with_snapshot_or_missing_snapshot() {
    let fact = input_fact();
    let snapshot_cursor = cursor(fact.log_id.clone(), 1, END_OF_FACT);
    let window = ReplayWindow {
        log_id: fact.log_id.clone(),
        earliest_available_fact_seq: 2,
        snapshot_cursor: Some(snapshot_cursor.clone()),
    };
    let since = cursor(
        fact.log_id.clone(),
        1,
        ProjectionSlot::Conversation.as_u16(),
    );

    let outcome = replay_reliable(Vec::new(), Some(&since), &window).expect("replay");
    let ReplayOutcome::ResetRequired(reset) = outcome else {
        panic!("expected reset");
    };
    assert_eq!(reset.reason, ResetReason::CursorExpired);
    assert_eq!(reset.snapshot_cursor, Some(snapshot_cursor));

    let window = ReplayWindow {
        snapshot_cursor: None,
        ..window
    };
    let outcome = replay_reliable(Vec::new(), Some(&since), &window).expect("replay");
    let ReplayOutcome::ResetRequired(reset) = outcome else {
        panic!("expected reset");
    };
    assert_eq!(reset.reason, ResetReason::SnapshotMissing);
    assert_eq!(reset.snapshot_cursor, None);
}
