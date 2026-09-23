//! P3-6 production wiring contract: durable ToolIntent before handler, unique
//! ToolFinished after join, and no re-execution once a terminal exists.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Mutex, Once, OnceLock};
use std::time::Duration;

use qaqh_message::MessageStore;
use qaqh_runtime::agent::engine_tool::ToolEngine;
use qaqh_runtime::agent::state::agent::AgentState;
use qaqh_runtime::agent::turn_lap_test_api::execute_admitted_batch;
use qaqh_runtime::agent::types::{
    AdmittedTool, CancelToken, Emitter, LoopPhase, PendingState, RingContext, StatsCollector,
};
use qaqh_session::canonical::{CanonicalSessionIdentity, CommittedFactReader, ulid_from_text};
use qaqh_session::session_fact_v2::{FactPayload, ToolTerminalStatus};
use qaqh_types::{ContentBlock, Message, ToolStatus};
use qaqh_workspace::permission::ToolCategory;
use qaqh_workspace::{ToolCallCtx, ToolHandler, ToolManager, ToolResult, ToolRisk};

static TEST_LOCK: Mutex<()> = Mutex::new(());
static DATA_ROOT: OnceLock<tempfile::TempDir> = OnceLock::new();
static SESSION_DIR: OnceLock<PathBuf> = OnceLock::new();
static INIT: Once = Once::new();
static EXECUTIONS: AtomicUsize = AtomicUsize::new(0);
static INTENT_SEEN_BEFORE_HANDLER: AtomicBool = AtomicBool::new(false);

fn register_ledger_probe(mgr: &mut ToolManager) {
    mgr.register(ToolHandler {
        key: "ledger_probe".to_string(),
        description: "P3-6 ledger wiring probe",
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {"call": {"type": "string"}},
            "required": ["call"]
        }),
        handler: ledger_probe,
        risk: ToolRisk::ReadOnly,
        category: ToolCategory::Read,
        default_timeout: Duration::from_secs(5),
    });
}

fn ledger_probe(_ctx: ToolCallCtx) -> ToolResult {
    let Some(session_dir) = SESSION_DIR.get() else {
        return ToolResult::error("ledger probe: session dir not configured");
    };
    let identity = match CanonicalSessionIdentity::open_or_create(session_dir) {
        Ok(identity) => identity,
        Err(error) => return ToolResult::error(format!("ledger probe identity: {error}")),
    };
    let facts = match CommittedFactReader::open(
        session_dir,
        identity.session_id.clone(),
        identity.log_id.clone(),
    )
    .and_then(|reader| reader.read_all())
    {
        Ok(facts) => facts,
        Err(error) => return ToolResult::error(format!("ledger probe read: {error}")),
    };
    let intents = facts
        .iter()
        .filter(|fact| matches!(fact.payload, FactPayload::ToolIntent(_)))
        .count();
    let finished = facts
        .iter()
        .filter(|fact| matches!(fact.payload, FactPayload::ToolFinished(_)))
        .count();
    if intents != 1 || finished != 0 {
        return ToolResult::error(format!(
            "ledger probe expected one committed intent and no terminal, got intents={intents} finished={finished}"
        ));
    }
    INTENT_SEEN_BEFORE_HANDLER.store(true, Ordering::SeqCst);
    EXECUTIONS.fetch_add(1, Ordering::SeqCst);
    ToolResult::ok("ledger probe completed")
}

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
        qaqh_workspace::runtime::init_tools("tool-ledger-wiring", &[register_ledger_probe], vec![]);
    });
}

fn store_with_tool_use(seed: &str, call_id: &str, tool_name: &str) -> MessageStore {
    let mut store = MessageStore::new(seed);
    store.push_user("run the ledger probe");
    let mut assistant = Message {
        msg_id: None,
        role: Message::ROLE_ASSISTANT.into(),
        name: None,
        content: Vec::new(),
    };
    assistant.content = vec![ContentBlock::ToolUse {
        id: call_id.to_string(),
        name: tool_name.to_string(),
        input: serde_json::json!({"call": call_id}),
    }];
    let turn_completed = store.push_assistant(assistant);
    assert!(!turn_completed, "tool_use step must remain open");
    store
}

