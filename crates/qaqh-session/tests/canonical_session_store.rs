//! CanonicalSessionStore append/projection facade contract.

use qaqh_session::canonical::{
    CanonicalError, CanonicalSessionStore, CommittedFactReader, WriterId,
};
use qaqh_session::projection::ProjectionSet;
use qaqh_session::session_fact_v2::{
    ActorKind, ActorRef, AssistantBlockKind, AssistantBlockSealed, BlockId, ContentHash,
    ContentRef, EventId, FactPayload, InputAccepted, InputId, InputKind, InputPurpose, LogId,
    SessionFact, SessionId, TurnId,
};

const ENVELOPE_FIXTURE: &str = include_str!("fixtures/session_fact_v2/envelope.jsonl");
const NOW_MS: i64 = 1_789_830_000_000;
const LEASE_MS: i64 = 10_000;

fn session_id() -> SessionId {
    SessionId::new("0198f1a0-0000-7000-8000-000000000001")
}

fn log_id() -> LogId {
    LogId::new("0198f1a0-0000-7000-8000-000000000002")
}

fn fact(payload: FactPayload, event_ordinal: u64) -> SessionFact {
    let mut fact: SessionFact =
        serde_json::from_str(ENVELOPE_FIXTURE.trim()).expect("parse envelope fixture");
    fact.session_id = session_id();
    fact.log_id = log_id();
    fact.fact_seq = 0;
    fact.event_id = EventId::new(format!("01J{event_ordinal:023}"));
    fact.payload = payload;
    fact
}

fn content_ref(session_id: u8) -> ContentRef {
    ContentRef::new(ContentHash::new(format!("sha256:{session_id:064x}")))
}

fn input_fact(event_ordinal: u64) -> SessionFact {
    fact(
        FactPayload::InputAccepted(InputAccepted {
            input_id: InputId::new("input_01J00000000000000000000000"),
            input_kind: InputKind::UserText,
            input_purpose: InputPurpose::TriggerTurn,
            content_ref: None,
            inline_text: Some("hello".into()),
            attachments: vec![],
            actor: ActorRef {
                kind: ActorKind::User,
                id: "local".into(),
                display_name: None,
            },
            client_request_id: None,
        }),
        event_ordinal,
    )
}

fn assistant_fact(event_ordinal: u64) -> SessionFact {
    let turn_id = TurnId::new("turn_01J00000000000000000000000");
    let mut fact = fact(
        FactPayload::AssistantBlockSealed(AssistantBlockSealed {
            turn_id: turn_id.clone(),
            block_id: BlockId::new("block_01J00000000000000000000000"),
            kind: AssistantBlockKind::Answer,
            content_ref: content_ref(1),
            model: "deepseek-v4.1-flash".into(),
            usage: None,
        }),
        event_ordinal,
    );
    fact.turn_id = Some(turn_id);
    fact
}

fn open_store(dir: &std::path::Path) -> CanonicalSessionStore {
    CanonicalSessionStore::open(dir, session_id(), log_id()).expect("open canonical session store")
}

#[test]
fn append_commits_then_updates_projection_and_returns_events() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut store = open_store(temp.path());
    let lease = store
        .acquire_writer(WriterId::new("writer-a"), NOW_MS, LEASE_MS)
        .expect("acquire writer");

    let outcome = store
        .append(&lease, input_fact(1), NOW_MS + 1)
        .expect("append input fact");

    assert_eq!(outcome.fact.fact_seq, 1);
    assert_eq!(outcome.events.len(), 2);
    assert_eq!(
        outcome
            .events
            .iter()
            .map(|event| event.projection_slot.expect("projection slot"))
            .collect::<Vec<_>>(),
        vec![
            qaqh_session::session_fact_v2::ProjectionSlot::Conversation,
            qaqh_session::session_fact_v2::ProjectionSlot::Timeline
        ]
    );
    assert_eq!(store.committed().committed_fact_seq, 1);
    assert_eq!(store.last_fact_seq(), 1);
    assert_eq!(store.snapshot().conversation.revision, 1);
    assert_eq!(store.snapshot().timeline.revision, 1);
}

#[test]
fn reopen_and_rebuild_are_equivalent_to_incremental_projection() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut store = open_store(temp.path());
    let lease = store
        .acquire_writer(WriterId::new("writer-a"), NOW_MS, LEASE_MS)
        .expect("acquire writer");
    store
        .append(&lease, input_fact(1), NOW_MS + 1)
        .expect("append input fact");
    let incremental = store.snapshot();
    drop(store);

    let mut reopened = open_store(temp.path());
    assert_eq!(reopened.snapshot(), incremental);
    let lease = reopened
        .acquire_writer(WriterId::new("writer-b"), NOW_MS + LEASE_MS + 1, LEASE_MS)
        .expect("take over writer after reopen");
    reopened
        .append(&lease, assistant_fact(2), NOW_MS + LEASE_MS + 2)
        .expect("append assistant fact");

    let reader = CommittedFactReader::open(temp.path(), session_id(), log_id())
        .expect("open committed reader");
    let rebuilt =
        ProjectionSet::rebuild(reader.read_all().expect("read committed facts").into_iter());
    assert_eq!(reopened.snapshot(), rebuilt.snapshot());
    assert_eq!(reopened.last_fact_seq(), 2);
}

#[test]
fn failed_append_does_not_advance_commit_or_projection() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut store = open_store(temp.path());
    let lease = store
        .acquire_writer(WriterId::new("writer-a"), NOW_MS, LEASE_MS)
        .expect("acquire writer");
    let mut invalid = input_fact(1);
    invalid.session_id = SessionId::new("0198f1a0-0000-7000-8000-000000000099");

    let error = store
        .append(&lease, invalid, NOW_MS + 1)
        .expect_err("identity mismatch must fail");
    assert!(matches!(
        error,
        CanonicalError::IdentityMismatch {
            field: "session_id"
        }
    ));
    assert_eq!(store.committed().committed_fact_seq, 0);
    assert_eq!(store.last_fact_seq(), 0);
    assert_eq!(store.snapshot(), Default::default());
}

#[test]
fn stale_writer_failure_does_not_update_projection() {
    let temp = tempfile::tempdir().expect("tempdir");
    let mut store = open_store(temp.path());
    let stale = store
        .acquire_writer(WriterId::new("writer-a"), NOW_MS, LEASE_MS)
        .expect("acquire first writer");
    store
        .acquire_writer(WriterId::new("writer-b"), NOW_MS + LEASE_MS + 1, LEASE_MS)
        .expect("take over expired writer");

    let error = store
        .append(&stale, input_fact(1), NOW_MS + LEASE_MS + 2)
        .expect_err("stale writer must fail");
    assert!(matches!(error, CanonicalError::StaleWriter(_)));
    assert_eq!(store.committed().committed_fact_seq, 0);
    assert_eq!(store.last_fact_seq(), 0);
}
