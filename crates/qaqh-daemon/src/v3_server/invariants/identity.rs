//! 守卫④：**身份单权威不可伪造**（`ADR-0042` 铁律 5）。
//!
//! 覆盖 5 条安全边界：
//! 1. 跨设备越权读他人会话 / 写他人 shell → `PermissionDenied`；
//! 2. 低 scope 调 9 条管理面 → 全拒，且主设备同调用通过；
//! 3. 冒充 `client_instance_id` 无效；未知令牌握手被拒；
//! 4. **幂等表设备隔离**：设备端重放主设备刚用过的 `command_id`，不得拿到主设备回执；
//! 5. **Blob 推送授权隔离**：向主设备独占会话推 Blob，未授权的已配对设备不得收到。

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use qaqh_client::v3::{ClientError, V3Client};
use qaqh_core_vocab::{InputId, JsonText, SessionId};
use qaqh_ringing_v3::{
    AccountRequest, ClientCredential, ClientHello, ClientKind, CommandRequest, ConfigRequest,
    ConfigSection, ControlRequest, ControlResponse, ErrorCode, FsRequest, GitxRequest,
    HandshakeRejectReason, MarketplaceRequest, PairingRequest, SessionRequest, ShellRequest,
    SystemRequest,
};

use crate::v3_server::facade::BlobPush;
use crate::v3_server::harness::{self, BackendFactory};

