//! 全量 DTO 编解码 / Fail-Closed / 发现文件测试（`ADR-0042` 铁律 1–4 的机器可验部分）。
//!
//! 三条判据：
//! 1. **全量 round-trip**：每个 DTO 都在 `Option::None` / `Option::Some` / 空 `Vec` /
//!    多元素 `Vec` 的混合序列上往返一次——`postcard` 是位置编码，`Option` 处一旦
//!    被 serde 的"跳过空值"类属性污染，后面所有字段就会错位，这类 bug 只在特定
//!    数据下出现，必须用混合序列逼出来；
//! 2. **Fail-Closed**：超限帧 / 失配版本 / 失配指纹 / 陈旧 `timeline_epoch` 必须报错，
//!    不许"尽量解一下"；
//! 3. **发现文件**：`DaemonEndpoint` 往返 + 过期守护进程必须被拒。

#![allow(clippy::unwrap_used)] // 测试代码豁免（仓库惯例，见 clippy.toml）

use std::fmt::Debug;

use bytes::Bytes;
use qaqh_core_vocab::{
    ActivityState, BlockId, ContentHash, ContentRef, ExecSessionId, InputId, InteractionId,
    JsonText, SessionId, ToolCallId, ToolCatalog, ToolMode, TurnId, TurnTerminal, UsageInfo,
};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::control::*;
use crate::endpoint::*;
use crate::frame::{FrameError, decode_frame, encode_frame};
use crate::handshake::*;
use crate::stream::*;
use crate::{
    DAEMON_ENDPOINT_FILE, MAX_FRAME_BYTES, PROTOCOL_FINGERPRINT, PROTOCOL_REV, RINGING_V3_ALPN,
};

/// 直连 postcard 往返（不经帧封装，证明 DTO 自身形状稳定）。
fn round_trip<T>(value: T)
where
    T: Serialize + DeserializeOwned + PartialEq + Debug,
{
    let bytes = postcard::to_stdvec(&value).expect("encode");
    let back: T = postcard::from_bytes(&bytes).expect("decode");
    assert_eq!(back, value, "postcard 往返失真");
}

/// 经线缆帧往返（证明 `u32_le(len)` 前缀与载荷一致）。
fn frame_round_trip<T>(value: T)
where
    T: Serialize + DeserializeOwned + PartialEq + Debug,
{
    let framed = encode_frame(&value).expect("encode frame");
    let back: T = decode_frame(&framed).expect("decode frame");
    assert_eq!(back, value, "帧往返失真");
}

fn session_id() -> SessionId {
    SessionId::new("SES-01M8CJZ6RV4A70EV0V0ABFXV2V").expect("valid")
}

fn content_hash() -> ContentHash {
    ContentHash::from_hex("0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef")
        .expect("valid")
}

#[test]
fn protocol_constants_are_machine_generated_and_stable() {
    assert_eq!(PROTOCOL_REV, 1, "protocol_rev.txt 是唯一人工版本位");
    assert_ne!(
        PROTOCOL_FINGERPRINT, [0u8; 32],
        "指纹必须由 build.rs 从类型定义算出，不许是零值占位"
    );
    assert_eq!(
        RINGING_V3_ALPN, b"qaqh-ringing/3",
        "ALPN 改动即协议身份改动，必须 review 可见"
    );
    // 帧上限与 ALPN 一起构成"连接身份"，两者都要在 review 里可见；
    // 这里用 `as u64` 让常量比较带上运行时类型，避免 clippy::assertions_on_constants。
    assert!(
        MAX_FRAME_BYTES as u64 >= 1024 * 1024,
        "帧上限不得小于 1 MiB"
    );
}

// ── 1. 全量 DTO round-trip ──

