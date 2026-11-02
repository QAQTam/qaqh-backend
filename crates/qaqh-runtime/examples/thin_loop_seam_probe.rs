//! Temporary isolation probe for the thin-loop P1–P8 seam.
//!
//! This is **not** a repository contract test. It runs on an isolated data root
//! and exists to prove the new seam end to end (including fault injection,
//! mid-turn crash recovery, four provider protocols, and real tool execution)
//! while RLC gates are still being assembled. Delete or replace it when the
//! real gates land.

// 临时隔离探针（非库代码、非契约测试）：探针失败就该立刻 panic，unwrap 是刻意的。
#![allow(clippy::unwrap_used)]

use qaqh_agentloop_core::{
    ActorIdentity, CompletionMatch, DispatchEvidence, EffectIdentity,
    commit_gate::{CommitGate, CommitState as GateState},
    mailbox::{CompletionReservation, FrameSize, Input, LaneBudget, Mailbox, MailboxHandle},
    supervisor::{Supervisor, SupervisorError, TaskCompletion, TaskOutcome},
};
use qaqh_runtime::{
    provider_job::{
        self, ProviderOutcome, ProviderRequestSpec, build_model_round_started,
        prepare_response_commit_facts,
    },
    quota_ledger::{QuotaLedger, QuotaLimits, ReleaseReason},
    tool_job::{
        self, ToolApprovalGrant, ToolFailure, ToolOutcome, ToolOutput, ToolPolicySnapshot,
        ToolPort, ToolRequest, WorkspaceToolPort,
    },
};
use qaqh_session::{
    canonical::{BatchFaultPoint, sha256_content_hash},
    context_projection::ContextBudget,
    session_commit::{CommittedExecution, FenceReason, SessionCommit, SessionCommitError},
    session_fact_v2::{
        ActorKind, ActorRef, ContentHash, ContextMessageCommitted, ContextMessageOrigin, EventId,
        ExecutionId, FactPayload, FactSchema, InputAccepted, InputId, InputKind, InputPurpose,
        InteractionExpired, InteractionExpiryReason, InteractionId, InteractionKind,
        InteractionRequested, InteractionResolved, InterruptReason, LogId, ModelRoundStarted,
        PolicyDecisionRef, ProviderFinish, ProviderResponseCommitted, QueuedInputDisposition,
        QueuedInputSettled, RecoveryAction, RecoveryId, RecoveryOutcome, RecoveryRef,
        RecoveryToolCompletion, RequestIdentity, SessionCreated, SessionFact, SessionId,
        SessionRecovered, SideEffectClass, ToolCallDeclared, ToolCallId, ToolError, ToolFinished,
        ToolIntent, ToolIntentPolicyOutcome, ToolMetrics, ToolReplayCapability, ToolTerminalStatus,
        TurnFinished, TurnId, TurnInterrupted, TurnMode, TurnStarted, TurnTerminal,
    },
};
use qaqh_types::{ContentBlock, Message, ToolDef, ToolFunction, ToolResult};
use tempfile;
use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicBool, AtomicUsize, Ordering},
    },
    time::Duration,
};

const SESSION: &str = "0198f1a0-0000-7000-8000-000000000001";
const LOG: &str = "0198f1a0-0000-7000-8000-000000000002";
const NOW_MS: i64 = 1_789_830_000_000;

/// Minimal chat-completions SSE: one content frame, one terminal frame, `[DONE]`.
const CHAT_SSE: &str = concat!(
    "data: {\"id\":\"mock\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"content\":\"live-ok\"},\"finish_reason\":null}]}\n\n",
    "data: {\"id\":\"mock\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":12,\"completion_tokens\":4,\"total_tokens\":16}}\n\n",
    "data: [DONE]\n\n",
);

/// Chat-completions SSE with reasoning + tool call + usage.
const CHAT_TOOL_SSE: &str = concat!(
    "data: {\"id\":\"chat-1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"reasoning_content\":\"plan tool\"},\"finish_reason\":null}]}\n\n",
    "data: {\"id\":\"chat-1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_01J00000000000000000000101\",\"type\":\"function\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"path\\\":\\\"hello.txt\\\"}\"}}]},\"finish_reason\":null}]}\n\n",
    "data: {\"id\":\"chat-1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":20,\"completion_tokens\":8,\"total_tokens\":28}}\n\n",
    "data: [DONE]\n\n",
);

/// Responses API SSE: encrypted reasoning item + function_call item + usage.
const RESPONSES_ROUND1_SSE: &str = concat!(
    "event: response.reasoning_summary_text.delta\n",
    "data: {\"type\":\"response.reasoning_summary_text.delta\",\"delta\":\"thinking summary\"}\n\n",
    "event: response.completed\n",
    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_1\",\"status\":\"completed\",\"output\":[{\"type\":\"reasoning\",\"id\":\"rs_encrypted1\",\"encrypted_content\":\"ENC_BLOB_V1\",\"summary\":[{\"type\":\"summary_text\",\"text\":\"thinking summary\"}]},{\"type\":\"function_call\",\"id\":\"fc_1\",\"call_id\":\"call_01J00000000000000000000201\",\"name\":\"read\",\"arguments\":\"{\\\"path\\\":\\\"note.txt\\\"}\",\"status\":\"completed\"}],\"usage\":{\"input_tokens\":30,\"output_tokens\":10,\"total_tokens\":40,\"input_tokens_details\":{\"cached_tokens\":10},\"output_tokens_details\":{\"reasoning_tokens\":4}}}}\n\n",
);

/// Responses API Round 2 SSE: final assistant text after tool result replay.
const RESPONSES_ROUND2_SSE: &str = concat!(
    "event: response.output_text.delta\n",
    "data: {\"type\":\"response.output_text.delta\",\"delta\":\"responses-round2-ok\"}\n\n",
    "event: response.completed\n",
    "data: {\"type\":\"response.completed\",\"response\":{\"id\":\"resp_2\",\"status\":\"completed\",\"output\":[{\"type\":\"message\",\"id\":\"msg_2\",\"role\":\"assistant\",\"status\":\"completed\",\"content\":[{\"type\":\"output_text\",\"text\":\"responses-round2-ok\"}]}],\"usage\":{\"input_tokens\":45,\"output_tokens\":6,\"total_tokens\":51}}}\n\n",
);

/// Anthropic Messages SSE: thinking + text + usage.
const MESSAGES_SSE: &str = concat!(
    "event: message_start\n",
    "data: {\"type\":\"message_start\",\"message\":{\"id\":\"msg_anth\",\"type\":\"message\",\"role\":\"assistant\",\"content\":[],\"model\":\"claude-probe\",\"stop_reason\":null,\"usage\":{\"input_tokens\":18,\"output_tokens\":1}}}\n\n",
    "event: content_block_start\n",
    "data: {\"type\":\"content_block_start\",\"index\":0,\"content_block\":{\"type\":\"thinking\",\"thinking\":\"\"}}\n\n",
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":0,\"delta\":{\"type\":\"thinking_delta\",\"thinking\":\"anth-think\"}}\n\n",
    "event: content_block_stop\n",
    "data: {\"type\":\"content_block_stop\",\"index\":0}\n\n",
    "event: content_block_start\n",
    "data: {\"type\":\"content_block_start\",\"index\":1,\"content_block\":{\"type\":\"text\",\"text\":\"\"}}\n\n",
    "event: content_block_delta\n",
    "data: {\"type\":\"content_block_delta\",\"index\":1,\"delta\":{\"type\":\"text_delta\",\"text\":\"anthropic-ok\"}}\n\n",
    "event: content_block_stop\n",
    "data: {\"type\":\"content_block_stop\",\"index\":1}\n\n",
    "event: message_delta\n",
    "data: {\"type\":\"message_delta\",\"delta\":{\"stop_reason\":\"end_turn\"},\"usage\":{\"input_tokens\":18,\"output_tokens\":7}}\n\n",
    "event: message_stop\n",
    "data: {\"type\":\"message_stop\"}\n\n",
);

/// Gemini streamGenerateContent SSE: thought part + text part + usageMetadata.
const GEMINI_SSE: &str = concat!(
    "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"gemini-thought\",\"thought\":true}]}}]}\n\n",
    "data: {\"candidates\":[{\"content\":{\"role\":\"model\",\"parts\":[{\"text\":\"gemini-ok\"}]},\"finishReason\":\"STOP\"}],\"usageMetadata\":{\"promptTokenCount\":14,\"candidatesTokenCount\":5,\"totalTokenCount\":22,\"thoughtsTokenCount\":3}}\n\n",
);

fn event_id(ordinal: u64) -> String {
    format!("01J{ordinal:023}")
}

fn fact(payload: FactPayload, ordinal: u64, turn_id: Option<TurnId>) -> SessionFact {
    SessionFact {
        schema: FactSchema::v2(),
        session_id: SessionId::new(SESSION),
        log_id: LogId::new(LOG),
        fact_seq: 0,
        event_id: EventId::new(event_id(ordinal)),
        ts_ms: NOW_MS + ordinal as i64,
        causation_id: None,
        turn_id,
        call_id: None,
        interaction_id: None,
        payload,
    }
}

fn budget() -> ContextBudget {
    ContextBudget {
        max_messages: 64,
        max_payload_bytes: 1 << 20,
        max_checkpoint_bytes: 1 << 20,
    }
}

fn digest(label: &str) -> ContentHash {
    sha256_content_hash(label.as_bytes())
}

fn text_message(role: &str, text: &str) -> Message {
    Message {
        msg_id: None,
        role: role.into(),
        name: None,
        content: vec![ContentBlock::Text { text: text.into() }],
    }
}

/// Commit one user input and mint the round-1 model capability.
fn mint_capability(
    dir: &std::path::Path,
) -> (SessionCommit, CommittedExecution, SessionFact, TurnId) {
    let mut commit = SessionCommit::create(dir, SessionId::new(SESSION), LogId::new(LOG), budget())
        .expect("create isolated session");

    let created = SessionCreated {
        created_at_ms: NOW_MS + 1,
        cwd: "/tmp".into(),
        model: "probe-model".into(),
        parent_session_id: None,
        schema_caps: vec![],
    };
    commit
        .commit(
            "batch-1",
            0,
            vec![fact(FactPayload::SessionCreated(created), 1, None)],
        )
        .expect("commit session created");

    let input_id = InputId::new("input_01J00000000000000000000001");
    commit
        .commit(
            "batch-2",
            1,
            vec![fact(
                FactPayload::InputAccepted(InputAccepted {
                    input_id: input_id.clone(),
                    input_kind: InputKind::UserText,
                    input_purpose: InputPurpose::TriggerTurn,
                    content_ref: None,
                    inline_text: Some("probe".into()),
                    attachments: vec![],
                    actor: ActorRef {
                        kind: ActorKind::User,
                        id: "probe-user".into(),
                        display_name: None,
                    },
                    client_request_id: None,
                }),
                2,
                None,
            )],
        )
        .expect("commit input");

    let turn_id = TurnId::new("turn_01J00000000000000000000002");
    commit
        .commit(
            "batch-3",
            2,
            vec![fact(
                FactPayload::TurnStarted(TurnStarted {
                    turn_id: turn_id.clone(),
                    input_id,
                    mode: TurnMode::Normal,
                    recovery_ref: None,
                }),
                3,
                Some(turn_id.clone()),
            )],
        )
        .expect("commit turn started");

    let message_ref = commit
        .put_blob(&serde_json::to_vec(&text_message("user", "probe")).unwrap())
        .expect("publish user message blob");
    commit
        .commit(
            "batch-4",
            3,
            vec![fact(
                FactPayload::ContextMessageCommitted(ContextMessageCommitted {
                    message_ref,
                    origin: ContextMessageOrigin::Input,
                }),
                4,
                Some(turn_id.clone()),
            )],
        )
        .expect("commit context message");

    let head = commit.head().expect("head");
    let round = fact(
        FactPayload::ModelRoundStarted(round_started(&commit, &turn_id, 1, head)),
        5,
        Some(turn_id.clone()),
    );
    let (_, capability) = commit
        .commit_with_effect("batch-5", head, vec![round.clone()], &event_id(5))
        .expect("commit model intent");
    let capability = capability.expect("fresh commit must mint one capability");
    (commit, capability, round, turn_id)
}

fn round_started(
    commit: &SessionCommit,
    turn_id: &TurnId,
    round: u32,
    head: u64,
) -> ModelRoundStarted {
    let messages = commit.request_messages().expect("frozen context");
    let request_hash = sha256_content_hash(&serde_json::to_vec(messages).unwrap());
    ModelRoundStarted {
        turn_id: turn_id.clone(),
        round,
        request_hash,
        context_revision: head,
        identity: Some(RequestIdentity {
            attempt: round,
            model: "probe-model".into(),
            provider_digest: digest("provider:probe"),
            params_digest: digest("params:max_tokens=64"),
            tool_catalog_digest: digest("tools:none"),
            prefix_digest: digest("prefix:probe"),
            request_digest: digest(&format!("request:round={round}:head={head}")),
        }),
    }
}

/// Minimal executor: records that it ran and obeys the cancel flag.
struct FakeTool {
    runs: Arc<AtomicUsize>,
}

