use std::sync::{Arc, Mutex};
use std::time::Duration;

use qaqh_client::v3::V3Client;
use qaqh_core_vocab::{GoalStatus, SessionId};
use qaqh_ringing_contract::fact::{ActorKind, ActorRef, BlockId, ContentValue, InputPurpose, TurnId};
use qaqh_ringing_contract::state::TimelineEntry;
use qaqh_ringing_runtime::{
    DeviceRegistry, PendingCommandStore, RingingHub, pairing::PairingTable,
};
use qaqh_ringing_v3::{
    AccountRequest, AccountResponse, BlockKind, ControlRequest, ControlResponse, SessionEvent,
    SessionRequest, SessionResponse,
};
use qaqh_runtime::QaqhService;
use qaqh_session::db::store::{NewBlock, NewSession, append_block, create_session, start_turn};
use qaqh_v3_transport::bind_local_ipc;
use tokio::sync::watch;

use super::RealBackend;
use crate::v3_server::state::{ServerConfig, ServerState};

fn insert_session(service: &QaqhService, session_id: &str, title: &str, cwd: &str) {
    let session_id = session_id.to_string();
    let title = title.to_string();
    let cwd = cwd.to_string();
    service
        .owners()
        .db()
        .write_blocking(move |conn| {
            create_session(
                conn,
                &NewSession {
                    session_id,
                    parent_session_id: None,
                    title,
                    title_source: Some("user".to_string()),
                    cwd: Some(cwd),
                    model: None,
                    tool_mode: Some("standard".to_string()),
                    model_selection: None,
                    custom_tools: None,
                    frozen_annotation: None,
                    ephemeral: false,
                },
            )
        })
        .expect("create persisted session");
}

