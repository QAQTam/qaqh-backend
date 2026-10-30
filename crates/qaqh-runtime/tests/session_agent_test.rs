//! Integration tests for SessionAgent and AgentHost (RLC-3.1 / RLC-3.2).

use std::{
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    time::Duration,
};

use qaqh_runtime::{
    provider_job::ProviderRequestSpec,
    quota_ledger::{QuotaLedger, QuotaLimits},
    session_agent::{AgentHost, SessionPhase},
    tool_job::{ToolPolicySnapshot, WorkspaceToolPort},
};
use qaqh_session::{
    context_projection::ContextBudget,
    session_commit::SessionCommit,
    session_fact_v2::{
        EventId, FactPayload, FactSchema, InteractionId, LogId, SessionCreated, SessionFact, SessionId,
    },
};

const SESSION: &str = "0198f1a0-0000-7000-8000-000000000001";
const LOG: &str = "0198f1a0-0000-7000-8000-000000000002";

const CHAT_SSE: &str = concat!(
    "data: {\"choices\":[{\"delta\":{\"content\":\"hello from agent\"},\"finish_reason\":null}]}\n\n",
    "data: {\"choices\":[{\"delta\":{},\"finish_reason\":\"stop\"}],\"usage\":{\"prompt_tokens\":10,\"completion_tokens\":5,\"total_tokens\":15}}\n\n",
    "data: [DONE]\n\n",
);

fn budget() -> ContextBudget {
    ContextBudget {
        max_messages: 64,
        max_payload_bytes: 1 << 20,
        max_checkpoint_bytes: 1 << 20,
    }
}

const CHAT_TOOL_SSE: &str = concat!(
    "data: {\"id\":\"chat-1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_01J00000000000000000000101\",\"type\":\"function\",\"function\":{\"name\":\"read\",\"arguments\":\"{\\\"path\\\":\\\"hello.txt\\\"}\"}}]},\"finish_reason\":null}]}\n\n",
    "data: {\"id\":\"chat-1\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":20,\"completion_tokens\":8,\"total_tokens\":28}}\n\n",
    "data: [DONE]\n\n",
);

#[tokio::test]
async fn session_agent_tool_loop_and_multi_round() {
    let tmp = tempfile::tempdir().unwrap();
    let server = tiny_http::Server::http("127.0.0.1:0").expect("bind");
    let base_url = format!("http://{}", server.server_addr());
    let req_count = Arc::new(AtomicUsize::new(0));
    let req_count_srv = Arc::clone(&req_count);

    std::thread::spawn(move || {
        for request in server.incoming_requests() {
            let count = req_count_srv.fetch_add(1, Ordering::SeqCst);
            let sse = if count == 0 { CHAT_TOOL_SSE } else { CHAT_SSE };
            let header = tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"text/event-stream"[..]).unwrap();
            let _ = request.respond(tiny_http::Response::from_string(sse).with_header(header));
        }
    });

    let session_dir = tmp.path().join("session-agent-2");
    let mut commit = SessionCommit::create(
        &session_dir,
        SessionId::new("0198f1a0-0000-7000-8000-000000000003"),
        LogId::new("0198f1a0-0000-7000-8000-000000000004"),
        budget(),
    )
    .expect("create commit");

    let created_fact = SessionFact {
        schema: FactSchema::v2(),
        session_id: commit.session_id().clone(),
        log_id: commit.log_id().clone(),
        fact_seq: 0,
        event_id: EventId::new("01J00000000000000000000002"),
        ts_ms: 1000,
        causation_id: None,
        turn_id: None,
        call_id: None,
        interaction_id: None,
        payload: FactPayload::SessionCreated(SessionCreated {
            created_at_ms: 1000,
            cwd: "/tmp".into(),
            model: "test-model".into(),
            parent_session_id: None,
            schema_caps: vec![],
        }),
    };
    commit.commit("b-init", 0, vec![created_fact]).expect("init session");

    let provider = qaqh_gate::ProviderConfig::chat(
        &base_url,
        "sk-test",
        "test-model",
        None,
        None,
        qaqh_types::ThinkingParamMode::OpenAi,
        qaqh_types::CacheTokenField::PromptCacheHitTokens,
        true,
        None,
    );
    let spec = ProviderRequestSpec::new(provider, 256);

    let ws = tmp.path().join("ws2");
    std::fs::create_dir_all(&ws).unwrap();
    std::fs::write(ws.join("hello.txt"), "tool content").unwrap();

    let manager = Arc::new(Mutex::new(qaqh_workspace::registration::build_tool_manager(&[])));
    let policy = ToolPolicySnapshot::workspace_write(&ws);
    let ledger = Arc::new(Mutex::new(
        QuotaLedger::open_at(
            &tmp.path().join("quota2"),
            "0198f1a0-0000-7000-8000-000000000003",
            QuotaLimits::new(4, 4).expect("limits"),
        )
        .expect("open quota ledger"),
    ));
    let port = Arc::new(WorkspaceToolPort::new(
        "0198f1a0-0000-7000-8000-000000000003",
        manager,
        policy,
        Some(ledger),
    ));

    let (handle, task) = AgentHost::spawn(
        "0198f1a0-0000-7000-8000-000000000003".into(),
        commit,
        spec,
        port,
    )
    .expect("spawn agent host");

    handle
        .submit_user_input("run read tool", vec![], None)
        .expect("submit input");

    // Wait for 2 rounds (tool call -> tool result -> final stop)
    let mut finished = false;
    for _ in 0..100 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if req_count.load(Ordering::SeqCst) >= 2 {
            finished = true;
            break;
        }
        if task.is_finished() {
            let res = task.await.unwrap();
            panic!("agent task terminated prematurely with: {res:?}");
        }
    }
    assert!(finished, "two provider requests (tool round + final stop) must have executed");

    handle.shutdown().expect("shutdown");
    let join_res = task.await.expect("join agent task");
    assert!(join_res.is_ok(), "agent task shut down cleanly");
}