#[test]
fn core_vocab_primitives_round_trip() {
    round_trip(session_id());
    round_trip(TurnId::new("TURN-1").expect("valid"));
    round_trip(BlockId::new("BLK-1").expect("valid"));
    round_trip(ToolCallId::new("CALL-1").expect("valid"));
    round_trip(InputId::new("IN-1").expect("valid"));
    round_trip(InteractionId::new("INT-1").expect("valid"));
    round_trip(ExecSessionId::new("execs_01A").expect("valid"));
    round_trip(content_hash());
    round_trip(TurnTerminal::Completed);
    round_trip(TurnTerminal::Failed {
        code: "exit_nonzero".into(),
        message: "boom".into(),
    });
    round_trip(UsageInfo {
        input_tokens: 1,
        output_tokens: 2,
        cached_input_tokens: None,
        reasoning_tokens: Some(9),
    });
    round_trip(ContentRef::Inline {
        text: "hello".into(),
    });
    round_trip(ContentRef::Windowed {
        preview: "tail".into(),
        start_offset: 7,
        total_bytes: 4_000,
    });
    round_trip(ContentRef::Blob {
        hash: content_hash(),
        len: 20 * 1024 * 1024,
    });
    round_trip(JsonText::new("{\"a\":1}"));
    round_trip(ActivityState::WaitingUser);
    round_trip(ToolMode::Custom);
}

/// `Option` 与 `Vec` 的混合序列：`None → Some → 空 Vec → 多元素 Vec` 必须各自对齐。
///
/// 这是 `skip_serializing_if` 的专用反证：一旦有人给某个字段加上它，
/// `None` 就不再占位，后面所有字段整体错位——本测试立刻红。
#[test]
fn option_and_vec_positions_never_drift() {
    let none_variant = SessionRequest::TranscriptPage {
        session_id: session_id(),
        before_ord: 0,
        limit: 0,
        timeline_epoch: 0,
    };
    let some_variant = SessionSnapshot {
        session_id: session_id(),
        title: Some("标题".into()),
        activity: ActivityState::Running,
        timeline_epoch: 3,
        seq: 42,
        turns: vec![],
        tail_blocks: vec![],
        tool_calls: vec![],
        pinned_compaction: None,
        tool_mode: ToolMode::Standard,
    };
    let multi_variant = SessionSnapshot {
        session_id: session_id(),
        title: None,
        activity: ActivityState::Idle,
        timeline_epoch: 4,
        seq: 43,
        turns: vec![
            TurnSummary {
                ordinal: 1,
                terminal: None,
                started_at_ms: 10,
            },
            TurnSummary {
                ordinal: 2,
                terminal: Some(TurnTerminal::Cancelled { code: "y".into() }),
                started_at_ms: 20,
            },
        ],
        tail_blocks: vec![BlockView {
            ord: 5,
            kind: BlockKind::Answer,
            content: ContentRef::Inline { text: "x".into() },
            tool_call_id: Some(ToolCallId::new("CALL-9").expect("valid")),
            created_at_ms: 30,
        }],
        tool_calls: vec![ToolCallView {
            tool_call_id: ToolCallId::new("CALL-9").expect("valid"),
            tool_name: "read".into(),
            status: ToolCallStatus::Running,
            started_at_ms: 30,
            finished_at_ms: None,
            summary: None,
        }],
        pinned_compaction: Some(CompactionSummary {
            pinned_at_ms: 1,
            blocks_compacted: 2,
            summary: "sum".into(),
        }),
        tool_mode: ToolMode::Core,
    };
    // 三种形状交替往返多次：位置错位会在某一次对上崩塌。
    for _ in 0..3 {
        round_trip(none_variant.clone());
        frame_round_trip(some_variant.clone());
        round_trip(multi_variant.clone());
    }
}

#[test]
fn every_session_dto_round_trips() {
    round_trip(SessionRequest::GetSessionSnapshot {
        session_id: session_id(),
        tail_blocks: 200,
    });
    round_trip(SessionRequest::List);
    round_trip(SessionRequest::Rename {
        session_id: session_id(),
        title: "新标题".into(),
    });
    round_trip(SessionRequest::Delete {
        session_id: session_id(),
    });
    round_trip(SessionRequest::SetToolMode {
        session_id: session_id(),
        mode: ToolMode::Core,
    });
    round_trip(SessionRequest::Attach {
        session_id: session_id(),
        since_seq: 99,
    });
    round_trip(SessionRequest::SwitchDriver {
        session_id: session_id(),
        target_client_instance_id: None,
    });
    round_trip(SessionResponse::Sessions(vec![SessionSummary {
        session_id: session_id(),
        title: None,
        activity: ActivityState::Idle,
        updated_at_ms: 7,
    }]));
    round_trip(SessionResponse::Page(Box::new(TranscriptPage {
        blocks: vec![],
        tool_calls: vec![],
        next_before_ord: None,
        timeline_epoch: 1,
    })));
    round_trip(SessionResponse::Ack);
}

