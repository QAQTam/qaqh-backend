use std::collections::BTreeSet;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use qaqh_client::v3::V3Client;
use qaqh_core_vocab::{GoalStatus, SessionId};
use qaqh_ringing_runtime::{
    DeviceRegistry, PendingCommandStore, RingingHub, pairing::PairingTable,
};
use qaqh_ringing_v3::{
    ClientKind, ControlRequest, ControlResponse, ErrorCode, FsRequest, PairingRequest,
    PairingResponse, SessionEvent, SessionRequest, SessionResponse, SystemRequest, SystemResponse,
    WorkspaceRequest,
};
use qaqh_runtime::QaqhService;
use qaqh_v3_transport::bind_local_ipc;
use tokio::sync::watch;

use super::RealBackend;
use crate::v3_server::facade::CallContext;
use crate::v3_server::identity::{ConnectionIdentity, Scope};
use crate::v3_server::state::{ServerConfig, ServerState};

#[tokio::test]
async fn real_service_sqlite_uds_and_client_share_one_v3_backend() {
    let temp = tempfile::tempdir().expect("tempdir");
    let service = QaqhService::init(temp.path().to_path_buf());
    let session_id = "01JREALV3SESSION0000000000";
    let cwd = temp.path().to_string_lossy().into_owned();
    service
        .owners()
        .db()
        .write_blocking(move |conn| {
            qaqh_session::db::store::create_session(
                conn,
                &qaqh_session::db::store::NewSession {
                    session_id: session_id.to_string(),
                    parent_session_id: None,
                    title: "real backend".to_string(),
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

    let hub = Arc::new(RingingHub::with_database(
        "test-epoch",
        Arc::clone(service.owners().db()),
    ));
    let devices = Arc::new(DeviceRegistry::new(Arc::clone(service.owners().db())));
    let config = ServerConfig {
        build_id: "real-backend-test".to_string(),
        server_instance_id: "real-backend-instance".to_string(),
        channel: "dev".to_string(),
        ..Default::default()
    };
    let os_user =
        std::env::var(if cfg!(windows) { "USERNAME" } else { "USER" }).expect("local OS user");
    let socket = super::local_ipc_path(temp.path());
    let endpoint =
        crate::v3_server::endpoint::local_ipc_endpoint(&config, socket.to_string_lossy());
    crate::v3_server::endpoint::publish(temp.path(), &endpoint).expect("publish endpoint");

    let state = ServerState::new(config.clone(), os_user.clone());
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
    assert!(
        backend
            .projectors
            .tasks
            .lock()
            .expect("projector registry")
            .is_empty(),
        "dormant persisted sessions must not start projector actors"
    );

    let persisted_id = SessionId::new(session_id).expect("valid session id");
    backend
        .ensure_projector_ready(&persisted_id)
        .await
        .expect("initial projector readiness");
    {
        let mut tasks = backend.projectors.tasks.lock().expect("projector registry");
        tasks
            .get_mut(&persisted_id)
            .expect("running projector")
            .0
            .abort();
    }
    for _ in 0..10 {
        let finished = backend
            .projectors
            .tasks
            .lock()
            .expect("projector registry")
            .get(&persisted_id)
            .is_some_and(|(task, _)| task.is_finished());
        if finished {
            break;
        }
        tokio::task::yield_now().await;
    }
    backend
        .ensure_projector_ready(&persisted_id)
        .await
        .expect("failed projector must be replaced");
    assert_eq!(
        backend
            .projectors
            .tasks
            .lock()
            .expect("projector registry")
            .len(),
        1
    );

    let runtime = backend
        .state
        .session(&persisted_id)
        .expect("session runtime");
    let device_key = "device:test".to_string();
    runtime.allow_device(&device_key);
    let (_revoked, revoked_rx) = watch::channel(false);
    let (blob_pushes, _blob_rx) = tokio::sync::mpsc::unbounded_channel();
    let device_context = CallContext {
        identity: Arc::new(ConnectionIdentity {
            device: device_key,
            device_id: "test-device".into(),
            scope: Scope::Device,
            client_instance_id: "test-instance".into(),
            client_kind: ClientKind::Mobile,
            revoked: revoked_rx,
        }),
        blob_pushes,
        attached_sessions: Arc::new(Mutex::new(BTreeSet::new())),
    };
    let write_error = super::fs::handle(
        &backend,
        &device_context,
        FsRequest::Write {
            session_id: persisted_id.clone(),
            path: "forbidden.txt".into(),
            bytes: bytes::Bytes::from_static(b"forbidden"),
        },
    )
    .await
    .expect_err("paired devices must not write session files");
    assert_eq!(write_error.code, ErrorCode::PermissionDenied);
    let list_error = super::workspace::handle(&backend, &device_context, WorkspaceRequest::List)
        .await
        .expect_err("paired devices must not enumerate owner workspaces");
    assert_eq!(list_error.code, ErrorCode::PermissionDenied);

    let mut listener = bind_local_ipc(&socket).await.expect("bind UDS");
    let server_backend = backend.clone();
    let server = tokio::spawn(async move {
        let transport = listener.accept().await.expect("accept local client");
        crate::v3_server::V3Server::with_backend(transport, state, server_backend)
            .serve()
            .await
    });

    let client = V3Client::connect_local(
        temp.path(),
        Some("real-backend-test"),
        "dev",
        "real-backend-client",
        false,
    )
    .await
    .expect("connect through real UDS");
    let health = client
        .call(ControlRequest::System(SystemRequest::Health))
        .await
        .expect("health");
    assert!(matches!(
        health,
        ControlResponse::System(SystemResponse::Health { .. })
    ));
    let health_again = client
        .call_idempotent("same-health", ControlRequest::System(SystemRequest::Health))
        .await
        .expect("first health");
    client
        .call(ControlRequest::Session(SessionRequest::Attach {
            session_id: SessionId::new(session_id).expect("valid session id"),
            since_seq: 0,
        }))
        .await
        .expect("attach real session");
    assert_eq!(
        backend
            .projectors
            .tasks
            .lock()
            .expect("projector registry")
            .len(),
        1,
        "attaching a dormant session must start exactly one projector"
    );
    let attached_session_id = SessionId::new(session_id).expect("valid session id");
    let mut events = client
        .attach_session(&attached_session_id, 0)
        .await
        .expect("subscribe to projected owner events");
    client
        .call(ControlRequest::Session(SessionRequest::SetGoal {
            session_id: attached_session_id.clone(),
            objective: "verify real owner projection".into(),
            status: Some(GoalStatus::Paused),
            token_budget: Some(128),
        }))
        .await
        .expect("set goal through real service");
    let goal = tokio::time::timeout(Duration::from_secs(2), async {
        loop {
            let frame = events.next_frame().await.expect("projected event frame");
            if let SessionEvent::GoalUpdated {
                goal: Some(goal), ..
            } = frame.event
            {
                break goal;
            }
        }
    })
    .await
    .expect("owner event must reach the V3 bus");
    assert_eq!(goal.objective, "verify real owner projection");
    assert_eq!(goal.token_budget, Some(128));
    drop(events);
    drop(client);
    server.await.expect("server task").expect("server loop");
    drop(backend);

    let restarted_state = ServerState::new(config, os_user);
    let restarted_backend = RealBackend::new(
        service.clone(),
        Arc::clone(&restarted_state),
        Arc::clone(&hub),
        Arc::new(Mutex::new(PairingTable::new())),
        Arc::clone(&devices),
        watch::channel(false).0,
        Arc::new(Mutex::new(PendingCommandStore::new(Arc::clone(
            service.owners().db(),
        )))),
    )
    .await
    .expect("restarted real backend");
    assert!(
        restarted_backend
            .projectors
            .tasks
            .lock()
            .expect("projector registry")
            .is_empty(),
        "daemon restart must leave dormant sessions unloaded"
    );
    let mut restarted_listener = bind_local_ipc(&socket).await.expect("rebind UDS");
    let restarted_server_backend = restarted_backend.clone();
    let restarted_server = tokio::spawn(async move {
        let transport = restarted_listener
            .accept()
            .await
            .expect("accept restarted local client");
        crate::v3_server::V3Server::with_backend(
            transport,
            restarted_state,
            restarted_server_backend,
        )
        .serve()
        .await
    });

    let restarted_client = V3Client::connect_local(
        temp.path(),
        Some("real-backend-test"),
        "dev",
        "real-backend-client",
        false,
    )
    .await
    .expect("reconnect through real UDS");
    let health_replay = restarted_client
        .call_idempotent("same-health", ControlRequest::System(SystemRequest::Health))
        .await
        .expect("durable replay after backend restart");
    assert_eq!(health_again, health_replay);

    let sessions = restarted_client
        .call(ControlRequest::Session(SessionRequest::List {
            include_archived: true,
        }))
        .await
        .expect("list real sessions");
    match sessions {
        ControlResponse::Session(SessionResponse::Sessions(sessions)) => {
            assert!(sessions.iter().any(|session| {
                session.session_id == SessionId::new(session_id).expect("valid id")
            }));
        }
        other => panic!("unexpected session response: {other:?}"),
    }

    let invite_request = ControlRequest::Pairing(PairingRequest::CreateInvite {
        ttl_ms: qaqh_ringing_runtime::DEFAULT_PAIRING_TOKEN_TTL_MS,
    });
    let invite = restarted_client
        .call_idempotent("pairing-invite", invite_request.clone())
        .await
        .expect("create pairing invite");
    let invite_replay = restarted_client
        .call_idempotent("pairing-invite", invite_request)
        .await
        .expect("in-memory invite replay");
    assert_eq!(invite, invite_replay);
    let ControlResponse::Pairing(PairingResponse::Invite { code, .. }) = invite else {
        panic!("unexpected invite response: {invite:?}");
    };
    let redeem = ControlRequest::Pairing(PairingRequest::Redeem {
        code: code.clone(),
        client_kind: ClientKind::Mobile,
    });
    let redeemed = restarted_client
        .call_idempotent("one-time-pairing", redeem.clone())
        .await
        .expect("redeem pairing invite");
    let ControlResponse::Pairing(PairingResponse::Redeemed { token, .. }) = redeemed else {
        panic!("unexpected redeemed response: {redeemed:?}");
    };
    let db = service.owners().db().clone();
    let (token_leaks, code_leaks) = db
        .read_blocking(move |conn| {
            let token_leaks = conn.query_row(
                "SELECT COUNT(*) FROM command_receipts WHERE instr(sync_result_json, ?1) > 0 OR instr(fingerprint, ?1) > 0",
                [&token],
                |row| row.get::<_, i64>(0),
            )?;
            let code_leaks = conn.query_row(
                "SELECT COUNT(*) FROM command_receipts WHERE instr(sync_result_json, ?1) > 0 OR instr(fingerprint, ?1) > 0",
                [&code],
                |row| row.get::<_, i64>(0),
            )?;
            Ok::<_, rusqlite::Error>((token_leaks, code_leaks))
        })
        .expect("inspect receipt storage");
    assert_eq!((token_leaks, code_leaks), (0, 0));
    let replay = restarted_client
        .call_idempotent("one-time-pairing", redeem)
        .await
        .expect_err("one-time pairing secrets must not be replayed");
    assert_eq!(replay.code(), Some(ErrorCode::Unavailable));

    drop(restarted_client);
    restarted_server
        .await
        .expect("restarted server task")
        .expect("restarted server loop");
    crate::v3_server::endpoint::retire(temp.path()).expect("retire endpoint");
}