const ASK_TOOL_SSE: &str = concat!(
    "data: {\"id\":\"chat-ask\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{\"tool_calls\":[{\"index\":0,\"id\":\"call_01J00000000000000000000201\",\"type\":\"function\",\"function\":{\"name\":\"ask\",\"arguments\":\"{\\\"input\\\":\\\"Continue?\\\"}\"}}]},\"finish_reason\":null}]}\n\n",
    "data: {\"id\":\"chat-ask\",\"object\":\"chat.completion.chunk\",\"choices\":[{\"index\":0,\"delta\":{},\"finish_reason\":\"tool_calls\"}],\"usage\":{\"prompt_tokens\":20,\"completion_tokens\":8,\"total_tokens\":28}}\n\n",
    "data: [DONE]\n\n",
);

#[tokio::test]
async fn session_agent_ask_interaction_and_resolution() {
    let tmp = tempfile::tempdir().unwrap();
    let server = tiny_http::Server::http("127.0.0.1:0").expect("bind");
    let base_url = format!("http://{}", server.server_addr());
    let req_count = Arc::new(AtomicUsize::new(0));
    let req_count_srv = Arc::clone(&req_count);

    std::thread::spawn(move || {
        for request in server.incoming_requests() {
            let count = req_count_srv.fetch_add(1, Ordering::SeqCst);
            let sse = if count == 0 { ASK_TOOL_SSE } else { CHAT_SSE };
            let header = tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"text/event-stream"[..]).unwrap();
            let _ = request.respond(tiny_http::Response::from_string(sse).with_header(header));
        }
    });

    let session_dir = tmp.path().join("session-agent-ask");
    let mut commit = SessionCommit::create(
        &session_dir,
        SessionId::new("0198f1a0-0000-7000-8000-000000000005"),
        LogId::new("0198f1a0-0000-7000-8000-000000000006"),
        budget(),
    )
    .expect("create commit");

    let created_fact = SessionFact {
        schema: FactSchema::v2(),
        session_id: commit.session_id().clone(),
        log_id: commit.log_id().clone(),
        fact_seq: 0,
        event_id: EventId::new("01J00000000000000000000003"),
        ts_ms: 1000,
        causation_id: None,
        turn_id: None,
        call_id: None,
        interaction_id: None,
        payload: FactPayload::SessionCreated(SessionCreated {
            created_at_ms: 1000,
            cwd: "/tmp".into(),
            model: "test-model".into(),
            parent_session_id: None,
            schema_caps: vec![],
        }),
    };
    commit.commit("b-init", 0, vec![created_fact]).expect("init session");

    let provider = qaqh_gate::ProviderConfig::chat(
        &base_url,
        "sk-test",
        "test-model",
        None,
        None,
        qaqh_types::ThinkingParamMode::OpenAi,
        qaqh_types::CacheTokenField::PromptCacheHitTokens,
        true,
        None,
    );
    let spec = ProviderRequestSpec::new(provider, 256);

    let ws = tmp.path().join("ws3");
    std::fs::create_dir_all(&ws).unwrap();
    let manager = Arc::new(Mutex::new(qaqh_workspace::registration::build_tool_manager(&[])));
    let policy = ToolPolicySnapshot::workspace_write(&ws);
    let ledger = Arc::new(Mutex::new(
        QuotaLedger::open_at(
            &tmp.path().join("quota3"),
            "0198f1a0-0000-7000-8000-000000000005",
            QuotaLimits::new(4, 4).expect("limits"),
        )
        .expect("open quota ledger"),
    ));
    let port = Arc::new(WorkspaceToolPort::new(
        "0198f1a0-0000-7000-8000-000000000005",
        manager,
        policy,
        Some(ledger),
    ));

    let (handle, task) = AgentHost::spawn(
        "0198f1a0-0000-7000-8000-000000000005".into(),
        commit,
        spec,
        port,
    )
    .expect("spawn agent host");

    handle
        .submit_user_input("start ask turn", vec![], None)
        .expect("submit input");

    // Wait until ask interaction request is committed in the log
    let mut interaction_id = None;
    let batches_file = session_dir.join("committed-batches").join("batches.jsonl");
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if task.is_finished() {
            let res = task.await.unwrap();
            panic!("agent task terminated with: {res:?}");
        }
        if let Ok(content) = std::fs::read_to_string(&batches_file) {
            for line in content.lines() {
                if let Ok(val) = serde_json::from_str::<serde_json::Value>(line) {
                    if let Some(facts) = val.get("facts").and_then(|f| f.as_array()) {
                        for f in facts {
                            if f.get("payload")
                                .and_then(|p| p.get("kind"))
                                .and_then(|k| k.as_str())
                                == Some("interaction_requested")
                            {
                                if let Some(id_str) = f.get("interaction_id").and_then(|i| i.as_str()) {
                                    interaction_id = Some(InteractionId::new(id_str));
                                    break;
                                }
                            }
                        }
                    }
                }
            }
            if interaction_id.is_some() {
                break;
            }
        }
    }
    let interaction_id = interaction_id.expect("interaction must be requested");

    // Resolve interaction
    handle
        .resolve_interaction(interaction_id, vec![("q1".into(), "yes".into())])
        .expect("resolve interaction");

    // Wait for the full turn to finish
    let mut finished = false;
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if task.is_finished() {
            let res = task.await.unwrap();
            panic!("agent task terminated with: {res:?}");
        }
        if req_count.load(Ordering::SeqCst) >= 2 {
            finished = true;
            break;
        }
    }
    assert!(finished, "two provider requests must have executed");

    handle.shutdown().expect("shutdown");
    let join_res = task.await.expect("join agent task");
    assert!(join_res.is_ok(), "agent task shut down cleanly");
}

