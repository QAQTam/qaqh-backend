//! SessionMetaProjection apply/rebuild contract.

use qaqh_session::projection::{Projection, SessionMetaProjection};
use qaqh_session::session_fact_v2::{
    CompactionApplied, ContentHash, ContentRef, DeleteReason, EventId, FactPayload, MetaDelta,
    MetadataSource, ModelRoundStarted, RecoveryId, RecoveryOutcome, SearchVisibility,
    SessionDeleted, SessionFact, SessionMetadataChanged, SessionMetadataPatch, SessionRecovered,
    SessionTitleChanged, TitleSource, TurnId,
};

const ENVELOPE_FIXTURE: &str = include_str!("fixtures/session_fact_v2/envelope.jsonl");

fn fact(ordinal: u64) -> SessionFact {
    let mut fact: SessionFact =
        serde_json::from_str(ENVELOPE_FIXTURE.trim()).expect("parse envelope fixture");
    fact.fact_seq = ordinal;
    fact.event_id = EventId::new(format!("01J{ordinal:023}"));
    fact
}

fn with_payload(mut fact: SessionFact, payload: FactPayload) -> SessionFact {
    fact.payload = payload;
    fact
}

fn created(ordinal: u64) -> SessionFact {
    fact(ordinal)
}

fn metadata_changed(ordinal: u64) -> SessionFact {
    with_payload(
        fact(ordinal),
        FactPayload::SessionMetadataChanged(SessionMetadataChanged {
            patch: SessionMetadataPatch {
                cwd: Some("/new-workspace".into()),
                model: Some("new-model".into()),
                archived: Some(true),
                search_visibility: Some(SearchVisibility::Hidden),
                parent_session_id: None,
                schema_caps: Some(vec!["new_cap".into()]),
            },
            source: MetadataSource::User,
            changed_at_ms: 1_789_830_000_002,
        }),
    )
}

fn title_changed(ordinal: u64) -> SessionFact {
    with_payload(
        fact(ordinal),
        FactPayload::SessionTitleChanged(SessionTitleChanged {
            title: "New title".into(),
            source: TitleSource::User,
            changed_at_ms: 1_789_830_000_003,
        }),
    )
}

fn unrelated_round_started(ordinal: u64) -> SessionFact {
    let turn_id = TurnId::new("turn_01J00000000000000000000000");
    let mut fact = with_payload(
        fact(ordinal),
        FactPayload::ModelRoundStarted(ModelRoundStarted {
            turn_id: turn_id.clone(),
            round: 1,
            request_hash: ContentHash::new(
                "sha256:7777777777777777777777777777777777777777777777777777777777777777",
            ),
            context_revision: 0,
        }),
    );
    fact.turn_id = Some(turn_id);
    fact
}

fn compaction_applied(ordinal: u64) -> SessionFact {
    with_payload(
        fact(ordinal),
        FactPayload::CompactionApplied(CompactionApplied {
            checkpoint_id: qaqh_session::session_fact_v2::CheckpointId::new(
                "checkpoint_01J00000000000000000000000",
            ),
            replaces_through_fact_seq: ordinal - 1,
            summary_ref: ContentRef::new(ContentHash::new(
                "sha256:8888888888888888888888888888888888888888888888888888888888888888",
            )),
            context_revision: 9,
            applied_at_ms: 1_789_830_000_005,
        }),
    )
}

fn recovered(ordinal: u64) -> SessionFact {
    with_payload(
        fact(ordinal),
        FactPayload::SessionRecovered(SessionRecovered {
            recovery_id: RecoveryId::new("recovery_01J00000000000000000000000"),
            recovery_event_id: EventId::new("01J00000000000000000000006"),
            recovery_input_fingerprint: ContentHash::new(
                "sha256:9999999999999999999999999999999999999999999999999999999999999999",
            ),
            outcome: RecoveryOutcome::Writable,
            last_good_fact_seq: ordinal - 1,
            torn_tail: false,
            torn_bytes: None,
            actions: Vec::new(),
            recovered_at_ms: 1_789_830_000_006,
        }),
    )
}