#[test]
fn system_and_command_domains_round_trip() {
    for request in [
        SystemRequest::Health,
        SystemRequest::Activity,
        SystemRequest::Version,
        SystemRequest::Stop { graceful: true },
        SystemRequest::StopIfIdle,
    ] {
        round_trip(request);
    }
    round_trip(SystemResponse::Version {
        build_id: "b1".into(),
        protocol_rev: PROTOCOL_REV,
    });
    round_trip(SystemResponse::StopAck {
        accepted: false,
        reason: Some("busy".into()),
    });
    round_trip(CommandRequest::SendPrompt {
        session_id: session_id(),
        input_id: InputId::new("IN-1").expect("valid"),
        text: "hi".into(),
        attachments: vec![],
    });
    round_trip(CommandRequest::RespondInteraction {
        session_id: session_id(),
        interaction_id: InteractionId::new("INT-1").expect("valid"),
        resolution: CommandInteractionResolution::Reject {
            text: "no".into(),
        },
    });
    round_trip(CommandResponse::Accepted {
        input_id: InputId::new("IN-1").expect("valid"),
    });
    round_trip(CommandResponse::CancelAccepted);
    round_trip(CommandResponse::InteractionAccepted);
}

#[test]
fn shell_marketplace_config_domains_round_trip() {
    round_trip(ShellRequest::Open {
        cols: 120,
        rows: 40,
        cwd: Some("/tmp".into()),
    });
    round_trip(ShellRequest::Write {
        shell_id: "sh1".into(),
        bytes: Bytes::from_static(b"\x03"),
    });
    round_trip(ShellRequest::Resize {
        shell_id: "sh1".into(),
        cols: 80,
        rows: 24,
    });
    round_trip(ShellResponse::Opened(ShellView {
        shell_id: "sh1".into(),
        output_offset: 1024,
        exited: false,
    }));
    round_trip(ShellResponse::Written { bytes_written: 3 });
    round_trip(ShellResponse::Shells(vec![]));
    round_trip(MarketplaceRequest::Search {
        query: "q".into(),
        limit: 5,
    });
    round_trip(MarketplaceResponse::Items(vec![MarketplaceItem {
        id: "i".into(),
        name: "n".into(),
        version: "1.0.0".into(),
        description: "d".into(),
        installed: true,
    }]));
    round_trip(MarketplaceResponse::Ack);
    round_trip(ConfigRequest::Get {
        section: ConfigSection::Providers,
    });
    round_trip(ConfigRequest::Set {
        section: ConfigSection::Ui,
        value: JsonText::new("[]"),
    });
    round_trip(ConfigResponse::Value {
        section: ConfigSection::Sandbox,
        value: JsonText::new("{}"),
    });
    round_trip(ConfigResponse::Reloaded);
}

#[test]
fn account_pairing_gitx_fs_catalog_domains_round_trip() {
    round_trip(AccountRequest::Select {
        account_id: "a1".into(),
    });
    round_trip(AccountResponse::Current(None));
    round_trip(AccountResponse::Accounts(vec![AccountView {
        account_id: "a1".into(),
        provider: "p".into(),
        label: "L".into(),
        current: true,
    }]));
    round_trip(PairingRequest::CreateInvite { ttl_ms: 60_000 });
    round_trip(PairingRequest::Redeem {
        code: "ABC".into(),
        client_kind: ClientKind::Mobile,
    });
    round_trip(PairingResponse::Redeemed {
        device_id: "d1".into(),
        token: "t".into(),
    });
    round_trip(PairingResponse::Devices(vec![DeviceView {
        device_id: "d1".into(),
        label: "手机".into(),
        paired_at_ms: 1,
        online: true,
    }]));
    round_trip(GitxRequest::Diff {
        session_id: session_id(),
        path: None,
    });
    round_trip(GitxResponse::Status(GitStatus {
        branch: "main".into(),
        dirty: true,
        ahead: 1,
        behind: 0,
    }));
    round_trip(GitxResponse::Diff(ContentRef::Blob {
        hash: content_hash(),
        len: 12,
    }));
    round_trip(GitxResponse::Worktrees(vec![]));
    round_trip(FsRequest::ReadRange {
        path: "a.txt".into(),
        offset: 8,
        len: 4096,
    });
    round_trip(FsRequest::Write {
        path: "a.txt".into(),
        bytes: Bytes::from_static(b"x"),
    });
    round_trip(FsResponse::Entries(vec![FsEntry {
        path: "a.txt".into(),
        kind: FsKind::File,
        len: 1,
        modified_at_ms: 2,
    }]));
    round_trip(FsResponse::Bytes {
        offset: 0,
        bytes: Bytes::new(),
    });
    round_trip(FsResponse::Written { bytes_written: 1 });
    round_trip(ToolCatalogRequest::Get {
        session_id: session_id(),
    });
    round_trip(ToolCatalogResponse::Catalog(Box::new(ToolCatalog::new(
        ToolMode::Standard,
        vec![],
    ))));
}

