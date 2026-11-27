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
        "real-reg-test",
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
