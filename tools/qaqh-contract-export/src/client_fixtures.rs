//! Synthetic bootstrap/team/approvals/content fixtures from the canonical DTOs.
//!
//! Shapes are locked by `qaqh-session::client_v2` tests and the daemon v2
//! handlers; every sample here is constructed from the same Rust DTOs those
//! handlers serialize, then serde round-trip validated by
//! [`super::write_fixture`].

use std::path::Path;

use qaqh_domain::RingingChannel;
use qaqh_domain::timeline::{
    TimelineEntry, TimelineEvent, TimelineSnapshot, TimelineTurn, TimelineTurnState,
};
use qaqh_ringing::{
    CanonicalCursor, CursorToken, RingingV2ApprovalRespondRequest, RingingV2Bootstrap,
    RingingV2ChannelSnapshot, RingingV2ContentUploadResponse, RingingV2Delivery,
    RingingV2DeviceApprovalKind, RingingV2DeviceChallenge, RingingV2DeviceChallenges,
    RingingV2DriverState, RingingV2EventEnvelope, RingingV2InteractionKind,
    RingingV2PendingApprovals, RingingV2PendingInteraction, RingingV2PendingInteractionView,
    RingingV2PendingPermissionView, RingingV2StreamKey, TimelinePage, TimelineSseFrame,
};
use qaqh_session::client_v2::{
    ClientV2ControlState, ClientV2TeamResponse, ClientV2ToolState, TEAM_SCHEMA,
};
use qaqh_session::projection::{ConversationSnapshot, TeamSnapshot};
use qaqh_session::session_fact_v2::{
    AgentPath, InterAgentDelivery, MessageId, ProjectionPayload, SessionId, TeamAgentResidency,
    TeamAgentSnapshot, TeamAgentStatus, TeamBoardSnapshot, TeamDelta, TeamInboxSummary,
};
use qaqh_session::team::{TaskBoardSnapshot, TaskView};

fn session_id() -> SessionId {
    SessionId::new("0199a0f0-0000-7000-8000-000000000001")
}

fn child_session_id() -> SessionId {
    SessionId::new("0199a0f0-0000-7000-8000-000000000002")
}

fn agent_path(raw: &str) -> AgentPath {
    AgentPath::parse_absolute(raw).expect("synthetic agent path")
}

pub(super) fn write(stage: &Path) -> Result<(), Box<dyn std::error::Error>> {
    write_bootstrap(stage)?;
    write_team(stage)?;
    write_approvals(stage)?;
    write_timeline(stage)?;
    write_device_approvals(stage)?;
    super::write_fixture(
        stage,
        "content-upload-response.json",
        &RingingV2ContentUploadResponse {
            content_id: "sha256:synthetic-content-hash".into(),
            media_type: "text/plain".into(),
            sha256: "sha256:synthetic-content-hash".into(),
            size: 42,
            truncated: false,
        },
    )?;
    write_event(stage)?;
    Ok(())
}

fn write_bootstrap(stage: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let snapshot_cursor =
        CursorToken::encode_snapshot(&CanonicalCursor::snapshot("fixture-log", 3))?;
    let control = ClientV2ControlState {
        session_id: Some(session_id()),
        activity: Default::default(),
        current_turn_id: None,
        current_call_id: None,
        round: None,
        tools: Vec::new(),
        subagents: Vec::new(),
        last_recovery: None,
        revision: 7,
        last_fact_seq: 42,
        interactions: vec![RingingV2PendingInteraction {
            interaction_id: "int_fixture".into(),
            call_id: "call_fixture".into(),
            turn_id: "turn_fixture".into(),
            kind: RingingV2InteractionKind::PlanReview,
            request: Some(qaqh_ringing::RingingV2ContentValue::Ref {
                content_ref: "sha256:synthetic-content-hash".into(),
            }),
        }],
        driver: Some(RingingV2DriverState {
            holder: None,
            driver_epoch: 4,
            can_claim: true,
        }),
    };
    let revision = control.revision;
    let bootstrap: RingingV2Bootstrap<
        ClientV2ControlState,
        ConversationSnapshot,
        ClientV2ToolState,
    > = RingingV2Bootstrap {
        schema: qaqh_ringing::RINGING_SCHEMA.into(),
        version: qaqh_ringing::RINGING_V2_VERSION,
        server_epoch: "fixture-epoch".into(),
        session_id: session_id().as_str().to_owned(),
        snapshot_cursor,
        control: RingingV2ChannelSnapshot {
            channel: RingingChannel::Control,
            state_revision: revision,
            snapshot_version: 1,
            state: control,
        },
        conversation: RingingV2ChannelSnapshot {
            channel: RingingChannel::Conversation,
            state_revision: 5,
            snapshot_version: 1,
            state: ConversationSnapshot::default(),
        },
        tool: RingingV2ChannelSnapshot {
            channel: RingingChannel::Tool,
            state_revision: revision,
            snapshot_version: 1,
            state: ClientV2ToolState { tools: Vec::new() },
        },
    };
    bootstrap
        .validate()
        .map_err(|code| format!("invalid bootstrap fixture: {code}"))?;
    super::write_fixture(stage, "bootstrap-response.json", &bootstrap)
}