impl ToolPort for FakeTool {
    fn execute(
        &self,
        request: &ToolRequest,
        cancel: Arc<AtomicBool>,
    ) -> Result<ToolOutput, ToolFailure> {
        self.runs.fetch_add(1, Ordering::SeqCst);
        if cancel.load(Ordering::Acquire) {
            return Err(ToolFailure::NotStarted("cancelled before start".into()));
        }
        assert_eq!(request.args, br#"{"cmd":"true"}"#);
        assert_eq!(request.tool_name, "probe_tool");
        Ok(ToolOutput {
            output: b"tool-ok".to_vec(),
            backgrounded: false,
        })
    }
}

/// Blocking executor that ignores cooperative cancel until explicitly released,
/// proving that `request_cancel` does not drop the handle or release capacity.
struct UnstoppableBlockingTool {
    entered: Arc<AtomicBool>,
    release: Arc<AtomicBool>,
}

impl ToolPort for UnstoppableBlockingTool {
    fn execute(
        &self,
        _request: &ToolRequest,
        _cancel: Arc<AtomicBool>,
    ) -> Result<ToolOutput, ToolFailure> {
        self.entered.store(true, Ordering::Release);
        while !self.release.load(Ordering::Acquire) {
            std::thread::sleep(Duration::from_millis(5));
        }
        Err(ToolFailure::AfterStart(
            "blocking tool ignored cancel and exited with uncertain side effects".into(),
        ))
    }
}

struct ProbeOutcome {
    evidence: DispatchEvidence,
    identity: Option<EffectIdentity>,
}

impl FrameSize for ProbeOutcome {
    fn retained_bytes(&self) -> usize {
        std::mem::size_of::<Self>() + 128
    }
}

/// Placeholder for command/control lanes; the mailbox requires an explicit
/// retained-size for every lane it admits.
#[derive(Debug, Clone, PartialEq, Eq)]
struct ProbeInput(&'static str);

impl FrameSize for ProbeInput {
    fn retained_bytes(&self) -> usize {
        64
    }
}

fn tools_reserve<D: FrameSize>(
    handle: &MailboxHandle<ProbeInput, ProbeInput, D>,
) -> CompletionReservation<D> {
    handle
        .try_reserve_completion(60_000)
        .expect("reserve completion capacity")
}

fn synthetic_supervisor_checks() {
    let actor = ActorIdentity {
        session_id: "synthetic".into(),
        incarnation: 7,
    };
    let identity = |effect: &str| EffectIdentity {
        actor: actor.clone(),
        input_id: "input".into(),
        effect_id: effect.into(),
        generation: 1,
        intent_digest: [1u8; 32],
    };

    let mut supervisor: Supervisor<(), ()> = Supervisor::new(1).expect("supervisor");
    assert!(matches!(
        supervisor.authorize(identity("e1"), ()),
        Err(SupervisorError::StaleActor)
    ));
    supervisor.adopt_actor(&actor);
    assert!(supervisor.authorize(identity("e1"), ()).is_ok());
    assert!(matches!(
        supervisor.authorize(identity("e1"), ()),
        Err(SupervisorError::DuplicateEffect)
    ));
    assert!(matches!(
        supervisor.authorize(identity("e2"), ()),
        Err(SupervisorError::Capacity)
    ));

    assert!(supervisor.request_cancel(&identity("e1")).is_ok());
    assert_eq!(supervisor.authorized_len(), 0);
    assert!(matches!(
        supervisor.request_cancel(&identity("e1")),
        Err(SupervisorError::UnknownEffect)
    ));

    assert!(supervisor.authorize(identity("e3"), ()).is_ok());
    let newer = ActorIdentity {
        session_id: "synthetic".into(),
        incarnation: 8,
    };
    supervisor.adopt_actor(&newer);
    assert_eq!(supervisor.authorized_len(), 0);
    assert!(matches!(
        supervisor.authorize(identity("e4"), ()),
        Err(SupervisorError::StaleActor)
    ));
    println!("ok: [1] synthetic supervisor semantics");
}

/// 15 & 16: Batch commit point fault injection and Unknown reconciliation (A03).
fn probe_batch_fault_injection_and_reconciliation(root: &std::path::Path) {
    let dir = root.join("session-fault-a03");
    let (mut owner, _cap, _round_fact, turn_id) = mint_capability(&dir);

    // Complete round 1 so we can test fault injection on round 2.
    let resp_ref = owner
        .put_blob(&serde_json::to_vec(&text_message("assistant", "r1")).unwrap())
        .expect("blob");
    let head = owner.head().expect("head");
    owner
        .commit(
            "batch-r1-resp",
            head,
            vec![fact(
                FactPayload::ProviderResponseCommitted(ProviderResponseCommitted {
                    turn_id: turn_id.clone(),
                    round: 1,
                    attempt: 1,
                    message_ref: resp_ref,
                    opaque_refs: vec![],
                    usage: None,
                    finish: ProviderFinish::Stop,
                    incomplete_reason: None,
                }),
                20,
                Some(turn_id.clone()),
            )],
        )
        .expect("commit r1 response");

    // ── Fault 1: Crash after journal append, before commit.json marker ──────
    let head_before = owner.head().expect("head before fault 1");
    let r2_fact = fact(
        FactPayload::ModelRoundStarted(round_started(&owner, &turn_id, 2, head_before)),
        21,
        Some(turn_id.clone()),
    );
    let mut gate = CommitGate::<String>::default();
    gate.start("batch-fault-before-marker".into())
        .expect("start commit gate");
    owner.inject_fault_once(BatchFaultPoint::AfterJournalAppendBeforeMarker);
    let err = owner
        .commit_with_effect(
            "batch-fault-before-marker",
            head_before,
            vec![r2_fact.clone()],
            &event_id(21),
        )
        .err()
        .expect("injected fault before marker must return Unknown");
    assert!(matches!(err, SessionCommitError::Unknown(_)));
    gate.unknown(&"batch-fault-before-marker".to_string())
        .expect("gate transitions to Fenced");
    assert!(matches!(gate.state(), GateState::Fenced { .. }));
    assert!(matches!(owner.head(), Err(SessionCommitError::Fenced)));

    drop(owner);
    let mut reopened =
        SessionCommit::open(&dir, SessionId::new(SESSION), LogId::new(LOG), budget())
            .expect("reopen after torn journal tail");
    assert_eq!(reopened.head().expect("head"), head_before);
    assert!(
        reopened
            .lookup_batch("batch-fault-before-marker")
            .expect("lookup")
            .is_none(),
        "un-markered journal tail must be truncated, never promoted"
    );
    gate.reconciled(&"batch-fault-before-marker".to_string())
        .expect("gate reconciled");
    assert_eq!(gate.state(), &GateState::Ready);

    // Retry the exact batch after reconciliation: succeeds and mints capability once.
    let (receipt, cap) = reopened
        .commit_with_effect(
            "batch-fault-before-marker",
            head_before,
            vec![r2_fact.clone()],
            &event_id(21),
        )
        .expect("retry after torn tail truncation");
    assert!(
        cap.is_some(),
        "fresh commit after torn tail must mint capability"
    );
    assert_eq!(receipt.last_fact_seq, head_before + 1);
    println!("ok: [15] A03 fault before marker truncates torn tail and reconciles cleanly");

    // ── Fault 2: Crash after commit.json marker write (lost response) ───────
    let resp2_ref = reopened
        .put_blob(&serde_json::to_vec(&text_message("assistant", "r2")).unwrap())
        .expect("blob");
    let head_r2 = reopened.head().expect("head");
    let resp2_fact = fact(
        FactPayload::ProviderResponseCommitted(ProviderResponseCommitted {
            turn_id: turn_id.clone(),
            round: 2,
            attempt: 2,
            message_ref: resp2_ref,
            opaque_refs: vec![],
            usage: None,
            finish: ProviderFinish::Stop,
            incomplete_reason: None,
        }),
        22,
        Some(turn_id.clone()),
    );
    gate.start("batch-fault-after-marker".into())
        .expect("start gate");
    reopened.inject_fault_once(BatchFaultPoint::AfterMarkerWrite);
    let err = reopened
        .commit(
            "batch-fault-after-marker",
            head_r2,
            vec![resp2_fact.clone()],
        )
        .expect_err("injected fault after marker must return Unknown");
    assert!(matches!(err, SessionCommitError::Unknown(_)));
    gate.unknown(&"batch-fault-after-marker".to_string())
        .expect("gate fenced");

    drop(reopened);
    let mut reopened2 =
        SessionCommit::open(&dir, SessionId::new(SESSION), LogId::new(LOG), budget())
            .expect("reopen after marker write fault");
    let found = reopened2
        .lookup_batch("batch-fault-after-marker")
        .expect("lookup")
        .expect("marker succeeded on disk so receipt must exist");
    assert_eq!(found.last_fact_seq, head_r2 + 1);
    assert_eq!(reopened2.request_messages().expect("context").len(), 3);
    gate.reconciled(&"batch-fault-after-marker".to_string())
        .expect("gate reconciled");

    // Retrying the identical batch id + facts is idempotent and does not double-fold.
    let retry_receipt = reopened2
        .commit("batch-fault-after-marker", head_r2, vec![resp2_fact])
        .expect("idempotent retry of already-markered batch");
    assert_eq!(retry_receipt, found);
    assert_eq!(reopened2.request_messages().expect("context").len(), 3);

    // Reusing the same batch id with different content is rejected.
    let conflicting = fact(
        FactPayload::SessionTitleChanged(qaqh_session::session_fact_v2::SessionTitleChanged {
            title: "conflict".into(),
            source: qaqh_session::session_fact_v2::TitleSource::User,
            changed_at_ms: NOW_MS + 99,
        }),
        99,
        None,
    );
    assert!(matches!(
        reopened2.commit("batch-fault-after-marker", head_r2, vec![conflicting]),
        Err(SessionCommitError::NotCommitted(_))
    ));
    println!("ok: [16] A03 fault after marker reconciles idempotently without double fold");
}

/// 17: Side effect observed, terminal commit fails + duplicate/conflicting completion (A04).
async fn probe_side_effect_settlement_retry_and_completion_identity(root: &std::path::Path) {
    let dir = root.join("session-a04");
    let (mut owner, _cap, _round_fact, turn_id) = mint_capability(&dir);

    // Commit assistant tool_use response + ToolCallDeclared + ToolIntent.
    let call_id = ToolCallId::new("call_01J00000000000000000000030");
    let exec_id = ExecutionId::new("exec_01J00000000000000000000031");
    let args = br#"{"cmd":"true"}"#.to_vec();
    let args_ref = owner.put_blob(&args).expect("args blob");
    let args_hash = sha256_content_hash(&args);
    let assistant_msg = Message {
        msg_id: None,
        role: "assistant".into(),
        name: None,
        content: vec![ContentBlock::ToolUse {
            id: call_id.as_str().to_owned(),
            name: "probe_tool".into(),
            input: serde_json::json!({"cmd": "true"}),
        }],
    };
    let msg_ref = owner
        .put_blob(&serde_json::to_vec(&assistant_msg).unwrap())
        .expect("msg blob");
    let mut declared = fact(
        FactPayload::ToolCallDeclared(ToolCallDeclared {
            turn_id: turn_id.clone(),
            call_id: call_id.clone(),
            tool_name: "probe_tool".into(),
            args_ref: args_ref.clone(),
            args_hash: args_hash.clone(),
        }),
        31,
        Some(turn_id.clone()),
    );
    declared.call_id = Some(call_id.clone());
    let mut intent = fact(
        FactPayload::ToolIntent(ToolIntent {
            call_id: call_id.clone(),
            execution_id: exec_id.clone(),
            idempotency_key: None,
            replay_capability: ToolReplayCapability::NoReplay,
            policy_decision: PolicyDecisionRef {
                outcome: ToolIntentPolicyOutcome::Allow,
                rule_id: "probe.allow".into(),
                decided_at_ms: NOW_MS + 32,
                reason_ref: None,
            },
            effective_args_ref: Some(args_ref),
            effective_args_hash: Some(args_hash),
            sandbox_spec_hash: digest("sandbox:readonly"),
            side_effect_class: SideEffectClass::WorkspaceWrite,
            intent_at_ms: NOW_MS + 32,
        }),
        32,
        Some(turn_id.clone()),
    );
    intent.call_id = Some(call_id.clone());
    let head = owner.head().expect("head");
    let (_, tool_cap) = owner
        .commit_with_effect(
            "batch-a04-intent",
            head,
            vec![
                fact(
                    FactPayload::ProviderResponseCommitted(ProviderResponseCommitted {
                        turn_id: turn_id.clone(),
                        round: 1,
                        attempt: 1,
                        message_ref: msg_ref,
                        opaque_refs: vec![],
                        usage: None,
                        finish: ProviderFinish::ToolUse,
                        incomplete_reason: None,
                    }),
                    30,
                    Some(turn_id.clone()),
                ),
                declared,
                intent,
            ],
            &event_id(32),
        )
        .expect("commit tool intent");
    let tool_cap = tool_cap.expect("mint tool capability");
    let tool_identity = tool_cap.identity().clone();

    let runs = Arc::new(AtomicUsize::new(0));
    let port = Arc::new(FakeTool {
        runs: Arc::clone(&runs),
    });
    let (handle, mut mailbox) =
        Mailbox::<ProbeInput, ProbeInput, TaskCompletion<ToolOutcome>>::new(
            LaneBudget::COMMANDS,
            LaneBudget::CONTROL,
            LaneBudget::COMPLETIONS,
        )
        .expect("mailbox");
    let mut supervisor: Supervisor<ToolOutcome, CommittedExecution> =
        Supervisor::new(1).expect("supervisor");
    supervisor.adopt_actor(&tool_identity.actor);
    let ticket = supervisor
        .authorize(tool_identity.clone(), tool_cap)
        .expect("authorize");
    let p = Arc::clone(&port);
    supervisor
        .start(ticket, tools_reserve(&handle), move |eff, cancel| {
            tool_job::run(p, "probe_tool".into(), eff, cancel)
        })
        .expect("start");
    let completion = loop {
        match supervisor.collect(&tool_identity).await {
            Ok(c) => break c,
            Err(SupervisorError::NotFinished) => tokio::task::yield_now().await,
            Err(e) => panic!("collect: {e:?}"),
        }
    };
    supervisor.deliver(&tool_identity).expect("deliver");
    let _ = mailbox.recv().await.expect("first delivery");
    assert_eq!(runs.load(Ordering::SeqCst), 1);

    // Terminal commit fails BEFORE marker write; supervisor has NOT acknowledged!
    let out_ref = owner.put_blob(b"tool-ok").expect("output blob");
    let mut finished_fact = fact(
        FactPayload::ToolFinished(ToolFinished {
            call_id: call_id.clone(),
            execution_id: Some(exec_id.clone()),
            terminal_status: ToolTerminalStatus::Succeeded,
            output_ref: Some(out_ref),
            error: None,
            metrics: ToolMetrics {
                started_at_ms: NOW_MS + 32,
                finished_at_ms: NOW_MS + 33,
                retry_count: 0,
                output_bytes: 7,
                progress_bytes_total: 7,
            },
            reconciled: false,
            evidence_ref: None,
            evidence_fact_seq: None,
            evidence_event_id: None,
            recovery_ref: None,
            finished_at_ms: NOW_MS + 33,
        }),
        33,
        Some(turn_id.clone()),
    );
    finished_fact.call_id = Some(call_id);
    let head = owner.head().expect("head");
    owner.inject_fault_once(BatchFaultPoint::AfterJournalAppendBeforeMarker);
    assert!(matches!(
        owner.commit("batch-a04-finish", head, vec![finished_fact.clone()]),
        Err(SessionCommitError::Unknown(_))
    ));

    // Reopen SessionCommit and redeliver the retained completion from Supervisor
    // without re-executing the tool (`runs` stays 1).
    drop(owner);
    let mut reopened =
        SessionCommit::open(&dir, SessionId::new(SESSION), LogId::new(LOG), budget())
            .expect("reopen");
    supervisor
        .redeliver(&tool_identity, tools_reserve(&handle))
        .expect("redeliver retained completion");
    let redelivered = match mailbox.recv().await.expect("redelivered frame") {
        Input::Completion(c) => c,
        _ => panic!("expected completion"),
    };
    assert!(Arc::ptr_eq(&redelivered, &completion));
    reopened
        .commit("batch-a04-finish", head, vec![finished_fact])
        .expect("settle retained tool result after reopen");
    supervisor
        .acknowledge(&tool_identity, &completion)
        .expect("acknowledge after durable settlement");
    assert_eq!(
        runs.load(Ordering::SeqCst),
        1,
        "tool must never be re-executed when retrying terminal settlement"
    );

    // Completion identity matching: stale incarnation, wrong generation, wrong digest.
    let mut stale = tool_identity.clone();
    stale.actor.incarnation += 1;
    assert_eq!(
        stale.match_expected(&tool_identity),
        CompletionMatch::StaleIncarnation
    );
    let mut wrong_gen = tool_identity.clone();
    wrong_gen.generation += 1;
    assert_eq!(
        wrong_gen.match_expected(&tool_identity),
        CompletionMatch::DifferentGeneration
    );
    let mut wrong_digest = tool_identity.clone();
    wrong_digest.intent_digest[0] ^= 0xff;
    assert_eq!(
        wrong_digest.match_expected(&tool_identity),
        CompletionMatch::DifferentIntent
    );
    println!(
        "ok: [17] A04 terminal commit failure retries settlement without re-running tool; conflicting completions rejected"
    );
}

/// 18: Mid-turn crash recovery (`SessionRecovered` + `TurnInterrupted` + `ToolFinished(Indeterminate)` + `InteractionExpired`).
fn probe_mid_turn_crash_recovery(root: &std::path::Path) {
    let dir = root.join("session-recovery");
    let (mut owner, _cap, _round_fact, turn_id) = mint_capability(&dir);

    let call_id = ToolCallId::new("call_01J00000000000000000000040");
    let exec_id = ExecutionId::new("exec_01J00000000000000000000041");
    let int_id = InteractionId::new("int_01J00000000000000000000042");
    let args = br#"{"path":"out.txt","content":"v1"}"#.to_vec();
    let args_ref = owner.put_blob(&args).expect("args blob");
    let args_hash = sha256_content_hash(&args);
    let req_ref = owner.put_blob(b"ask permission").expect("req blob");

    let assistant_msg = Message {
        msg_id: None,
        role: "assistant".into(),
        name: None,
        content: vec![ContentBlock::ToolUse {
            id: call_id.as_str().to_owned(),
            name: "write".into(),
            input: serde_json::json!({"path": "out.txt", "content": "v1"}),
        }],
    };
    let msg_ref = owner
        .put_blob(&serde_json::to_vec(&assistant_msg).unwrap())
        .expect("assistant msg blob");

    let mut declared = fact(
        FactPayload::ToolCallDeclared(ToolCallDeclared {
            turn_id: turn_id.clone(),
            call_id: call_id.clone(),
            tool_name: "write".into(),
            args_ref: args_ref.clone(),
            args_hash: args_hash.clone(),
        }),
        41,
        Some(turn_id.clone()),
    );
    declared.call_id = Some(call_id.clone());

    let mut intent = fact(
        FactPayload::ToolIntent(ToolIntent {
            call_id: call_id.clone(),
            execution_id: exec_id.clone(),
            idempotency_key: None,
            replay_capability: ToolReplayCapability::NoReplay,
            policy_decision: PolicyDecisionRef {
                outcome: ToolIntentPolicyOutcome::Allow,
                rule_id: "probe.allow".into(),
                decided_at_ms: NOW_MS + 42,
                reason_ref: None,
            },
            effective_args_ref: Some(args_ref),
            effective_args_hash: Some(args_hash),
            sandbox_spec_hash: digest("sandbox:write"),
            side_effect_class: SideEffectClass::WorkspaceWrite,
            intent_at_ms: NOW_MS + 42,
        }),
        42,
        Some(turn_id.clone()),
    );
    intent.call_id = Some(call_id.clone());

    let mut interaction = fact(
        FactPayload::InteractionRequested(InteractionRequested {
            interaction_id: int_id.clone(),
            call_id: Some(call_id.clone()),
            turn_id: turn_id.clone(),
            kind: InteractionKind::Permission,
            request_ref: req_ref,
            choices: Vec::new(),
            expires_at_ms: None,
            requested_at_ms: NOW_MS + 43,
        }),
        43,
        Some(turn_id.clone()),
    );
    interaction.interaction_id = Some(int_id.clone());
    interaction.call_id = Some(call_id.clone());

    let head = owner.head().expect("head");
    owner
        .commit(
            "batch-crash-mid-turn",
            head,
            vec![
                fact(
                    FactPayload::ProviderResponseCommitted(ProviderResponseCommitted {
                        turn_id: turn_id.clone(),
                        round: 1,
                        attempt: 1,
                        message_ref: msg_ref,
                        opaque_refs: vec![],
                        usage: None,
                        finish: ProviderFinish::ToolUse,
                        incomplete_reason: None,
                    }),
                    40,
                    Some(turn_id.clone()),
                ),
                declared,
                intent,
                interaction,
            ],
        )
        .expect("commit mid-turn state before crash");

    // Simulate crash by dropping owner and reopening.
    drop(owner);
    let mut reopened =
        SessionCommit::open(&dir, SessionId::new(SESSION), LogId::new(LOG), budget())
            .expect("reopen crashed session");
    let obligations = reopened.obligations().expect("obligations");
    assert!(obligations.has_open_work());
    assert_eq!(obligations.active_turn.as_ref(), Some(&turn_id));
    assert_eq!(obligations.unsettled_tools.len(), 1);
    assert_eq!(obligations.unsettled_tools[0].call_id, call_id);
    assert_eq!(
        obligations.unsettled_tools[0].execution_id.as_ref(),
        Some(&exec_id)
    );
    assert_eq!(obligations.unsettled_interactions.len(), 1);
    assert_eq!(obligations.unsettled_interactions[0].interaction_id, int_id);

    let last_good_seq = reopened.head().expect("head");
    let recovery_ref = RecoveryRef {
        recovery_id: RecoveryId::new("recovery_01J00000000000000000000049"),
        recovery_event_id: EventId::new(event_id(48)),
        recovery_input_fingerprint: digest("recovery:mid-turn"),
    };

    // Sealing SessionRecovered before settling open obligations must be rejected.
    let premature_recovered = fact(
        FactPayload::SessionRecovered(SessionRecovered {
            recovery_id: recovery_ref.recovery_id.clone(),
            recovery_event_id: recovery_ref.recovery_event_id.clone(),
            recovery_input_fingerprint: recovery_ref.recovery_input_fingerprint.clone(),
            outcome: RecoveryOutcome::Writable,
            last_good_fact_seq: last_good_seq,
            torn_tail: false,
            torn_bytes: None,
            actions: vec![],
            recovered_at_ms: NOW_MS + 48,
        }),
        48,
        None,
    );
    assert!(
        reopened
            .commit(
                "batch-premature-recovery",
                last_good_seq,
                vec![premature_recovered]
            )
            .is_err(),
        "SessionRecovered must fail while turn/tools/interactions remain unsettled"
    );

    // Build the complete recovery batch:
    // 1. ToolFinished(Indeterminate) with recovery_ref
    // 2. ContextMessageCommitted(ToolResult) so the assistant tool_use has a matching result
    // 3. InteractionExpired(RestartPolicy) with recovery_ref
    // 4. TurnInterrupted(Restart) with recovery_ref
    // 5. SessionRecovered summarizing all actions
    let tool_err = ToolError {
        code: "recovery_indeterminate".into(),
        message: "process restarted while write tool was in flight".into(),
        retryable: false,
        details_ref: None,
    };
    let metrics = ToolMetrics {
        started_at_ms: NOW_MS + 42,
        finished_at_ms: NOW_MS + 44,
        retry_count: 0,
        output_bytes: 0,
        progress_bytes_total: 0,
    };
    let mut tool_finished = fact(
        FactPayload::ToolFinished(ToolFinished {
            call_id: call_id.clone(),
            execution_id: Some(exec_id.clone()),
            terminal_status: ToolTerminalStatus::Indeterminate,
            output_ref: None,
            error: Some(tool_err.clone()),
            metrics: metrics.clone(),
            reconciled: false,
            evidence_ref: None,
            evidence_fact_seq: None,
            evidence_event_id: None,
            recovery_ref: Some(recovery_ref.clone()),
            finished_at_ms: NOW_MS + 44,
        }),
        44,
        Some(turn_id.clone()),
    );
    tool_finished.call_id = Some(call_id.clone());

    let tool_result_msg = Message {
        msg_id: None,
        role: "tool".into(),
        name: None,
        content: vec![ContentBlock::ToolResult {
            tool_use_id: call_id.as_str().to_owned(),
            result: ToolResult::error("indeterminate after crash recovery"),
        }],
    };
    let tool_result_ref = reopened
        .put_blob(&serde_json::to_vec(&tool_result_msg).unwrap())
        .expect("tool result blob");
    let tool_context = fact(
        FactPayload::ContextMessageCommitted(ContextMessageCommitted {
            message_ref: tool_result_ref,
            origin: ContextMessageOrigin::ToolResult,
        }),
        45,
        Some(turn_id.clone()),
    );

    let mut int_expired = fact(
        FactPayload::InteractionExpired(InteractionExpired {
            interaction_id: int_id.clone(),
            reason: InteractionExpiryReason::RestartPolicy,
            recovery_ref: Some(recovery_ref.clone()),
            expired_at_ms: NOW_MS + 46,
        }),
        46,
        Some(turn_id.clone()),
    );
    int_expired.interaction_id = Some(int_id.clone());

    let interrupted = fact(
        FactPayload::TurnInterrupted(TurnInterrupted {
            turn_id: turn_id.clone(),
            reason: InterruptReason::Restart,
            last_fact_seq: last_good_seq,
            recovery_ref: recovery_ref.clone(),
        }),
        47,
        Some(turn_id.clone()),
    );

    let recovered = fact(
        FactPayload::SessionRecovered(SessionRecovered {
            recovery_id: recovery_ref.recovery_id.clone(),
            recovery_event_id: recovery_ref.recovery_event_id.clone(),
            recovery_input_fingerprint: recovery_ref.recovery_input_fingerprint.clone(),
            outcome: RecoveryOutcome::Writable,
            last_good_fact_seq: last_good_seq,
            torn_tail: false,
            torn_bytes: None,
            actions: vec![
                RecoveryAction::ToolFinished {
                    completion: RecoveryToolCompletion {
                        call_id,
                        execution_id: Some(exec_id),
                        terminal_status: ToolTerminalStatus::Indeterminate,
                        output_ref: None,
                        error: Some(tool_err),
                        metrics,
                        reconciled: false,
                        recovery_ref,
                        finished_at_ms: NOW_MS + 44,
                        evidence_ref: None,
                        evidence_fact_seq: None,
                        evidence_event_id: None,
                    },
                },
                RecoveryAction::InteractionExpired {
                    interaction_id: int_id,
                    reason: InteractionExpiryReason::RestartPolicy,
                },
                RecoveryAction::TurnInterrupted {
                    turn_id,
                    last_fact_seq: last_good_seq,
                },
            ],
            recovered_at_ms: NOW_MS + 48,
        }),
        48,
        None,
    );

    reopened
        .commit(
            "batch-recovery-seal",
            last_good_seq,
            vec![
                tool_finished,
                tool_context,
                int_expired,
                interrupted,
                recovered,
            ],
        )
        .expect("commit recovery batch");

    let after = reopened.obligations().expect("obligations after recovery");
    assert!(!after.has_open_work(), "all open work must be settled");
    assert_eq!(reopened.request_messages().expect("valid context").len(), 3);
    println!(
        "ok: [18] A04/A05 mid-turn crash recovery settles Indeterminate tool, expires interaction, interrupts turn, and seals SessionRecovered"
    );
}

/// 19: Mailbox control under command load + blocking task ignoring cancel (A02/A06).
async fn probe_mailbox_control_and_unstoppable_blocking_task(root: &std::path::Path) {
    let dir = root.join("session-a02-a06");
    let (mut owner, _cap, _round_fact, turn_id) = mint_capability(&dir);

    let call_id = ToolCallId::new("call_01J00000000000000000000050");
    let exec_id = ExecutionId::new("exec_01J00000000000000000000051");
    let args = br#"{"cmd":"hang"}"#.to_vec();
    let args_ref = owner.put_blob(&args).expect("blob");
    let args_hash = sha256_content_hash(&args);
    let resp_ref = owner
        .put_blob(&serde_json::to_vec(&text_message("assistant", "ok")).unwrap())
        .expect("blob");
    let mut declared = fact(
        FactPayload::ToolCallDeclared(ToolCallDeclared {
            turn_id: turn_id.clone(),
            call_id: call_id.clone(),
            tool_name: "hang_tool".into(),
            args_ref: args_ref.clone(),
            args_hash: args_hash.clone(),
        }),
        51,
        Some(turn_id.clone()),
    );
    declared.call_id = Some(call_id.clone());
    let mut intent = fact(
        FactPayload::ToolIntent(ToolIntent {
            call_id: call_id.clone(),
            execution_id: exec_id,
            idempotency_key: None,
            replay_capability: ToolReplayCapability::NoReplay,
            policy_decision: PolicyDecisionRef {
                outcome: ToolIntentPolicyOutcome::Allow,
                rule_id: "probe.allow".into(),
                decided_at_ms: NOW_MS + 52,
                reason_ref: None,
            },
            effective_args_ref: Some(args_ref.clone()),
            effective_args_hash: Some(args_hash.clone()),
            sandbox_spec_hash: digest("sandbox:exec"),
            side_effect_class: SideEffectClass::Process,
            intent_at_ms: NOW_MS + 52,
        }),
        52,
        Some(turn_id.clone()),
    );
    intent.call_id = Some(call_id);
    let head = owner.head().expect("head");
    let (_, cap) = owner
        .commit_with_effect(
            "batch-a06",
            head,
            vec![
                fact(
                    FactPayload::ProviderResponseCommitted(ProviderResponseCommitted {
                        turn_id: turn_id.clone(),
                        round: 1,
                        attempt: 1,
                        message_ref: resp_ref,
                        opaque_refs: vec![],
                        usage: None,
                        finish: ProviderFinish::Stop,
                        incomplete_reason: None,
                    }),
                    50,
                    Some(turn_id),
                ),
                declared,
                intent,
            ],
            &event_id(52),
        )
        .expect("commit hang tool intent");
    let cap = cap.expect("capability");
    let identity = cap.identity().clone();

    let cmd_budget = LaneBudget {
        items: 2,
        bytes: 256,
        max_frame_bytes: 128,
    };
    let (handle, mut mailbox) =
        Mailbox::<ProbeInput, ProbeInput, TaskCompletion<ToolOutcome>>::new(
            cmd_budget,
            LaneBudget::CONTROL,
            LaneBudget::COMPLETIONS,
        )
        .expect("mailbox");

    // Fill the command lane completely.
    handle.try_command(ProbeInput("cmd-1")).expect("cmd 1");
    handle.try_command(ProbeInput("cmd-2")).expect("cmd 2");
    assert!(handle.try_command(ProbeInput("cmd-overflow")).is_err());

    let entered = Arc::new(AtomicBool::new(false));
    let release = Arc::new(AtomicBool::new(false));
    let port = Arc::new(UnstoppableBlockingTool {
        entered: Arc::clone(&entered),
        release: Arc::clone(&release),
    });

    let mut supervisor: Supervisor<ToolOutcome, CommittedExecution> =
        Supervisor::new(1).expect("supervisor");
    supervisor.adopt_actor(&identity.actor);
    let ticket = supervisor
        .authorize(identity.clone(), cap)
        .expect("authorize");
    let p = Arc::clone(&port);
    supervisor
        .start(ticket, tools_reserve(&handle), move |eff, cancel| {
            tool_job::run(p, "hang_tool".into(), eff, cancel)
        })
        .expect("start hang tool");

    while !entered.load(Ordering::Acquire) {
        tokio::task::yield_now().await;
    }

    // Even with the command lane full and the blocking task running, control is admitted
    // and delivered without waiting for the blocking task to finish.
    handle
        .try_control(ProbeInput("cancel-now"))
        .expect("control lane independent of full command lane");
    let first = mailbox.recv().await.expect("first frame");
    let second = mailbox.recv().await.expect("second frame");
    assert!(
        matches!(first, Input::Control(ProbeInput("cancel-now")))
            || matches!(second, Input::Control(ProbeInput("cancel-now"))),
        "control frame must be serviced while blocking task is still running"
    );

    // Request cancellation; the blocking worker ignores it until `release` is set.
    supervisor
        .request_cancel(&identity)
        .expect("request cancel on running task");
    assert!(
        matches!(
            supervisor.collect(&identity).await,
            Err(SupervisorError::NotFinished)
        ),
        "request_cancel must not pretend an unstoppable blocking task has finished"
    );

    // Release the blocking worker and verify it reports StartedUnknown (never fake Cancelled).
    release.store(true, Ordering::Release);
    let completion = loop {
        match supervisor.collect(&identity).await {
            Ok(c) => break c,
            Err(SupervisorError::NotFinished) => tokio::task::yield_now().await,
            Err(e) => panic!("collect: {e:?}"),
        }
    };
    supervisor.deliver(&identity).expect("deliver");
    // Drain the remaining command frame and the completion frame.
    let mut saw_completion = false;
    while !saw_completion {
        if let Some(Input::Completion(c)) = mailbox.recv().await {
            match &c.outcome {
                TaskOutcome::Returned(outcome) => {
                    assert_eq!(outcome.evidence, DispatchEvidence::StartedUnknown);
                    assert!(outcome.terminal.is_none());
                }
                TaskOutcome::JoinFailed(e) => panic!("join failed: {e}"),
            }
            saw_completion = true;
        }
    }
    supervisor
        .acknowledge(&identity, &completion)
        .expect("acknowledge after settlement");
    println!(
        "ok: [19] A02/A06 control lane serviced under full command lane; blocking task ignoring cancel keeps slot until exit and reports StartedUnknown"
    );
}

/// 20: RequestIdentity wire verification + incomplete response barrier + multimodal context (A07).
async fn probe_request_identity_incomplete_barrier_and_multimodal(root: &std::path::Path) {
    let dir = root.join("session-a07-identity");
    let mut owner = SessionCommit::create(&dir, SessionId::new(SESSION), LogId::new(LOG), budget())
        .expect("create session");
    owner
        .commit(
            "b-1",
            0,
            vec![fact(
                FactPayload::SessionCreated(SessionCreated {
                    created_at_ms: NOW_MS + 1,
                    cwd: "/tmp".into(),
                    model: "probe-model".into(),
                    parent_session_id: None,
                    schema_caps: vec![],
                }),
                1,
                None,
            )],
        )
        .expect("created");
    let input_id = InputId::new("input_01J00000000000000000000060");
    let turn_id = TurnId::new("turn_01J00000000000000000000061");
    owner
        .commit(
            "b-2",
            1,
            vec![
                fact(
                    FactPayload::InputAccepted(InputAccepted {
                        input_id: input_id.clone(),
                        input_kind: InputKind::UserText,
                        input_purpose: InputPurpose::TriggerTurn,
                        content_ref: None,
                        inline_text: Some("see image".into()),
                        attachments: vec![],
                        actor: ActorRef {
                            kind: ActorKind::User,
                            id: "u".into(),
                            display_name: None,
                        },
                        client_request_id: None,
                    }),
                    2,
                    None,
                ),
                fact(
                    FactPayload::TurnStarted(TurnStarted {
                        turn_id: turn_id.clone(),
                        input_id,
                        mode: TurnMode::Normal,
                        recovery_ref: None,
                    }),
                    3,
                    Some(turn_id.clone()),
                ),
            ],
        )
        .expect("start turn");

    // Unscoped ImageRef must be rejected by ContextProjector.
    let bad_img_msg = Message {
        msg_id: None,
        role: "user".into(),
        name: None,
        content: vec![ContentBlock::ImageRef {
            sha256: "a".repeat(64),
            mime_type: "image/png".into(),
            bytes_len: 16,
        }],
    };
    let bad_ref = owner
        .put_blob(&serde_json::to_vec(&bad_img_msg).unwrap())
        .expect("bad img blob");
    assert!(
        owner
            .commit(
                "b-bad-img",
                3,
                vec![fact(
                    FactPayload::ContextMessageCommitted(ContextMessageCommitted {
                        message_ref: bad_ref,
                        origin: ContextMessageOrigin::Input,
                    }),
                    4,
                    Some(turn_id.clone()),
                )],
            )
            .is_err(),
        "unscoped ImageRef must be rejected"
    );

    // Self-contained inline ContentBlock::Image is accepted and preserved.
    let multimodal_user = Message {
        msg_id: None,
        role: "user".into(),
        name: None,
        content: vec![
            ContentBlock::text("inspect pixel"),
            ContentBlock::image(
                "image/png",
                "iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAQAAAC1HAwCAAAAC0lEQVR42mNk+A8AAQUBAScY42YAAAAASUVORK5CYII=",
            ),
        ],
    };
    let mm_ref = owner
        .put_blob(&serde_json::to_vec(&multimodal_user).unwrap())
        .expect("mm blob");
    owner
        .commit(
            "b-3",
            3,
            vec![fact(
                FactPayload::ContextMessageCommitted(ContextMessageCommitted {
                    message_ref: mm_ref,
                    origin: ContextMessageOrigin::Input,
                }),
                4,
                Some(turn_id.clone()),
            )],
        )
        .expect("commit multimodal user message");

    let provider = qaqh_gate::ProviderConfig::chat(
        "http://127.0.0.1:9",
        "key",
        "probe-model",
        None,
        None,
        qaqh_types::ThinkingParamMode::OpenAi,
        qaqh_types::CacheTokenField::PromptCacheHitTokens,
        true,
        None,
    );
    let spec = ProviderRequestSpec::new(provider.clone(), 128).with_effort("high");
    let round_payload =
        build_model_round_started(&owner, turn_id.clone(), 1, 1, &spec).expect("build round");
    let head = owner.head().expect("head");
    let (_, cap) = owner
        .commit_with_effect(
            "b-4",
            head,
            vec![fact(
                FactPayload::ModelRoundStarted(round_payload),
                5,
                Some(turn_id.clone()),
            )],
            &event_id(5),
        )
        .expect("commit model round with wire RequestIdentity");
    let cap = cap.expect("capability");

    // Mutating max_tokens (or tools/model) between commit and dispatch is caught
    // by `run_verified` before touching the network -> DispatchEvidence::NotStarted.
    let tampered_spec = ProviderRequestSpec::new(provider, 256).with_effort("high");
    let (_tx, rx) = tokio::sync::watch::channel(false);
    let outcome = provider_job::run_verified(tampered_spec, cap, rx).await;
    assert_eq!(outcome.evidence, DispatchEvidence::NotStarted);

    // Commit an incomplete response (ProviderFinish::Length) and verify that
    // ToolCallDeclared, ToolIntent, and ModelRoundStarted are all blocked until TurnInterrupted.
    let partial_ref = owner
        .put_blob(&serde_json::to_vec(&text_message("assistant", "truncated...")).unwrap())
        .expect("partial blob");
    let head = owner.head().expect("head");
    owner
        .commit(
            "b-5",
            head,
            vec![fact(
                FactPayload::ProviderResponseCommitted(ProviderResponseCommitted {
                    turn_id: turn_id.clone(),
                    round: 1,
                    attempt: 1,
                    message_ref: partial_ref,
                    opaque_refs: vec![],
                    usage: None,
                    finish: ProviderFinish::Length,
                    incomplete_reason: Some("max_tokens reached".into()),
                }),
                6,
                Some(turn_id.clone()),
            )],
        )
        .expect("commit incomplete response");

    let args = br#"{"path":"a"}"#.to_vec();
    let args_ref = owner.put_blob(&args).expect("args");
    let args_hash = sha256_content_hash(&args);
    let call_id = ToolCallId::new("call_01J00000000000000000000062");
    let mut bad_declare = fact(
        FactPayload::ToolCallDeclared(ToolCallDeclared {
            turn_id: turn_id.clone(),
            call_id: call_id.clone(),
            tool_name: "read".into(),
            args_ref,
            args_hash,
        }),
        7,
        Some(turn_id.clone()),
    );
    bad_declare.call_id = Some(call_id);
    let head = owner.head().expect("head");
    assert!(
        owner.commit("b-bad-tool", head, vec![bad_declare]).is_err(),
        "ToolCallDeclared must be rejected after an incomplete provider response"
    );
    assert!(
        owner
            .commit(
                "b-bad-finish",
                head,
                vec![fact(
                    FactPayload::TurnFinished(TurnFinished {
                        turn_id: turn_id.clone(),
                        terminal: TurnTerminal::Completed,
                        usage: None,
                        error: None,
                        finished_at_ms: NOW_MS + 8,
                    }),
                    8,
                    Some(turn_id.clone()),
                )],
            )
            .is_err(),
        "TurnFinished must be rejected on a damaged turn"
    );

    // Explicit TurnInterrupted clears the damaged turn and preserves only the complete prefix.
    owner
        .commit(
            "b-6",
            head,
            vec![fact(
                FactPayload::TurnInterrupted(TurnInterrupted {
                    turn_id: turn_id.clone(),
                    reason: InterruptReason::CancelBeforeSeal,
                    last_fact_seq: head,
                    recovery_ref: RecoveryRef {
                        recovery_id: RecoveryId::new("recovery_01J00000000000000000000069"),
                        recovery_event_id: EventId::new(event_id(9)),
                        recovery_input_fingerprint: digest("incomplete-turn"),
                    },
                }),
                9,
                Some(turn_id),
            )],
        )
        .expect("interrupt damaged turn");
    let msgs = owner.request_messages().expect("context after interrupt");
    assert_eq!(
        msgs.len(),
        1,
        "incomplete assistant response must not enter replayable context"
    );
    println!(
        "ok: [20] A07 RequestIdentity mismatch refused before wire; multimodal validated; incomplete response blocks tools/rounds until TurnInterrupted"
    );
}

/// 21: Four provider protocols (Chat, Responses with opaque_refs, Messages, Gemini) + restart golden (A07).
async fn probe_four_provider_protocols_and_opaque_replay_golden(root: &std::path::Path) {
    let captured_requests: Arc<Mutex<Vec<(String, serde_json::Value)>>> =
        Arc::new(Mutex::new(Vec::new()));
    let captured_for_server = Arc::clone(&captured_requests);

    let server = tiny_http::Server::http("127.0.0.1:0").expect("bind multi-protocol server");
    let base_url = format!("http://{}", server.server_addr());
    std::thread::spawn(move || {
        for mut request in server.incoming_requests() {
            let url = request.url().to_string();
            let mut body = String::new();
            let _ = request.as_reader().read_to_string(&mut body);
            let parsed =
                serde_json::from_str::<serde_json::Value>(&body).unwrap_or(serde_json::Value::Null);
            captured_for_server
                .lock()
                .unwrap()
                .push((url.clone(), parsed.clone()));

            let sse = if url.contains("/chat/completions") {
                CHAT_TOOL_SSE
            } else if url.contains("/responses") {
                // Distinguish Responses Round 1 vs Round 2 by checking if its call_id output is in `input`.
                let has_tool_output = body.contains("call_01J00000000000000000000201");
                if has_tool_output {
                    RESPONSES_ROUND2_SSE
                } else {
                    RESPONSES_ROUND1_SSE
                }
            } else if url.contains("/v1/messages") {
                MESSAGES_SSE
            } else if url.contains(":streamGenerateContent") {
                GEMINI_SSE
            } else {
                CHAT_SSE
            };
            let header =
                tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"text/event-stream"[..])
                    .expect("header");
            let _ = request.respond(tiny_http::Response::from_string(sse).with_header(header));
        }
    });

    let dir = root.join("session-four-providers");
    let mut owner = SessionCommit::create(&dir, SessionId::new(SESSION), LogId::new(LOG), budget())
        .expect("create session");
    let frozen_sys = text_message("system", "Frozen system prompt v1");
    let sys_ref = owner
        .put_blob(&serde_json::to_vec(&frozen_sys).unwrap())
        .expect("sys blob");
    let input_id = InputId::new("input_01J00000000000000000000070");
    let turn_id = TurnId::new("turn_01J00000000000000000000071");
    let user_ref = owner
        .put_blob(&serde_json::to_vec(&text_message("user", "run all four")).unwrap())
        .expect("user blob");

    owner
        .commit(
            "p-init",
            0,
            vec![
                fact(
                    FactPayload::SessionCreated(SessionCreated {
                        created_at_ms: NOW_MS + 1,
                        cwd: "/tmp".into(),
                        model: "multi-model".into(),
                        parent_session_id: None,
                        schema_caps: vec![],
                    }),
                    1,
                    None,
                ),
                fact(
                    FactPayload::ContextMessageCommitted(ContextMessageCommitted {
                        message_ref: sys_ref,
                        origin: ContextMessageOrigin::FrozenPrefix,
                    }),
                    2,
                    None,
                ),
                fact(
                    FactPayload::InputAccepted(InputAccepted {
                        input_id: input_id.clone(),
                        input_kind: InputKind::UserText,
                        input_purpose: InputPurpose::TriggerTurn,
                        content_ref: None,
                        inline_text: Some("run all four".into()),
                        attachments: vec![],
                        actor: ActorRef {
                            kind: ActorKind::User,
                            id: "u".into(),
                            display_name: None,
                        },
                        client_request_id: None,
                    }),
                    3,
                    None,
                ),
                fact(
                    FactPayload::TurnStarted(TurnStarted {
                        turn_id: turn_id.clone(),
                        input_id,
                        mode: TurnMode::Normal,
                        recovery_ref: None,
                    }),
                    4,
                    Some(turn_id.clone()),
                ),
                fact(
                    FactPayload::ContextMessageCommitted(ContextMessageCommitted {
                        message_ref: user_ref,
                        origin: ContextMessageOrigin::Input,
                    }),
                    5,
                    Some(turn_id.clone()),
                ),
            ],
        )
        .expect("commit init");

    let read_tool_def = ToolDef {
        call_type: "function".into(),
        function: ToolFunction {
            name: "read".into(),
            description: "Read file".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {"path": {"type": "string"}},
                "required": ["path"]
            }),
        },
    };

    // ── 1) OpenAI Chat Completions (reasoning + tool_calls + usage) ─────────
    let chat_provider = qaqh_gate::ProviderConfig::chat(
        &base_url,
        "sk-chat",
        "gpt-probe",
        None,
        Some("/v1/chat/completions".into()),
        qaqh_types::ThinkingParamMode::OpenAi,
        qaqh_types::CacheTokenField::PromptCacheHitTokens,
        true,
        None,
    )
    .with_stream_usage(true);
    let chat_spec =
        ProviderRequestSpec::new(chat_provider, 128).with_tools(vec![read_tool_def.clone()]);
    let r1 = build_model_round_started(&owner, turn_id.clone(), 1, 1, &chat_spec).unwrap();
    let head = owner.head().unwrap();
    let (_, cap1) = owner
        .commit_with_effect(
            "p-r1",
            head,
            vec![fact(
                FactPayload::ModelRoundStarted(r1),
                6,
                Some(turn_id.clone()),
            )],
            &event_id(6),
        )
        .unwrap();
    let (_tx, rx) = tokio::sync::watch::channel(false);
    let out1 = provider_job::run_verified(chat_spec, cap1.unwrap(), rx).await;
    assert_eq!(out1.evidence, DispatchEvidence::StartedTerminal);
    assert_eq!(out1.finish, Some(ProviderFinish::ToolUse));
    assert_eq!(out1.usage.as_ref().unwrap().total_tokens, 28);
    let r1_facts = prepare_response_commit_facts(
        &owner,
        &turn_id,
        1,
        1,
        "test-model",
        None,
        &out1,
        EventId::new(event_id(7)),
        |idx| EventId::new(event_id(8 + idx as u64)),
        |idx| EventId::new(event_id(8 + 1000 + idx as u64)),
        NOW_MS + 7,
    )
    .unwrap();
    // ProviderResponseCommitted + sealed display block(s) + ToolCallDeclared
    assert!(
        r1_facts.len() >= 2,
        "response, display blocks and tool declaration"
    );
    let head = owner.head().unwrap();
    owner.commit("p-r1-commit", head, r1_facts).unwrap();

    // Settle the declared tool call from Chat round 1 so Round 2 can start.
    let chat_call_id = ToolCallId::new("call_01J00000000000000000000101");
    let mut chat_tool_fin = fact(
        FactPayload::ToolFinished(ToolFinished {
            call_id: chat_call_id.clone(),
            execution_id: None,
            terminal_status: ToolTerminalStatus::Cancelled,
            output_ref: None,
            error: None,
            metrics: ToolMetrics {
                started_at_ms: NOW_MS + 9,
                finished_at_ms: NOW_MS + 9,
                retry_count: 0,
                output_bytes: 0,
                progress_bytes_total: 0,
            },
            reconciled: false,
            evidence_ref: None,
            evidence_fact_seq: None,
            evidence_event_id: None,
            recovery_ref: None,
            finished_at_ms: NOW_MS + 9,
        }),
        9,
        Some(turn_id.clone()),
    );
    chat_tool_fin.call_id = Some(chat_call_id.clone());
    let chat_tool_msg = Message {
        msg_id: None,
        role: "tool".into(),
        name: None,
        content: vec![ContentBlock::ToolResult {
            tool_use_id: chat_call_id.as_str().to_owned(),
            result: ToolResult::ok("hello-content"),
        }],
    };
    let chat_tool_ref = owner
        .put_blob(&serde_json::to_vec(&chat_tool_msg).unwrap())
        .unwrap();
    let head = owner.head().unwrap();
    owner
        .commit(
            "p-r1-tool",
            head,
            vec![
                chat_tool_fin,
                fact(
                    FactPayload::ContextMessageCommitted(ContextMessageCommitted {
                        message_ref: chat_tool_ref,
                        origin: ContextMessageOrigin::ToolResult,
                    }),
                    10,
                    Some(turn_id.clone()),
                ),
            ],
        )
        .unwrap();

    // ── 2) OpenAI Responses API (encrypted_content opaque_refs + replay golden)
    let resp_provider = qaqh_gate::ProviderConfig::responses(
        &base_url,
        "sk-resp",
        "gpt-5-probe",
        Some("/v1/responses".into()),
    );
    let resp_spec =
        ProviderRequestSpec::new(resp_provider.clone(), 256).with_tools(vec![read_tool_def]);
    let r2 = build_model_round_started(&owner, turn_id.clone(), 2, 1, &resp_spec).unwrap();
    let head = owner.head().unwrap();
    let (_, cap2) = owner
        .commit_with_effect(
            "p-r2",
            head,
            vec![fact(
                FactPayload::ModelRoundStarted(r2),
                11,
                Some(turn_id.clone()),
            )],
            &event_id(11),
        )
        .unwrap();
    let (_tx, rx) = tokio::sync::watch::channel(false);
    let out2 = provider_job::run_verified(resp_spec.clone(), cap2.unwrap(), rx).await;
    assert_eq!(out2.evidence, DispatchEvidence::StartedTerminal);
    assert_eq!(out2.finish, Some(ProviderFinish::ToolUse));
    assert!(
        !out2.opaque_items.is_empty(),
        "Responses output items (encrypted_content + function_call) must be extracted into opaque_items"
    );
    assert_eq!(out2.usage.as_ref().unwrap().reasoning_tokens, 4);

    let r2_facts = prepare_response_commit_facts(
        &owner,
        &turn_id,
        2,
        1,
        "test-model",
        None,
        &out2,
        EventId::new(event_id(12)),
        |idx| EventId::new(event_id(13 + idx as u64)),
        |idx| EventId::new(event_id(13 + 1000 + idx as u64)),
        NOW_MS + 12,
    )
    .unwrap();
    if let FactPayload::ProviderResponseCommitted(committed) = &r2_facts[0].payload {
        assert_eq!(committed.opaque_refs.len(), out2.opaque_items.len());
    } else {
        panic!("expected ProviderResponseCommitted");
    }
    let head = owner.head().unwrap();
    owner.commit("p-r2-commit", head, r2_facts).unwrap();

    // Commit tool result for the Responses function_call (`call_01J00000000000000000000201`).
    let resp_call_id = ToolCallId::new("call_01J00000000000000000000201");
    let mut resp_tool_fin = fact(
        FactPayload::ToolFinished(ToolFinished {
            call_id: resp_call_id.clone(),
            execution_id: None,
            terminal_status: ToolTerminalStatus::Cancelled,
            output_ref: None,
            error: None,
            metrics: ToolMetrics {
                started_at_ms: NOW_MS + 15,
                finished_at_ms: NOW_MS + 15,
                retry_count: 0,
                output_bytes: 0,
                progress_bytes_total: 0,
            },
            reconciled: false,
            evidence_ref: None,
            evidence_fact_seq: None,
            evidence_event_id: None,
            recovery_ref: None,
            finished_at_ms: NOW_MS + 15,
        }),
        15,
        Some(turn_id.clone()),
    );
    resp_tool_fin.call_id = Some(resp_call_id.clone());
    let resp_tool_msg = Message {
        msg_id: None,
        role: "tool".into(),
        name: None,
        content: vec![ContentBlock::ToolResult {
            tool_use_id: resp_call_id.as_str().to_owned(),
            result: ToolResult::ok("note-body"),
        }],
    };
    let resp_tool_ref = owner
        .put_blob(&serde_json::to_vec(&resp_tool_msg).unwrap())
        .unwrap();
    let head = owner.head().unwrap();
    owner
        .commit(
            "p-r2-tool",
            head,
            vec![
                resp_tool_fin,
                fact(
                    FactPayload::ContextMessageCommitted(ContextMessageCommitted {
                        message_ref: resp_tool_ref,
                        origin: ContextMessageOrigin::ToolResult,
                    }),
                    16,
                    Some(turn_id.clone()),
                ),
            ],
        )
        .unwrap();

    // Capture the pre-restart request identity and outbound Responses wire body for Round 3,
    // then reopen from disk and verify the reopened session produces the exact same
    // RequestIdentity and outbound wire body (including `ENC_BLOB_V1` and `function_call_output`)!
    let pre_restart_r3 =
        build_model_round_started(&owner, turn_id.clone(), 3, 1, &resp_spec).unwrap();
    let pre_restart_messages = owner.request_messages().unwrap().to_vec();

    drop(owner);
    let mut owner = SessionCommit::open(&dir, SessionId::new(SESSION), LogId::new(LOG), budget())
        .expect("reopen four-provider session");
    let post_restart_r3 =
        build_model_round_started(&owner, turn_id.clone(), 3, 1, &resp_spec).unwrap();
    assert_eq!(
        pre_restart_r3, post_restart_r3,
        "reopened session must produce identical ModelRoundStarted and RequestIdentity"
    );
    assert_eq!(
        serde_json::to_vec(&pre_restart_messages).unwrap(),
        serde_json::to_vec(owner.request_messages().unwrap()).unwrap(),
        "reopened context messages (including reassembled opaque ResponseOutputItem) must match byte-for-byte"
    );

    let head = owner.head().unwrap();
    let (_, cap3) = owner
        .commit_with_effect(
            "p-r3",
            head,
            vec![fact(
                FactPayload::ModelRoundStarted(post_restart_r3),
                17,
                Some(turn_id.clone()),
            )],
            &event_id(17),
        )
        .unwrap();
    let (_tx, rx) = tokio::sync::watch::channel(false);
    let out3 = provider_job::run_verified(resp_spec, cap3.unwrap(), rx).await;
    assert_eq!(out3.evidence, DispatchEvidence::StartedTerminal);
    assert_eq!(out3.finish, Some(ProviderFinish::Stop));
    let wire_body = captured_requests
        .lock()
        .unwrap()
        .last()
        .unwrap()
        .1
        .to_string();
    assert!(
        wire_body.contains("ENC_BLOB_V1") && wire_body.contains("function_call_output"),
        "reopened Responses request must echo encrypted_content and function_call_output on the wire"
    );
    let r3_facts = prepare_response_commit_facts(
        &owner,
        &turn_id,
        3,
        1,
        "test-model",
        None,
        &out3,
        EventId::new(event_id(18)),
        |_| unreachable!(),
        |idx| EventId::new(event_id(18 + 1000 + idx as u64)),
        NOW_MS + 18,
    )
    .unwrap();
    let head = owner.head().unwrap();
    owner.commit("p-r3-commit", head, r3_facts).unwrap();

    // ── 3) Anthropic Messages API (`/v1/messages`) ──────────────────────────
    let anth_provider = qaqh_gate::ProviderConfig::messages(
        &base_url,
        "sk-anth",
        "claude-probe",
        Some("/v1/messages".into()),
    );
    let anth_spec = ProviderRequestSpec::new(anth_provider, 256).with_effort("medium");
    let r4 = build_model_round_started(&owner, turn_id.clone(), 4, 1, &anth_spec).unwrap();
    let head = owner.head().unwrap();
    let (_, cap4) = owner
        .commit_with_effect(
            "p-r4",
            head,
            vec![fact(
                FactPayload::ModelRoundStarted(r4),
                19,
                Some(turn_id.clone()),
            )],
            &event_id(19),
        )
        .unwrap();
    let (_tx, rx) = tokio::sync::watch::channel(false);
    let out4 = provider_job::run_verified(anth_spec, cap4.unwrap(), rx).await;
    assert_eq!(out4.evidence, DispatchEvidence::StartedTerminal);
    assert_eq!(out4.finish, Some(ProviderFinish::Stop));
    assert!(out4.usage.is_some());
    let r4_facts = prepare_response_commit_facts(
        &owner,
        &turn_id,
        4,
        1,
        "test-model",
        None,
        &out4,
        EventId::new(event_id(20)),
        |_| unreachable!(),
        |idx| EventId::new(event_id(20 + 1000 + idx as u64)),
        NOW_MS + 20,
    )
    .unwrap();
    let head = owner.head().unwrap();
    owner.commit("p-r4-commit", head, r4_facts).unwrap();

    // ── 4) Google Gemini API (`:streamGenerateContent`) ─────────────────────
    let gemini_provider = qaqh_gate::ProviderConfig::gemini(
        &base_url,
        "sk-gemini",
        "gemini-probe",
        Some("/v1beta/models/{model}:generateContent".into()),
    );
    let gemini_spec = ProviderRequestSpec::new(gemini_provider, 256).with_effort("high");
    let r5 = build_model_round_started(&owner, turn_id.clone(), 5, 1, &gemini_spec).unwrap();
    let head = owner.head().unwrap();
    let (_, cap5) = owner
        .commit_with_effect(
            "p-r5",
            head,
            vec![fact(
                FactPayload::ModelRoundStarted(r5),
                21,
                Some(turn_id.clone()),
            )],
            &event_id(21),
        )
        .unwrap();
    let (_tx, rx) = tokio::sync::watch::channel(false);
    let out5 = provider_job::run_verified(gemini_spec, cap5.unwrap(), rx).await;
    assert_eq!(out5.evidence, DispatchEvidence::StartedTerminal);
    assert_eq!(out5.finish, Some(ProviderFinish::Stop));
    assert_eq!(out5.usage.as_ref().unwrap().reasoning_tokens, 3);
    let r5_facts = prepare_response_commit_facts(
        &owner,
        &turn_id,
        5,
        1,
        "test-model",
        None,
        &out5,
        EventId::new(event_id(22)),
        |_| unreachable!(),
        |idx| EventId::new(event_id(22 + 1000 + idx as u64)),
        NOW_MS + 22,
    )
    .unwrap();
    let head = owner.head().unwrap();
    owner.commit("p-r5-commit", head, r5_facts).unwrap();

    println!(
        "ok: [21] A07 Chat, Responses (with opaque_refs replay), Anthropic Messages, and Gemini rounds committed and verified across restart"
    );
}