fn deleted(ordinal: u64) -> SessionFact {
    with_payload(
        fact(ordinal),
        FactPayload::SessionDeleted(SessionDeleted {
            tombstone_at_ms: 1_789_830_000_007,
            reason: DeleteReason::User,
            purge_after_ms: Some(1_789_830_000_100),
        }),
    )
}

#[test]
fn session_meta_projection_applies_and_rebuilds_equivalently() {
    let facts = vec![
        created(1),
        metadata_changed(2),
        title_changed(3),
        unrelated_round_started(4),
        compaction_applied(5),
        recovered(6),
        deleted(7),
    ];

    let mut projection = SessionMetaProjection::default();
    let deltas: Vec<MetaDelta> = facts
        .iter()
        .filter_map(|fact| projection.apply(fact))
        .collect();

    assert_eq!(deltas.len(), 6);
    assert!(matches!(&deltas[0], MetaDelta::Created { revision: 1, .. }));
    assert!(matches!(
        &deltas[1],
        MetaDelta::MetadataChanged { revision: 2, .. }
    ));
    assert!(matches!(
        &deltas[2],
        MetaDelta::TitleChanged {
            revision: 3,
            source: TitleSource::User,
            ..
        }
    ));
    assert!(matches!(
        &deltas[3],
        MetaDelta::ContextRevision {
            revision: 4,
            context_revision: 9,
            ..
        }
    ));
    assert!(matches!(
        &deltas[4],
        MetaDelta::Recovered {
            revision: 5,
            outcome: RecoveryOutcome::Writable,
            ..
        }
    ));
    assert!(matches!(
        &deltas[5],
        MetaDelta::Deleted {
            revision: 6,
            reason: DeleteReason::User,
            purge_after_ms: Some(1_789_830_000_100),
            ..
        }
    ));

    let snapshot = projection.snapshot();
    assert_eq!(
        snapshot.session_id.as_ref().map(|id| id.as_str()),
        Some("0198f1a0-0000-7000-8000-000000000001")
    );
    assert_eq!(snapshot.cwd.as_deref(), Some("/new-workspace"));
    assert_eq!(snapshot.model.as_deref(), Some("new-model"));
    assert!(snapshot.archived);
    assert_eq!(snapshot.search_visibility, Some(SearchVisibility::Hidden));
    assert_eq!(snapshot.title.as_deref(), Some("New title"));
    assert_eq!(snapshot.context_revision, Some(9));
    assert_eq!(snapshot.last_recovery, Some(RecoveryOutcome::Writable));
    assert!(snapshot.deleted.is_some());
    assert_eq!(snapshot.revision, 6);
    assert_eq!(snapshot.last_fact_seq, 7);

    let rebuilt = SessionMetaProjection::rebuild(facts.into_iter());
    assert_eq!(snapshot, rebuilt.snapshot());
    assert_eq!(rebuilt.last_fact_seq(), 7);
}

#[test]
fn unrelated_fact_advances_progress_without_a_delta() {
    let mut projection = SessionMetaProjection::default();
    assert!(projection.apply(&unrelated_round_started(4)).is_none());
    assert_eq!(projection.last_fact_seq(), 4);
    assert_eq!(projection.snapshot().revision, 0);
}

#[test]
fn snapshot_roundtrips_with_tombstone_and_context_state() {
    let facts = vec![created(1), compaction_applied(2), recovered(3), deleted(4)];
    let rebuilt = SessionMetaProjection::rebuild(facts.into_iter());
    let snapshot = rebuilt.snapshot();
    let encoded = serde_json::to_vec(&snapshot).expect("serialize snapshot");
    let decoded: qaqh_session::projection::SessionMetaSnapshot =
        serde_json::from_slice(&encoded).expect("deserialize snapshot");
    assert_eq!(snapshot, decoded);
}
