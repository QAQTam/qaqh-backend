//! P3-7 terminal-write failure contract: if the handler completes but the
//! canonical writer is fenced before ToolFinished commits, the call must remain
//! open and later recover as Indeterminate rather than replaying side effects.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex, OnceLock};
use std::time::Duration;

use qaqh_message::MessageStore;
use qaqh_runtime::agent::engine_tool::ToolEngine;
use qaqh_runtime::agent::state::agent::AgentState;
use qaqh_runtime::agent::turn_lap_test_api::execute_admitted_batch;
use qaqh_runtime::agent::types::{
    AdmittedTool, CancelToken, Emitter, LoopPhase, PendingState, RingContext, StatsCollector,
};
use qaqh_session::canonical::{
    CanonicalLog, CanonicalSessionIdentity, CommittedFactReader, ToolLedger,
    ToolRecoveryDisposition, WriterId,
};
use qaqh_session::session_fact_v2::FactPayload;
use qaqh_types::{ContentBlock, Message, ToolStatus};
use qaqh_workspace::permission::ToolCategory;
use qaqh_workspace::{ToolCallCtx, ToolHandler, ToolManager, ToolResult, ToolRisk};

static SESSION_DIR: OnceLock<PathBuf> = OnceLock::new();

fn unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

fn register_probe(mgr: &mut ToolManager) {
    mgr.register(ToolHandler {
        key: "fenced_finish_probe".to_string(),
        description: "P3-7 terminal write failure probe",
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {"call": {"type": "string"}},
            "required": ["call"]
        }),
        handler: probe,
        risk: ToolRisk::ReadOnly,
        category: ToolCategory::Read,
        default_timeout: Duration::from_secs(5),
    });
}

fn probe(_ctx: ToolCallCtx) -> ToolResult {
    let Some(session_dir) = SESSION_DIR.get() else {
        return ToolResult::error("fenced finish probe: session dir not configured");
    };
    let identity = match CanonicalSessionIdentity::open_or_create(session_dir) {
        Ok(identity) => identity,
        Err(error) => return ToolResult::error(format!("identity: {error}")),
    };
    let mut log = match CanonicalLog::open(
        session_dir,
        identity.session_id.clone(),
        identity.log_id.clone(),
    ) {
        Ok(log) => log,
        Err(error) => return ToolResult::error(format!("open canonical log: {error}")),
    };
    if let Err(error) = log.rotate_writer_fence(WriterId::new("fence-intruder"), 2, 2, unix_ms(), 1)
    {
        return ToolResult::error(format!("rotate writer fence: {error}"));
    }
    ToolResult::ok("handler completed before terminal write")
}

fn store_with_tool_use(session_id: &str, call_id: &str, tool_name: &str) -> MessageStore {
    let mut store = MessageStore::new(session_id);
    store.push_user("run the fenced finish probe");
    let assistant = Message {
        msg_id: None,
        role: Message::ROLE_ASSISTANT.into(),
        name: None,
        content: vec![ContentBlock::ToolUse {
            id: call_id.to_string(),
            name: tool_name.to_string(),
            input: serde_json::json!({"call": call_id}),
        }],
    };
    let turn_completed = store.push_assistant(assistant);
    assert!(!turn_completed, "tool_use step must remain open");
    store
}

fn tool_scope(call_id: &str, session_id: &str) -> qaqh_workspace::runtime::ToolExecutionScope {
    qaqh_workspace::runtime::ToolExecutionScope::capture(
        qaqh_workspace::tool_api::ToolCallContext {
            call_id: call_id.to_string(),
            session_id: session_id.to_string(),
            workspace_root: PathBuf::from(qaqh_workspace::current_workspace()),
            mode: qaqh_workspace::tool_api::AgentMode::Code,
            permission_level: qaqh_workspace::permission::PermissionLevel::SkipPermissions,
            sandbox: qaqh_workspace::tool_api::SandboxMode::Main,
            sandbox_spec: qaqh_workspace::tool_api::SandboxSpec::workspace_write(PathBuf::from(
                qaqh_workspace::current_workspace(),
            )),
            exec_default_shell: None,
            timeout: Duration::ZERO,
            cancellation: qaqh_workspace::tool_api::CancellationToken::new(),
            progress: None,
            source: qaqh_workspace::tool_api::ToolCallSource::Model,
        },
    )
}

