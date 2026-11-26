//! 守卫⑤：**多端联动原语**（`ADR-0042` 铁律 4）。
//!
//! ## 单点定序
//!
//! 驱动权的每一次变更都由服务端在一处定序，然后**广播给所有附着端**：两台设备
//! 必须看到**同一个序号**。只要有第二个地方能改驱动权，两台设备就会各自认为自己
//! 能发指令——那是最难查的一类"偶发重复执行"。
//!
//! ## 非驱动端发指令当场拒绝
//!
//! 不是"排队等轮到我"，而是立刻 `PermissionDenied`：晚 200ms 拒绝等于用户已经
//! 以为发出去了。
//!
//! ## 吊销即时踢流
//!
//! 主设备吊销手机后，手机的流必须**立刻**结束（不是"下一次 RPC 才发现没权限"）。

use std::sync::Arc;
use std::time::Duration;

use qaqh_core_vocab::InputId;
use qaqh_client::v3::SessionEventsStream;
use qaqh_ringing_v3::{
    CommandRequest, ControlRequest, ControlResponse, ErrorCode, PairingRequest, PairingResponse,
    SessionEvent, SessionRequest, SystemRequest,
};
use qaqh_v3_transport::MuxTransport;
use tokio::io::AsyncRead;

use crate::v3_server::harness;

const OWNER_INSTANCE: &str = "guard-05-owner";
const DEVICE_INSTANCE: &str = "guard-05-device";

pub(crate) async fn guard<B: MuxTransport>(pair: fn() -> (B, B)) {
    let state = harness::state();
    let session = harness::session_for(&state, &harness::paired_device());
    let session_id = session.id.clone();

    // 主设备连上并附着：首个附着者取得驱动权。
    let (owner_transport, owner_server_transport) = pair();
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

    // 手机连上并附着：拿不到驱动权。
    let (device_transport, device_server_transport) = pair();
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

    // ── 首次驱动权：两端必须看到同一条 ──
    let claim = next_driver_changed(&mut owner_stream).await;
    assert_eq!(claim.0, OWNER_INSTANCE);
    let mirrored = next_driver_changed(&mut device_stream).await;
    assert_eq!(mirrored.0, claim.0, "两端看到的驱动端必须一致");
    assert_eq!(mirrored.1, claim.1, "两端看到的驱动权序号必须一致");

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
    assert_eq!(owner_handoff.1, device_handoff.1, "交棒后两端序号必须一致");
    assert_eq!(device_handoff.0, DEVICE_INSTANCE);
    assert!(
        device_handoff.1 > claim.1,
        "驱动权序号必须单调递增（{} → {}）",
        claim.1,
        device_handoff.1
    );

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

    // ── 由当前驱动端**让出**（已配对设备不得把驱动权指派给别的设备，只能让出） ──
    // ── 让出后由服务端挑"下一个别的设备"，同样只能单点定序 ──
    device
        .call(ControlRequest::Session(SessionRequest::SwitchDriver {
            session_id: session_id.clone(),
            target_client_instance_id: None,
        }))
        .await
        .expect("驱动端让出");
    let back = next_driver_changed(&mut device_stream).await;
    assert_eq!(back.0, OWNER_INSTANCE);
    assert!(
        back.1 > device_handoff.1,
        "第三次切换的序号必须继续递增（{} → {}）",
        device_handoff.1,
        back.1
    );
    let mirrored_back = next_driver_changed(&mut owner_stream).await;
    assert_eq!(mirrored_back.1, back.1, "交回后两端序号必须一致");

    // ── 吊销设备：即时踢流 ──
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
    // `online` 在骨架期的口径＝"至少附着了一个会话"。手机此刻正附着着，所以必须在线。
    assert!(phone.online, "手机已附着会话，设备表必须显示在线");
    owner
        .call(ControlRequest::Pairing(PairingRequest::RevokeDevice {
            device_id: phone.device_id.clone(),
        }))
        .await
        .expect("吊销必须受理");

    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    loop {
        match tokio::time::timeout_at(deadline, device_stream.next_frame()).await {
            // 吊销前已在队列里的帧：放行，但最多 2 秒。
            Ok(Ok(_)) => continue,
            Ok(Err(_)) => break,
            Err(_) => panic!("吊销后 2s 内设备事件流仍未关闭（不是即时踢流）"),
        }
    }
    let control_dead = tokio::time::timeout(
        Duration::from_secs(2),
        device.call(ControlRequest::System(SystemRequest::Health)),
    )
    .await;
    match control_dead {
        Ok(Err(_)) => {}
        Ok(Ok(_)) => panic!("吊销后设备控制面仍然可用"),
        Err(_) => panic!("吊销后设备控制面在 2s 内没有收流"),
    }
    // 附着表也要跟着干净（不能留下幽灵在线设备）。
    for _ in 0..100 {
        if !session.attached().contains_key(&harness::paired_device()) {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert!(
        !session.attached().contains_key(&harness::paired_device()),
        "吊销后附着表必须清掉该设备"
    );

    println!(
        "[v3-guard-05] 驱动权序号 {} → {} → {}（两端一致）；非驱动端 2 次拒绝；吊销 2s 内踢流并清附着",
        claim.1, device_handoff.1, back.1
    );
}

fn send_prompt(
    session_id: &qaqh_core_vocab::SessionId,
    input: &str,
) -> ControlRequest {
    ControlRequest::Command(CommandRequest::SendPrompt {
        session_id: session_id.clone(),
        input_id: InputId::new(input).expect("input id"),
        text: format!("{input} 的正文"),
        attachments: Vec::new(),
    })
}

/// 读到下一条 `DriverChanged`，返回 `(驱动端实例, 序号)`。
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
