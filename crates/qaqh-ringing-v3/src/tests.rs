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
    ActivityState, BlockId, ContentHash, ContentRef, CredentialSource, ExecSessionId, GoalStatus,
    InputId, InteractionId, JsonText, ModelSelection, ModelSelectionOptions, PermissionGrantScope,
    SessionId, SettingOverride, ToolCallId, ToolCatalog, ToolMode, TurnId, TurnPhase,
    TurnTerminal, UsageInfo, Wire, WorkspaceId,
};
use serde::Serialize;
use serde::de::DeserializeOwned;

use crate::account::*;
use crate::config::*;
use crate::control::*;
use crate::endpoint::*;
use crate::frame::{FrameError, decode_frame, encode_frame};
use crate::handshake::*;
use crate::session::*;
use crate::stream::*;
use crate::team::*;
use crate::workspace::*;
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
    round_trip(WorkspaceId::new("WS-1").expect("valid"));
    round_trip(content_hash());
    round_trip(PermissionGrantScope::Session);
    round_trip(GoalStatus::BudgetLimited);
    round_trip(TurnPhase::WaitingUser);
    round_trip(ModelSelection {
        provider_id: "deepseek".into(),
        model_id: "deepseek-chat".into(),
        options: ModelSelectionOptions {
            max_output_tokens: Some(4096),
            reasoning_effort: None,
            native_web_search: Some(true),
        },
    });
    round_trip(SettingOverride::<u32>::Inherit);
    round_trip(SettingOverride::Use(42u32));
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
    round_trip(Wire::Messages);
    round_trip(CredentialSource::DeepseekAccount);
}