/// 22: Real WorkspaceToolPort + pre-dispatch permission/approval re-check + QueuedInputSettled + QuotaLedger retention (A05/A08).
async fn probe_real_workspace_tool_port_permissions_quota_and_queued_inputs(
    root: &std::path::Path,
) {
    let ws_dir = root.join("workspace-real-tools");
    std::fs::create_dir_all(&ws_dir).expect("create workspace dir");
    let session_dir = root.join("session-real-tools");
    let quota_dir = root.join("quota-data");

    let mut owner = SessionCommit::create(
        &session_dir,
        SessionId::new(SESSION),
        LogId::new(LOG),
        budget(),
    )
    .expect("create session");

    let input_1 = InputId::new("input_01J00000000000000000000080");
    let input_queued_cancel = InputId::new("input_01J00000000000000000000081");
    let input_queued_next = InputId::new("input_01J00000000000000000000082");
    let turn_1 = TurnId::new("turn_01J00000000000000000000083");
    let turn_2 = TurnId::new("turn_01J00000000000000000000084");

    let user_ref = owner
        .put_blob(&serde_json::to_vec(&text_message("user", "write file")).unwrap())
        .unwrap();
    owner
        .commit(
            "rt-init",
            0,
            vec![
                fact(
                    FactPayload::SessionCreated(SessionCreated {
                        created_at_ms: NOW_MS + 1,
                        cwd: ws_dir.to_string_lossy().into_owned(),
                        model: "probe-model".into(),
                        parent_session_id: None,
                        schema_caps: vec![],
                    }),
                    1,
                    None,
                ),
                fact(
                    FactPayload::InputAccepted(InputAccepted {
                        input_id: input_1.clone(),
                        input_kind: InputKind::UserText,
                        input_purpose: InputPurpose::TriggerTurn,
                        content_ref: None,
                        inline_text: Some("write file".into()),
                        attachments: vec![],
                        actor: ActorRef {
                            kind: ActorKind::User,
                            id: "u".into(),
                            display_name: None,
                        },
                        client_request_id: None,
                    }),
                    2,
                    None,
                ),
                fact(
                    FactPayload::InputAccepted(InputAccepted {
                        input_id: input_queued_cancel.clone(),
                        input_kind: InputKind::UserText,
                        input_purpose: InputPurpose::QueueOnly,
                        content_ref: None,
                        inline_text: Some("cancel me while queued".into()),
                        attachments: vec![],
                        actor: ActorRef {
                            kind: ActorKind::User,
                            id: "u".into(),
                            display_name: None,
                        },
                        client_request_id: None,
                    }),
                    3,
                    None,
                ),
                fact(
                    FactPayload::InputAccepted(InputAccepted {
                        input_id: input_queued_next.clone(),
                        input_kind: InputKind::UserText,
                        input_purpose: InputPurpose::QueueOnly,
                        content_ref: None,
                        inline_text: Some("run me second".into()),
                        attachments: vec![],
                        actor: ActorRef {
                            kind: ActorKind::User,
                            id: "u".into(),
                            display_name: None,
                        },
                        client_request_id: None,
                    }),
                    4,
                    None,
                ),
                fact(
                    FactPayload::TurnStarted(TurnStarted {
                        turn_id: turn_1.clone(),
                        input_id: input_1,
                        mode: TurnMode::Normal,
                        recovery_ref: None,
                    }),
                    5,
                    Some(turn_1.clone()),
                ),
                fact(
                    FactPayload::ContextMessageCommitted(ContextMessageCommitted {
                        message_ref: user_ref,
                        origin: ContextMessageOrigin::Input,
                    }),
                    6,
                    Some(turn_1.clone()),
                ),
            ],
        )
        .expect("commit init + queued inputs");

    // Settle `input_queued_cancel` via `QueuedInputSettled` WITHOUT starting a fake empty turn.
    let head = owner.head().unwrap();
    owner
        .commit(
            "rt-queued-settle",
            head,
            vec![fact(
                FactPayload::QueuedInputSettled(QueuedInputSettled {
                    input_id: input_queued_cancel.clone(),
                    disposition: QueuedInputDisposition::Cancelled,
                    reason: Some("user cancelled queued message".into()),
                    recovery_ref: None,
                    settled_at_ms: NOW_MS + 7,
                }),
                7,
                None,
            )],
        )
        .expect("settle queued input without empty turn");
    assert_eq!(
        owner.obligations().unwrap().pending_inputs,
        vec![input_queued_next.clone()]
    );

    // Open round 1 and commit response so we can run real workspace tools.
    let head = owner.head().unwrap();
    let r1 = fact(
        FactPayload::ModelRoundStarted(round_started(&owner, &turn_1, 1, head)),
        8,
        Some(turn_1.clone()),
    );
    let resp_ref = owner
        .put_blob(&serde_json::to_vec(&text_message("assistant", "running tools")).unwrap())
        .unwrap();
    owner
        .commit(
            "rt-round1",
            head,
            vec![
                r1,
                fact(
                    FactPayload::ProviderResponseCommitted(ProviderResponseCommitted {
                        turn_id: turn_1.clone(),
                        round: 1,
                        attempt: 1,
                        message_ref: resp_ref,
                        opaque_refs: vec![],
                        usage: None,
                        finish: ProviderFinish::Stop,
                        incomplete_reason: None,
                    }),
                    9,
                    Some(turn_1.clone()),
                ),
            ],
        )
        .unwrap();

    // 探针刻意与生产装配分叉：它要一个带配额的 port 和固定策略快照，而不是
    // 会话配置推导出来的那一份。生产路径的唯一入口是
    // `qaqh_agent_runtime::session_agent::assembly::assemble`（见 `registry::spawn`）。
    let manager = Arc::new(Mutex::new(
        qaqh_workspace::registration::build_tool_manager(&[]),
    ));
    let ledger = Arc::new(Mutex::new(
        QuotaLedger::open_at(
            &quota_dir,
            SESSION,
            QuotaLimits::new(4, 4).expect("quota limits"),
        )
        .expect("open quota ledger"),
    ));
    let policy = ToolPolicySnapshot::workspace_write(&ws_dir);
    let lifecycle = {
        let dir = Box::leak(Box::new(tempfile::tempdir().expect("tempdir")));
        let db = std::sync::Arc::new(
            qaqh_session::db::Db::open(dir.path().join("qaqh.db")).expect("open test db"),
        );
        qaqh_session::owner::SessionOwner::new(SESSION, db).lifecycle()
    };
    let ws_port = Arc::new(WorkspaceToolPort::new(
        SESSION,
        Arc::clone(&manager),
        policy.clone(),
        Some(Arc::clone(&ledger)),
        lifecycle,
    ));

    let (tool_handle, mut tool_mailbox) =
        Mailbox::<ProbeInput, ProbeInput, TaskCompletion<ToolOutcome>>::new(
            LaneBudget::COMMANDS,
            LaneBudget::CONTROL,
            LaneBudget::COMPLETIONS,
        )
        .unwrap();
    let mut tools: Supervisor<ToolOutcome, CommittedExecution> = Supervisor::new(2).unwrap();

    // ── A) Real `write` and `read` tools inside workspace ───────────────────
    let write_args = serde_json::to_vec(&serde_json::json!({
        "path": "real_note.txt",
        "content": "hello from WorkspaceToolPort\n"
    }))
    .unwrap();
    let write_call = ToolCallId::new("call_01J00000000000000000000085");
    let write_exec = ExecutionId::new("exec_01J00000000000000000000086");
    let write_args_ref = owner.put_blob(&write_args).unwrap();
    let write_args_hash = sha256_content_hash(&write_args);
    let mut w_decl = fact(
        FactPayload::ToolCallDeclared(ToolCallDeclared {
            turn_id: turn_1.clone(),
            call_id: write_call.clone(),
            tool_name: "write".into(),
            args_ref: write_args_ref.clone(),
            args_hash: write_args_hash.clone(),
        }),
        10,
        Some(turn_1.clone()),
    );
    w_decl.call_id = Some(write_call.clone());
    let mut w_intent = fact(
        FactPayload::ToolIntent(ToolIntent {
            call_id: write_call.clone(),
            execution_id: write_exec.clone(),
            idempotency_key: None,
            replay_capability: ToolReplayCapability::NoReplay,
            policy_decision: PolicyDecisionRef {
                outcome: ToolIntentPolicyOutcome::Allow,
                rule_id: "ws.write".into(),
                decided_at_ms: NOW_MS + 11,
                reason_ref: None,
            },
            effective_args_ref: Some(write_args_ref),
            effective_args_hash: Some(write_args_hash),
            sandbox_spec_hash: digest("sandbox:ws"),
            side_effect_class: SideEffectClass::WorkspaceWrite,
            intent_at_ms: NOW_MS + 11,
        }),
        11,
        Some(turn_1.clone()),
    );
    w_intent.call_id = Some(write_call.clone());
    let head = owner.head().unwrap();
    let (_, w_cap) = owner
        .commit_with_effect("rt-write", head, vec![w_decl, w_intent], &event_id(11))
        .unwrap();
    let w_cap = w_cap.unwrap();
    let w_id = w_cap.identity().clone();
    tools.adopt_actor(&w_id.actor);
    let ticket = tools.authorize(w_id.clone(), w_cap).unwrap();
    let port_clone = Arc::clone(&ws_port);
    tools
        .start(ticket, tools_reserve(&tool_handle), move |eff, cancel| {
            tool_job::run(port_clone, "write".into(), eff, cancel)
        })
        .unwrap();
    let w_comp = loop {
        match tools.collect(&w_id).await {
            Ok(c) => break c,
            Err(SupervisorError::NotFinished) => tokio::task::yield_now().await,
            Err(e) => panic!("{e:?}"),
        }
    };
    tools.deliver(&w_id).unwrap();
    let Input::Completion(delivered) = tool_mailbox.recv().await.unwrap() else {
        panic!("expected completion");
    };
    match &delivered.outcome {
        TaskOutcome::Returned(out) => {
            assert_eq!(out.evidence, DispatchEvidence::StartedTerminal);
            assert_eq!(out.terminal, Some(ToolTerminalStatus::Succeeded));
        }
        TaskOutcome::JoinFailed(e) => panic!("{e}"),
    }
    tools.acknowledge(&w_id, &w_comp).unwrap();
    assert_eq!(
        std::fs::read_to_string(ws_dir.join("real_note.txt")).unwrap(),
        "hello from WorkspaceToolPort\n"
    );
    assert_eq!(
        ledger.lock().unwrap().snapshot().unwrap().held,
        0,
        "completed tool must release its quota reservation"
    );

    // ── A2) Backgrounded execution retains QuotaLedger reservation until settled
    let bg_policy = ToolPolicySnapshot {
        workspace_root: ws_dir.clone(),
        mode: qaqh_workspace::tool_api::AgentMode::Code,
        permission_level: qaqh_workspace::PermissionLevel::SkipPermissions,
        sandbox: qaqh_workspace::tool_api::SandboxMode::Main,
        trusted_dirs: std::collections::HashSet::new(),
        sandbox_spec: qaqh_workspace::tool_api::SandboxSpec::for_permission(
            ws_dir.clone(),
            qaqh_workspace::PermissionLevel::SkipPermissions,
            true,
        ),
    };
    ws_port.update_policy(bg_policy);
    let bg_args = serde_json::to_vec(&serde_json::json!({
        "command": "sleep 2",
        "background_after_secs": 0
    }))
    .unwrap();
    let bg_req = ToolRequest {
        execution_id: "exec_01J00000000000000000000099".into(),
        call_id: "call_01J00000000000000000000099".into(),
        tool_name: "exec".into(),
        side_effect_class: SideEffectClass::Process,
        replay: ToolReplayCapability::NoReplay,
        args: bg_args,
    };
    let bg_cancel = Arc::new(AtomicBool::new(false));
    let bg_out = ws_port
        .execute(&bg_req, Arc::clone(&bg_cancel))
        .expect("exec backgrounded");
    assert!(
        bg_out.backgrounded,
        "exec with background_after_secs=0 must background"
    );
    assert_eq!(ws_port.retained_owners_len(), 1);
    assert_eq!(
        ledger.lock().unwrap().snapshot().unwrap().held,
        1,
        "backgrounded process must hold its QuotaLedger reservation"
    );
    let retained = ws_port
        .retained_owner("exec_01J00000000000000000000099")
        .expect("retained owner");
    if let Some(pid) = retained.process_id {
        let kill_args = serde_json::to_vec(&serde_json::json!({
            "action": "kill",
            "id": pid
        }))
        .unwrap();
        let _ = ws_port.execute(
            &ToolRequest {
                execution_id: "exec_01J00000000000000000000100".into(),
                call_id: "call_01J00000000000000000000100".into(),
                tool_name: "process".into(),
                side_effect_class: SideEffectClass::Process,
                replay: ToolReplayCapability::NoReplay,
                args: kill_args,
            },
            Arc::clone(&bg_cancel),
        );
    }
    assert!(
        ws_port
            .settle_retained_owner(
                "exec_01J00000000000000000000099",
                ReleaseReason::Reconciliation
            )
            .expect("settle retained owner")
    );
    assert_eq!(ws_port.retained_owners_len(), 0);
    assert_eq!(
        ledger.lock().unwrap().snapshot().unwrap().held,
        0,
        "settling retained backgrounded owner releases its QuotaLedger reservation"
    );
    let mut w_fin = fact(
        FactPayload::ToolFinished(ToolFinished {
            call_id: write_call.clone(),
            execution_id: Some(write_exec),
            terminal_status: ToolTerminalStatus::Succeeded,
            output_ref: None,
            error: None,
            metrics: ToolMetrics {
                started_at_ms: NOW_MS + 11,
                finished_at_ms: NOW_MS + 12,
                retry_count: 0,
                output_bytes: 16,
                progress_bytes_total: 16,
            },
            reconciled: false,
            evidence_ref: None,
            evidence_fact_seq: None,
            evidence_event_id: None,
            recovery_ref: None,
            finished_at_ms: NOW_MS + 12,
        }),
        12,
        Some(turn_1.clone()),
    );
    w_fin.call_id = Some(write_call);
    let head = owner.head().unwrap();
    owner.commit("rt-write-fin", head, vec![w_fin]).unwrap();

    // ── B) Pre-dispatch policy re-check (mode switched to Plan blocks write;
    //       ReadOnly permission requires matching execution_id approval grant)
    let mut ro_policy = policy.clone();
    ro_policy.permission_level = qaqh_workspace::PermissionLevel::ReadOnly;
    ws_port.update_policy(ro_policy);

    let guarded_args = serde_json::to_vec(&serde_json::json!({
        "path": "guarded.txt",
        "content": "approved content\n"
    }))
    .unwrap();
    let guarded_ref = owner.put_blob(&guarded_args).unwrap();
    let guarded_hash = sha256_content_hash(&guarded_args);
    let g_call = ToolCallId::new("call_01J00000000000000000000087");
    let g_exec = ExecutionId::new("exec_01J00000000000000000000088");
    let mut g_decl = fact(
        FactPayload::ToolCallDeclared(ToolCallDeclared {
            turn_id: turn_1.clone(),
            call_id: g_call.clone(),
            tool_name: "write".into(),
            args_ref: guarded_ref.clone(),
            args_hash: guarded_hash.clone(),
        }),
        13,
        Some(turn_1.clone()),
    );
    g_decl.call_id = Some(g_call.clone());
    let mut g_intent = fact(
        FactPayload::ToolIntent(ToolIntent {
            call_id: g_call.clone(),
            execution_id: g_exec.clone(),
            idempotency_key: None,
            replay_capability: ToolReplayCapability::NoReplay,
            policy_decision: PolicyDecisionRef {
                outcome: ToolIntentPolicyOutcome::Allow,
                rule_id: "user.approved".into(),
                decided_at_ms: NOW_MS + 14,
                reason_ref: None,
            },
            effective_args_ref: Some(guarded_ref),
            effective_args_hash: Some(guarded_hash.clone()),
            sandbox_spec_hash: digest("sandbox:ws"),
            side_effect_class: SideEffectClass::WorkspaceWrite,
            intent_at_ms: NOW_MS + 14,
        }),
        14,
        Some(turn_1.clone()),
    );
    g_intent.call_id = Some(g_call.clone());
    let head = owner.head().unwrap();
    let (_, g_cap) = owner
        .commit_with_effect("rt-guarded", head, vec![g_decl, g_intent], &event_id(14))
        .unwrap();
    let g_cap = g_cap.unwrap();

    // Grant approval then revoke it before dispatch -> refused as Denied before start!
    ws_port.grant_approval(ToolApprovalGrant {
        interaction_id: "int_1".into(),
        call_id: g_call.as_str().to_owned(),
        execution_id: g_exec.as_str().to_owned(),
        args_hash: guarded_hash.0.clone(),
        workspace_root: ws_dir.clone(),
    });
    assert!(ws_port.revoke_approval(g_exec.as_str()));
    let (_tx, rx) = tokio::sync::watch::channel(false);
    let denied_out = tool_job::run(Arc::clone(&ws_port), "write".into(), g_cap, rx).await;
    assert_eq!(denied_out.evidence, DispatchEvidence::NotStarted);
    assert_eq!(denied_out.terminal, Some(ToolTerminalStatus::Denied));
    assert!(!ws_dir.join("guarded.txt").exists());

    let mut g_fin = fact(
        FactPayload::ToolFinished(ToolFinished {
            call_id: g_call.clone(),
            execution_id: Some(g_exec),
            terminal_status: ToolTerminalStatus::Denied,
            output_ref: None,
            error: None,
            metrics: ToolMetrics {
                started_at_ms: NOW_MS + 14,
                finished_at_ms: NOW_MS + 15,
                retry_count: 0,
                output_bytes: 0,
                progress_bytes_total: 0,
            },
            reconciled: false,
            evidence_ref: None,
            evidence_fact_seq: None,
            evidence_event_id: None,
            recovery_ref: None,
            finished_at_ms: NOW_MS + 15,
        }),
        15,
        Some(turn_1.clone()),
    );
    g_fin.call_id = Some(g_call);
    let head = owner.head().unwrap();
    owner.commit("rt-guarded-fin", head, vec![g_fin]).unwrap();

    // Finish turn 1 and verify `input_queued_next` (after `input_queued_cancel` was settled)
    // starts turn 2 cleanly!
    let head = owner.head().unwrap();
    owner
        .commit(
            "rt-turn1-fin",
            head,
            vec![
                fact(
                    FactPayload::TurnFinished(TurnFinished {
                        turn_id: turn_1.clone(),
                        terminal: TurnTerminal::Completed,
                        usage: None,
                        error: None,
                        finished_at_ms: NOW_MS + 16,
                    }),
                    16,
                    Some(turn_1),
                ),
                fact(
                    FactPayload::TurnStarted(TurnStarted {
                        turn_id: turn_2.clone(),
                        input_id: input_queued_next,
                        mode: TurnMode::Normal,
                        recovery_ref: None,
                    }),
                    17,
                    Some(turn_2),
                ),
            ],
        )
        .expect("second queued input starts turn 2 after first queued input was settled");

    println!(
        "ok: [22] A05/A08 WorkspaceToolPort runs real tools, enforces pre-dispatch approval revocation, releases quota, and settles queued input without empty turn"
    );
}

