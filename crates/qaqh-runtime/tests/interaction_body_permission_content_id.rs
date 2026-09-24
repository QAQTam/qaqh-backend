//! #345/#352 production contract: permission 的 canonical `request_ref` 必须等于
//! 授权详情正文 bytes 的 sha256 —— 也就是 hub 写进 content store 时拿到的
//! content_id。
//!
//! 纯 v2 的 wire 上没有 tool 频道快照，壳层（TUI / webui gateway）只能从 canonical
//! ref 取正文渲染授权面板；engine 与 hub 各自序列化一份就会漂移，而漂移不会有编译期
//! 错误，只会让客户端取不到详情。这里把它钉死。
//!
//! 单独一个 test binary：`SessionManager::init` 是进程级单例，重复 init 会 panic。

use qaqh_domain::{PermissionCategory, PermissionRisk, interaction_body};
use qaqh_runtime::agent::engine_turn::TurnEngine;
use qaqh_runtime::agent::state::agent::AgentState;
use qaqh_runtime::agent::turn_lap_test_api::observe_permission_yield_for_test;
use qaqh_session::canonical::{CanonicalSessionIdentity, CommittedFactReader};
use qaqh_session::session_fact_v2::{FactPayload, InteractionKind};

#[test]
fn permission_request_ref_matches_interaction_body_content_id() {
    let data_root = tempfile::tempdir().expect("data root tempdir");
    // SAFETY: this integration-test binary has one test and sets the root
    // before any platform data-dir access.
    unsafe {
        std::env::set_var("QAQH_DATA_DIR", data_root.path());
    }
    qaqh_session::SessionManager::init(qaqh_types::platform::data_dir());

    let seed = "permission-body-content-id";
    let wire_turn = "turn-perm-body";
    let wire_call = "call-perm-body";
    let paths = vec!["/tmp/x".to_string()];
    let body = interaction_body::permission_body(
        "exec",
        Some("run ls"),
        "needs shell",
        &paths,
        PermissionCategory::Exec,
        3,
        PermissionRisk::High,
        "runs a command",
    );

    let mut agent = AgentState::init("perm-body-test", qaqh_config::Config::default());
    agent.session.seed = seed.to_string();
    agent.ephemeral = false;

    let mut engine = TurnEngine::new();
    observe_permission_yield_for_test(
        &mut engine,
        &mut agent,
        wire_turn,
        "input-perm-body",
        wire_call,
        body.clone(),
    )
    .expect("persist permission yield");

    let session_dir = qaqh_types::platform::sessions_dir().join(seed);
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
            FactPayload::InteractionRequested(payload)
                if payload.kind == InteractionKind::Permission =>
            {
                Some(payload)
            }
            _ => None,
        })
        .expect("permission interaction request fact");

    let expected_id = qaqh_types::sha256_hex(&body);
    assert_eq!(
        request.request_ref.hash().as_str(),
        format!("sha256:{expected_id}"),
        "canonical request_ref must be the permission body's content id"
    );

    // registry::stash_interaction_body 走的就是这条写入路径（unpinned + TTL）：
    // 同一份 bytes 必须得到同一个 content_id，ref 才解析得到正文。
    let hub = qaqh_runtime::RingingHub::new("perm-body-epoch");
    let stored_id = hub.put_content(
        seed,
        interaction_body::INTERACTION_BODY_MEDIA_TYPE,
        body.clone(),
        false,
    );
    assert_eq!(stored_id, expected_id);
    let entry = hub
        .get_content_any(&expected_id)
        .expect("ref resolves to stored body");
    assert_eq!(entry.seed, seed);
    assert_eq!(entry.bytes, body);
    assert!(
        !entry.pinned,
        "permission body is TTL-backed, not pinned (no domain event to unpin on)"
    );
}