#[tokio::test]
async fn session_agent_turn_lifecycle_and_shutdown() {
    let tmp = tempfile::tempdir().unwrap();
    let server = tiny_http::Server::http("127.0.0.1:0").expect("bind");
    let base_url = format!("http://{}", server.server_addr());
    let req_count = Arc::new(AtomicUsize::new(0));
    let req_count_srv = Arc::clone(&req_count);

    std::thread::spawn(move || {
        for request in server.incoming_requests() {
            req_count_srv.fetch_add(1, Ordering::SeqCst);
            let header = tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"text/event-stream"[..]).unwrap();
            let _ = request.respond(tiny_http::Response::from_string(CHAT_SSE).with_header(header));
        }
    });

    let session_dir = tmp.path().join("session-agent-1");
    // Production materializes the identity sidecar before the first commit.
    let identity = qaqh_session::canonical::CanonicalSessionIdentity::open_or_create(&session_dir)
        .expect("identity");
    let mut commit = SessionCommit::create(
        &session_dir,
        identity.session_id.clone(),
        identity.log_id.clone(),
        budget(),
    )
    .expect("create commit");

    let created_fact = SessionFact {
        schema: FactSchema::v2(),
        session_id: commit.session_id().clone(),
        log_id: commit.log_id().clone(),
        fact_seq: 0,
        event_id: EventId::new("01J00000000000000000000001"),
        ts_ms: 1000,
        causation_id: None,
        turn_id: None,
        call_id: None,
        interaction_id: None,
        payload: FactPayload::SessionCreated(SessionCreated {
            created_at_ms: 1000,
            cwd: "/tmp".into(),
            model: "test-model".into(),
            parent_session_id: None,
            schema_caps: vec![],
        }),
    };
    commit.commit("b-init", 0, vec![created_fact]).expect("init session");

    let provider = qaqh_gate::ProviderConfig::chat(
        &base_url,
        "sk-test",
        "test-model",
        None,
        None,
        qaqh_types::ThinkingParamMode::OpenAi,
        qaqh_types::CacheTokenField::PromptCacheHitTokens,
        true,
        None,
    );
    let spec = ProviderRequestSpec::new(provider, 256);

    let ws = tmp.path().join("ws");
    std::fs::create_dir_all(&ws).unwrap();
    let manager = Arc::new(Mutex::new(qaqh_workspace::registration::build_tool_manager(&[])));
    let policy = ToolPolicySnapshot::workspace_write(&ws);
    let ledger = Arc::new(Mutex::new(
        QuotaLedger::open_at(
            &tmp.path().join("quota"),
            identity.session_id.as_str(),
            QuotaLimits::new(4, 4).expect("limits"),
        )
        .expect("open quota ledger"),
    ));
    let port = Arc::new(WorkspaceToolPort::new(
        identity.session_id.as_str(),
        manager,
        policy,
        Some(ledger),
    ));

    let (handle, task) = AgentHost::spawn(
        identity.session_id.as_str().to_string(),
        commit,
        spec,
        port,
    )
    .expect("spawn agent host");

    // Submit user input
    let input_id = handle
        .submit_user_input("hello agent", vec![], None)
        .expect("submit input");
    assert!(!input_id.as_str().is_empty());

    // Wait for the turn to complete
    let mut finished = false;
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if req_count.load(Ordering::SeqCst) >= 1 {
            finished = true;
            break;
        }
        if task.is_finished() {
            let res = task.await.unwrap();
            panic!("agent task terminated prematurely with: {res:?}");
        }
    }
    assert!(finished, "provider request must have executed");

    // Clean shutdown
    handle.shutdown().expect("shutdown");
    let join_res = task.await.expect("join agent task");
    assert!(join_res.is_ok(), "agent task shut down cleanly");

    // Reopen the journal: the answer must be a durable display fact, the turn
    // must be settled, and nothing may be left for the next owner to reconcile.
    let reopened = SessionCommit::open(
        &session_dir,
        identity.session_id.clone(),
        identity.log_id.clone(),
        budget(),
    )
    .expect("reopen commit");
    let snapshot = reopened.snapshot().expect("projection snapshot");
    let answer = snapshot
        .conversation
        .context
        .iter()
        .find_map(|entry| match &entry.kind {
            qaqh_session::projection::ConversationContextKind::AssistantBlock(block)
                if block.block_kind == qaqh_session::session_fact_v2::AssistantBlockKind::Answer =>
            {
                Some(block)
            }
            _ => None,
        })
        .expect("assistant answer block must be projected");
    match &answer.content {
        qaqh_session::session_fact_v2::ContentValue::Ref { content_ref } => {
            let bytes = reopened
                .get_blob(content_ref, 4096)
                .expect("read answer blob");
            assert_eq!(
                String::from_utf8_lossy(&bytes),
                "hello from agent",
                "assistant text must survive in the journal"
            );
        }
        other => panic!("unexpected answer content: {other:?}"),
    }
    let obligations = reopened.obligations().expect("obligations");
    assert!(obligations.active_turn.is_none(), "turn must be settled");
    // Ringing v2 must read the same batch journal: bootstrap used to require
    // the legacy `events.commit.json` and reported `SnapshotsMissing` here.
    let hub = qaqh_runtime::ringing::V2ProjectionHub::new("session-agent-test");
    let bootstrap = hub
        .bootstrap(&session_dir, identity.session_id.as_str())
        .expect("v2 bootstrap must resolve a committed batch journal");
    assert!(
        bootstrap.last_fact_seq > 1,
        "bootstrap cursor must cover the committed turn: {bootstrap:?}"
    );
    assert!(
        bootstrap
            .projections
            .conversation
            .context
            .iter()
            .any(|entry| matches!(
                entry.kind,
                qaqh_session::projection::ConversationContextKind::AssistantBlock(_)
            )),
        "bootstrap projections must expose the sealed answer"
    );
    assert!(
        obligations.unsettled_tools.is_empty() && obligations.unsettled_interactions.is_empty(),
        "no open work may remain: {obligations:?}"
    );
}