fn admitted_call(session_id: &str, call_id: &str, tool_name: &str) -> AdmittedTool {
    let args = serde_json::json!({"call": call_id});
    let auth = match qaqh_workspace::authorize_call(session_id, call_id, tool_name, &args, 3) {
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
        scope: tool_scope(call_id, session_id),
    }
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

fn run_call(agent: &mut AgentState, session_id: &str, call_id: &str, tool_name: &str) -> bool {
    let emitter = RecordingEmitter::default();
    let cancel = CancelToken::new();
    let mut phase = LoopPhase::ToolsRunning;
    let mut pending = PendingState::default();
    let writer_dead = Arc::new(AtomicBool::new(false));
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
        vec![admitted_call(session_id, call_id, tool_name)],
        &[call_id.to_string()],
        &HashSet::new(),
        "turn-fenced-finish",
        0,
    )
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
fn fenced_finish_keeps_intent_open_and_recovers_as_indeterminate() {
    let data_root = tempfile::tempdir().expect("data root tempdir");
    // SAFETY: this integration-test binary is single-test and sets the root
    // before any platform data-dir access.
    unsafe {
        std::env::set_var("QAQH_DATA_DIR", data_root.path());
    }
    qaqh_session::SessionManager::init(qaqh_types::platform::data_dir());
    qaqh_workspace::runtime::init_tools("fenced-finish-test", &[register_probe], vec![]);

    let workspace = tempfile::tempdir().expect("workspace tempdir");
    qaqh_workspace::set_workspace(&workspace.path().to_string_lossy());

    let session_id = "tool-ledger-fenced-finish";
    let call_id = "call-fenced-finish";
    let session_dir = qaqh_types::platform::sessions_dir().join(session_id);
    SESSION_DIR
        .set(session_dir.clone())
        .expect("session dir configured once");
    let identity = CanonicalSessionIdentity::open_or_create(&session_dir).expect("identity");

    let mut agent = AgentState::init("fenced-finish-test", qaqh_config::Config::default());
    agent.session.session_id = session_id.to_string();
    agent.ephemeral = false;
    agent.config.permission_level = 3; // skip-permissions(三档制)
    agent.msg = store_with_tool_use(session_id, call_id, "fenced_finish_probe");

    assert!(
        run_call(&mut agent, session_id, call_id, "fenced_finish_probe"),
        "ledger failure is a completed tool round with an error result"
    );
    let result = tool_result(&agent.msg, call_id);
    assert_eq!(result.status, ToolStatus::Error);
    assert_eq!(
        result.error.as_ref().map(|error| error.code.as_str()),
        Some("ledger_write_failed")
    );

    let facts = CommittedFactReader::open(
        &session_dir,
        identity.session_id.clone(),
        identity.log_id.clone(),
    )
    .and_then(|reader| reader.read_all())
    .expect("read facts after fenced finish");
    assert_eq!(
        facts
            .iter()
            .filter(|fact| matches!(fact.payload, FactPayload::ToolIntent(_)))
            .count(),
        1
    );
    assert_eq!(
        facts
            .iter()
            .filter(|fact| matches!(fact.payload, FactPayload::ToolFinished(_)))
            .count(),
        0,
        "failed terminal append must leave the intent open"
    );

    std::thread::sleep(Duration::from_millis(20));
    let ledger = ToolLedger::open(
        &session_dir,
        identity.session_id,
        identity.log_id,
        WriterId::new("recovery-checker"),
        unix_ms(),
        10_000,
    )
    .expect("reopen ledger after intruder lease expiry");
    let canonical_call = facts
        .iter()
        .find_map(|fact| match &fact.payload {
            FactPayload::ToolIntent(intent) => Some(intent.call_id.clone()),
            _ => None,
        })
        .expect("intent call id");
    assert_eq!(
        ledger.recovery_disposition(&canonical_call),
        Some(ToolRecoveryDisposition::IndeterminateRequired {
            execution_id: match facts.iter().find_map(|fact| match &fact.payload {
                FactPayload::ToolIntent(intent) => Some(intent.execution_id.clone()),
                _ => None,
            }) {
                Some(execution_id) => execution_id,
                None => panic!("intent execution id"),
            },
        })
    );
    assert_eq!(
        ledger
            .get(&canonical_call)
            .and_then(|entry| entry.finished())
            .map(|finished| finished.terminal_status),
        None
    );
}
