//! 守卫⑤：**多端联动原语**（`ADR-0042` 铁律 4/5：双端同看会话 + 双端同看 Shell + 吊销即时踢流）。

use std::sync::Arc;
use std::time::Duration;

use bytes::Bytes;
use qaqh_client::v3::SessionEventsStream;
use qaqh_core_vocab::InputId;
use qaqh_ringing_v3::{
    CommandRequest, ControlRequest, ControlResponse, ErrorCode, PairingRequest, PairingResponse,
    SessionEvent, SessionRequest, ShellRequest, SystemRequest,
};
use tokio::io::AsyncRead;

use crate::v3_server::harness::{self, BackendFactory};

const OWNER_INSTANCE: &str = "guard-05-owner";
const DEVICE_INSTANCE: &str = "guard-05-device";

pub(crate) async fn guard<F: BackendFactory>(factory: F) {
    let state = harness::state();
    let session = harness::session_for(&state, &harness::paired_device());
    let session_id = session.id.clone();

    let (owner_transport, owner_server_transport) = factory.pair().await;
    let (_owner_server, _owner_task) =
        harness::spawn_server(owner_server_transport, Arc::clone(&state)).await;
    let owner = harness::connect(owner_transport, harness::owner_hello(OWNER_INSTANCE)).await;
    owner
        .call(ControlRequest::Session(SessionRequest::Attach {
            session_id: session_id.clone(),
            since_seq: 0,
        }))
        .await
        .expect("主设备 Attach");
    let mut owner_stream = owner
        .attach_session(&session_id, 0)
        .await
        .expect("主设备事件流");

    let (device_transport, device_server_transport) = factory.pair().await;
    let (_device_server, _device_task) =
        harness::spawn_server(device_server_transport, Arc::clone(&state)).await;
    let device = harness::connect(device_transport, harness::device_hello(DEVICE_INSTANCE)).await;
    device
        .call(ControlRequest::Session(SessionRequest::Attach {
            session_id: session_id.clone(),
            since_seq: 0,
        }))
        .await
        .expect("手机 Attach");
    let mut device_stream = device
        .attach_session(&session_id, 0)
        .await
        .expect("手机事件流");

    // ── 双端同看同一个 Shell：第二端附着绝不能踢掉第一端，两边同收原样 PTY 字节 ──
    let shared_shell = harness::open_shell(&owner, &session_id).await;
    let mut owner_pty = owner
        .attach_shell(&shared_shell, 0)
        .await
        .expect("主设备附着 Shell");
    let mut device_pty = device
        .attach_shell(&shared_shell, 0)
        .await
        .expect("手机同时附着同一 Shell");
    owner
        .call(ControlRequest::Shell(ShellRequest::Write {
            shell_id: shared_shell.clone(),
            bytes: Bytes::from_static(b"multi-viewer\n"),
        }))
        .await
        .expect("写 Shell");
    let f_owner = tokio::time::timeout(Duration::from_secs(5), owner_pty.next_frame())
        .await
        .expect("主端应收到 PTY")
        .expect("主端帧");
    let f_device = tokio::time::timeout(Duration::from_secs(5), device_pty.next_frame())
        .await
        .expect("手机端应同时收到 PTY（不得被踢）")
        .expect("手机端帧");
    assert_eq!(f_owner.bytes.as_ref(), b"multi-viewer\n");
    assert_eq!(f_device.bytes.as_ref(), b"multi-viewer\n");

    // ── 首次驱动权：两端必须看到同一条 ──
    let claim = next_driver_changed(&mut owner_stream).await;
    assert_eq!(claim.0, OWNER_INSTANCE);
    let mirrored = next_driver_changed(&mut device_stream).await;
    assert_eq!(mirrored.0, claim.0);
    assert_eq!(mirrored.1, claim.1);

    // ── 非驱动端发指令：当场拒绝 ──
    let error = device
        .call(send_prompt(&session_id, "guard-05-device-1"))
        .await
        .expect_err("非驱动端发 SendPrompt 必须拒绝");
    assert_eq!(error.code(), Some(ErrorCode::PermissionDenied));

    // ── 交棒给手机 ──
    owner
        .call(ControlRequest::Session(SessionRequest::SwitchDriver {
            session_id: session_id.clone(),
            target_client_instance_id: Some(DEVICE_INSTANCE.to_string()),
        }))
        .await
        .expect("主设备交棒");
    let owner_handoff = next_driver_changed(&mut owner_stream).await;
    let device_handoff = next_driver_changed(&mut device_stream).await;
    assert_eq!(owner_handoff.1, device_handoff.1);
    assert_eq!(device_handoff.0, DEVICE_INSTANCE);
    assert!(device_handoff.1 > claim.1);

    // ── 交棒之后：手机能投递，主设备不能 ──
    device
        .call(send_prompt(&session_id, "guard-05-device-2"))
        .await
        .expect("驱动端必须能投递");
    let error = owner
        .call(send_prompt(&session_id, "guard-05-owner-2"))
        .await
        .expect_err("交棒后原驱动端必须被拒");
    assert_eq!(error.code(), Some(ErrorCode::PermissionDenied));

    // ── 让出驱动权 ──
    device
        .call(ControlRequest::Session(SessionRequest::SwitchDriver {
            session_id: session_id.clone(),
            target_client_instance_id: None,
        }))
        .await
        .expect("驱动端让出");
    let back = next_driver_changed(&mut device_stream).await;
    assert_eq!(back.0, OWNER_INSTANCE);
    assert!(back.1 > device_handoff.1);
    let mirrored_back = next_driver_changed(&mut owner_stream).await;
    assert_eq!(mirrored_back.1, back.1);

    // ── 吊销设备：即时踢流 + 清权限与附着 ──
    let devices = match owner
        .call(ControlRequest::Pairing(PairingRequest::ListDevices))
        .await
        .expect("主设备枚举设备表")
    {
        ControlResponse::Pairing(PairingResponse::Devices(devices)) => devices,
        other => panic!("设备表回执形状不符：{other:?}"),
    };
    let phone = devices
        .iter()
        .find(|device| device.label == "守卫手机")
        .expect("已配对设备必须在表里");
    assert!(phone.online);
    owner
        .call(ControlRequest::Pairing(PairingRequest::RevokeDevice {
            device_id: phone.device_id.clone(),
        }))
        .await
        .expect("吊销必须受理");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        match tokio::time::timeout_at(deadline, device_stream.next_frame()).await {
            Ok(Ok(_)) => continue,
            Ok(Err(_)) => break,
            Err(_) => panic!("吊销后 2s 内设备事件流仍未关闭"),
        }
    }
    let control_dead = tokio::time::timeout(
        Duration::from_secs(2),
        device.call(ControlRequest::System(SystemRequest::Health)),
    )
    .await;
    assert!(matches!(control_dead, Ok(Err(_))));
    for _ in 0..100 {
        if !session.attached().contains_key(&harness::paired_device()) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(!session.attached().contains_key(&harness::paired_device()));
    assert!(!session.can_access(&harness::paired_device()));

    println!(
        "[v3-guard-05/{}] 双端同看 Shell 一致；驱动权序号 {} → {} → {}；吊销 2s 内踢流并清权限",
        F::name(),
        claim.1,
        device_handoff.1,
        back.1
    );
}

fn send_prompt(session_id: &qaqh_core_vocab::SessionId, input: &str) -> ControlRequest {
    ControlRequest::Command(CommandRequest::SendPrompt {
        session_id: session_id.clone(),
        input_id: InputId::new(input).expect("input id"),
        text: format!("{input} 的正文"),
        attachments: Vec::new(),
    })
}

async fn next_driver_changed<S>(stream: &mut SessionEventsStream<S>) -> (String, u64)
where
    S: AsyncRead + Unpin,
{
    let deadline = tokio::time::Instant::now() + Duration::from_secs(10);
    loop {
        let frame = tokio::time::timeout_at(deadline, stream.next_frame())
            .await
            .expect("等待 DriverChanged 超时")
            .expect("事件帧");
        if let SessionEvent::DriverChanged {
            driver_client_instance_id,
            ordinal,
            ..
        } = frame.event
        {
            return (driver_client_instance_id, ordinal);
        }
    }
}
