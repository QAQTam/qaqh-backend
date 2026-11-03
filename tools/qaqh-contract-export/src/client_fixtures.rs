//! Synthetic bootstrap/team/approvals/content fixtures from the contract DTOs.
//!
//! Shapes are locked by `qaqh-ringing-contract` and the daemon v2 handlers;
//! every sample here is constructed from the same Rust DTOs those handlers
//! serialize, then serde round-trip validated by [`super::write_fixture`].

use std::path::Path;

use qaqh_ringing::{
    RingingV2ApprovalRespondRequest, RingingV2ContentUploadResponse, RingingV2DeviceApprovalKind,
    RingingV2DeviceChallenge, RingingV2DeviceChallenges, RingingV2PendingApprovals,
    RingingV2PendingInteractionView, RingingV2PendingPermissionView,
};
use qaqh_ringing_contract::fact::{
    ActivityState, AgentPath, ContentValue, InterAgentDelivery, InteractionKind, MessageId,
    SessionId,
};
use qaqh_ringing_contract::state::{DriverState, InteractionView};
use qaqh_ringing_contract::team::{
    TaskBoardSnapshot, TaskView, TeamAgentResidency, TeamAgentSnapshot, TeamAgentStatus,
    TeamBoardSnapshot, TeamInboxSummary, TeamSnapshot,
};
use qaqh_ringing_contract::{SessionBootstrap, SessionEvent, SessionEventFrame, TEAM_SCHEMA};

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
    let bootstrap = SessionBootstrap {
        schema: qaqh_ringing::RINGING_SCHEMA.into(),
        version: qaqh_ringing::RINGING_V2_VERSION,
        server_epoch: "fixture-epoch".into(),
        session_id: session_id().as_str().to_owned(),
        state: qaqh_ringing_contract::SessionState {
            session_id: Some(session_id()),
            seq: 42,
            activity: ActivityState::Idle,
            current_turn_id: None,
            title: Some("synthetic session".into()),
            turns: Vec::new(),
            timeline: Vec::new(),
            tools: Vec::new(),
            interactions: Vec::new(),
            subagents: Vec::new(),
            driver: Some(DriverState {
                holder: None,
                epoch: 4,
            }),
        },
    };
    super::write_fixture(stage, "bootstrap-response.json", &bootstrap)
}

fn write_team(stage: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let response = qaqh_ringing_contract::TeamResponse {
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
            kind: InteractionKind::Plan,
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
    // 单流事件帧:seq + 业务事件(tag = type)。壳层按 SessionEvent 变体匹配,
    // 内部调度细节不进入公共协议。
    let frame = SessionEventFrame {
        seq: 3,
        event: SessionEvent::InteractionRequested {
            interaction: InteractionView {
                interaction_id: qaqh_ringing_contract::fact::InteractionId::new("int_fixture"),
                call_id: Some(qaqh_ringing_contract::fact::ToolCallId::new("call_fixture")),
                turn_id: Some(qaqh_ringing_contract::fact::TurnId::new("turn_fixture")),
                kind: InteractionKind::Plan,
                request: ContentValue::Inline {
                    text: "approve the plan".into(),
                },
                choices: Vec::new(),
                expires_at_ms: None,
                resolution: None,
            },
        },
    };
    super::write_fixture(stage, "event-frame-interaction.json", &frame)
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