pub(crate) async fn guard<F: BackendFactory>(factory: F) {
    let state = harness::state();
    let session = harness::session_for(&state, &harness::owner_device());
    let session_id = session.id.clone();

    let (owner_transport, owner_server_transport) = factory.pair().await;
    let (_owner_server, _owner_task) =
        harness::spawn_server(owner_server_transport, Arc::clone(&state)).await;
    let owner = harness::connect(owner_transport, harness::owner_hello("guard-04-owner")).await;
    owner
        .call(ControlRequest::Session(SessionRequest::Attach {
            session_id: session_id.clone(),
            since_seq: 0,
        }))
        .await
        .expect("主设备 Attach");
    let shell_id = harness::open_shell(&owner, &session_id).await;

    let (device_transport, device_server_transport) = factory.pair().await;
    let (_device_server, _device_task) =
        harness::spawn_server(device_server_transport, Arc::clone(&state)).await;
    let mut device =
        harness::connect(device_transport, harness::device_hello("guard-04-device")).await;
    let mut device_blobs = device.take_blob_inbox().expect("设备 blob 收件箱");

    // ── ① 越权访问他人会话 & shell ──
    let error = device
        .call(ControlRequest::Session(
            SessionRequest::GetSessionSnapshot {
                session_id: session_id.clone(),
                tail_blocks: 4,
                tail_turns: 8,
            },
        ))
        .await
        .expect_err("已配对设备不得读主设备的会话");
    assert_eq!(error.code(), Some(ErrorCode::PermissionDenied));

    let error = device
        .call(ControlRequest::Shell(ShellRequest::Write {
            shell_id: shell_id.clone(),
            bytes: Bytes::from_static(b"whoami\r"),
        }))
        .await
        .expect_err("已配对设备不得写主设备的 shell");
    assert_eq!(error.code(), Some(ErrorCode::PermissionDenied));

    // ── ② 低 scope 调管理面：逐条当场拒绝 ──
    let admin = admin_surface(&session_id);
    assert_eq!(admin.len(), 9);
    for (index, request) in admin.into_iter().enumerate() {
        let error = device
            .call(request)
            .await
            .err()
            .unwrap_or_else(|| panic!("管理面第 {index} 条竟被已配对设备调通"));
        assert_eq!(error.code(), Some(ErrorCode::PermissionDenied));
    }

    // ── ④ 幂等表设备隔离：设备端重放主设备的 command_id 绝不能拿到主设备的回执 ──
    let owner_list = owner
        .call_idempotent(
            "shared-owner-cmd-id",
            ControlRequest::Pairing(PairingRequest::ListDevices),
        )
        .await
        .expect("主设备必须能枚举设备表");
    assert!(matches!(owner_list, ControlResponse::Pairing(_)));
    let device_replay_err = device
        .call_idempotent(
            "shared-owner-cmd-id",
            ControlRequest::Pairing(PairingRequest::ListDevices),
        )
        .await
        .expect_err("设备端用主设备的 command_id 必须独立鉴权并被拒绝，绝不能命中主设备缓存");
    assert_eq!(device_replay_err.code(), Some(ErrorCode::PermissionDenied));

    // ── ⑤ Blob 推送授权隔离：主设备独占会话的大输出不得广播给未授权设备 ──
    let secret_blob = b"secret-owner-session-blob".to_vec();
    let secret_hash = state.blobs.put(secret_blob.clone());
    let _ = state.push_bus.send(BlobPush {
        session_id: session_id.clone(),
        hash: secret_hash,
        len: secret_blob.len() as u64,
    });
    assert!(
        tokio::time::timeout(Duration::from_millis(80), device_blobs.next())
            .await
            .is_err(),
        "未授权设备绝不能收到他人会话外置的 Blob"
    );

    // ── ② 对照：主设备同调用通过 ──
    owner
        .call(ControlRequest::Session(
            SessionRequest::GetSessionSnapshot {
                session_id: session_id.clone(),
                tail_blocks: 4,
                tail_turns: 8,
            },
        ))
        .await
        .expect("主设备必须能读自己的会话");
    assert!(!state.shutdown_requested());

    // ── ③ 冒充实例名拿不到驱动权 ──
    let (spoof_transport, spoof_server_transport) = factory.pair().await;
    let (_spoof_server, _spoof_task) =
        harness::spawn_server(spoof_server_transport, Arc::clone(&state)).await;
    let spoof = harness::connect(spoof_transport, harness::device_hello("guard-04-owner")).await;
    session.allow_device(&harness::paired_device());
    spoof
        .call(ControlRequest::Session(SessionRequest::Attach {
            session_id: session_id.clone(),
            since_seq: 0,
        }))
        .await
        .expect("设备可附着（已授权）");
    let error = spoof
        .call(ControlRequest::Command(CommandRequest::SendPrompt {
            session_id: session_id.clone(),
            input_id: InputId::new("guard-04-spoof").expect("input id"),
            text: "冒充".to_string(),
            attachments: Vec::new(),
            images: Vec::new(),
        }))
        .await
        .expect_err("冒充实例名不得取得驱动权");
    assert_eq!(error.code(), Some(ErrorCode::PermissionDenied));

    // ── ③ 未知令牌连握手都过不去 ──
    let (unknown_transport, unknown_server_transport) = factory.pair().await;
    let (_unknown_server, _unknown_task) =
        harness::spawn_server(unknown_server_transport, Arc::clone(&state)).await;
    let outcome = V3Client::connect(
        unknown_transport,
        ClientHello::new(
            "guard-04-unknown",
            ClientKind::Mobile,
            ClientCredential::PairedDevice {
                token: "not-a-real-token".to_string(),
            },
        ),
    )
    .await;
    assert!(matches!(
        outcome,
        Err(ClientError::HandshakeRejected(
            HandshakeRejectReason::CredentialRejected { .. }
        ))
    ));

    println!(
        "[v3-guard-04/{}] 管理面 9 条全拒、幂等设备隔离、Blob 授权隔离、冒充实例无效、未知令牌被拒",
        F::name()
    );
}

fn admin_surface(session_id: &SessionId) -> Vec<ControlRequest> {
    vec![
        ControlRequest::System(SystemRequest::Stop { graceful: true }),
        ControlRequest::System(SystemRequest::StopIfIdle),
        ControlRequest::Pairing(PairingRequest::ListDevices),
        ControlRequest::Pairing(PairingRequest::RevokeDevice {
            device_id: "dev-0001".to_string(),
        }),
        ControlRequest::Config(ConfigRequest::Set {
            section: ConfigSection::Providers,
            value: JsonText::new("{}"),
        }),
        ControlRequest::Marketplace(MarketplaceRequest::Install {
            id: "evil".to_string(),
            version: None,
        }),
        ControlRequest::Account(AccountRequest::Select {
            account_id: "someone-else".to_string(),
        }),
        ControlRequest::Gitx(GitxRequest::Commit {
            session_id: session_id.clone(),
            message: "rm -rf".to_string(),
        }),
        ControlRequest::Fs(FsRequest::Write {
            session_id: session_id.clone(),
            path: "/etc/passwd".to_string(),
            bytes: Bytes::from_static(b"x"),
        }),
    ]
}
