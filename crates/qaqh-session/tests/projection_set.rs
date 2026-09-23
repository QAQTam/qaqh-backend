//! ProjectionSet aggregate apply/rebuild contract.

use qaqh_session::projection::{ProjectionSet, ProjectionSetSnapshot};
use qaqh_session::session_fact_v2::{
    EventId, FactPayload, ProjectionSlot, SessionFact, ToolTerminalStatus, projection_slots,
};

const ENVELOPE_FIXTURE: &str = include_str!("fixtures/session_fact_v2/envelope.jsonl");
const PAYLOAD_FIXTURES: &str = include_str!("fixtures/session_fact_v2/payloads.jsonl");

fn facts() -> Vec<SessionFact> {
    PAYLOAD_FIXTURES
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .enumerate()
        .map(|(index, line)| {
            let ordinal = index as u64 + 1;
            let payload: FactPayload = serde_json::from_str(line).expect("parse payload fixture");
            let mut fact: SessionFact =
                serde_json::from_str(ENVELOPE_FIXTURE.trim()).expect("parse envelope fixture");
            fact.fact_seq = ordinal;
            fact.event_id = EventId::new(format!("01J{ordinal:023}"));
            fact.payload = payload;
            fact
        })
        .collect()
}

#[test]
fn projection_set_publishes_static_slots_for_all_fixtures() {
    let facts = facts();
    assert_eq!(facts.len(), 21);

    let mut set = ProjectionSet::default();
    let mut published = [0_usize; 5];

    for fact in &facts {
        let expected = projection_slots(&fact.payload);
        let deltas = set.apply(fact);
        let actual: Vec<ProjectionSlot> = deltas.iter().map(|delta| delta.slot).collect();

        assert_eq!(
            actual.as_slice(),
            expected,
            "slot mismatch for {:?}",
            fact.payload
        );
        for delta in &deltas {
            assert_eq!(delta.payload_slot(), Some(delta.slot));
            published[delta.slot.as_u16() as usize] += 1;
        }
    }

    assert_eq!(
        published,
        [8, 4, 12, 3, 6],
        "turn/compaction facts update timeline state but do not publish a timeline slot"
    );
    assert_eq!(set.last_fact_seq(), 21);

    let snapshot = set.snapshot();
    assert_eq!(snapshot.conversation.revision, 8);
    assert_eq!(snapshot.timeline.revision, 7);
    assert_eq!(
        snapshot.control.revision, 13,
        "tool_finished must update internal control state even without a control reliable delta"
    );
    assert_eq!(snapshot.resources.revision, 3);
    assert_eq!(snapshot.meta.revision, 6);

    let rebuilt = ProjectionSet::rebuild(facts.into_iter());
    assert_eq!(snapshot, rebuilt.snapshot());
}

#[test]
fn tool_finished_updates_control_state_but_is_filtered_from_reliable_output() {
    let fact = facts()
        .into_iter()
        .find(|fact| matches!(&fact.payload, FactPayload::ToolFinished(_)))
        .expect("tool_finished fixture");

    let mut set = ProjectionSet::default();
    let deltas = set.apply(&fact);

    assert!(
        deltas
            .iter()
            .all(|delta| delta.slot != ProjectionSlot::Control)
    );
    assert!(deltas.iter().any(|delta| {
        delta.slot == ProjectionSlot::Conversation
            && matches!(
                &delta.payload,
                qaqh_session::session_fact_v2::ProjectionPayload::ConversationDelta(_)
            )
    }));
    assert!(deltas.iter().any(|delta| {
        delta.slot == ProjectionSlot::Timeline
            && matches!(
                &delta.payload,
                qaqh_session::session_fact_v2::ProjectionPayload::TimelineDelta(_)
            )
    }));

    let snapshot = set.snapshot();
    assert_eq!(snapshot.control.revision, 1);
    assert_eq!(snapshot.control.tools.len(), 1);
    assert_eq!(
        snapshot.control.tools[0].terminal_status,
        Some(ToolTerminalStatus::Succeeded)
    );
}

#[test]
fn projection_set_snapshot_roundtrips() {
    let set = ProjectionSet::rebuild(facts().into_iter());
    let snapshot = set.snapshot();
    let encoded = serde_json::to_vec(&snapshot).expect("serialize projection-set snapshot");
    let decoded: ProjectionSetSnapshot =
        serde_json::from_slice(&encoded).expect("deserialize projection-set snapshot");
    assert_eq!(snapshot, decoded);
}