fn write_team(stage: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let response = ClientV2TeamResponse {
        schema: TEAM_SCHEMA.to_owned(),
        session_id: session_id().as_str().to_owned(),
        team: TeamSnapshot {
            root_session_id: Some(session_id()),
            agents: vec![
                TeamAgentSnapshot {
                    agent_id: session_id(),
                    agent_path: agent_path("/root"),
                    nickname: Some("main".into()),
                    role: None,
                    status: TeamAgentStatus::Running,
                    residency: TeamAgentResidency::Loaded,
                    parent_agent_path: None,
                    current_task_id: None,
                },
                TeamAgentSnapshot {
                    agent_id: child_session_id(),
                    agent_path: agent_path("/root/writer"),
                    nickname: Some("writer".into()),
                    role: None,
                    status: TeamAgentStatus::Completed,
                    residency: TeamAgentResidency::Unloaded,
                    parent_agent_path: Some(agent_path("/root")),
                    current_task_id: None,
                },
            ],
            unread_messages: vec![TeamInboxSummary {
                message_id: MessageId::new("msg-fixture"),
                author: agent_path("/root/writer"),
                recipient: agent_path("/root"),
                task_id: Some("task-fixture".into()),
                delivery: InterAgentDelivery::Queue,
                created_at_ms: 1_759_000_000_000,
            }],
            revision: 7,
            last_fact_seq: 42,
        },
        tasks: TaskBoardSnapshot {
            team_id: None,
            revision: 0,
            last_fact_seq: 0,
            tasks: Vec::<TaskView>::new(),
        },
        board: TeamBoardSnapshot::default(),
    };
    super::write_fixture(stage, "team-response.json", &response)
}

fn write_approvals(stage: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let pending = RingingV2PendingApprovals {
        pending_permission: Some(RingingV2PendingPermissionView {
            tool_call_id: "call_fixture".into(),
            tool_name: Some(serde_json::json!("fs.write")),
            action_summary: Some(serde_json::json!("write synthetic file")),
            reason: Some(serde_json::json!("synthetic reason")),
            paths: serde_json::json!(["/synthetic/workspace/README.md"]),
            category: Some(serde_json::json!("filesystem")),
            level: Some(serde_json::json!("tier-2")),
            risk: Some(serde_json::json!("medium")),
            consequence: Some(serde_json::json!("overwrites the file")),
            details_unavailable: false,
        }),
        pending_interaction: Some(RingingV2PendingInteractionView {
            id: "int_fixture".into(),
            kind: RingingV2InteractionKind::PlanReview,
            details: Some(serde_json::json!({"plan": "synthetic plan"})),
        }),
    };
    super::write_fixture(stage, "approvals-pending.json", &pending)?;
    // 空槽形态：键仍在、值为 null（SDK 解码负例的对照样本）。
    let empty = RingingV2PendingApprovals {
        pending_permission: None,
        pending_interaction: None,
    };
    super::write_fixture(stage, "approvals-empty.json", &empty)
}

