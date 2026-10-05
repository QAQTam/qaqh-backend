//! P3-7 production contract: yielding for an interaction durably records the
//! canonical `InteractionRequested` before the modal is considered pending.

use qaqh_runtime::agent::engine_turn::TurnEngine;
use qaqh_runtime::agent::state::agent::AgentState;
use qaqh_runtime::agent::turn_lap_test_api::{
    observe_yield_for_test, record_interaction_resolution_for_test,
};
use qaqh_session::canonical::{CanonicalSessionIdentity, CommittedFactReader, ulid_from_text};
use qaqh_session::session_fact_v2::{FactPayload, InteractionKind, ToolCallId, TurnId};

#[test]
fn yield_persists_canonical_interaction_request() {
    let data_root = tempfile::tempdir().expect("data root tempdir");
    // SAFETY: this integration-test binary has one test and sets the root
    // before any platform data-dir access.
    unsafe {
        std::env::set_var("QAQH_DATA_DIR", data_root.path());
    }
    qaqh_session::SessionManager::init(qaqh_types::platform::data_dir());

    let session_id = "interaction-request-ledger";
    let wire_turn = "turn-interaction-request";
    let wire_call = "call-interaction-request";
    let mut agent = AgentState::init("interaction-request-test", qaqh_config::Config::default());
    agent.session.session_id = session_id.to_string();
    agent.ephemeral = false;

    let mut engine = TurnEngine::new();
    observe_yield_for_test(
        &mut engine,
        &mut agent,
        wire_turn,
        "input-interaction-request",
        wire_call,
    )
    .expect("persist yield");

    let session_dir = qaqh_types::platform::sessions_dir().join(session_id);
    let identity = CanonicalSessionIdentity::open_or_create(&session_dir).expect("identity");
    let facts = CommittedFactReader::open(
        &session_dir,
        identity.session_id.clone(),
        identity.log_id.clone(),
    )
    .and_then(|reader| reader.read_all())
    .expect("read canonical facts");

    let requests = facts
        .iter()
        .filter_map(|fact| match &fact.payload {
            FactPayload::InteractionRequested(payload) => Some(payload),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(requests.len(), 1);
    let request = requests[0];
    assert_eq!(request.kind, InteractionKind::Permission);
    assert_eq!(
        request.turn_id,
        TurnId::new(format!("turn_{}", ulid_from_text(wire_turn)))
    );
    assert_eq!(
        request.call_id.as_ref(),
        Some(&ToolCallId::new(format!(
            "call_{}",
            ulid_from_text(wire_call)
        )))
    );
    assert_eq!(
        request.interaction_id.as_str(),
        format!("int_{}", ulid_from_text(wire_call))
    );

    let command_id = qaqh_session::canonical::generate_ulid();
    record_interaction_resolution_for_test(&mut agent, wire_call, "approved", Some(&command_id))
        .expect("persist resolution");
    let facts = CommittedFactReader::open(
        &session_dir,
        identity.session_id.clone(),
        identity.log_id.clone(),
    )
    .and_then(|reader| reader.read_all())
    .expect("read canonical facts after resolution");
    let resolutions = facts
        .iter()
        .filter_map(|fact| match &fact.payload {
            FactPayload::InteractionResolved(payload) => Some((fact, payload)),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(resolutions.len(), 1);
    let (resolution_fact, resolution) = resolutions[0];
    assert_eq!(resolution_fact.turn_id.as_ref(), Some(&request.turn_id));
    assert_eq!(resolution_fact.call_id.as_ref(), request.call_id.as_ref());
    assert_eq!(resolution.interaction_id, request.interaction_id);
    assert_eq!(
        resolution.decision,
        Some(qaqh_session::session_fact_v2::InteractionDecision::Approved)
    );
    assert_eq!(
        resolution_fact.causation_id.as_ref().map(|id| id.as_str()),
        Some(command_id.as_str())
    );

    // 2026-10-05 回归（`docs/bug-ringing-v2-commands-stuck-in-running.md`）：
    // 移动端提交的 command_id 是 UUID。writer 侧曾按 `is_ulid` 过滤，把它直接
    // 丢成 `None`，于是这条远程审批的 canonical 终态永远找不回它的回执，
    // 客户端看到的就是"审批永远停在 Running"。UUID 必须经 `ulid_from_text`
    // 归一后照样落进因果通道（磁盘契约保持 ULID）。
    let uuid_command = "92455601-b53a-4f25-8df5-94124676055b".to_string();
    let uuid_wire_call = "call-uuid-command";
    observe_yield_for_test(
        &mut engine,
        &mut agent,
        wire_turn,
        "input-uuid-command",
        uuid_wire_call,
    )
    .expect("persist uuid yield");
    record_interaction_resolution_for_test(
        &mut agent,
        uuid_wire_call,
        "approved",
        Some(&uuid_command),
    )
    .expect("persist uuid resolution");

    let facts = CommittedFactReader::open(
        &session_dir,
        identity.session_id.clone(),
        identity.log_id.clone(),
    )
    .and_then(|reader| reader.read_all())
    .expect("read canonical facts after uuid resolution");
    let uuid_resolutions: Vec<_> = facts
        .iter()
        .filter(|fact| {
            matches!(fact.payload, FactPayload::InteractionResolved(_))
                && fact.causation_id.as_ref().map(|id| id.as_str())
                    == Some(ulid_from_text(&uuid_command).as_str())
        })
        .collect();
    assert_eq!(
        uuid_resolutions.len(),
        1,
        "the UUID command id must reach the canonical causation lane"
    );
    for fact in &facts {
        fact.validate()
            .unwrap_or_else(|error| panic!("canonical fact {} must validate: {error}", fact.fact_seq));
    }
}
