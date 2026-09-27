//! #345 production contract: ask 的 canonical `request_ref` 必须等于
//! 「交互正文 bytes 的 sha256」——也就是 hub 写进 content store 时拿到的
//! content_id。两处各自序列化一份是最容易被忽略的漂移点（没有编译期错误），
//! 这里把它钉死。
//!
//! 单独一个 test binary：`SessionManager::init` 是进程级单例，重复 init 会 panic。

use qaqh_domain::{AskMode, AskQuestion, interaction_body};
use qaqh_runtime::agent::engine_turn::TurnEngine;
use qaqh_runtime::agent::state::agent::AgentState;
use qaqh_runtime::agent::turn_lap_test_api::observe_ask_yield_for_test;
use qaqh_session::canonical::{CanonicalSessionIdentity, CommittedFactReader};
use qaqh_session::session_fact_v2::{FactPayload, InteractionKind};

#[test]
fn ask_request_ref_matches_interaction_body_content_id() {
    let data_root = tempfile::tempdir().expect("data root tempdir");
    // SAFETY: this integration-test binary has one test and sets the root
    // before any platform data-dir access.
    unsafe {
        std::env::set_var("QAQH_DATA_DIR", data_root.path());
    }
    qaqh_session::SessionManager::init(qaqh_types::platform::data_dir());

    let session_id = "ask-body-content-id";
    let wire_turn = "turn-ask-body";
    let wire_call = "call-ask-body";
    let mode = AskMode::Batch;
    let questions = vec![
        AskQuestion {
            id: "q1".into(),
            question: "first?".into(),
            options: vec!["a".into(), "b".into()],
            allow_custom: true,
        },
        AskQuestion {
            id: "q2".into(),
            question: "second?".into(),
            options: Vec::new(),
            allow_custom: false,
        },
    ];

    let mut agent = AgentState::init("ask-body-test", qaqh_config::Config::default());
    agent.session.session_id = session_id.to_string();
    agent.ephemeral = false;

    let mut engine = TurnEngine::new();
    observe_ask_yield_for_test(
        &mut engine,
        &mut agent,
        wire_turn,
        "input-ask-body",
        wire_call,
        mode,
        questions.clone(),
    )
    .expect("persist ask yield");

    let session_dir = qaqh_types::platform::sessions_dir().join(session_id);
    let identity = CanonicalSessionIdentity::open_or_create(&session_dir).expect("identity");
    let facts = CommittedFactReader::open(
        &session_dir,
        identity.session_id.clone(),
        identity.log_id.clone(),
    )
    .and_then(|reader| reader.read_all())
    .expect("read canonical facts");

    let request = facts
        .iter()
        .find_map(|fact| match &fact.payload {
            FactPayload::InteractionRequested(payload) if payload.kind == InteractionKind::Ask => {
                Some(payload)
            }
            _ => None,
        })
        .expect("ask interaction request fact");

    let body = interaction_body::ask_body(mode, &questions);
    let expected_id = qaqh_types::sha256_hex(&body);
    assert_eq!(
        request.request_ref.hash().as_str(),
        format!("sha256:{expected_id}"),
        "canonical request_ref must be the interaction body's content id"
    );

    // 同一 content_id 在 hub 里必须取得到正文（ref 与 bytes 对得上）。
    let hub = qaqh_runtime::RingingHub::new("ask-body-epoch");
    let stored_id = hub
        .put_interaction_content(
            session_id,
            wire_call,
            interaction_body::INTERACTION_BODY_MEDIA_TYPE,
            body,
        )
        .expect("interaction body admitted");
    assert_eq!(stored_id, expected_id);
    let entry = hub
        .get_content_any(&expected_id)
        .expect("ref resolves to stored body");
    assert_eq!(entry.owners, vec![session_id.to_string()]);
    assert!(
        entry.pinned,
        "interaction body must stay pinned while pending"
    );
}
