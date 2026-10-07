//! 2026-10-07 幽灵审批回归。
//!
//! 症状：桌面壳弹出 ask 后点击"继续"等语义无法推进，审批卡反复拉起。
//! 根因：`record_interaction_resolution` 对入站 canonical id（`int_<ULID>`，
//! v2 投影透传）再做一次 sha256 派生，resolution fact 落在错位 id 上，
//! control 投影永远折叠不了 resolution；abort/丢弃悬空回合也只清内存态，
//! `InteractionRequested` 永远悬在 fact 链上。
//!
//! 本文件锁死三条契约：
//! 1. canonical 入站 id 的 ask 应答必须把 resolution 落在请求 fact 的 id 上，
//!    投影折叠后交互不再 pending（壳层卡片随之消失）；
//! 2. 中止悬空回合必须为挂起交互落 `InteractionExpired(TurnCancelled)`，
//!    投影折叠后不再投影成待审批；
//! 3. 对排队中的后续 ask 错位应答只拒绝、不过期（轮到它时仍可应答）。

use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, Once, OnceLock};

use qaqh_domain::{AskAnswer, AskMode, AskQuestion};
use qaqh_runtime::agent::engine_tool::ToolEngine;
use qaqh_runtime::agent::engine_turn::TurnEngine;
use qaqh_runtime::agent::state::agent::AgentState;
use qaqh_runtime::agent::types::{
    CancelToken, Emitter, LoopPhase, PendingState, RingContext, StatsCollector,
};
use qaqh_runtime::agent::turn_lap_test_api::{
    observe_ask_yield_for_test, observe_multi_ask_yield_for_test,
};
use qaqh_session::canonical::{CanonicalSessionIdentity, CommittedFactReader, ulid_from_text};
use qaqh_session::projection::ControlProjection;
use qaqh_session::projection::Projection as _;
use qaqh_session::session_fact_v2::{
    FactPayload, InteractionExpiryReason, InteractionId, InteractionKind,
};

static TEST_LOCK: Mutex<()> = Mutex::new(());
static DATA_ROOT: OnceLock<tempfile::TempDir> = OnceLock::new();
static INIT: Once = Once::new();

/// 会话级全局（SessionManager/WorkspaceStore）每进程只允许初始化一次：
/// 用例间共享同一 data root，靠 TEST_LOCK 串行 + 唯一 session_id 隔离。
fn init_process() {
    DATA_ROOT.get_or_init(|| {
        let dir = tempfile::tempdir().expect("data root tempdir");
        // SAFETY: this test binary is serialized by TEST_LOCK and this runs
        // before any platform::data_dir() read.
        unsafe {
            std::env::set_var("QAQH_DATA_DIR", dir.path());
        }
        dir
    });
    INIT.call_once(|| {
        qaqh_session::SessionManager::init(qaqh_types::platform::data_dir());
    });
}

fn canonical_interaction_id(wire: &str) -> String {
    format!("int_{}", ulid_from_text(wire))
}

fn read_facts(session_id: &str) -> Vec<qaqh_session::session_fact_v2::SessionFact> {
    let session_dir = qaqh_types::platform::sessions_dir().join(session_id);
    let identity = CanonicalSessionIdentity::open_or_create(&session_dir).expect("identity");
    CommittedFactReader::open(
        &session_dir,
        identity.session_id.clone(),
        identity.log_id.clone(),
    )
    .and_then(|reader| reader.read_all())
    .expect("read canonical facts")
}

struct RecordingEmitter {
    domains: Mutex<Vec<qaqh_domain::DomainEvent>>,
}

impl Emitter for RecordingEmitter {
    fn emit_domain(&self, event: qaqh_domain::DomainEvent) {
        self.domains.lock().expect("domain lock").push(event);
    }
}

struct CtxHarness {
    emitter: RecordingEmitter,
    phase: LoopPhase,
    pending: PendingState,
    writer_dead: Arc<AtomicBool>,
    stats: StatsCollector,
    flow: qaqh_message::ContextFlow,
    cancel: CancelToken,
}

impl CtxHarness {
    fn new() -> Self {
        Self {
            emitter: RecordingEmitter {
                domains: Mutex::new(Vec::new()),
            },
            phase: LoopPhase::ToolsRunning,
            pending: PendingState::default(),
            writer_dead: Arc::new(AtomicBool::new(false)),
            stats: StatsCollector::new(),
            flow: qaqh_message::ContextFlow::new(),
            cancel: CancelToken::new(),
        }
    }

    fn ctx<'a>(&'a mut self, agent: &'a mut AgentState) -> RingContext<'a> {
        RingContext {
            agent,
            emitter: &self.emitter,
            cancel: &self.cancel,
            phase: &mut self.phase,
            pending: &mut self.pending,
            writer_dead: &self.writer_dead,
            stats: &mut self.stats,
            flow: &mut self.flow,
        }
    }
}

fn ask_fixture() -> (AskMode, Vec<AskQuestion>) {
    (
        AskMode::Single,
        vec![AskQuestion {
            id: "q1".into(),
            question: "继续吗？".into(),
            options: vec!["继续".into()],
            allow_custom: false,
        }],
    )
}

fn answers_fixture() -> Vec<AskAnswer> {
    vec![AskAnswer {
        question_id: "q1".into(),
        answer: "继续".into(),
    }]
}

fn setup_agent(session_id: &str) -> AgentState {
    let mut agent = AgentState::init("ask-fold-test", qaqh_config::Config::default());
    agent.session.session_id = session_id.to_string();
    agent.ephemeral = false;
    agent
}

