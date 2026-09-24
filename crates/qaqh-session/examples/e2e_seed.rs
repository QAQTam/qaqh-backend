//! Dev seeder for real-machine smoke tests (see `scripts/v2-smoke.sh`).
//!
//! Usage: `e2e_seed <session_dir> [--resolved]`
//!
//! Creates a canonical session with `SessionCreated`, optionally followed by
//! an ask interaction that is already resolved (for the first-answer-wins
//! real-machine probe).

use qaqh_session::canonical::{
    CanonicalSessionIdentity, CanonicalSessionStore, WriterId, generate_ulid, sha256_content_hash,
};
use qaqh_session::session_fact_v2::{
    ActorKind, ActorRef, ContentRef, EventId, FactPayload, FactSchema, InteractionDecision,
    InteractionId, InteractionKind, InteractionRequested, InteractionResolved, SessionCreated,
    SessionFact, ToolCallId, TurnId,
};

fn main() {
    let mut args = std::env::args().skip(1);
    let dir = args.next().expect("session dir");
    let resolved = args.next().as_deref() == Some("--resolved");
    let dir = std::path::PathBuf::from(dir);
    std::fs::create_dir_all(&dir).expect("create session dir");

    let identity = CanonicalSessionIdentity::open_or_create(&dir).expect("identity");
    let now = 1_789_830_000_000_i64;
    let mut store =
        CanonicalSessionStore::open(&dir, identity.session_id.clone(), identity.log_id.clone())
            .expect("store");
    let lease = store
        .acquire_writer(WriterId::new("e2e-seed"), now, 600_000)
        .expect("writer lease");

    let envelope = |ts_ms: i64,
                    turn_id: Option<TurnId>,
                    call_id: Option<ToolCallId>,
                    interaction_id: Option<InteractionId>,
                    payload: FactPayload| SessionFact {
        schema: FactSchema::v2(),
        session_id: identity.session_id.clone(),
        log_id: identity.log_id.clone(),
        fact_seq: 0,
        event_id: EventId::new(generate_ulid()),
        ts_ms,
        causation_id: None,
        turn_id,
        call_id,
        interaction_id,
        payload,
    };

    store
        .append(
            &lease,
            envelope(
                now,
                None,
                None,
                None,
                FactPayload::SessionCreated(SessionCreated {
                    created_at_ms: now,
                    cwd: "/tmp".into(),
                    model: "e2e".into(),
                    parent_session_id: None,
                    schema_caps: Vec::new(),
                }),
            ),
            now,
        )
        .expect("append created");

    if resolved {
        let turn_id = TurnId::new(format!("turn_{}", generate_ulid()));
        let call_id = ToolCallId::new(format!("call_{}", generate_ulid()));
        let interaction_id = InteractionId::new(format!("int_{}", generate_ulid()));
        store
            .append(
                &lease,
                envelope(
                    now + 1,
                    Some(turn_id.clone()),
                    Some(call_id.clone()),
                    Some(interaction_id.clone()),
                    FactPayload::InteractionRequested(InteractionRequested {
                        interaction_id: interaction_id.clone(),
                        call_id: Some(call_id.clone()),
                        turn_id: turn_id.clone(),
                        kind: InteractionKind::Ask,
                        request_ref: ContentRef::new(sha256_content_hash(b"e2e-request")),
                        expires_at_ms: None,
                        requested_at_ms: now + 1,
                    }),
                ),
                now + 1,
            )
            .expect("append requested");
        store
            .append(
                &lease,
                envelope(
                    now + 2,
                    Some(turn_id),
                    Some(call_id),
                    Some(interaction_id.clone()),
                    FactPayload::InteractionResolved(InteractionResolved {
                        interaction_id: interaction_id.clone(),
                        decision_ref: ContentRef::new(sha256_content_hash(b"answered")),
                        decision: Some(InteractionDecision::Answered),
                        resolved_by: ActorRef {
                            kind: ActorKind::User,
                            id: "e2e-user".into(),
                            display_name: None,
                        },
                        resolution_seq: 1,
                        resolved_at_ms: now + 2,
                    }),
                ),
                now + 2,
            )
            .expect("append resolved");
        println!("{interaction_id}");
    }
}