#[tokio::test]
async fn real_backend_serves_accounts_dynamic_sessions_direct_attach_and_compaction() {
    let temp = tempfile::tempdir().expect("tempdir");
    let service = QaqhService::init(temp.path().to_path_buf());
    let cwd = temp.path().to_string_lossy().into_owned();
    let initial_id = "01JINITIALV3SESSION0000000";
    insert_session(&service, initial_id, "initial", &cwd);

    // Seed an input block + a compaction block on initial_id so snapshot/page verify compaction separation.
    let input_entry = TimelineEntry::Input {
        block_id: BlockId::new("blk-input-1"),
        turn_id: Some(TurnId::new("turn-1")),
        purpose: InputPurpose::TriggerTurn,
        content: ContentValue::Inline {
            text: "hello".to_string(),
        },
        attachments: Vec::new(),
        images: Vec::new(),
        inter_agent: None,
        subagent_terminal: None,
        actor: ActorRef {
            kind: ActorKind::User,
            id: "u1".into(),
            display_name: None,
        },
    };
    let compaction_entry = TimelineEntry::Compaction {
        summary: ContentValue::Inline {
            text: "pinned summary".to_string(),
        },
    };
    let input_json = serde_json::to_string(&input_entry).expect("serialize input");
    let compaction_json = serde_json::to_string(&compaction_entry).expect("serialize compaction");
    service
        .owners()
        .db()
        .write_blocking(move |conn| {
            start_turn(conn, "turn-1", initial_id, None)?;
            append_block(
                conn,
                &NewBlock {
                    block_id: "blk-input-1".into(),
                    session_id: initial_id.into(),
                    turn_id: Some("turn-1".into()),
                    kind: "input",
                    text: None,
                    call_id: None,
                    payload_json: input_json,
                },
            )?;
            append_block(
                conn,
                &NewBlock {
                    block_id: "compaction-1".into(),
                    session_id: initial_id.into(),
                    turn_id: None,
                    kind: "compaction",
                    text: None,
                    call_id: None,
                    payload_json: compaction_json,
                },
            )?;
            Ok::<_, qaqh_session::db::DbError>(())
        })
        .expect("seed blocks");

    let hub = Arc::new(RingingHub::with_database(
        "test-epoch",
        Arc::clone(service.owners().db()),
    ));
    let devices = Arc::new(DeviceRegistry::new(Arc::clone(service.owners().db())));
    let config = ServerConfig {
        build_id: "real-reg-test".to_string(),
        server_instance_id: "real-reg-instance".to_string(),
        channel: "dev".to_string(),
        ..Default::default()
    };
    let os_user =
        std::env::var(if cfg!(windows) { "USERNAME" } else { "USER" }).expect("local OS user");
    let socket = super::local_ipc_path(temp.path());
    let endpoint =
        crate::v3_server::endpoint::local_ipc_endpoint(&config, socket.to_string_lossy());
    crate::v3_server::endpoint::publish(temp.path(), &endpoint).expect("publish endpoint");

    let state = ServerState::new(config, os_user);
    let backend = RealBackend::new(
        service.clone(),
        Arc::clone(&state),
        Arc::clone(&hub),
        Arc::new(Mutex::new(PairingTable::new())),
        Arc::clone(&devices),
        watch::channel(false).0,
        Arc::new(Mutex::new(PendingCommandStore::new(Arc::clone(
            service.owners().db(),
        )))),
    )
    .await
    .expect("real backend");

    let mut listener = bind_local_ipc(&socket).await.expect("bind UDS");
    let server = tokio::spawn(async move {
        let transport = listener.accept().await.expect("accept client");
        crate::v3_server::V3Server::with_backend(transport, state, backend)
            .serve()
            .await
    });

    let client = V3Client::connect_local(
        temp.path(),
        Some("real-reg-test"),
        "dev",
        "real-reg-client",
        false,
    )
    .await
    .expect("connect_local");

    // 1. Account RPCs over real RealBackend decode cleanly into AccountResponse variants.
    assert!(matches!(
        client
            .call(ControlRequest::Account(AccountRequest::SiwcStatus))
            .await
            .expect("siwc.status"),
        ControlResponse::Account(AccountResponse::SiwcStatus(_))
    ));
    assert!(matches!(
        client
            .call(ControlRequest::Account(AccountRequest::SiwcProfiles))
            .await
            .expect("siwc.profiles"),
        ControlResponse::Account(AccountResponse::SiwcProfiles(_))
    ));
    let ds_status = match client
        .call(ControlRequest::Account(AccountRequest::DsStatus))
        .await
        .expect("dsaccount.status")
    {
        ControlResponse::Account(AccountResponse::DsStatus(view)) => view,
        other => panic!("unexpected DsStatus response: {other:?}"),
    };
    let attempt_id = ds_status
        .flow
        .attempt_id
        .unwrap_or_else(|| "probe-idle".to_string());
    assert!(matches!(
        client
            .call(ControlRequest::Account(AccountRequest::DsSignInPoll {
                attempt_id
            }))
            .await
            .expect("dsaccount.sign_in.status"),
        ControlResponse::Account(AccountResponse::DsSignInPoll(_))
    ));

    // 2. Compaction is pinned separately and excluded from tail_blocks & TranscriptPage.
    let initial_sid = SessionId::new(initial_id).expect("valid id");
    let snapshot = match client
        .call(ControlRequest::Session(
            SessionRequest::GetSessionSnapshot {
                session_id: initial_sid.clone(),
                tail_blocks: 50,
                tail_turns: 20,
            },
        ))
        .await
        .expect("snapshot")
    {
        ControlResponse::Session(SessionResponse::Snapshot(snap)) => snap,
        other => panic!("unexpected snapshot: {other:?}"),
    };
    assert_eq!(
        snapshot
            .pinned_compaction
            .as_ref()
            .map(|c| c.summary.as_str()),
        Some("pinned summary")
    );
    assert_eq!(snapshot.tail_blocks.len(), 1);
    assert_eq!(snapshot.tail_blocks[0].kind, BlockKind::User);

    let page = match client
        .call(ControlRequest::Session(SessionRequest::TranscriptPage {
            session_id: initial_sid,
            before_ord: 0,
            limit: 50,
            timeline_epoch: snapshot.timeline_epoch,
        }))
        .await
        .expect("page")
    {
        ControlResponse::Session(SessionResponse::Page(page)) => page,
        other => panic!("unexpected page: {other:?}"),
    };
    assert_eq!(page.blocks.len(), 1);
    assert_eq!(page.blocks[0].kind, BlockKind::User);

    // 3. Session created in SQLite AFTER RealBackend::new is discoverable via List & Snapshot.
    let dynamic_id = "01JDYNAMICV3SESSION0000000";
    insert_session(&service, dynamic_id, "dynamic child", &cwd);
    let dynamic_sid = SessionId::new(dynamic_id).expect("valid dynamic id");

    let sessions = match client
        .call(ControlRequest::Session(SessionRequest::List {
            include_archived: true,
        }))
        .await
        .expect("list after dynamic create")
    {
        ControlResponse::Session(SessionResponse::Sessions(list)) => list,
        other => panic!("unexpected list: {other:?}"),
    };
    assert!(
        sessions
            .iter()
            .any(|session| session.session_id == dynamic_sid),
        "dynamically created session must appear in V3 list"
    );
    assert!(matches!(
        client
            .call(ControlRequest::Session(
                SessionRequest::GetSessionSnapshot {
                    session_id: dynamic_sid.clone(),
                    tail_blocks: 10,
                    tail_turns: 5,
                },
            ))
            .await
            .expect("dynamic snapshot"),
        ControlResponse::Session(SessionResponse::Snapshot(_))
    ));

    // 4. Direct attach_session on a dormant session without prior SessionRequest::Attach starts its projector.
    let mut events = client
        .attach_session(&dynamic_sid, 0)
        .await
        .expect("direct attach_session on dormant session");
    client
        .call(ControlRequest::Session(SessionRequest::SetGoal {
            session_id: dynamic_sid,
            objective: "direct stream attach".into(),
            status: Some(GoalStatus::Active),
            token_budget: None,
        }))
        .await
        .expect("set goal");
    let goal = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let frame = events.next_frame().await.expect("event frame");
            if let SessionEvent::GoalUpdated {
                goal: Some(goal), ..
            } = frame.event
            {
                break goal;
            }
        }
    })
    .await
    .expect("direct attach_session must receive projected live events");
    assert_eq!(goal.objective, "direct stream attach");

    drop(events);
    drop(client);
    server.await.expect("server task").expect("server loop");
    crate::v3_server::endpoint::retire(temp.path()).expect("retire endpoint");
}