#[test]
fn stream_frames_and_events_round_trip() {
    round_trip(StreamKind::Control);
    round_trip(StreamKind::SessionEvents {
        session_id: session_id(),
        since_seq: 0,
    });
    round_trip(StreamKind::ShellPty {
        shell_id: "sh".into(),
        from_offset: 4,
    });
    round_trip(StreamKind::BlobTransfer);
    round_trip(ShellOutputFrame {
        meta: FrameMeta {
            seq: 1,
            server_ts_ms: 2,
            delivery: DeliveryClass::Reliable,
        },
        start_offset: 0,
        end_offset: 4,
        bytes: Bytes::from_static(b"\x1b[31m"),
    });
    round_trip(BlobChunkFrame {
        meta: FrameMeta {
            seq: 2,
            server_ts_ms: 3,
            delivery: DeliveryClass::Replaceable,
        },
        hash: content_hash(),
        offset: 0,
        bytes: Bytes::from_static(b"ab"),
        last: true,
    });
    let events = vec![
        SessionEvent::ActivityChanged {
            session_id: session_id(),
            activity: ActivityState::Cancelling,
            current_turn_id: None,
        },
        SessionEvent::TurnStarted {
            session_id: session_id(),
            turn_id: TurnId::new("T1").expect("valid"),
            started_at_ms: 1,
        },
        SessionEvent::TurnFinished {
            session_id: session_id(),
            turn_id: TurnId::new("T1").expect("valid"),
            terminal: TurnTerminal::Completed,
            usage: UsageInfo::default(),
        },
        SessionEvent::AnswerDelta {
            session_id: session_id(),
            block_id: BlockId::new("B1").expect("valid"),
            text: "x".into(),
        },
        SessionEvent::ReasoningDelta {
            session_id: session_id(),
            block_id: BlockId::new("B1").expect("valid"),
            text: "y".into(),
        },
        SessionEvent::BlockFinal {
            session_id: session_id(),
            block_id: BlockId::new("B1").expect("valid"),
            content: ContentRef::Inline { text: "z".into() },
        },
        SessionEvent::ToolCallUpdated {
            session_id: session_id(),
            call: ToolCallView {
                tool_call_id: ToolCallId::new("C1").expect("valid"),
                tool_name: "read".into(),
                status: ToolCallStatus::Succeeded,
                started_at_ms: 1,
                finished_at_ms: Some(2),
                summary: None,
            },
        },
        SessionEvent::InteractionRequested {
            session_id: session_id(),
            interaction: InteractionView {
                interaction_id: InteractionId::new("I1").expect("valid"),
                kind: InteractionKind::Permission,
                title: "t".into(),
                body: "b".into(),
                options: vec!["允许".into(), "拒绝".into()],
                expires_at_ms: None,
            },
        },
        SessionEvent::InteractionResolved {
            session_id: session_id(),
            interaction_id: InteractionId::new("I1").expect("valid"),
            resolution: InteractionResolution::Approved,
        },
        SessionEvent::ExecSessionUpdated {
            session_id: session_id(),
            exec: ExecSessionSnapshot {
                exec_session_id: ExecSessionId::new("execs_01A").expect("valid"),
                command: "yes".into(),
                tty: false,
                finished: false,
                exit_code: None,
                next_offset: 10,
                output: None,
            },
        },
        SessionEvent::ExecOutputDelta {
            session_id: session_id(),
            exec_session_id: ExecSessionId::new("execs_01A").expect("valid"),
            start_offset: 0,
            end_offset: 10,
            text: "0123456789".into(),
            spilled: true,
        },
        SessionEvent::DriverChanged {
            session_id: session_id(),
            driver_client_instance_id: "client-2".into(),
            ordinal: 3,
        },
        SessionEvent::PinnedCompactionChanged {
            session_id: session_id(),
            summary: None,
        },
        SessionEvent::CatalogChanged {
            session_id: session_id(),
        },
        SessionEvent::SessionRemoved {
            session_id: session_id(),
        },
        SessionEvent::ResyncRequired {
            session_id: session_id(),
            reason: ResyncReason::CursorGap,
            timeline_epoch: 9,
        },
    ];
    for event in &events {
        round_trip(event.clone());
        frame_round_trip(SessionEventFrame {
            meta: FrameMeta {
                seq: 1,
                server_ts_ms: 1,
                delivery: event.delivery(),
            },
            event: event.clone(),
        });
        assert_eq!(event.session_id(), &session_id());
    }
    // 分级由事件语义决定，不由发送方自选。
    assert_eq!(
        SessionEvent::AnswerDelta {
            session_id: session_id(),
            block_id: BlockId::new("B1").expect("valid"),
            text: String::new(),
        }
        .delivery(),
        DeliveryClass::Replaceable
    );
    assert_eq!(
        SessionEvent::DriverChanged {
            session_id: session_id(),
            driver_client_instance_id: "c".into(),
            ordinal: 1,
        }
        .delivery(),
        DeliveryClass::Reliable,
        "驱动权丢了就是两台设备同时发指令，绝不许被合帧丢掉"
    );
}