fn write_event(stage: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let event: RingingV2EventEnvelope<ProjectionPayload> = RingingV2EventEnvelope {
        schema: qaqh_ringing::RINGING_SCHEMA.into(),
        version: qaqh_ringing::RINGING_V2_VERSION,
        server_epoch: "fixture-epoch".into(),
        session_id: session_id().as_str().to_owned(),
        event_id: "event-fixture-1".into(),
        stream_key: RingingV2StreamKey::Channel(RingingChannel::Control),
        // ephemeral 事件不携带可靠位置字段（validate() 拒绝 cursor/log_id/
        // fact_seq/projection_index/revision）——这正是 fixture 要教的约束。
        delivery: RingingV2Delivery::Ephemeral,
        cursor: None,
        log_id: None,
        fact_seq: None,
        projection_index: None,
        revision: None,
        causation_id: None,
        correlation_id: None,
        ts_ms: Some(1_759_000_000_000),
        payload: ProjectionPayload::TeamDelta(TeamDelta::AgentResidencyChanged {
            revision: 3,
            agent_id: child_session_id(),
            residency: TeamAgentResidency::Unloaded,
        }),
    };
    event
        .validate()
        .map_err(|code| format!("invalid event fixture: {code}"))?;
    super::write_fixture(stage, "event-envelope-team-delta.json", &event)
}

/// Timeline 分页响应与 SSE 帧（`GET .../timeline` + `GET .../timeline/events`）。
/// 契约所有权在 `qaqh-ringing::v2::timeline`；daemon handler 直接序列化
/// 同一份 DTO。回合一律合成文本，无真实会话正文。
fn write_timeline(stage: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let page = TimelinePage {
        schema: qaqh_ringing::RINGING_SCHEMA.into(),
        version: qaqh_ringing::RINGING_V2_VERSION,
        server_epoch: "fixture-epoch".into(),
        session_id: session_id().as_str().to_owned(),
        snapshot: TimelineSnapshot {
            watermark: 1,
            turns: vec![TimelineTurn {
                turn_id: "t1".into(),
                turn_index: Some(0),
                created_seq: 1,
                user_text: "synthetic user turn".into(),
                sealed: true,
                offloaded: false,
                state: TimelineTurnState::Completed,
                failure: None,
                rounds: Vec::new(),
            }],
        },
        has_more: false,
        total_turns: 1,
        truncated_before: false,
    };
    page.validate_for(session_id().as_str())
        .map_err(|code| format!("invalid timeline page fixture: {code}"))?;
    super::write_fixture(stage, "timeline-page.json", &page)?;

    let frame = TimelineSseFrame {
        schema: qaqh_ringing::RINGING_SCHEMA.into(),
        version: qaqh_ringing::RINGING_V2_VERSION,
        server_epoch: "fixture-epoch".into(),
        session_id: session_id().as_str().to_owned(),
        entry: TimelineEntry {
            timeline_seq: 1,
            turn_id: "t1".into(),
            round_num: None,
            event: TimelineEvent::TurnOpened {
                user_text: "synthetic user turn".into(),
            },
        },
    };
    super::write_fixture(stage, "timeline-entry-event.json", &frame)
}

/// device 审批面（opaque challenge）：`GET .../approvals`（device 身份）与
/// `POST .../approvals/respond`。形状与 admin 面刻意不同；challenge_id 是
/// 64 hex 不透明 token，canonical id 不出现在任何 fixture 里。
fn write_device_approvals(stage: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let synthetic_challenge_id = "00112233445566778899aabbccddeeff00112233445566778899aabbccddeeff";
    let challenges = RingingV2DeviceChallenges {
        challenges: vec![
            RingingV2DeviceChallenge {
                challenge_id: synthetic_challenge_id.into(),
                kind: RingingV2DeviceApprovalKind::ToolPermission,
                expires_in: 300,
                details: serde_json::json!({
                    "tool_name": "fs.write",
                    "action_summary": "write synthetic file",
                    "reason": "synthetic reason",
                    "paths": ["/synthetic/workspace/README.md"],
                    "category": "filesystem",
                    "level": "tier-2",
                    "risk": "medium",
                    "consequence": "overwrites the file",
                }),
            },
            RingingV2DeviceChallenge {
                challenge_id: synthetic_challenge_id.into(),
                kind: RingingV2DeviceApprovalKind::Plan,
                expires_in: 300,
                details: serde_json::json!({"plan": "synthetic plan"}),
            },
        ],
    };
    super::write_fixture(stage, "device-challenges.json", &challenges)?;

    let respond = RingingV2ApprovalRespondRequest {
        challenge_id: synthetic_challenge_id.into(),
        decision: "approve".into(),
        payload: serde_json::json!({"message": "synthetic approval message"}),
    };
    super::write_fixture(stage, "approval-respond-request.json", &respond)
}