#[tokio::test]
async fn p3_5e_gap_projections_preserve_rich_fields_deltas_images_and_ms_timestamps() {
    use base64::Engine as _;
    use qaqh_core_vocab::{
        ContentRef, CredentialSource, InputId, ToolCallId, Wire,
    };
    use qaqh_ringing_contract::commands::{ImageBlock, InterAgentEnvelope};
    use qaqh_ringing_contract::fact::{
        AssistantBlockKind, ContentRef as OldRef, InterAgentDelivery, InteractionChoice,
        InteractionChoiceTone, InteractionDecision, InteractionId, InteractionKind, ToolMetrics,
    };
    use qaqh_ringing_contract::state::{
        CodeDelta, ExecSessionState, InteractionView as OldInteractionView,
        PermissionRequestDetails, ToolCallState, ToolEffectInfo,
    };
    use qaqh_ringing_v3::{
        BlockActor, ChoiceTone, CommandRequest, CommandResponse, ConfigRequest, ConfigResponse,
        ExecSessionStateView, ImageRefView, InteractionDecisionKind, ToolCallStatus,
        ToolEffectView,
    };
    use qaqh_session::db::store::{
        ExecSessionRow, NewInteraction, ToolCallFinish, ToolCallUpsert, create_interaction,
        finish_tool_call, upsert_exec_session, upsert_tool_call,
    };

    let temp = tempfile::tempdir().expect("tempdir");
    let service = QaqhService::init(temp.path().to_path_buf());
    let cwd = temp.path().to_string_lossy().into_owned();
    let session_id = "01JP35EGAPREGRESSION000000";
    let custom_tools = vec!["read".to_string(), "edit".to_string()];
    let custom_tools_clone = custom_tools.clone();
    let cwd_clone = cwd.clone();
    service
        .owners()
        .db()
        .write_blocking(move |conn| {
            create_session(
                conn,
                &NewSession {
                    session_id: session_id.to_string(),
                    parent_session_id: None,
                    title: "gap check".to_string(),
                    title_source: Some("user".to_string()),
                    cwd: Some(cwd_clone),
                    model: None,
                    tool_mode: Some("custom".to_string()),
                    model_selection: None,
                    custom_tools: Some(custom_tools_clone),
                    frozen_annotation: None,
                    ephemeral: false,
                },
            )?;
            conn.execute(
                "UPDATE sessions SET created_at_ms = 1700000000123, updated_at_ms = 1700000005456 WHERE session_id = ?1",
                [session_id],
            )?;
            Ok::<_, qaqh_session::db::DbError>(())
        })
        .expect("create session with custom_tools and ms timestamps");

    // 1x1 valid PNG bytes.
    let png_bytes: Vec<u8> = vec![
        0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a, 0x00, 0x00, 0x00, 0x0d, b'I', b'H', b'D',
        b'R', 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x00, 0x01, 0x08, 0x06, 0x00, 0x00, 0x00, 0x1f,
        0x15, 0xc4, 0x89, 0x00, 0x00, 0x00, 0x0a, b'I', b'D', b'A', b'T', 0x78, 0x9c, 0x63, 0x00,
        0x01, 0x00, 0x00, 0x05, 0x00, 0x01, 0x0d, 0x0a, 0x2d, 0xb4, 0x00, 0x00, 0x00, 0x00, b'I',
        b'E', b'N', b'D', 0xae, 0x42, 0x60, 0x82,
    ];
    let png_b64 = base64::engine::general_purpose::STANDARD.encode(&png_bytes);
    let hub = Arc::new(RingingHub::with_database(
        "test-epoch",
        Arc::clone(service.owners().db()),
    ));
    let attach_hex = hub
        .put_content_async(session_id, "text/plain", b"hello attachment".to_vec(), false)
        .await
        .expect("store attachment");

    let input_entry = TimelineEntry::Input {
        block_id: BlockId::new("blk-u1"),
        turn_id: Some(TurnId::new("turn-gap-1")),
        purpose: InputPurpose::Steer,
        content: ContentValue::Inline {
            text: "check image".into(),
        },
        attachments: vec![OldRef::new(qaqh_ringing_contract::fact::ContentHash::new(
            format!("sha256:{attach_hex}"),
        ))],
        images: vec![ImageBlock {
            mime_type: "image/png".into(),
            data: png_b64,
        }],
        inter_agent: Some(InterAgentEnvelope {
            message_id: "msg-1".into(),
            root_session_id: session_id.into(),
            author: "/root".into(),
            recipient: "/root/worker".into(),
            other_recipients: vec!["/root/obs".into()],
            task_id: Some("task-1".into()),
            reply_to: None,
            causation_id: None,
            delivery: InterAgentDelivery::Steer,
            created_at_ms: 1234,
        }),
        subagent_terminal: None,
        actor: ActorRef {
            kind: ActorKind::Agent,
            id: "/root".into(),
            display_name: None,
        },
    };
    let answer_entry = TimelineEntry::AssistantBlock {
        block_id: BlockId::new("blk-a1"),
        turn_id: TurnId::new("turn-gap-1"),
        ord: 2,
        block_kind: AssistantBlockKind::Answer,
        content: ContentValue::Inline {
            text: "done".into(),
        },
        model: "deepseek-chat".into(),
        usage: Some(qaqh_types::UsageInfo {
            prompt_tokens: 100,
            completion_tokens: 20,
            total_tokens: 120,
            prompt_cache_hit_tokens: 80,
            prompt_cache_miss_tokens: 40,
            reasoning_tokens: 5,
            cache_usage_reported: Some(true),
            extras: Default::default(),
        }),
        generated_ms: Some(321),
        opaque: vec![],
    };
    let tool_block = TimelineEntry::ToolCall {
        call_id: qaqh_ringing_contract::fact::ToolCallId::new("call-gap-1"),
        turn_id: Some(TurnId::new("turn-gap-1")),
    };
    let perm_details = PermissionRequestDetails {
        tool_name: "write".into(),
        action_summary: Some("Write Cargo.toml".into()),
        justification: Some("Need dependency".into()),
        reason: "Requires write approval".into(),
        paths: vec!["Cargo.toml".into()],
        category: "write".into(),
        level: 2,
        risk: "medium".into(),
        consequence: "Modifies Cargo.toml".into(),
    };
    let perm_interaction = OldInteractionView {
        interaction_id: InteractionId::new("int-gap-1"),
        call_id: Some(qaqh_ringing_contract::fact::ToolCallId::new("call-gap-1")),
        turn_id: Some(TurnId::new("turn-gap-1")),
        kind: InteractionKind::Permission,
        request: ContentValue::Inline {
            text: serde_json::to_string(&perm_details).expect("perm json"),
        },
        choices: vec![InteractionChoice {
            decision: InteractionDecision::Approved,
            label: "Allow".into(),
            description: Some("Once".into()),
            tone: InteractionChoiceTone::Primary,
        }],
        expires_at_ms: Some(9_999),
        resolution: None,
    };

    let input_json = serde_json::to_string(&input_entry).expect("input json");
    let answer_json = serde_json::to_string(&answer_entry).expect("answer json");
    let tool_json = serde_json::to_string(&tool_block).expect("tool json");
    let perm_json = serde_json::to_string(&perm_interaction).expect("perm json");
    service
        .owners()
        .db()
        .write_blocking(move |conn| {
            start_turn(conn, "turn-gap-1", session_id, Some("in-gap-1"))?;
            qaqh_session::db::store::finish_turn(conn, "turn-gap-1", "completed", None)?;
            append_block(
                conn,
                &NewBlock {
                    block_id: "blk-u1".into(),
                    session_id: session_id.into(),
                    turn_id: Some("turn-gap-1".into()),
                    kind: "input",
                    text: None,
                    call_id: None,
                    payload_json: input_json,
                },
            )?;
            append_block(
                conn,
                &NewBlock {
                    block_id: "blk-a1".into(),
                    session_id: session_id.into(),
                    turn_id: Some("turn-gap-1".into()),
                    kind: "assistant",
                    text: None,
                    call_id: None,
                    payload_json: answer_json,
                },
            )?;
            append_block(
                conn,
                &NewBlock {
                    block_id: format!("tool_call:{session_id}:call-gap-1"),
                    session_id: session_id.into(),
                    turn_id: Some("turn-gap-1".into()),
                    kind: "tool_call",
                    text: None,
                    call_id: Some("call-gap-1".into()),
                    payload_json: tool_json,
                },
            )?;
            upsert_tool_call(
                conn,
                &ToolCallUpsert {
                    call_id: "call-gap-1".into(),
                    session_id: session_id.into(),
                    turn_id: Some("turn-gap-1".into()),
                    tool_name: "edit".into(),
                    args_json: Some(
                        r#"{"file_path":"a.rs","old_string":"x","new_string":"y"}"#.into(),
                    ),
                    state_json: serde_json::to_string(&ToolCallState::Declared).expect("state"),
                    state_kind: "declared",
                },
            )?;
            finish_tool_call(
                conn,
                &ToolCallFinish {
                    call_id: "call-gap-1".into(),
                    session_id: session_id.into(),
                    turn_id: Some("turn-gap-1".into()),
                    tool_name: "edit".into(),
                    state_json: serde_json::to_string(&ToolCallState::Finished {
                        outcome: qaqh_types::ToolCallOutcome::Completed,
                    })
                    .expect("finished state"),
                    exec_session_id: Some("execs_gap1".into()),
                    output_text: Some("ok".into()),
                    output_ref: None,
                    output_truncated: true,
                    diff_text: Some("--- a.rs\n+++ a.rs\n".into()),
                    diff_ref: None,
                    meta_json: Some(
                        serde_json::to_string(&ToolMetrics {
                            started_at_ms: 100,
                            finished_at_ms: 250,
                            retry_count: 1,
                            output_bytes: 2,
                            progress_bytes_total: 2,
                        })
                        .expect("metrics"),
                    ),
                    effects_json: Some(
                        serde_json::to_string(&vec![ToolEffectInfo::SubagentSpawned {
                            child_session_id: session_id.into(),
                            name: "worker".into(),
                            task_text: "subtask".into(),
                        }])
                        .expect("effects"),
                    ),
                    code_delta_json: Some(
                        serde_json::to_string(&CodeDelta {
                            lines_added: 3,
                            lines_removed: 1,
                            files_created: 0,
                            files_deleted: 0,
                            file: Some("a.rs".into()),
                        })
                        .expect("code_delta"),
                    ),
                },
            )?;
            upsert_exec_session(
                conn,
                &ExecSessionRow {
                    exec_session_id: "execs_gap1".into(),
                    session_id: session_id.into(),
                    origin_turn_id: Some("turn-gap-1".into()),
                    origin_call_id: Some("call-gap-1".into()),
                    tty: true,
                    command: "cargo test".into(),
                    cwd: Some(cwd.clone()),
                    state_json: serde_json::to_string(&ExecSessionState::Running)
                        .expect("exec state"),
                    state_kind: "running".into(),
                    started_at_ms: 150,
                    finished_at_ms: None,
                    next_offset: 11,
                    output_complete: false,
                    capture_omitted_bytes: 0,
                    stream_dropped_bytes: 0,
                    output_content_id: None,
                    output_preview: "running...\n".into(),
                    preview_start_offset: 0,
                },
            )?;
            create_interaction(
                conn,
                &NewInteraction {
                    interaction_id: "int-gap-1".into(),
                    session_id: session_id.into(),
                    turn_id: Some("turn-gap-1".into()),
                    call_id: Some("call-gap-1".into()),
                    kind: "\"permission\"".into(),
                    payload_json: perm_json,
                },
            )?;
            Ok::<_, qaqh_session::db::DbError>(())
        })
        .expect("seed rich gap fixtures");

    let devices = Arc::new(DeviceRegistry::new(Arc::clone(service.owners().db())));
    let config = ServerConfig {
        build_id: "gap-reg-test".into(),
        server_instance_id: "gap-reg-instance".into(),
        channel: "dev".into(),
        ..Default::default()
    };
    let os_user =
        std::env::var(if cfg!(windows) { "USERNAME" } else { "USER" }).expect("local OS user");
    let socket = super::local_ipc_path(temp.path());
    let endpoint =
        crate::v3_server::endpoint::local_ipc_endpoint(&config, socket.to_string_lossy());
    crate::v3_server::endpoint::publish(temp.path(), &endpoint).expect("publish endpoint");

    let state = ServerState::new(config, os_user);
    let backend = RealBackend::new(
        service.clone(),
        Arc::clone(&state),
        Arc::clone(&hub),
        Arc::new(Mutex::new(PairingTable::new())),
        Arc::clone(&devices),
        watch::channel(false).0,
        Arc::new(Mutex::new(PendingCommandStore::new(Arc::clone(
            service.owners().db(),
        )))),
    )
    .await
    .expect("real backend");

    let mut listener = bind_local_ipc(&socket).await.expect("bind UDS");
    let server = tokio::spawn(async move {
        let transport = listener.accept().await.expect("accept client");
        crate::v3_server::V3Server::with_backend(transport, state, backend)
            .serve()
            .await
    });

    let client =
        V3Client::connect_local(temp.path(), Some("gap-reg-test"), "dev", "gap-reg-client", false)
            .await
            .expect("connect_local");

    // 1. SessionSummary & Snapshot carry custom_tools and true millisecond timestamps.
    let sid = SessionId::new(session_id).expect("sid");
    let list = match client
        .call(ControlRequest::Session(SessionRequest::List {
            include_archived: true,
        }))
        .await
        .expect("list")
    {
        ControlResponse::Session(SessionResponse::Sessions(list)) => list,
        other => panic!("unexpected list: {other:?}"),
    };
    let summary = list
        .iter()
        .find(|s| s.session_id == sid)
        .expect("session summary");
    assert_eq!(summary.custom_tools, custom_tools);
    assert_eq!(summary.created_at_ms, 1700000000123);
    assert_eq!(summary.updated_at_ms, 1700000005456);

    let snap = match client
        .call(ControlRequest::Session(
            SessionRequest::GetSessionSnapshot {
                session_id: sid.clone(),
                tail_blocks: 50,
                tail_turns: 20,
            },
        ))
        .await
        .expect("snapshot")
    {
        ControlResponse::Session(SessionResponse::Snapshot(snap)) => snap,
        other => panic!("unexpected snapshot: {other:?}"),
    };
    assert_eq!(snap.custom_tools, custom_tools);
    assert_eq!(snap.created_at_ms, 1700000000123);
    assert_eq!(snap.turns.len(), 1);
    assert_eq!(snap.turns[0].turn_id.as_str(), "turn-gap-1");
    assert_eq!(
        snap.turns[0].input_id.as_ref().map(|id| id.as_str()),
        Some("in-gap-1")
    );

    // 2. BlockView rich fields + raw PNG blob fetch (no base64).
    let u1 = snap
        .tail_blocks
        .iter()
        .find(|b| b.block_id.as_str() == "blk-u1")
        .expect("blk-u1");
    assert_eq!(u1.actor, Some(BlockActor::Agent));
    assert_eq!(u1.purpose, Some(qaqh_ringing_v3::InputPurpose::Steer));
    assert!(u1.inter_agent.is_some());
    assert_eq!(u1.attachments.len(), 1);
    assert_eq!(u1.images.len(), 1);
    assert_eq!(u1.images[0].mime_type, "image/png");
    let fetched_png = client
        .fetch_blob(&u1.images[0].hash)
        .await
        .expect("fetch raw png blob");
    assert_eq!(fetched_png.as_ref(), png_bytes.as_slice());

    let a1 = snap
        .tail_blocks
        .iter()
        .find(|b| b.block_id.as_str() == "blk-a1")
        .expect("blk-a1");
    assert_eq!(a1.model.as_deref(), Some("deepseek-chat"));
    assert_eq!(a1.generated_ms, Some(321));
    let usage = a1.usage.expect("block usage");
    // total_input prefers hit + miss = 80 + 40 = 120 over prompt_tokens=100.
    assert_eq!(usage.input_tokens, 120);
    assert_eq!(usage.cached_input_tokens, Some(80));

    // 3. ToolCallView rich fields.
    assert_eq!(snap.tool_calls.len(), 1);
    let call = &snap.tool_calls[0];
    assert_eq!(call.turn_id.as_ref().map(|id| id.as_str()), Some("turn-gap-1"));
    assert!(matches!(call.args, Some(ContentRef::Inline { .. })));
    assert!(matches!(call.output, Some(ContentRef::Inline { .. })));
    assert!(call.output_truncated);
    assert!(matches!(call.diff, Some(ContentRef::Inline { .. })));
    assert_eq!(call.code_delta.as_ref().map(|d| d.lines_added), Some(3));
    assert_eq!(call.metrics.as_ref().map(|m| m.retry_count), Some(1));
    assert_eq!(
        call.exec_session_id.as_ref().map(|id| id.as_str()),
        Some("execs_gap1")
    );
    assert_eq!(call.effects.len(), 1);
    assert!(matches!(
        &call.effects[0],
        ToolEffectView::SubagentSpawned { name, .. } if name == "worker"
    ));

    // 4. ExecSessionSnapshot rich fields.
    assert_eq!(snap.exec_sessions.len(), 1);
    let exec = &snap.exec_sessions[0];
    assert_eq!(exec.state, ExecSessionStateView::Running);
    assert_eq!(exec.output_preview, "running...\n");
    assert_eq!(exec.preview_start_offset, 0);
    assert_eq!(
        exec.origin_call_id.as_ref().map(|id| id.as_str()),
        Some("call-gap-1")
    );

    // 5. InteractionView structured permission (no JSON string in body).
    assert_eq!(snap.pending_interactions.len(), 1);
    let ia = &snap.pending_interactions[0];
    assert_eq!(ia.body, "Requires write approval");
    assert!(!ia.body.starts_with('{'));
    assert_eq!(ia.choices.len(), 1);
    assert_eq!(ia.choices[0].decision, InteractionDecisionKind::Approved);
    assert_eq!(ia.choices[0].tone, ChoiceTone::Primary);
    assert_eq!(
        ia.permission.as_ref().map(|p| p.tool_name.as_str()),
        Some("write")
    );

    // 6. ProviderPresetView uses typed Wire + CredentialSource enums.
    let presets = match client
        .call(ControlRequest::Config(ConfigRequest::ListProviderPresets))
        .await
        .expect("presets")
    {
        ControlResponse::Config(ConfigResponse::ProviderPresets(presets)) => presets,
        other => panic!("unexpected presets: {other:?}"),
    };
    let ds = presets
        .iter()
        .find(|p| p.id == "deepseek-oauth")
        .expect("deepseek-oauth preset");
    assert_eq!(ds.wire, Wire::Responses);
    assert_eq!(ds.credential, CredentialSource::DeepseekAccount);

    // 7. Live event projections: AnswerDelta incremental diff, ToolArgumentsDelta metadata,
    //    ToolCallUpdated preserves args across state changes, ProviderRetrying max_attempts,
    //    HandoffSealed fields, and TimelineTruncated removed_turn_ids.
    let mut events = client.attach_session(&sid, 0).await.expect("attach_session");
    let owner = service.owners().get_or_create(session_id);
    let live_turn = TurnId::new("turn-live-2");
    let live_block = BlockId::new("blk-live-a2");
    let live_call = qaqh_ringing_contract::fact::ToolCallId::new("call-live-2");

    owner.message_delta(
        &live_turn,
        &live_block,
        AssistantBlockKind::Answer,
        "Hel",
    );
    owner.message_delta(
        &live_turn,
        &live_block,
        AssistantBlockKind::Answer,
        "Hello",
    );
    owner.tool_arguments_delta(
        &live_turn,
        &live_call,
        "apply_patch",
        "*** Begin Patch",
        Some(2),
        Some(1),
        vec!["src/main.rs".into()],
        false,
    );
    owner.tool_arguments_delta(
        &live_turn,
        &live_call,
        "apply_patch",
        "*** Begin Patch\n*** End Patch",
        Some(3),
        Some(1),
        vec!["src/main.rs".into()],
        true,
    );
    owner.provider_retrying(&live_turn, 2, 5, 3, "429");
    owner.publish(qaqh_ringing_contract::SessionEvent::ToolDeclared {
        call_id: live_call.clone(),
        turn_id: live_turn.clone(),
        tool_name: "write".into(),
        args: Some(serde_json::json!({"file_path":"b.rs"})),
    });
    owner.publish(qaqh_ringing_contract::SessionEvent::ToolStateChanged {
        call_id: live_call.clone(),
        turn_id: live_turn.clone(),
        state: ToolCallState::AwaitingInteraction {
            interaction_id: "int-live-2".into(),
        },
    });
    owner.publish(qaqh_ringing_contract::SessionEvent::ToolStateChanged {
        call_id: live_call.clone(),
        turn_id: live_turn.clone(),
        state: ToolCallState::Cancelling,
    });
    owner.publish(qaqh_ringing_contract::SessionEvent::HandoffSealed {
        sealed_session_id: session_id.into(),
        chain_root: session_id.into(),
        successor_session_id: "01JSUCCESSOR00000000000000".into(),
        turn_id: live_turn.clone(),
        work_summary: "handoff done".into(),
        at_ms: 4567,
    });
    let removed = owner
        .truncate_from_turn("turn-gap-1")
        .await
        .expect("truncate_from_turn");
    assert_eq!(removed.len(), 1);

    let mut answer_text = String::new();
    let mut saw_args_complete = false;
    let mut saw_awaiting_with_args = false;
    let mut saw_cancelling = false;
    let mut saw_retry = false;
    let mut saw_handoff = false;
    let mut saw_truncated = false;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(5);
    while !(answer_text == "Hello"
        && saw_args_complete
        && saw_awaiting_with_args
        && saw_cancelling
        && saw_retry
        && saw_handoff
        && saw_truncated)
    {
        let frame = tokio::time::timeout_at(deadline, events.next_frame())
            .await
            .expect("event timeout")
            .expect("frame");
        match frame.event {
            SessionEvent::AnswerDelta { text, .. } => answer_text.push_str(&text),
            SessionEvent::ToolArgumentsDelta {
                tool_name,
                lines_added,
                target_files,
                args_complete,
                ..
            } => {
                assert_eq!(tool_name, "apply_patch");
                assert_eq!(target_files, vec!["src/main.rs"]);
                if args_complete {
                    assert_eq!(lines_added, Some(3));
                    saw_args_complete = true;
                }
            }
            SessionEvent::ToolCallUpdated { call, .. }
                if call.tool_call_id == ToolCallId::new("call-live-2").unwrap() =>
            {
                if call.status == ToolCallStatus::AwaitingInteraction {
                    assert_eq!(
                        call.interaction_id.as_ref().map(|id| id.as_str()),
                        Some("int-live-2")
                    );
                    assert!(
                        call.args.is_some(),
                        "ToolStateChanged must preserve ToolDeclared args"
                    );
                    saw_awaiting_with_args = true;
                } else if call.status == ToolCallStatus::Cancelling {
                    saw_cancelling = true;
                }
            }
            SessionEvent::ProviderRetrying {
                attempt,
                max_attempts,
                ..
            } => {
                assert_eq!((attempt, max_attempts), (2, 5));
                saw_retry = true;
            }
            SessionEvent::HandoffSealed {
                sealed_session_id,
                at_ms,
                summary: ContentRef::Inline { text },
                ..
            } => {
                assert_eq!(sealed_session_id, sid);
                assert_eq!(at_ms, 4567);
                assert_eq!(text, "handoff done");
                saw_handoff = true;
            }
            SessionEvent::TimelineTruncated {
                removed_turn_ids, ..
            } => {
                assert_eq!(
                    removed_turn_ids,
                    vec![qaqh_core_vocab::TurnId::new("turn-gap-1").unwrap()]
                );
                saw_truncated = true;
            }
            _ => {}
        }
    }

    // 8. Image upload + image-only SendPrompt succeeds without error.
    client
        .call(ControlRequest::Session(
            qaqh_ringing_v3::SessionRequest::Attach {
                session_id: sid.clone(),
                since_seq: 0,
            },
        ))
        .await
        .expect("attach control for driver");
    let img_hash = client
        .upload_blob(&sid, &png_bytes)
        .await
        .expect("upload_blob png");
    let reply = client
        .call_idempotent(
            "img-prompt-1",
            ControlRequest::Command(CommandRequest::SendPrompt {
                session_id: sid,
                input_id: InputId::new("in-img-1").unwrap(),
                text: String::new(),
                attachments: vec![],
                images: vec![ImageRefView {
                    hash: img_hash,
                    len: png_bytes.len() as u64,
                    mime_type: "image/png".into(),
                }],
            }),
        )
        .await
        .expect("image-only SendPrompt");
    assert!(matches!(
        reply,
        ControlResponse::Command(CommandResponse::Accepted { .. })
    ));

    drop(events);
    drop(client);
    server.await.expect("server task").expect("server loop");
    crate::v3_server::endpoint::retire(temp.path()).expect("retire endpoint");
}