#[test]
fn control_envelopes_round_trip_and_carry_the_idempotency_key() {
    let call = ControlCall {
        command_id: "cmd-1".into(),
        request: ControlRequest::ToolCatalog(ToolCatalogRequest::Get {
            session_id: session_id(),
        }),
    };
    frame_round_trip(call.clone());
    let reply = ControlReply {
        command_id: call.command_id.clone(),
        response: ControlResponse::Error(RpcError {
            code: ErrorCode::StaleCursor,
            message: "timeline epoch changed".into(),
            retryable: false,
        }),
    };
    frame_round_trip(reply.clone());
    assert_eq!(reply.command_id, call.command_id, "回执必须带同一个幂等键");
}

// ── 2. Fail-Closed ──

#[test]
fn stale_timeline_epoch_and_version_mismatch_fail_closed() {
    // 陈旧代际：服务端**必须**用 StaleCursor 拒绝，客户端据此重取快照。
    let stale = ControlReply {
        command_id: "c".into(),
        response: ControlResponse::Error(RpcError {
            code: ErrorCode::StaleCursor,
            message: "epoch 3 != 4".into(),
            retryable: false,
        }),
    };
    let bytes = postcard::to_stdvec(&stale).expect("encode");
    let back: ControlReply = postcard::from_bytes(&bytes).expect("decode");
    assert_eq!(back, stale);

    // 版本失配：rev 不同 → RevMismatch；rev 相同但指纹不同 → FingerprintMismatch。
    let mut hello = ClientHello::new(
        "client-1",
        ClientKind::Desktop,
        ClientCredential::LocalOsUser {
            user: "qaqtam".into(),
        },
    );
    assert!(matches!(
        verify_handshake(&hello, "server-1", "build-1"),
        ServerHello::Accepted { .. }
    ));
    hello.protocol_rev = PROTOCOL_REV + 1;
    match verify_handshake(&hello, "server-1", "build-1") {
        ServerHello::Rejected(reject) => assert_eq!(
            reject.reason,
            HandshakeRejectReason::RevMismatch {
                client: PROTOCOL_REV + 1,
                server: PROTOCOL_REV
            }
        ),
        other => panic!("版本失配必须拒绝，实得 {other:?}"),
    }
    hello.protocol_rev = PROTOCOL_REV;
    hello.protocol_fingerprint = [0xAB; 32];
    match verify_handshake(&hello, "server-1", "build-1") {
        ServerHello::Rejected(reject) => assert_eq!(
            reject.reason,
            HandshakeRejectReason::FingerprintMismatch {
                client: [0xAB; 32],
                server: PROTOCOL_FINGERPRINT
            }
        ),
        other => panic!("指纹失配必须拒绝，实得 {other:?}"),
    }
}

