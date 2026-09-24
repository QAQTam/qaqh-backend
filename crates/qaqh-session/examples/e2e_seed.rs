//! Dev seeder for real-machine smoke tests (see `scripts/v2-smoke.sh`).
//!
//! Usage: `e2e_seed <session_dir> [--resolved]`
//!
//! Creates a canonical session with `SessionCreated`, optionally followed by
//! one resolved interaction per kind (ask / permission / plan) for the
//! first-answer-wins real-machine probes. Prints a JSON object keyed by kind,
//! each carrying `interaction_id` and `call_id`.

use std::time::{SystemTime, UNIX_EPOCH};

use qaqh_session::canonical::{
    CanonicalSessionIdentity, CanonicalSessionStore, CommittedFactReader, WriterId, generate_ulid,
    sha256_content_hash,
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
    let now = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64;
    let mut store =
        CanonicalSessionStore::open(&dir, identity.session_id.clone(), identity.log_id.clone())
            .expect("store");
    let has_created =
        CommittedFactReader::open(&dir, identity.session_id.clone(), identity.log_id.clone())
            .expect("reader")
            .read_all()
            .expect("facts")
            .iter()
            .any(|fact| matches!(&fact.payload, FactPayload::SessionCreated(_)));
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

    if !has_created {
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
    }

    if resolved {
        let mut seeded = serde_json::Map::new();
        let kinds = [
            ("ask", InteractionKind::Ask, InteractionDecision::Answered),
            (
                "permission",
                InteractionKind::Permission,
                InteractionDecision::Approved,
            ),
            ("plan", InteractionKind::Plan, InteractionDecision::Rejected),
        ];
        for (ts, (key, kind, decision)) in kinds.into_iter().enumerate() {
            let base = now + 1 + (ts as i64) * 2;
            let turn_id = TurnId::new(format!("turn_{}", generate_ulid()));
            let call_id = ToolCallId::new(format!("call_{}", generate_ulid()));
            let interaction_id = InteractionId::new(format!("int_{}", generate_ulid()));
            store
                .append(
                    &lease,
                    envelope(
                        base,
                        Some(turn_id.clone()),
                        Some(call_id.clone()),
                        Some(interaction_id.clone()),
                        FactPayload::InteractionRequested(InteractionRequested {
                            interaction_id: interaction_id.clone(),
                            call_id: Some(call_id.clone()),
                            turn_id: turn_id.clone(),
                            kind,
                            request_ref: ContentRef::new(sha256_content_hash(b"e2e-request")),
                            expires_at_ms: None,
                            requested_at_ms: base,
                        }),
                    ),
                    base,
                )
                .expect("append requested");
            store
                .append(
                    &lease,
                    envelope(
                        base + 1,
                        Some(turn_id),
                        Some(call_id.clone()),
                        Some(interaction_id.clone()),
                        FactPayload::InteractionResolved(InteractionResolved {
                            interaction_id: interaction_id.clone(),
                            decision_ref: ContentRef::new(sha256_content_hash(b"decision")),
                            decision: Some(decision),
                            resolved_by: ActorRef {
                                kind: ActorKind::User,
                                id: "e2e-user".into(),
                                display_name: None,
                            },
                            resolution_seq: 1,
                            resolved_at_ms: base + 1,
                        }),
                    ),
                    base + 1,
                )
                .expect("append resolved");
            seeded.insert(
                key.to_string(),
                serde_json::json!({
                    "interaction_id": interaction_id.as_str(),
                    "call_id": call_id.as_str(),
                }),
            );
        }
        println!("{}", serde_json::Value::Object(seeded));
    }
    store.release_writer(&lease, now).expect("release writer");
}