#[tokio::test]
async fn session_agent_cancel_turn_and_recover() {
    let tmp = tempfile::tempdir().unwrap();
    let server = tiny_http::Server::http("127.0.0.1:0").expect("bind");
    let base_url = format!("http://{}", server.server_addr());

    std::thread::spawn(move || {
        for request in server.incoming_requests() {
            // Intentionally block or delay response so cancel takes effect during active provider request
            std::thread::sleep(Duration::from_millis(500));
            let header = tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"text/event-stream"[..]).unwrap();
            let _ = request.respond(tiny_http::Response::from_string(CHAT_SSE).with_header(header));
        }
    });

    let session_dir = tmp.path().join("session-agent-cancel");
    let mut commit = SessionCommit::create(
        &session_dir,
        SessionId::new("0198f1a0-0000-7000-8000-000000000007"),
        LogId::new("0198f1a0-0000-7000-8000-000000000008"),
        budget(),
    )
    .expect("create commit");

    let created_fact = SessionFact {
        schema: FactSchema::v2(),
        session_id: commit.session_id().clone(),
        log_id: commit.log_id().clone(),
        fact_seq: 0,
        event_id: EventId::new("01J00000000000000000000004"),
        ts_ms: 1000,
        causation_id: None,
        turn_id: None,
        call_id: None,
        interaction_id: None,
        payload: FactPayload::SessionCreated(SessionCreated {
            created_at_ms: 1000,
            cwd: "/tmp".into(),
            model: "test-model".into(),
            parent_session_id: None,
            schema_caps: vec![],
        }),
    };
    commit.commit("b-init", 0, vec![created_fact]).expect("init session");

    let provider = qaqh_gate::ProviderConfig::chat(
        &base_url,
        "sk-test",
        "test-model",
        None,
        None,
        qaqh_types::ThinkingParamMode::OpenAi,
        qaqh_types::CacheTokenField::PromptCacheHitTokens,
        true,
        None,
    );
    let spec = ProviderRequestSpec::new(provider, 256);

    let ws = tmp.path().join("ws4");
    std::fs::create_dir_all(&ws).unwrap();
    let manager = Arc::new(Mutex::new(qaqh_workspace::registration::build_tool_manager(&[])));
    let policy = ToolPolicySnapshot::workspace_write(&ws);
    let ledger = Arc::new(Mutex::new(
        QuotaLedger::open_at(
            &tmp.path().join("quota4"),
            "0198f1a0-0000-7000-8000-000000000007",
            QuotaLimits::new(4, 4).expect("limits"),
        )
        .expect("open quota ledger"),
    ));
    let port = Arc::new(WorkspaceToolPort::new(
        "0198f1a0-0000-7000-8000-000000000007",
        manager,
        policy,
        Some(ledger),
    ));

    let (handle, task) = AgentHost::spawn(
        "0198f1a0-0000-7000-8000-000000000007".into(),
        commit,
        spec,
        port,
    )
    .expect("spawn agent host");

    handle
        .submit_user_input("prompt to cancel", vec![], None)
        .expect("submit input");

    // Wait slightly so request starts, then cancel
    tokio::time::sleep(Duration::from_millis(50)).await;
    handle.cancel(None, "user abort").expect("cancel");

    // Wait for cancel to settle TurnInterrupted
    let batches_file = session_dir.join("committed-batches").join("batches.jsonl");
    let mut interrupted = false;
    for _ in 0..50 {
        tokio::time::sleep(Duration::from_millis(50)).await;
        if let Ok(content) = std::fs::read_to_string(&batches_file) {
            if content.contains("turn_interrupted") {
                interrupted = true;
                break;
            }
        }
    }
    assert!(interrupted, "TurnInterrupted must be committed");

    handle.shutdown().expect("shutdown");
    let join_res = task.await.expect("join agent task");
    assert!(join_res.is_ok(), "agent task shut down cleanly");
}

