//! Static projection-slot registry contract for all canonical fact kinds.

use qaqh_session::session_fact_v2::{
    END_OF_FACT, FactPayload, MAX_RELIABLE_PROJECTION_INDEX, ProjectionIndex, ProjectionSlot,
    projection_slots,
};
use serde_json::Value;

const PAYLOAD_FIXTURES: &str = include_str!("fixtures/session_fact_v2/payloads.jsonl");

fn payload_lines() -> Vec<&'static str> {
    PAYLOAD_FIXTURES
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect()
}

fn expected_slots(kind: &str) -> &'static [ProjectionSlot] {
    use ProjectionSlot::{Control, Conversation, Mailbox, Meta, Resources, Timeline};
    match kind {
        "session_created" => &[Control, Meta],
        "input_accepted" => &[Conversation, Timeline, Mailbox],
        "turn_started" => &[Conversation, Control],
        "model_round_started" => &[Control],
        "assistant_block_sealed" => &[Conversation, Timeline],
        "tool_call_declared" => &[Conversation, Timeline],
        "tool_intent" => &[Control],
        "tool_finished" => &[Conversation, Timeline],
        "interaction_requested" => &[Control],
        "interaction_resolved" => &[Control],
        "interaction_expired" => &[Control],
        "turn_finished" => &[Conversation, Control],
        "turn_interrupted" => &[Conversation, Control],
        "session_recovered" => &[Control, Meta],
        "compaction_applied" => &[Conversation, Meta],
        "session_metadata_changed" => &[Meta],
        "session_title_changed" => &[Meta],
        "session_deleted" => &[Meta],
        "workspace_resource_changed" => &[Resources],
        "subagent_spawned" => &[Control, Resources],
        "subagent_finished" => &[Control, Resources],
        "inter_agent_communication" => &[Mailbox],
        other => panic!("unexpected fixture kind: {other}"),
    }
}

#[test]
fn all_payload_fixtures_have_the_frozen_slot_mapping() {
    let lines = payload_lines();
    assert_eq!(lines.len(), 22, "fixture must cover all fact kinds");

    for line in lines {
        let value: Value = serde_json::from_str(line).expect("parse fixture json");
        let kind = value["kind"].as_str().expect("fixture kind");
        let payload: FactPayload = serde_json::from_str(line).expect("parse payload");
        let slots = projection_slots(&payload);
        assert_eq!(slots, expected_slots(kind), "slot mismatch for {kind}");

        let mut previous = None;
        for slot in slots {
            let index = slot.as_u16();
            assert!(
                index <= MAX_RELIABLE_PROJECTION_INDEX,
                "slot {slot:?} exceeds reliable projection range"
            );
            if let Some(previous) = previous {
                assert!(previous < index, "slots must be strictly ascending");
            }
            previous = Some(index);
        }
    }
}

#[test]
fn projection_slot_indices_are_the_frozen_repr_values() {
    assert_eq!(ProjectionSlot::Conversation.as_u16(), 0);
    assert_eq!(ProjectionSlot::Timeline.as_u16(), 1);
    assert_eq!(ProjectionSlot::Control.as_u16(), 2);
    assert_eq!(ProjectionSlot::Resources.as_u16(), 3);
    assert_eq!(ProjectionSlot::Meta.as_u16(), 4);
    assert_eq!(ProjectionSlot::Mailbox.as_u16(), 5);
    assert_eq!(MAX_RELIABLE_PROJECTION_INDEX, 65_534);
    assert_eq!(END_OF_FACT, 65_535);
    let _: ProjectionIndex = ProjectionSlot::Mailbox.as_u16();
}

#[test]
fn unknown_fact_kind_remains_fail_closed() {
    let unknown = serde_json::from_str::<FactPayload>(r#"{"kind":"future_kind","data":{}}"#);
    assert!(unknown.is_err());
}