/// 契约 1：canonical 入站 id 的应答，resolution fact 必须落在请求 fact 的 id
/// 上，投影折叠后该交互不再 pending。
#[test]
fn canonical_ask_response_folds_in_projection() {
    let _guard = TEST_LOCK.lock().expect("test lock");
    init_process();

    let session_id = "ask-fold-canonical";
    let wire_turn = "turn-ask-fold";
    let wire_ask = "call-ask-fold";
    let mut agent = setup_agent(session_id);
    let mut engine = TurnEngine::new();
    let (mode, questions) = ask_fixture();
    observe_ask_yield_for_test(
        &mut engine,
        &mut agent,
        wire_turn,
        "input-ask-fold",
        wire_ask,
        mode,
        questions,
    )
    .expect("ask yield");

    let request_id = canonical_interaction_id(wire_ask);
    let requested = read_facts(session_id)
        .into_iter()
        .filter_map(|fact| match fact.payload {
            FactPayload::InteractionRequested(payload) => Some(payload.interaction_id),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(requested.len(), 1);
    assert_eq!(requested[0].as_str(), request_id);

    // 桌面壳透传的是 canonical id（v2 投影暴露形态）。
    // 队列已空：引擎随即进入下一轮 lap，测试环境无 LLM 端点，outcome 形态
    // 不重要——契约断言只看 fact 链与投影。
    let mut harness = CtxHarness::new();
    let mut tool = ToolEngine::new();
    let _ = engine.handle_ask_response(
        &mut harness.ctx(&mut agent),
        &mut tool,
        &request_id,
        "cmd-ask-fold",
        &answers_fixture(),
        None,
    );

    let snapshot = ControlProjection::rebuild(read_facts(session_id).into_iter()).snapshot();
    assert_eq!(snapshot.interactions.len(), 1);
    assert_eq!(snapshot.interactions[0].interaction_id.as_str(), request_id);
    assert!(
        snapshot.interactions[0].resolution.is_some(),
        "canonical 入站 id 的 resolution 必须折叠到请求 fact 的 id 上"
    );
    assert!(snapshot.interactions[0].expired_reason.is_none());
}

/// 契约 2：中止悬空回合必须为挂起交互落 expiry fact，投影不再投影成待审批。
#[test]
fn aborting_suspension_expires_pending_interactions() {
    let _guard = TEST_LOCK.lock().expect("test lock");
    init_process();

    let session_id = "ask-fold-abort";
    let mut agent = setup_agent(session_id);
    let mut engine = TurnEngine::new();
    let (mode, questions) = ask_fixture();
    observe_ask_yield_for_test(
        &mut engine,
        &mut agent,
        "turn-ask-abort",
        "input-ask-abort",
        "call-ask-abort",
        mode,
        questions,
    )
    .expect("ask yield");
    let request_id = canonical_interaction_id("call-ask-abort");

    let mut harness = CtxHarness::new();
    let aborted = engine.abort_suspended(&mut harness.ctx(&mut agent));
    assert_eq!(aborted.as_deref(), Some("turn-ask-abort"));

    let snapshot = ControlProjection::rebuild(read_facts(session_id).into_iter()).snapshot();
    assert_eq!(snapshot.interactions.len(), 1);
    assert_eq!(snapshot.interactions[0].interaction_id.as_str(), request_id);
    assert_eq!(
        snapshot.interactions[0].expired_reason,
        Some(InteractionExpiryReason::TurnCancelled),
        "中止悬空回合后投影不得再把该交互投影成待审批"
    );
}

/// 契约 3：排队中的后续 ask（未轮到）被错位应答时只拒绝，不过期。
#[test]
fn mismatched_response_keeps_queued_ask_pending() {
    let _guard = TEST_LOCK.lock().expect("test lock");
    init_process();

    let session_id = "ask-fold-queued";
    let mut agent = setup_agent(session_id);
    let mut engine = TurnEngine::new();
    let (mode, questions) = ask_fixture();
    observe_multi_ask_yield_for_test(
        &mut engine,
        &mut agent,
        "turn-ask-queued",
        "input-ask-queued",
        &["call-ask-queued-front", "call-ask-queued-back"],
        mode,
        questions,
    )
    .expect("ask yield");
    let front_id = canonical_interaction_id("call-ask-queued-front");
    let back_id = canonical_interaction_id("call-ask-queued-back");

    // 对排队中的后续 ask 应答：拒绝，但不得把它从 fact 链上过期掉。
    let mut harness = CtxHarness::new();
    let mut tool = ToolEngine::new();
    let _ = engine.handle_ask_response(
        &mut harness.ctx(&mut agent),
        &mut tool,
        &back_id,
        "cmd-ask-queued",
        &answers_fixture(),
        None,
    );

    let snapshot = ControlProjection::rebuild(read_facts(session_id).into_iter()).snapshot();
    let back = snapshot
        .interactions
        .iter()
        .find(|state| state.interaction_id.as_str() == back_id)
        .expect("queued ask must stay in the projection");
    assert!(
        back.resolution.is_none() && back.expired_reason.is_none(),
        "排队中的后续 ask 不得被错位应答过期"
    );
    let front = snapshot
        .interactions
        .iter()
        .find(|state| state.interaction_id.as_str() == front_id)
        .expect("front ask must stay pending");
    assert!(front.resolution.is_none() && front.expired_reason.is_none());
    assert_eq!(front.kind, InteractionKind::Ask);
    let _ = InteractionId::new("int_irrelevant");
}