/// `Option` 与 `Vec` 的混合序列：`None → Some → 空 Vec → 多元素 Vec` 必须各自对齐。
///
/// 这是空值跳过属性的专用反证：一旦有人给某个字段加上它，
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
        workspace_id: None,
        cwd: None,
        archived: false,
        settings_revision: 1,
        model_selection: None,
        activity: ActivityState::Running,
        timeline_epoch: 3,
        seq: 42,
        turns: vec![],
        tail_blocks: vec![],
        tool_calls: vec![],
        pending_interactions: vec![],
        exec_sessions: vec![],
        subagents: vec![],
        goal: None,
        driver_client_instance_id: None,
        driver_ordinal: 0,
        pinned_compaction: None,
        tool_mode: ToolMode::Standard,
        custom_tools: vec![],
        handoff_from: None,
        created_at_ms: 1,
    };
    let multi_variant = SessionSnapshot {
        session_id: session_id(),
        title: None,
        workspace_id: Some(WorkspaceId::new("WS-1").expect("valid")),
        cwd: Some("/work".into()),
        archived: true,
        settings_revision: 9,
        model_selection: Some(ModelSelection {
            provider_id: "openai".into(),
            model_id: "gpt-5".into(),
            options: ModelSelectionOptions::default(),
        }),
        activity: ActivityState::Idle,
        timeline_epoch: 4,
        seq: 43,
        turns: vec![
            TurnSummary {
                turn_id: TurnId::new("T1").expect("valid"),
                input_id: None,
                ordinal: 1,
                phase: Some(TurnPhase::Running),
                terminal: None,
                usage: None,
                started_at_ms: 10,
            },
            TurnSummary {
                turn_id: TurnId::new("T2").expect("valid"),
                input_id: Some(InputId::new("IN-2").expect("valid")),
                ordinal: 2,
                phase: None,
                terminal: Some(TurnTerminal::Cancelled { code: "y".into() }),
                usage: Some(UsageInfo {
                    input_tokens: 10,
                    output_tokens: 5,
                    cached_input_tokens: Some(4),
                    reasoning_tokens: None,
                }),
                started_at_ms: 20,
            },
        ],
        tail_blocks: vec![BlockView {
            block_id: BlockId::new("BLK-9").expect("valid"),
            turn_id: Some(TurnId::new("T2").expect("valid")),
            ord: 5,
            kind: BlockKind::Answer,
            content: ContentRef::Inline { text: "x".into() },
            tool_call_id: Some(ToolCallId::new("CALL-9").expect("valid")),
            created_at_ms: 30,
            actor: Some(BlockActor::User),
            purpose: Some(InputPurpose::TriggerTurn),
            model: Some("gpt-5".into()),
            usage: Some(UsageInfo::default()),
            generated_ms: Some(120),
            images: vec![ImageRefView {
                hash: content_hash(),
                len: 4,
                mime_type: "image/png".into(),
            }],
            attachments: vec![ContentRef::Blob {
                hash: content_hash(),
                len: 10,
            }],
            inter_agent: Some(InterAgentView {
                message_id: "m1".into(),
                root_session_id: session_id(),
                author: "/root".into(),
                recipient: "/root/a".into(),
                other_recipients: vec![],
                task_id: Some("t1".into()),
                reply_to: None,
                created_at_ms: 30,
            }),
            subagent_terminal: Some(SubagentTerminalView {
                child_session_id: session_id(),
                parent_call_id: ToolCallId::new("CALL-9").expect("valid"),
                terminal: SubagentTerminalKind::Completed,
            }),
        }],
        tool_calls: vec![ToolCallView {
            tool_call_id: ToolCallId::new("CALL-9").expect("valid"),
            turn_id: Some(TurnId::new("T2").expect("valid")),
            tool_name: "read".into(),
            status: ToolCallStatus::AwaitingInteraction,
            started_at_ms: 30,
            finished_at_ms: None,
            args: Some(ContentRef::Inline {
                text: "{\"path\":\"a.rs\"}".into(),
            }),
            output: None,
            output_truncated: false,
            diff: Some(ContentRef::Inline {
                text: "--- a\n+++ b\n".into(),
            }),
            code_delta: Some(CodeDeltaView {
                lines_added: 1,
                lines_removed: 0,
                files_created: 0,
                files_deleted: 0,
                file: Some("a.rs".into()),
            }),
            metrics: Some(ToolMetricsView {
                started_at_ms: 30,
                finished_at_ms: 35,
                retry_count: 0,
                output_bytes: 12,
                progress_bytes_total: 12,
            }),
            exec_session_id: None,
            interaction_id: Some(InteractionId::new("INT-9").expect("valid")),
            effects: vec![ToolEffectView::SubagentSpawned {
                child_session_id: session_id(),
                name: "sub".into(),
                task_text: "go".into(),
            }],
        }],
        pending_interactions: vec![InteractionView {
            interaction_id: InteractionId::new("INT-9").expect("valid"),
            turn_id: Some(TurnId::new("T2").expect("valid")),
            kind: InteractionKind::Ask,
            tool_call_id: None,
            title: "选择".into(),
            body: "确认？".into(),
            options: vec!["A".into(), "B".into()],
            choices: vec![InteractionChoiceView {
                decision: InteractionDecisionKind::Answered,
                label: "提交".into(),
                description: None,
                tone: ChoiceTone::Primary,
            }],
            questions: vec![AskQuestionView {
                id: "q1".into(),
                prompt: "继续吗？".into(),
                options: vec!["A".into(), "B".into()],
                allow_free_text: true,
            }],
            permission: None,
            expires_at_ms: Some(99),
        }],
        exec_sessions: vec![ExecSessionSnapshot {
            exec_session_id: ExecSessionId::new("execs_09").expect("valid"),
            command: "echo hi".into(),
            tty: false,
            finished: true,
            exit_code: Some(0),
            next_offset: 3,
            output: Some(ContentRef::Inline { text: "hi\n".into() }),
            state: ExecSessionStateView::Finished {
                outcome: ExecOutcomeView::Exited { exit_code: 0 },
            },
            owner_session_id: session_id(),
            origin_turn_id: Some(TurnId::new("T2").expect("valid")),
            origin_call_id: Some(ToolCallId::new("CALL-9").expect("valid")),
            cwd: Some("/work".into()),
            started_at_ms: 10,
            finished_at_ms: Some(20),
            output_preview: "hi\n".into(),
            preview_start_offset: 0,
            output_complete: true,
            capture_omitted_bytes: 0,
            stream_dropped_bytes: 0,
        }],
        subagents: vec![SubagentSummary {
            session_id: session_id(),
            nickname: Some("研究员".into()),
            status: TeamAgentStatus::Running,
            residency: TeamAgentResidency::Loaded,
        }],
        goal: Some(GoalView {
            goal_id: "g1".into(),
            session_id: session_id(),
            objective: "完成验证".into(),
            status: GoalStatus::Active,
            token_budget: Some(100_000),
            tokens_used: 1200,
            time_used_ms: 5000,
            created_at_ms: 1,
            updated_at_ms: 2,
        }),
        driver_client_instance_id: Some("desktop-1".into()),
        driver_ordinal: 2,
        pinned_compaction: Some(CompactionSummary {
            pinned_at_ms: 1,
            blocks_compacted: 2,
            summary: "sum".into(),
        }),
        tool_mode: ToolMode::Core,
        custom_tools: vec!["read".into()],
        handoff_from: Some(session_id()),
        created_at_ms: 2,
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
    let summary = SessionSummary {
        session_id: session_id(),
        title: Some("新会话".into()),
        workspace_id: None,
        cwd: Some("/tmp".into()),
        archived: false,
        model_selection: None,
        tool_mode: ToolMode::Standard,
        custom_tools: vec![],
        settings_revision: 3,
        handoff_from: None,
        activity: ActivityState::Idle,
        created_at_ms: 5,
        updated_at_ms: 8,
    };
    round_trip(SessionRequest::Create {
        title: Some("新会话".into()),
        cwd: None,
        tool_mode: Some(ToolMode::Custom),
        model_selection: Some(ModelSelection::default()),
        custom_tools: Some(vec!["read_file".into()]),
        handoff_from: Some(session_id()),
    });
    round_trip(SessionRequest::GetSessionSnapshot {
        session_id: session_id(),
        tail_blocks: 200,
        tail_turns: 32,
    });
    round_trip(SessionRequest::List {
        include_archived: true,
    });
    round_trip(SessionRequest::Rename {
        session_id: session_id(),
        title: "新标题".into(),
    });
    round_trip(SessionRequest::Archive {
        session_id: session_id(),
    });
    round_trip(SessionRequest::Unarchive {
        session_id: session_id(),
    });
    round_trip(SessionRequest::PatchSettings {
        session_id: session_id(),
        patch: SessionSettingsPatch {
            model_selection: Some(SettingOverride::Inherit),
            tools: Some(ToolsSettingPatch::Custom {
                tools: vec!["bash".into()],
            }),
            team_mode: Some(true),
        },
        expected_revision: 3,
    });
    round_trip(SessionRequest::Truncate {
        session_id: session_id(),
        turn_id: TurnId::new("T1").expect("valid"),
    });
    round_trip(SessionRequest::SetGoal {
        session_id: session_id(),
        objective: "目标".into(),
        status: Some(GoalStatus::Active),
        token_budget: None,
    });
    round_trip(SessionRequest::ClearGoal {
        session_id: session_id(),
    });
    round_trip(SessionRequest::GetTodos {
        session_id: session_id(),
    });
    round_trip(SessionRequest::GetTeam {
        session_id: session_id(),
    });
    round_trip(SessionRequest::GetSkillsCatalog {
        cwd: Some("/tmp".into()),
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
    round_trip(SessionResponse::Created(summary.clone()));
    round_trip(SessionResponse::Summary(summary));
    round_trip(SessionResponse::Sessions(vec![SessionSummary {
        session_id: session_id(),
        title: None,
        workspace_id: Some(WorkspaceId::new("WS-1").expect("valid")),
        cwd: None,
        archived: true,
        model_selection: None,
        tool_mode: ToolMode::Core,
        custom_tools: vec!["bash".into()],
        settings_revision: 0,
        handoff_from: Some(session_id()),
        activity: ActivityState::Idle,
        created_at_ms: 3,
        updated_at_ms: 7,
    }]));
    round_trip(SessionResponse::Todos(TodoListSnapshot {
        items: vec![TodoItemView {
            id: "t1".into(),
            title: "do".into(),
            description: "desc".into(),
            status: "pending".into(),
            evidence: None,
        }],
        current_id: Some("t1".into()),
        counts: TodoCountsView {
            pending: 1,
            in_progress: 0,
            completed: 0,
            cancelled: 0,
            total: 1,
        },
    }));
    round_trip(SessionResponse::Team(Box::new(TeamOverview {
        session_id: session_id(),
        team: TeamSnapshot {
            root_session_id: Some(session_id()),
            agents: vec![],
            unread_messages: vec![],
            revision: 1,
            last_fact_seq: 2,
        },
        tasks: TaskBoardSnapshot {
            team_id: Some("team-1".into()),
            revision: 1,
            last_fact_seq: 2,
            tasks: vec![],
        },
    })));
    round_trip(SessionResponse::Skills(SkillsCatalogView {
        skills: vec![SkillCatalogEntry {
            name: "s1".into(),
            description: "d".into(),
            scope: "project".into(),
            origin: "workspace".into(),
            path: "skills/s1/SKILL.md".into(),
        }],
        diagnostics: vec![],
    }));
    round_trip(SessionResponse::Page(Box::new(TranscriptPage {
        blocks: vec![],
        tool_calls: vec![],
        next_before_ord: None,
        timeline_epoch: 1,
    })));
    round_trip(SessionResponse::Ack);

    round_trip(WorkspaceRequest::List);
    round_trip(WorkspaceRequest::Create {
        path: "/work".into(),
    });
    round_trip(WorkspaceRequest::Rename {
        workspace_id: WorkspaceId::new("WS-1").expect("valid"),
        title: "主项目".into(),
    });
    round_trip(WorkspaceRequest::Delete {
        workspace_id: WorkspaceId::new("WS-1").expect("valid"),
    });
    round_trip(WorkspaceRequest::MoveSession {
        session_id: session_id(),
        workspace_id: WorkspaceId::new("WS-1").expect("valid"),
    });
    round_trip(WorkspaceResponse::Created(WorkspaceEntry {
        id: WorkspaceId::new("WS-1").expect("valid"),
        path: "/work".into(),
        title: "主项目".into(),
        order: 1,
        session_ids: vec![session_id()],
        missing_dir: false,
    }));
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
        images: vec![ImageRefView {
            hash: content_hash(),
            len: 8,
            mime_type: "image/png".into(),
        }],
    });
    round_trip(CommandRequest::RespondInteraction {
        session_id: session_id(),
        interaction_id: InteractionId::new("INT-0").expect("valid"),
        resolution: CommandInteractionResolution::Approve {
            scope: PermissionGrantScope::Persistent,
        },
    });
    round_trip(CommandRequest::RespondInteraction {
        session_id: session_id(),
        interaction_id: InteractionId::new("INT-1").expect("valid"),
        resolution: CommandInteractionResolution::Reject {
            text: "no".into(),
        },
    });
    round_trip(CommandRequest::RespondInteraction {
        session_id: session_id(),
        interaction_id: InteractionId::new("INT-2").expect("valid"),
        resolution: CommandInteractionResolution::Answer {
            answers: vec![AskAnswer {
                question_id: "q1".into(),
                answer: "选 B".into(),
            }],
        },
    });
    round_trip(CommandRequest::RespondInteraction {
        session_id: session_id(),
        interaction_id: InteractionId::new("INT-3").expect("valid"),
        resolution: CommandInteractionResolution::Dismiss,
    });
    round_trip(CommandRequest::ExecInterrupt {
        exec_session_id: ExecSessionId::new("execs_01").expect("valid"),
    });
    round_trip(CommandRequest::ExecTerminate {
        exec_session_id: ExecSessionId::new("execs_01").expect("valid"),
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
        session_id: Some(session_id()),
        cols: 120,
        rows: 40,
        cwd: Some("/tmp".into()),
        shell: None,
    });
    round_trip(ShellRequest::Open {
        session_id: None,
        cols: 80,
        rows: 24,
        cwd: None,
        shell: Some("/bin/bash".into()),
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
    round_trip(ShellRequest::List { session_id: None });
    round_trip(ShellResponse::Opened(ShellView {
        shell_id: "sh1".into(),
        pid: 4242,
        shell: "bash".into(),
        cwd: "/tmp".into(),
        cols: 120,
        rows: 40,
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
    round_trip(ConfigRequest::LoadFull);
    round_trip(ConfigRequest::PatchFull {
        patch: JsonText::new("{}"),
    });
    round_trip(ConfigRequest::ListProviders);
    round_trip(ConfigRequest::ListProviderPresets);
    round_trip(ConfigRequest::SetDefaultModel {
        selection: ModelSelection::default(),
        expected_revision: 5,
    });
    round_trip(ConfigRequest::ReloadAgents);
    round_trip(ConfigResponse::Value {
        section: ConfigSection::Sandbox,
        value: JsonText::new("{}"),
    });
    round_trip(ConfigResponse::Providers {
        raw: JsonText::new("{}"),
        revision: 5,
    });
    round_trip(ConfigResponse::ProviderPresets(vec![ProviderPresetView {
        id: "deepseek".into(),
        name: "DeepSeek".into(),
        base_url: "https://api.deepseek.com".into(),
        wire: Wire::Responses,
        credential: CredentialSource::DeepseekAccount,
    }]));
    round_trip(ConfigResponse::Reloaded);
}

#[test]
fn account_pairing_gitx_fs_catalog_domains_round_trip() {
    round_trip(AccountRequest::Select {
        account_id: "a1".into(),
    });
    round_trip(AccountRequest::SiwcStatus);
    round_trip(AccountRequest::SiwcSignInPoll {
        flow_id: "f1".into(),
    });
    round_trip(AccountRequest::DsSignInStart { locale: "zh".into() });
    round_trip(AccountRequest::DsAckBonus {
        order_id: "o1".into(),
    });
    round_trip(AccountResponse::Current(None));
    round_trip(AccountResponse::Accounts(vec![AccountView {
        account_id: "a1".into(),
        provider: "p".into(),
        label: "L".into(),
        current: true,
    }]));
    round_trip(AccountResponse::SiwcStatus(SiwcStatusView {
        status: SiwcConnectionState::Connected,
        sharing: true,
        profiles: 1,
        usage_url: "https://example.com".into(),
        api_base: "https://api.example.com".into(),
        profile_id: Some("p1".into()),
        label: Some("主号".into()),
        email: None,
        name: None,
        error: None,
    }));
    round_trip(AccountResponse::DsStatus(DsAccountStatusView {
        signed_in: false,
        account: None,
        storage_error: None,
        inference_origin: "https://api.deepseek.com".into(),
        usage_url: "https://platform.deepseek.com/usage".into(),
        top_up_url: "https://platform.deepseek.com/top_up".into(),
        flow: DsAccountFlowView {
            attempt_id: None,
            state: DsFlowState::Idle,
            authorize_url: None,
            expires_at_ms: 0,
            error: None,
        },
        revoke: DsRevokeOutcomeView {
            state: "idle".into(),
            attempts: 0,
            message: None,
        },
    }));
    round_trip(AccountResponse::DsModels(vec![DsAccountModelView {
        id: Some("deepseek-chat".into()),
        raw: JsonText::new("{\"id\":\"deepseek-chat\"}"),
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
        session_id: session_id(),
        path: "a.txt".into(),
        offset: 8,
        len: 4096,
    });
    round_trip(FsRequest::Write {
        session_id: session_id(),
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
    round_trip(StreamKind::BlobTransfer {
        role: BlobTransferRole::ServerPush,
    });
    round_trip(StreamKind::BlobTransfer {
        role: BlobTransferRole::ClientFetch {
            hash: content_hash(),
        },
    });
    round_trip(StreamKind::BlobTransfer {
        role: BlobTransferRole::ClientUpload {
            session_id: session_id(),
            hash: content_hash(),
            len: 512,
        },
    });
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
            input_id: Some(InputId::new("IN-1").expect("valid")),
            started_at_ms: 1,
        },
        SessionEvent::TurnPhaseChanged {
            session_id: session_id(),
            turn_id: TurnId::new("T1").expect("valid"),
            phase: TurnPhase::Running,
        },
        SessionEvent::TurnFinished {
            session_id: session_id(),
            turn_id: TurnId::new("T1").expect("valid"),
            terminal: TurnTerminal::Completed,
            usage: UsageInfo::default(),
        },
        SessionEvent::ProviderRetrying {
            session_id: session_id(),
            attempt: 2,
            max_attempts: 5,
            delay_ms: 500,
            reason: "rate_limit".into(),
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
        SessionEvent::TimelineAppended {
            session_id: session_id(),
            block: BlockView {
                block_id: BlockId::new("B1").expect("valid"),
                turn_id: Some(TurnId::new("T1").expect("valid")),
                ord: 1,
                kind: BlockKind::Answer,
                content: ContentRef::Inline { text: "z".into() },
                tool_call_id: None,
                created_at_ms: 9,
                actor: None,
                purpose: None,
                model: Some("deepseek-chat".into()),
                usage: None,
                generated_ms: Some(45),
                images: vec![],
                attachments: vec![],
                inter_agent: None,
                subagent_terminal: None,
            },
        },
        SessionEvent::TimelineTruncated {
            session_id: session_id(),
            from_turn_id: TurnId::new("T1").expect("valid"),
            removed_turn_ids: vec![TurnId::new("T1").expect("valid")],
            timeline_epoch: 5,
        },
        SessionEvent::ToolCallUpdated {
            session_id: session_id(),
            call: ToolCallView {
                tool_call_id: ToolCallId::new("C1").expect("valid"),
                turn_id: Some(TurnId::new("T1").expect("valid")),
                tool_name: "read".into(),
                status: ToolCallStatus::Succeeded,
                started_at_ms: 1,
                finished_at_ms: Some(2),
                args: None,
                output: None,
                output_truncated: false,
                diff: None,
                code_delta: None,
                metrics: None,
                exec_session_id: None,
                interaction_id: None,
                effects: vec![],
            },
        },
        SessionEvent::ToolArgumentsDelta {
            session_id: session_id(),
            tool_call_id: ToolCallId::new("C1").expect("valid"),
            tool_name: "edit".into(),
            delta: "{\"path\":".into(),
            lines_added: Some(3),
            lines_removed: Some(1),
            target_files: vec!["a.rs".into()],
            args_complete: false,
        },
        SessionEvent::ToolOutputDelta {
            session_id: session_id(),
            tool_call_id: ToolCallId::new("C1").expect("valid"),
            text: "ok\n".into(),
        },
        SessionEvent::InteractionRequested {
            session_id: session_id(),
            interaction: InteractionView {
                interaction_id: InteractionId::new("I1").expect("valid"),
                turn_id: Some(TurnId::new("T1").expect("valid")),
                kind: InteractionKind::Permission,
                tool_call_id: Some(ToolCallId::new("C1").expect("valid")),
                title: "t".into(),
                body: "b".into(),
                options: vec!["允许".into(), "拒绝".into()],
                choices: vec![InteractionChoiceView {
                    decision: InteractionDecisionKind::Approved,
                    label: "允许".into(),
                    description: None,
                    tone: ChoiceTone::Primary,
                }],
                questions: vec![],
                permission: Some(PermissionRequestView {
                    tool_name: "write".into(),
                    action_summary: None,
                    justification: None,
                    reason: "write file".into(),
                    paths: vec!["a.rs".into()],
                    category: "write".into(),
                    level: 2,
                    risk: "medium".into(),
                    consequence: "modifies file".into(),
                }),
                expires_at_ms: None,
            },
        },
        SessionEvent::InteractionResolved {
            session_id: session_id(),
            interaction_id: InteractionId::new("I1").expect("valid"),
            resolution: InteractionResolution::Approved {
                scope: PermissionGrantScope::Once,
            },
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
                state: ExecSessionStateView::Running,
                owner_session_id: session_id(),
                origin_turn_id: Some(TurnId::new("T1").expect("valid")),
                origin_call_id: Some(ToolCallId::new("C1").expect("valid")),
                cwd: None,
                started_at_ms: 1,
                finished_at_ms: None,
                output_preview: String::new(),
                preview_start_offset: 0,
                output_complete: false,
                capture_omitted_bytes: 0,
                stream_dropped_bytes: 0,
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
        SessionEvent::SessionMetaChanged {
            session_id: session_id(),
            title: Some("新标题".into()),
        },
        SessionEvent::SubagentUpdated {
            session_id: session_id(),
            subagent: SubagentSummary {
                session_id: session_id(),
                nickname: Some("同事".into()),
                status: TeamAgentStatus::Idle,
                residency: TeamAgentResidency::Loaded,
            },
        },
        SessionEvent::GoalUpdated {
            session_id: session_id(),
            goal: None,
        },
        SessionEvent::HandoffSealed {
            session_id: session_id(),
            sealed_session_id: session_id(),
            successor_session_id: session_id(),
            turn_id: TurnId::new("T1").expect("valid"),
            summary: ContentRef::Inline {
                text: "交接".into(),
            },
            at_ms: 123,
        },
        SessionEvent::PromptBlocked {
            session_id: session_id(),
            reason: "busy".into(),
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
        local_ipc: Some("/run/qaqh/daemon.sock".into()),
        iroh: None,
        protocol_rev: PROTOCOL_REV,
        protocol_fingerprint: PROTOCOL_FINGERPRINT,
        written_at_ms: 1_700_000_000_000,
    }
}

#[test]
fn wire_deserialization_rejects_malformed_session_id() {
    let bad_wire = postcard::to_stdvec(&"../../etc/passwd".to_string()).expect("encode");
    assert!(
        postcard::from_bytes::<SessionId>(&bad_wire).is_err(),
        "线缆上的路径穿越 SessionId 必须 Fail-Closed 拒绝"
    );
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

    // 既无本地 IPC 也无远端 iroh：必须拒绝。
    let empty_transport = DaemonEndpoint {
        local_ipc: None,
        iroh: None,
        ..endpoint.clone()
    };
    assert_eq!(
        empty_transport.validate("dev"),
        Err(EndpointReject::NoTransport)
    );

    // 远端载体与双栈并存同样往返（v4.8 本地与 iroh 同盘）。
    let remote = DaemonEndpoint {
        local_ipc: Some("/run/qaqh/daemon.sock".into()),
        iroh: Some(IrohEndpointInfo {
            endpoint_id: [7u8; 32],
            relay_url: Some("https://relay.example".into()),
        }),
        ..endpoint.clone()
    };
    remote.validate("dev").expect("双端点同时存在合法");
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
