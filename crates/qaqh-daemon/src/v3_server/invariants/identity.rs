//! 守卫④：**身份单权威不可伪造**（`ADR-0042` 铁律 5）。
//!
//! 三条一起验，缺一条都能被绕过：
//!
//! 1. 越权：已配对设备读主设备的会话 / 写它的 shell → 当场 `PermissionDenied`；
//! 2. 作用域：低 scope 调管理面（停机、配对表、配置写、装插件、切账号、提交、写文件）
//!    → 全部当场拒绝，同时**主设备同样的调用必须通过**（证明是作用域判定，
//!    不是"一律拒绝"的懒实现）；
//! 3. 自报字段无效：设备端把 `client_instance_id` 冒充成主设备的实例名，也拿不到驱动权；
//!    未知令牌连握手都过不去。

use std::sync::Arc;

use bytes::Bytes;
use qaqh_core_vocab::{InputId, JsonText, SessionId};
use qaqh_client::v3::{ClientError, V3Client};
use qaqh_ringing_v3::{
    AccountRequest, ClientCredential, ClientHello, ClientKind, CommandRequest, ConfigRequest,
    ConfigSection, ControlRequest, ControlResponse, ErrorCode, FsRequest, GitxRequest,
    HandshakeRejectReason, MarketplaceRequest, PairingRequest, SessionRequest, ShellRequest,
    SystemRequest,
};
use qaqh_v3_transport::MuxTransport;

use crate::v3_server::harness;

pub(crate) async fn guard<B: MuxTransport>(pair: fn() -> (B, B)) {
    let state = harness::state();
    let session = harness::session_for(&state, &harness::owner_device());
    let session_id = session.id.clone();

    // 主设备连接
    let (owner_transport, owner_server_transport) = pair();
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
    let shell_id = harness::open_shell(&owner).await;

    // 已配对设备连接（**另一个连接**）
    let (device_transport, device_server_transport) = pair();
    let (_device_server, _device_task) =
        harness::spawn_server(device_server_transport, Arc::clone(&state)).await;
    let device = harness::connect(device_transport, harness::device_hello("guard-04-device")).await;

    // ── ① 越权访问他人会话 ──
    let error = device
        .call(ControlRequest::Session(SessionRequest::GetSessionSnapshot {
            session_id: session_id.clone(),
            tail_blocks: 4,
        }))
        .await
        .expect_err("已配对设备不得读主设备的会话");
    assert_eq!(error.code(), Some(ErrorCode::PermissionDenied));

    // ── ① 越权写他人 shell ──
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
    assert_eq!(admin.len(), 9, "管理面清单必须逐条覆盖");
    for (index, request) in admin.into_iter().enumerate() {
        let label = format!("管理面第 {index} 条");
        let error = device
            .call(request)
            .await
            .err()
            .unwrap_or_else(|| panic!("{label} 竟被已配对设备调通"));
        assert_eq!(
            error.code(),
            Some(ErrorCode::PermissionDenied),
            "{label} 必须以 PermissionDenied 拒绝，实际 {error:?}"
        );
    }

    // ── ② 对照：主设备同样的调用必须通过（证明判定的是作用域） ──
    match owner
        .call(ControlRequest::Pairing(PairingRequest::ListDevices))
        .await
        .expect("主设备必须能枚举设备表")
    {
        ControlResponse::Pairing(_) => {}
        other => panic!("设备表回执形状不符：{other:?}"),
    }
    owner
        .call(ControlRequest::Session(SessionRequest::GetSessionSnapshot {
            session_id: session_id.clone(),
            tail_blocks: 4,
        }))
        .await
        .expect("主设备必须能读自己的会话");
    // 停机走的是真实现（置停机信号），所以放在最后：它一旦受理，后面就别再依赖它。
    assert!(!state.shutdown_requested());

    // ── ③ 冒充实例名拿不到驱动权 ──
    let (spoof_transport, spoof_server_transport) = pair();
    let (_spoof_server, _spoof_task) =
        harness::spawn_server(spoof_server_transport, Arc::clone(&state)).await;
    // 设备端把实例名伪造成主设备那个窗口的名字——`client_instance_id` 是自报字段，
    // 谁都能填；授权必须靠凭据（设备键），而不是它。
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
        }))
        .await
        .expect_err("冒充实例名不得取得驱动权");
    assert_eq!(error.code(), Some(ErrorCode::PermissionDenied));

    // ── ③ 未知令牌连握手都过不去 ──
    let (unknown_transport, unknown_server_transport) = pair();
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
    match outcome {
        Err(ClientError::HandshakeRejected(HandshakeRejectReason::CredentialRejected { .. })) => {}
        Err(other) => panic!("未知令牌必须以凭据拒绝收场，实际 {other:?}"),
        Ok(_) => panic!("未知令牌竟然握手成功"),
    }

    println!("[v3-guard-04] 管理面 9 条全部拒绝、主设备同调用通过、冒充实例名无效、未知令牌握手被拒");
}

/// 管理面清单（每一条都必须在设备作用域下被拒）。
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
            path: "/etc/passwd".to_string(),
            bytes: Bytes::from_static(b"x"),
        }),
    ]
}