fn tool_scope(call_id: &str, seed: &str) -> qaqh_workspace::runtime::ToolExecutionScope {
    qaqh_workspace::runtime::ToolExecutionScope::capture(
        qaqh_workspace::tool_api::ToolCallContext {
            call_id: call_id.to_string(),
            session_id: seed.to_string(),
            workspace_root: PathBuf::from(qaqh_workspace::current_workspace()),
            mode: qaqh_workspace::tool_api::AgentMode::Code,
            permission_level: qaqh_workspace::permission::PermissionLevel::Unrestricted,
            sandbox: qaqh_workspace::tool_api::SandboxMode::Main,
            timeout: Duration::ZERO,
            cancellation: qaqh_workspace::tool_api::CancellationToken::new(),
            progress: None,
            source: qaqh_workspace::tool_api::ToolCallSource::Model,
        },
    )
}

fn run_admitted_batch(
    agent: &mut AgentState,
    admitted: AdmittedTool,
    order: Vec<String>,
    serial: HashSet<String>,
    cancel: CancelToken,
) -> bool {
    let emitter = RecordingEmitter::default();
    let mut phase = LoopPhase::ToolsRunning;
    let mut pending = PendingState::default();
    let writer_dead = std::sync::Arc::new(AtomicBool::new(false));
    let mut stats = StatsCollector::new();
    let mut flow = qaqh_message::ContextFlow::new();
    let tool = ToolEngine::new();
    let mut ctx = RingContext {
        agent,
        emitter: &emitter,
        cancel: &cancel,
        phase: &mut phase,
        pending: &mut pending,
        writer_dead: &writer_dead,
        stats: &mut stats,
        flow: &mut flow,
    };
    execute_admitted_batch(
        &mut ctx,
        &tool,
        vec![admitted],
        &order,
        &serial,
        "turn-ledger",
        0,
    )
}

fn admitted_call(seed: &str, call_id: &str, tool_name: &str) -> AdmittedTool {
    let args = serde_json::json!({"call": call_id});
    let auth = match qaqh_workspace::authorize_call(seed, call_id, tool_name, &args, 4) {
        qaqh_workspace::Admission::Authorized(auth) => auth,
        qaqh_workspace::Admission::ApprovalRequired(_) => {
            panic!("call {call_id} unexpectedly requires approval")
        }
        qaqh_workspace::Admission::Denied(reason) => {
            panic!("call {call_id} was denied: {reason}")
        }
    };
    AdmittedTool {
        call_id: call_id.to_string(),
        auth: Box::new(auth),
        scope: tool_scope(call_id, seed),
    }
}

fn run_call(agent: &mut AgentState, seed: &str, call_id: &str, tool_name: &str) -> bool {
    let admitted = admitted_call(seed, call_id, tool_name);
    run_admitted_batch(
        agent,
        admitted,
        vec![call_id.to_string()],
        HashSet::new(),
        CancelToken::new(),
    )
}

#[derive(Default)]
struct RecordingEmitter {
    domains: Mutex<Vec<qaqh_domain::DomainEvent>>,
    timelines: Mutex<Vec<qaqh_domain::TimelineIntent>>,
}

impl Emitter for RecordingEmitter {
    fn emit_domain(&self, event: qaqh_domain::DomainEvent) {
        self.domains.lock().expect("domain lock").push(event);
    }
    fn emit_timeline(&self, intent: qaqh_domain::TimelineIntent) {
        self.timelines.lock().expect("timeline lock").push(intent);
    }
}

fn tool_result(store: &MessageStore, call_id: &str) -> qaqh_types::ToolResult {
    for message in store.to_vec() {
        for block in message.content {
            if let ContentBlock::ToolResult {
                tool_use_id,
                result,
            } = block
                && tool_use_id == call_id
            {
                return result;
            }
        }
    }
    panic!("missing tool result for {call_id}");
}