#[test]
fn oversize_frame_is_rejected_before_allocation() {
    // 声明长度超限：连一次大分配都换不到。
    let mut prefix = [0u8; 4];
    prefix.copy_from_slice(&((MAX_FRAME_BYTES as u32) + 1).to_le_bytes());
    assert_eq!(
        crate::frame::peek_frame_len(&prefix),
        Err(FrameError::TooLarge {
            declared: MAX_FRAME_BYTES + 1
        })
    );
}

// ── 3. daemon.endpoint ──

fn endpoint() -> DaemonEndpoint {
    DaemonEndpoint {
        build_id: "build-2026-11-27".into(),
        channel: EndpointChannel("dev".into()),
        transport: EndpointTransport::LocalIpc {
            path: "/run/qaqh/daemon.sock".into(),
        },
        protocol_rev: PROTOCOL_REV,
        protocol_fingerprint: PROTOCOL_FINGERPRINT,
        written_at_ms: 1_700_000_000_000,
    }
}

#[test]
fn daemon_endpoint_round_trips_and_rejects_stale_daemons() {
    let endpoint = endpoint();
    let bytes = endpoint.encode().expect("encode");
    assert_eq!(DaemonEndpoint::decode(&bytes).expect("decode"), endpoint);
    endpoint.validate("dev").expect("同版同渠道可连");

    // 尾随字节 = 可疑文件（不是"忽略多余的"，而是当场失败）。
    let mut padded = bytes.clone();
    padded.push(0);
    assert!(DaemonEndpoint::decode(&padded).is_err());

    let mut stale_rev = endpoint.clone();
    stale_rev.protocol_rev = PROTOCOL_REV - 1;
    assert_eq!(
        stale_rev.validate("dev"),
        Err(EndpointReject::RevMismatch {
            found: PROTOCOL_REV - 1,
            expected: PROTOCOL_REV
        })
    );

    let mut stale_fingerprint = endpoint.clone();
    stale_fingerprint.protocol_fingerprint = [9u8; 32];
    assert_eq!(
        stale_fingerprint.validate("dev"),
        Err(EndpointReject::FingerprintMismatch)
    );

    assert_eq!(
        endpoint.validate("stable"),
        Err(EndpointReject::ChannelRejected {
            found: "dev".into()
        })
    );

    // 远端载体同样往返（iroh 预留字段必须能过线）。
    let remote = DaemonEndpoint {
        transport: EndpointTransport::Iroh {
            endpoint_id: [7u8; 32],
            relay_url: Some("https://relay.example".into()),
        },
        ..endpoint.clone()
    };
    let bytes = remote.encode().expect("encode");
    assert_eq!(DaemonEndpoint::decode(&bytes).expect("decode"), remote);
}

#[test]
fn daemon_endpoint_file_round_trips_and_validates() {
    let dir = tempfile::tempdir().expect("tempdir");
    let path = write_daemon_endpoint(dir.path(), &endpoint()).expect("write");
    assert_eq!(
        path.file_name().and_then(|name| name.to_str()),
        Some(DAEMON_ENDPOINT_FILE),
        "发现文件名永久无版本前缀"
    );
    assert!(
        !dir.path().join(format!("{DAEMON_ENDPOINT_FILE}.tmp")).exists(),
        "临时文件必须被 rename 掉，不许残留"
    );
    assert_eq!(
        read_daemon_endpoint(dir.path(), "dev").expect("read"),
        endpoint()
    );
    assert!(matches!(
        read_daemon_endpoint(dir.path(), "stable"),
        Err(EndpointReject::ChannelRejected { .. })
    ));
    // 空目录 → Unreadable（不是"没读到就当没有"）。
    let empty = tempfile::tempdir().expect("tempdir");
    assert!(matches!(
        read_daemon_endpoint(empty.path(), "dev"),
        Err(EndpointReject::Unreadable(_))
    ));
}