#[tokio::test]
async fn session_agent_recovers_an_open_turn_after_crash() {
    // A crashed owner leaves an open turn, an open model round and an
    // unresolved in-flight effect. The next owner must settle them and accept
    // new input instead of failing with "turn already active".
    let tmp = tempfile::tempdir().unwrap();
    let server = tiny_http::Server::http("127.0.0.1:0").expect("bind");
    let base_url = format!("http://{}", server.server_addr());
    let req_count = Arc::new(AtomicUsize::new(0));
    let req_count_srv = Arc::clone(&req_count);
    std::thread::spawn(move || {
        for request in server.incoming_requests() {
            req_count_srv.fetch_add(1, Ordering::SeqCst);
            std::thread::sleep(Duration::from_millis(300));
            let header =
                tiny_http::Header::from_bytes(&b"Content-Type"[..], &b"text/event-stream"[..])
                    .unwrap();
            let _ = request.respond(tiny_http::Response::from_string(CHAT_SSE).with_header(header));
        }
    });

    let session_dir = tmp.path().join("session-agent-recover");
    let session_id = SessionId::new("0198f1a0-0000-7000-8000-000000000009");
    let log_id = LogId::new("0198f1a0-0000-7000-8000-00000000000a");
    let mut commit = SessionCommit::create(&session_dir, session_id.clone(), log_id.clone(), budget())
        .expect("create commit");
    let created_fact = SessionFact {
        schema: FactSchema::v2(),
        session_id: commit.session_id().clone(),
        log_id: commit.log_id().clone(),
        fact_seq: 0,
        event_id: EventId::new("01J00000000000000000000009"),
        ts_ms: 1000,
        causation_id: None,
        turn_id: None,
        call_id: None,
        interaction_id: None,
        payload: FactPayload::SessionCreated(SessionCreated {
            created_at_ms: 1000,
            cwd: "/tmp".into(),
            model: "test-model".into(),
            parent_session_id: None,
            schema_caps: vec![],
        }),
    };
    commit.commit("b-init", 0, vec![created_fact]).expect("init session");

    let provider = qaqh_gate::ProviderConfig::chat(
        &base_url,
        "sk-test",
        "test-model",
        None,
        None,
        qaqh_types::ThinkingParamMode::OpenAi,
        qaqh_types::CacheTokenField::PromptCacheHitTokens,
        true,
        None,
    );
    let ws = tmp.path().join("ws-recover");
    std::fs::create_dir_all(&ws).unwrap();
    let policy = || ToolPolicySnapshot::workspace_write(&ws);
    let port = || {
        Arc::new(WorkspaceToolPort::new(
            session_id.as_str(),
            Arc::new(Mutex::new(qaqh_workspace::registration::build_tool_manager(&[]))),
            policy(),
            None,
        ))
    };

    let (handle, task) = AgentHost::spawn(
        session_id.as_str().to_string(),
        commit,
        ProviderRequestSpec::new(provider.clone(), 256),
        port(),
    )
    .expect("spawn first host");
    handle
        .submit_user_input("first turn", vec![], None)
        .expect("submit first input");
    for _ in 0..100 {
        if req_count.load(Ordering::SeqCst) >= 1 {
            break;
        }
        tokio::time::sleep(Duration::from_millis(20)).await;
    }
    // Crash the owner mid-flight: no shutdown, no settlement.
    task.abort();
    let _ = task.await;

    let reopened = SessionCommit::open(&session_dir, session_id.clone(), log_id.clone(), budget())
        .expect("reopen after crash");
    let obligations = reopened.obligations().expect("obligations after crash");
    assert!(
        obligations.active_turn.is_some(),
        "the crash must leave an open turn for recovery"
    );
    drop(reopened);

    let (handle, task) = AgentHost::spawn(
        session_id.as_str().to_string(),
        SessionCommit::open(&session_dir, session_id.clone(), log_id.clone(), budget())
            .expect("reopen for respawn"),
        ProviderRequestSpec::new(provider, 256),
        port(),
    )
    .expect("respawn host");
    handle
        .submit_user_input("second turn", vec![], None)
        .expect("submit second input");

    let mut completed = false;
    for _ in 0..200 {
        tokio::time::sleep(Duration::from_millis(40)).await;
        if task.is_finished() {
            let res = task.await.expect("join");
            panic!("recovered agent terminated: {res:?}");
        }
        if req_count.load(Ordering::SeqCst) >= 2 {
            completed = true;
            break;
        }
    }
    assert!(completed, "recovered owner must serve the new turn");

    handle.shutdown().expect("shutdown");
    assert!(
        task.await.expect("join recovered agent").is_ok(),
        "recovered agent must shut down cleanly"
    );

    let settled = SessionCommit::open(&session_dir, session_id, log_id, budget())
        .expect("reopen settled");
    let obligations = settled.obligations().expect("obligations after recovery");
    assert!(!obligations.has_open_work(), "recovery must settle every obligation");
}