#[test]
fn intent_precedes_handler_finish_is_unique_and_terminal_blocks_replay() {
    let _guard = TEST_LOCK.lock().unwrap_or_else(|error| error.into_inner());
    init_process();

    let workspace = tempfile::tempdir().expect("workspace tempdir");
    qaqh_workspace::set_workspace(&workspace.path().to_string_lossy());

    let seed = format!(
        "tool-ledger-{}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|duration| duration.as_nanos())
            .unwrap_or(0)
    );
    let session_dir = qaqh_types::platform::sessions_dir().join(&seed);
    SESSION_DIR
        .set(session_dir.clone())
        .expect("session dir configured once");

    let mut agent = AgentState::init("tool-ledger-test", qaqh_config::Config::default());
    agent.session.seed = seed.clone();
    agent.ephemeral = false;
    agent.config.permission_level = 4;
    agent.msg = store_with_tool_use(&seed, "call-ledger-1", "ledger_probe");

    assert!(
        run_call(&mut agent, &seed, "call-ledger-1", "ledger_probe"),
        "first execution must complete"
    );
    let first_result = tool_result(&agent.msg, "call-ledger-1");
    assert_eq!(
        first_result.status,
        ToolStatus::Ok,
        "handler must observe committed intent; result={first_result:?}"
    );
    assert!(INTENT_SEEN_BEFORE_HANDLER.load(Ordering::SeqCst));
    assert_eq!(EXECUTIONS.load(Ordering::SeqCst), 1);
    assert_eq!(
        tool_result(&agent.msg, "call-ledger-1").status,
        ToolStatus::Ok
    );

    let identity = CanonicalSessionIdentity::open_or_create(&session_dir).expect("identity");
    let facts = CommittedFactReader::open(
        &session_dir,
        identity.session_id.clone(),
        identity.log_id.clone(),
    )
    .and_then(|reader| reader.read_all())
    .expect("read canonical facts");
    let intents = facts
        .iter()
        .filter(|fact| matches!(fact.payload, FactPayload::ToolIntent(_)))
        .count();
    let finished = facts
        .iter()
        .filter_map(|fact| match &fact.payload {
            FactPayload::ToolFinished(finished) => Some(finished),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(intents, 1, "exactly one ToolIntent must be committed");
    assert_eq!(
        finished.len(),
        1,
        "exactly one ToolFinished must be committed"
    );
    assert_eq!(finished[0].terminal_status, ToolTerminalStatus::Succeeded);
    let expected_turn = format!("turn_{}", ulid_from_text("turn-ledger"));
    assert!(
        facts
            .iter()
            .filter(|fact| matches!(
                fact.payload,
                FactPayload::ToolIntent(_) | FactPayload::ToolFinished(_)
            ))
            .all(|fact| fact.turn_id.as_ref().map(|turn| turn.as_str())
                == Some(expected_turn.as_str())),
        "tool facts must carry the canonical turn alias"
    );

    // Replay the same call in the same actor. The ledger terminal is the
    // execution guard: the handler counter must not move.
    agent.msg = store_with_tool_use(&seed, "call-ledger-1", "ledger_probe");
    assert!(
        run_call(&mut agent, &seed, "call-ledger-1", "ledger_probe"),
        "blocked replay still produces a normal tool result"
    );
    assert_eq!(
        EXECUTIONS.load(Ordering::SeqCst),
        1,
        "a terminal call must never re-enter the handler"
    );
    let blocked = tool_result(&agent.msg, "call-ledger-1");
    assert_eq!(blocked.status, ToolStatus::Error);
    assert_eq!(
        blocked.error.as_ref().map(|error| error.code.as_str()),
        Some("LEDGER_BLOCKED")
    );

    // A serial tail cancelled before spawn must be sealed as Cancelled without
    // leaving an orphan ToolIntent behind for recovery to interpret.
    let cancelled_call = "call-ledger-cancelled-tail";
    agent.msg = store_with_tool_use(&seed, cancelled_call, "ledger_probe");
    let cancel = CancelToken::new();
    cancel.set();
    let serial = HashSet::from([cancelled_call.to_string()]);
    assert!(
        !run_admitted_batch(
            &mut agent,
            admitted_call(&seed, cancelled_call, "ledger_probe"),
            vec![cancelled_call.to_string()],
            serial,
            cancel,
        ),
        "a pre-cancelled serial call must report an interrupted batch"
    );
    assert_eq!(
        tool_result(&agent.msg, cancelled_call).status,
        ToolStatus::Cancelled
    );
    let facts = CommittedFactReader::open(
        &session_dir,
        identity.session_id.clone(),
        identity.log_id.clone(),
    )
    .and_then(|reader| reader.read_all())
    .expect("read facts after cancelled tail");
    assert_eq!(
        facts
            .iter()
            .filter(|fact| matches!(fact.payload, FactPayload::ToolIntent(_)))
            .count(),
        1,
        "cancelled serial tail must not append an orphan ToolIntent"
    );
    let cancelled_canonical = format!("call_{}", ulid_from_text(cancelled_call));
    let cancelled_finished = facts
        .iter()
        .find_map(|fact| match &fact.payload {
            FactPayload::ToolFinished(finished)
                if finished.call_id.as_str() == cancelled_canonical =>
            {
                Some((fact, finished))
            }
            _ => None,
        })
        .expect("cancelled serial tail must append a canonical terminal");
    assert_eq!(
        cancelled_finished.1.terminal_status,
        ToolTerminalStatus::Cancelled
    );
    assert_eq!(cancelled_finished.1.execution_id, None);
    assert_eq!(
        cancelled_finished
            .0
            .turn_id
            .as_ref()
            .map(|turn| turn.as_str()),
        Some(expected_turn.as_str())
    );
}