#[tokio::main(flavor = "current_thread")]
async fn main() {
    let temp = tempfile::tempdir().expect("tempdir");
    // Pin QAQH_DATA_DIR to the isolated probe directory before any workspace/audit call.
    // SAFETY: 探针进程单用途，main 一开始设置、无并发读者。
    unsafe {
        std::env::set_var("QAQH_DATA_DIR", temp.path().join(".qaqh"));
    }
    qaqh_workspace::set_workspace(&temp.path().to_string_lossy());

    synthetic_supervisor_checks();

    let dir = temp.path().join("session");
    let (mut owner, capability, round_fact, turn_id) = mint_capability(&dir);

    // ── Real committed capability through the supervisor ────────────────────
    let (handle, mut mailbox) =
        Mailbox::<ProbeInput, ProbeInput, TaskCompletion<ProbeOutcome>>::new(
            LaneBudget::COMMANDS,
            LaneBudget::CONTROL,
            LaneBudget::COMPLETIONS,
        )
        .expect("mailbox");

    let mut supervisor: Supervisor<ProbeOutcome, CommittedExecution> =
        Supervisor::new(1).expect("supervisor");
    supervisor.adopt_actor(&capability.identity().actor.clone());
    let identity = capability.identity().clone();
    assert_eq!(identity.generation, 1);
    assert_eq!(identity.effect_id, format!("model:{}", event_id(5)));

    let ticket = supervisor
        .authorize(identity.clone(), capability)
        .expect("authorize committed intent");
    let reservation = handle
        .try_reserve_completion(4096)
        .expect("reserve completion");
    let expected = identity.clone();
    supervisor
        .start(ticket, reservation, |effect, _cancel| async move {
            match effect.into_parts() {
                Ok(parts) => {
                    println!(
                        "ok: [2a] capability carried {} frozen messages",
                        parts.request_messages.as_ref().map_or(0, Vec::len)
                    );
                    assert_eq!(parts.identity, expected);
                    ProbeOutcome {
                        evidence: DispatchEvidence::StartedTerminal,
                        identity: Some(parts.identity),
                    }
                }
                Err(_) => ProbeOutcome {
                    evidence: DispatchEvidence::NotStarted,
                    identity: None,
                },
            }
        })
        .expect("start authorized effect");

    let completion = loop {
        match supervisor.collect(&identity).await {
            Ok(completion) => break completion,
            Err(SupervisorError::NotFinished) => tokio::task::yield_now().await,
            Err(error) => panic!("collect failed: {error:?}"),
        }
    };
    supervisor.deliver(&identity).expect("deliver completion");
    let delivered = mailbox.recv().await.expect("completion frame");
    let delivered = match delivered {
        Input::Completion(completion) => completion,
        _ => panic!("unexpected mailbox input"),
    };
    assert_eq!(delivered.identity, identity);
    if let TaskOutcome::Returned(out) = &delivered.outcome {
        assert_eq!(out.evidence, DispatchEvidence::StartedTerminal);
        assert_eq!(out.identity.as_ref(), Some(&identity));
    }
    supervisor
        .acknowledge(&identity, &completion)
        .expect("acknowledge");
    println!("ok: [2] authorize -> start -> completion -> acknowledge");

    // ── Response bundle: a complete committed response reopens the round ────
    let response_ref = owner
        .put_blob(&serde_json::to_vec(&text_message("assistant", "pong")).unwrap())
        .expect("publish response blob");
    let head = owner.head().expect("head");
    owner
        .commit(
            "batch-6",
            head,
            vec![fact(
                FactPayload::ProviderResponseCommitted(ProviderResponseCommitted {
                    turn_id: turn_id.clone(),
                    round: 1,
                    attempt: 1,
                    message_ref: response_ref,
                    opaque_refs: vec![],
                    usage: None,
                    finish: ProviderFinish::Stop,
                    incomplete_reason: None,
                }),
                6,
                Some(turn_id.clone()),
            )],
        )
        .expect("commit provider response");
    let replayable = owner.request_messages().expect("context").to_vec();
    assert_eq!(replayable.len(), 2);
    assert_eq!(replayable[1].role, "assistant");
    println!("ok: [3] committed response becomes the next request prefix");

    // ── Request identity: round 2 mints generation from its attempt ─────────
    let head = owner.head().expect("head");
    let (_, second) = owner
        .commit_with_effect(
            "batch-7",
            head,
            vec![fact(
                FactPayload::ModelRoundStarted(round_started(&owner, &turn_id, 2, head)),
                7,
                Some(turn_id.clone()),
            )],
            &event_id(7),
        )
        .expect("commit second model intent");
    let second = second.expect("second round must mint a capability");
    assert_eq!(second.identity().generation, 2);
    println!("ok: [5] generation follows the committed attempt");

    assert!(owner.request_messages().is_err());
    let response_ref = owner
        .put_blob(&serde_json::to_vec(&text_message("assistant", "pong-2")).unwrap())
        .expect("publish second response blob");
    let head = owner.head().expect("head");
    owner
        .commit(
            "batch-8",
            head,
            vec![fact(
                FactPayload::ProviderResponseCommitted(ProviderResponseCommitted {
                    turn_id: turn_id.clone(),
                    round: 2,
                    attempt: 2,
                    message_ref: response_ref,
                    opaque_refs: vec![],
                    usage: None,
                    finish: ProviderFinish::Stop,
                    incomplete_reason: None,
                }),
                8,
                Some(turn_id.clone()),
            )],
        )
        .expect("commit second response");
    assert_eq!(owner.request_messages().expect("context").len(), 3);
    println!("ok: [4] open round blocks the prefix until its response commits");

    // ── Tool chain: declared -> committed intent -> supervised execution ────
    let call_id = ToolCallId::new("call_01J00000000000000000000009");
    let execution_id = ExecutionId::new("exec_01J00000000000000000000010");
    let args = br#"{"cmd":"true"}"#.to_vec();
    let args_ref = owner.put_blob(&args).expect("publish tool args");
    let args_hash = sha256_content_hash(&args);

    let head = owner.head().expect("head");
    let mut declared = fact(
        FactPayload::ToolCallDeclared(ToolCallDeclared {
            turn_id: turn_id.clone(),
            call_id: call_id.clone(),
            tool_name: "probe_tool".into(),
            args_ref: args_ref.clone(),
            args_hash: args_hash.clone(),
        }),
        9,
        Some(turn_id.clone()),
    );
    declared.call_id = Some(call_id.clone());
    owner
        .commit("batch-9", head, vec![declared])
        .expect("commit tool declaration");

    let head = owner.head().expect("head");
    let mut intent_fact = fact(
        FactPayload::ToolIntent(ToolIntent {
            call_id: call_id.clone(),
            execution_id: execution_id.clone(),
            idempotency_key: None,
            replay_capability: ToolReplayCapability::NoReplay,
            policy_decision: PolicyDecisionRef {
                outcome: ToolIntentPolicyOutcome::Allow,
                rule_id: "probe.allow".into(),
                decided_at_ms: NOW_MS + 10,
                reason_ref: None,
            },
            effective_args_ref: Some(args_ref.clone()),
            effective_args_hash: Some(args_hash.clone()),
            sandbox_spec_hash: digest("sandbox:readonly"),
            side_effect_class: SideEffectClass::ReadOnly,
            intent_at_ms: NOW_MS + 10,
        }),
        10,
        Some(turn_id.clone()),
    );
    intent_fact.call_id = Some(call_id.clone());
    let (_, tool_capability) = owner
        .commit_with_effect("batch-10", head, vec![intent_fact], &event_id(10))
        .expect("commit tool intent");
    let tool_capability = tool_capability.expect("tool intent must mint a capability");
    assert_eq!(
        tool_capability.identity().effect_id,
        format!("tool:{}", execution_id.as_str())
    );

    let runs = Arc::new(AtomicUsize::new(0));
    let port = Arc::new(FakeTool {
        runs: Arc::clone(&runs),
    });
    let (tool_handle, mut tool_mailbox) =
        Mailbox::<ProbeInput, ProbeInput, TaskCompletion<ToolOutcome>>::new(
            LaneBudget::COMMANDS,
            LaneBudget::CONTROL,
            LaneBudget::COMPLETIONS,
        )
        .expect("tool mailbox");
    let mut tools: Supervisor<ToolOutcome, CommittedExecution> =
        Supervisor::new(2).expect("tool supervisor");
    tools.adopt_actor(&tool_capability.identity().actor.clone());
    let tool_identity = tool_capability.identity().clone();
    let ticket = tools
        .authorize(tool_identity.clone(), tool_capability)
        .expect("authorize tool intent");
    let reservation = tools_reserve(&tool_handle);
    let start_port = Arc::clone(&port);
    tools
        .start(ticket, reservation, move |effect, cancel| {
            tool_job::run(start_port.clone(), "probe_tool".into(), effect, cancel)
        })
        .expect("start tool");
    let tool_completion = loop {
        match tools.collect(&tool_identity).await {
            Ok(completion) => break completion,
            Err(SupervisorError::NotFinished) => tokio::task::yield_now().await,
            Err(error) => panic!("tool collect failed: {error:?}"),
        }
    };
    tools
        .deliver(&tool_identity)
        .expect("deliver tool completion");
    let delivered = match tool_mailbox.recv().await.expect("tool completion frame") {
        Input::Completion(completion) => completion,
        _ => panic!("unexpected mailbox input"),
    };
    match &delivered.outcome {
        TaskOutcome::Returned(outcome) => {
            assert_eq!(outcome.evidence, DispatchEvidence::StartedTerminal);
            assert_eq!(outcome.terminal, Some(ToolTerminalStatus::Succeeded));
            assert_eq!(outcome.output, b"tool-ok");
        }
        TaskOutcome::JoinFailed(error) => {
            panic!("tool task join failed: {error}")
        }
    }
    assert_eq!(runs.load(Ordering::SeqCst), 1);
    tools
        .acknowledge(&tool_identity, &tool_completion)
        .expect("acknowledge tool");

    let output_ref = owner.put_blob(b"tool-ok").expect("publish tool output");
    let head = owner.head().expect("head");
    let mut finished = fact(
        FactPayload::ToolFinished(ToolFinished {
            call_id: call_id.clone(),
            execution_id: Some(execution_id.clone()),
            terminal_status: ToolTerminalStatus::Succeeded,
            output_ref: Some(output_ref),
            error: None,
            metrics: ToolMetrics {
                started_at_ms: NOW_MS + 10,
                finished_at_ms: NOW_MS + 20,
                retry_count: 0,
                output_bytes: 7,
                progress_bytes_total: 7,
            },
            reconciled: false,
            evidence_ref: None,
            evidence_fact_seq: None,
            evidence_event_id: None,
            recovery_ref: None,
            finished_at_ms: NOW_MS + 20,
        }),
        11,
        Some(turn_id.clone()),
    );
    finished.call_id = Some(call_id.clone());
    owner
        .commit("batch-11", head, vec![finished])
        .expect("commit tool result");
    println!("ok: [6] supervised tool result lands as a committed fact");

    // ── Cancel before start withdraws the ticket, not the work ──────────────
    let call2 = ToolCallId::new("call_01J00000000000000000000012");
    let execution2 = ExecutionId::new("exec_01J00000000000000000000013");
    let head = owner.head().expect("head");
    let mut declared2 = fact(
        FactPayload::ToolCallDeclared(ToolCallDeclared {
            turn_id: turn_id.clone(),
            call_id: call2.clone(),
            tool_name: "probe_tool".into(),
            args_ref: args_ref.clone(),
            args_hash: args_hash.clone(),
        }),
        12,
        Some(turn_id.clone()),
    );
    declared2.call_id = Some(call2.clone());
    let mut intent2 = fact(
        FactPayload::ToolIntent(ToolIntent {
            call_id: call2.clone(),
            execution_id: execution2,
            idempotency_key: None,
            replay_capability: ToolReplayCapability::NoReplay,
            policy_decision: PolicyDecisionRef {
                outcome: ToolIntentPolicyOutcome::Allow,
                rule_id: "probe.allow".into(),
                decided_at_ms: NOW_MS + 11,
                reason_ref: None,
            },
            effective_args_ref: Some(args_ref.clone()),
            effective_args_hash: Some(args_hash.clone()),
            sandbox_spec_hash: digest("sandbox:readonly"),
            side_effect_class: SideEffectClass::ReadOnly,
            intent_at_ms: NOW_MS + 11,
        }),
        13,
        Some(turn_id.clone()),
    );
    intent2.call_id = Some(call2.clone());
    let (_, cancelled_capability) = owner
        .commit_with_effect("batch-12", head, vec![declared2, intent2], &event_id(13))
        .expect("commit second tool intent");
    let cancelled_capability = cancelled_capability.expect("second tool intent mints a capability");
    let cancelled_identity = cancelled_capability.identity().clone();
    let cancelled_ticket = tools
        .authorize(cancelled_identity.clone(), cancelled_capability)
        .expect("authorize second tool");
    assert!(tools.request_cancel(&cancelled_identity).is_ok());
    let cancel_port = Arc::clone(&port);
    assert!(matches!(
        tools.start(
            cancelled_ticket,
            tools_reserve(&tool_handle),
            move |effect, cancel| {
                tool_job::run(cancel_port.clone(), "probe_tool".into(), effect, cancel)
            }
        ),
        Err(SupervisorError::UnknownEffect)
    ));
    assert_eq!(
        runs.load(Ordering::SeqCst),
        1,
        "cancelled tool must not run"
    );
    println!("ok: [7 & 14] cancelling an unstarted authorization withdraws its ticket");

    // ── Interaction answers are revision- and target-checked ────────────────
    let interaction_id = InteractionId::new("int_01J00000000000000000000014");
    let request_ref = owner
        .put_blob(b"permission request")
        .expect("publish request");
    let head = owner.head().expect("head");
    let mut request = fact(
        FactPayload::InteractionRequested(InteractionRequested {
            interaction_id: interaction_id.clone(),
            call_id: Some(call_id.clone()),
            turn_id: turn_id.clone(),
            kind: InteractionKind::Permission,
            request_ref,
            choices: Vec::new(),
            expires_at_ms: None,
            requested_at_ms: NOW_MS + 30,
        }),
        14,
        Some(turn_id.clone()),
    );
    request.interaction_id = Some(interaction_id.clone());
    request.call_id = Some(call_id.clone());
    owner
        .commit("batch-13", head, vec![request])
        .expect("commit interaction request");

    let decision_ref = owner.put_blob(b"approve").expect("publish decision");
    let resolve = |seq: u64, call: Option<ToolCallId>| {
        let mut resolved = fact(
            FactPayload::InteractionResolved(InteractionResolved {
                interaction_id: interaction_id.clone(),
                decision_ref: decision_ref.clone(),
                decision: None,
                resolved_by: ActorRef {
                    kind: ActorKind::User,
                    id: "probe-user".into(),
                    display_name: None,
                },
                resolution_seq: seq,
                resolved_at_ms: NOW_MS + 40,
            }),
            15,
            Some(turn_id.clone()),
        );
        resolved.interaction_id = Some(interaction_id.clone());
        resolved.call_id = call;
        resolved
    };
    let head = owner.head().expect("head");
    assert!(
        owner
            .commit("batch-14a", head, vec![resolve(2, Some(call_id.clone()))])
            .is_err(),
        "an answer with a stale revision must be rejected"
    );
    let head = owner.head().expect("head");
    assert!(
        owner
            .commit("batch-14b", head, vec![resolve(1, Some(call2.clone()))])
            .is_err(),
        "an answer targeting another call must be rejected"
    );
    let head = owner.head().expect("head");
    owner
        .commit("batch-14c", head, vec![resolve(1, Some(call_id.clone()))])
        .expect("commit valid answer");
    println!("ok: [8] interaction answers are revision- and target-checked");

    // ── Real gate against a local fake provider ─────────────────────────────
    let server = tiny_http::Server::http("127.0.0.1:0").expect("bind fake provider");
    let base_url = format!("http://{}", server.server_addr());
    std::thread::spawn(move || {
        for request in server.incoming_requests() {
            let header =
                tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"text/event-stream"[..])
                    .expect("static header");
            let _ = request.respond(tiny_http::Response::from_string(CHAT_SSE).with_header(header));
        }
    });
    let provider = qaqh_gate::ProviderConfig::chat(
        &base_url,
        "test-key",
        "probe-model",
        Some(qaqh_types::UserSendMode::Body),
        None,
        qaqh_types::ThinkingParamMode::OpenAi,
        qaqh_types::CacheTokenField::PromptCacheHitTokens,
        true,
        None,
    )
    .with_stream_usage(true);

    let head = owner.head().expect("head");
    let (_, provider_capability) = owner
        .commit_with_effect(
            "batch-15",
            head,
            vec![fact(
                FactPayload::ModelRoundStarted(round_started(&owner, &turn_id, 3, head)),
                16,
                Some(turn_id.clone()),
            )],
            &event_id(16),
        )
        .expect("commit third model intent");
    let provider_capability = provider_capability.expect("third round mints a capability");

    let (provider_handle, mut provider_mailbox) =
        Mailbox::<ProbeInput, ProbeInput, TaskCompletion<ProviderOutcome>>::new(
            LaneBudget::COMMANDS,
            LaneBudget::CONTROL,
            LaneBudget::COMPLETIONS,
        )
        .expect("provider mailbox");
    let mut agents: Supervisor<ProviderOutcome, CommittedExecution> =
        Supervisor::new(1).expect("provider supervisor");
    agents.adopt_actor(&provider_capability.identity().actor.clone());
    let provider_identity = provider_capability.identity().clone();
    let ticket = agents
        .authorize(provider_identity.clone(), provider_capability)
        .expect("authorize provider intent");
    agents
        .start(
            ticket,
            tools_reserve(&provider_handle),
            move |effect, cancel| provider_job::run(provider, effect, 64, cancel),
        )
        .expect("start provider job");
    let provider_completion = loop {
        match agents.collect(&provider_identity).await {
            Ok(completion) => break completion,
            Err(SupervisorError::NotFinished) => tokio::task::yield_now().await,
            Err(error) => panic!("provider collect failed: {error:?}"),
        }
    };
    agents
        .deliver(&provider_identity)
        .expect("deliver provider completion");
    let received = match provider_mailbox
        .recv()
        .await
        .expect("provider completion frame")
    {
        Input::Completion(completion) => completion,
        _ => panic!("unexpected mailbox input"),
    };
    let (live_message, live_finish, live_usage) = match &received.outcome {
        TaskOutcome::Returned(outcome) => {
            assert_eq!(outcome.evidence, DispatchEvidence::StartedTerminal);
            let message = outcome
                .message
                .clone()
                .expect("provider returned a message");
            assert_eq!(message.role, "assistant");
            (
                message,
                outcome.finish.unwrap_or(ProviderFinish::Unknown),
                outcome.usage.clone(),
            )
        }
        TaskOutcome::JoinFailed(error) => {
            panic!("provider task join failed: {error}")
        }
    };
    agents
        .acknowledge(&provider_identity, &provider_completion)
        .expect("acknowledge provider");
    println!("ok: [9] real gate round completed against a local provider");

    let live_ref = owner
        .put_blob(&serde_json::to_vec(&live_message).unwrap())
        .expect("publish live response blob");
    let head = owner.head().expect("head");
    owner
        .commit(
            "batch-16",
            head,
            vec![fact(
                FactPayload::ProviderResponseCommitted(ProviderResponseCommitted {
                    turn_id: turn_id.clone(),
                    round: 3,
                    attempt: 3,
                    message_ref: live_ref,
                    opaque_refs: vec![],
                    usage: live_usage,
                    finish: live_finish,
                    incomplete_reason: None,
                }),
                17,
                Some(turn_id.clone()),
            )],
        )
        .expect("commit live provider response");
    let final_prefix = owner.request_messages().expect("context").to_vec();
    assert_eq!(final_prefix.len(), 4);
    assert_eq!(final_prefix[3].role, "assistant");
    println!("ok: [10] live response is committed and replayable");

    // ── Fence: a dropped owner cannot be consumed ───────────────────────────
    let other_dir = temp.path().join("session-fenced");
    let (other_owner, fenced_capability, _, _) = mint_capability(&other_dir);
    drop(other_owner);
    assert!(matches!(
        fenced_capability.into_parts(),
        Err(FenceReason::OwnerGone)
    ));
    println!("ok: [11] dropped commit owner fences the capability");

    // ── Replay: reopening never reissues authority ──────────────────────────
    drop(owner);
    let mut reopened =
        SessionCommit::open(&dir, SessionId::new(SESSION), LogId::new(LOG), budget())
            .expect("reopen session");
    assert_eq!(
        serde_json::to_vec(reopened.request_messages().expect("recovered context")).unwrap(),
        serde_json::to_vec(final_prefix.as_slice()).unwrap(),
        "recovered request prefix must match the committed one"
    );
    let (_, replayed) = reopened
        .commit_with_effect("batch-5", 4, vec![round_fact], &event_id(5))
        .expect("replay batch");
    assert!(replayed.is_none(), "replay must never reissue authority");
    println!("ok: [12 & 13] replay of a committed batch issues no capability and preserves prefix");

    // ── P5–P8 Extended Fault Injection, Provider Golden & Real Tool Checks ──
    probe_batch_fault_injection_and_reconciliation(temp.path());
    probe_side_effect_settlement_retry_and_completion_identity(temp.path()).await;
    probe_mid_turn_crash_recovery(temp.path());
    probe_mailbox_control_and_unstoppable_blocking_task(temp.path()).await;
    probe_request_identity_incomplete_barrier_and_multimodal(temp.path()).await;
    probe_four_provider_protocols_and_opaque_replay_golden(temp.path()).await;
    probe_real_workspace_tool_port_permissions_quota_and_queued_inputs(temp.path()).await;

    println!("PROBE OK (22 checks)");
}
