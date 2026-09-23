//! P3-7 recovery-window contract: an open non-replayable intent survives
//! restart, is sealed as Indeterminate, and never re-enters the handler.

use std::collections::HashSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use qaqh_message::MessageStore;
use qaqh_runtime::agent::engine_tool::ToolEngine;
use qaqh_runtime::agent::state::agent::AgentState;
use qaqh_runtime::agent::turn_lap_test_api::execute_admitted_batch;
use qaqh_runtime::agent::types::{
    AdmittedTool, CancelToken, Emitter, LoopPhase, PendingState, RingContext, StatsCollector,
};
use qaqh_session::canonical::{
    CanonicalSessionIdentity, CommittedFactReader, ToolLedger, WriterId, generate_ulid,
    sha256_content_hash, ulid_from_text,
};
use qaqh_session::session_fact_v2::{
    EventId, ExecutionId, FactPayload, PolicyDecisionRef, SideEffectClass, ToolCallId, ToolIntent,
    ToolIntentPolicyOutcome, ToolReplayCapability, ToolTerminalStatus, TurnId,
};
use qaqh_types::{ContentBlock, Message, ToolStatus};
use qaqh_workspace::permission::ToolCategory;
use qaqh_workspace::{ToolCallCtx, ToolHandler, ToolManager, ToolResult, ToolRisk};

static EXECUTIONS: AtomicUsize = AtomicUsize::new(0);

fn register_probe(mgr: &mut ToolManager) {
    mgr.register(ToolHandler {
        key: "open_intent_probe".to_string(),
        description: "P3-7 open intent recovery probe",
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
    EXECUTIONS.fetch_add(1, Ordering::SeqCst);
    ToolResult::ok("must not execute")
}

fn unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|duration| duration.as_millis() as i64)
        .unwrap_or(0)
}

fn store_with_tool_use(seed: &str, call_id: &str, tool_name: &str) -> MessageStore {
    let mut store = MessageStore::new(seed);
    store.push_user("recover the open tool intent");
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

fn run_call(agent: &mut AgentState, seed: &str, call_id: &str, tool_name: &str) -> bool {
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
        vec![admitted_call(seed, call_id, tool_name)],
        &[call_id.to_string()],
        &HashSet::new(),
        "turn-open-intent",
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
fn open_non_replay_intent_is_sealed_and_handler_never_runs() {
    let data_root = tempfile::tempdir().expect("data root tempdir");
    // SAFETY: this integration-test binary is single-test and sets the root
    // before any platform data-dir access.
    unsafe {
        std::env::set_var("QAQH_DATA_DIR", data_root.path());
    }
    qaqh_session::SessionManager::init(qaqh_types::platform::data_dir());
    qaqh_workspace::runtime::init_tools("open-intent-test", &[register_probe], vec![]);

    let workspace = tempfile::tempdir().expect("workspace tempdir");
    qaqh_workspace::set_workspace(&workspace.path().to_string_lossy());

    let seed = "tool-ledger-open-intent";
    let call_id = "call-open-intent";
    let session_dir = qaqh_types::platform::sessions_dir().join(seed);
    let identity = CanonicalSessionIdentity::open_or_create(&session_dir).expect("identity");

    let now = unix_ms();
    let mut setup = ToolLedger::open(
        &session_dir,
        identity.session_id.clone(),
        identity.log_id.clone(),
        WriterId::new("recovery-window-setup"),
        now,
        1,
    )
    .expect("open setup ledger");
    let canonical_call = ToolCallId::new(format!("call_{}", ulid_from_text(call_id)));
    setup
        .append_intent(
            EventId::new(generate_ulid()),
            Some(TurnId::new(format!(
                "turn_{}",
                ulid_from_text("turn-open-intent")
            ))),
            ToolIntent {
                call_id: canonical_call.clone(),
                execution_id: ExecutionId::new(format!("exec_{}", generate_ulid())),
                idempotency_key: None,
                replay_capability: ToolReplayCapability::NoReplay,
                policy_decision: PolicyDecisionRef {
                    outcome: ToolIntentPolicyOutcome::Allow,
                    rule_id: "test.precommitted".into(),
                    decided_at_ms: now,
                    reason_ref: None,
                },
                effective_args_ref: None,
                effective_args_hash: None,
                sandbox_spec_hash: sha256_content_hash(b"test-sandbox"),
                side_effect_class: SideEffectClass::ReadOnly,
                intent_at_ms: now,
            },
            now,
        )
        .expect("append pre-committed intent");
    drop(setup);

    // Let the setup writer lease expire so the production actor can take over.
    std::thread::sleep(Duration::from_millis(20));

    let mut agent = AgentState::init("open-intent-test", qaqh_config::Config::default());
    agent.session.seed = seed.to_string();
    agent.ephemeral = false;
    agent.config.permission_level = 4;
    agent.msg = store_with_tool_use(seed, call_id, "open_intent_probe");

    assert!(
        run_call(&mut agent, seed, call_id, "open_intent_probe"),
        "blocked recovery still produces a normal tool result"
    );
    assert_eq!(
        EXECUTIONS.load(Ordering::SeqCst),
        0,
        "an open non-replayable intent must never re-enter the handler"
    );
    let result = tool_result(&agent.msg, call_id);
    assert_eq!(result.status, ToolStatus::Error);
    assert_eq!(
        result.error.as_ref().map(|error| error.code.as_str()),
        Some("LEDGER_BLOCKED")
    );

    let facts = CommittedFactReader::open(
        &session_dir,
        identity.session_id.clone(),
        identity.log_id.clone(),
    )
    .and_then(|reader| reader.read_all())
    .expect("read recovery facts");
    assert_eq!(
        facts
            .iter()
            .filter(|fact| matches!(fact.payload, FactPayload::ToolIntent(_)))
            .count(),
        1
    );
    let finished = facts
        .iter()
        .filter_map(|fact| match &fact.payload {
            FactPayload::ToolFinished(finished) => Some(finished),
            _ => None,
        })
        .collect::<Vec<_>>();
    assert_eq!(finished.len(), 1);
    assert_eq!(
        finished[0].terminal_status,
        ToolTerminalStatus::Indeterminate
    );
    assert_eq!(finished[0].call_id, canonical_call);
    let expected_turn = format!("turn_{}", ulid_from_text("turn-open-intent"));
    let finished_fact = facts
        .iter()
        .find(|fact| matches!(&fact.payload, FactPayload::ToolFinished(payload) if payload.call_id == canonical_call))
        .expect("finished fact");
    assert_eq!(
        finished_fact.turn_id.as_ref().map(|turn| turn.as_str()),
        Some(expected_turn.as_str())
    );
}
