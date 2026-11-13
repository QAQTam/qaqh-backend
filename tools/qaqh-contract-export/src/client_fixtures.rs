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
    ActivityState, AgentPath, ContentValue, InterAgentActor, InterAgentDelivery, InteractionKind,
    MessageId, SessionId,
};
use qaqh_ringing_contract::state::{DriverState, InteractionView, PermissionRequestDetails};
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
        session_contract_revision: qaqh_ringing::SESSION_CONTRACT_REVISION.into(),
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
            exec_sessions: Vec::new(),
            driver: Some(DriverState {
                holder: None,
                epoch: 4,
            }),
            // ADR-0015：夹具会话无 goal（缺席 = 无目标,默认态）。
            goal: None,
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
            // ADR-0019：署名是 InterAgentActor union——同根 path、跨根 session id，
            // 夹具两种都钉住，前端拿到的是可判别形状而非字符串猜测。
            unread_messages: vec![
                TeamInboxSummary {
                    message_id: MessageId::new("msg-fixture"),
                    author: InterAgentActor::Path(agent_path("/root/writer")),
                    recipient: InterAgentActor::Path(agent_path("/root")),
                    task_id: Some("task-fixture".into()),
                    delivery: InterAgentDelivery::Queue,
                    created_at_ms: 1_759_000_000_000,
                },
                TeamInboxSummary {
                    message_id: MessageId::new("msg-fixture-cross-root"),
                    author: InterAgentActor::Session(child_session_id()),
                    recipient: InterAgentActor::Path(agent_path("/root")),
                    task_id: None,
                    delivery: InterAgentDelivery::Trigger,
                    created_at_ms: 1_759_000_000_001,
                },
            ],
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
    // 审批卡正文的 canonical 形状（JSON-in-JSON 边界，ADR-0004 §决策 3）。视图的取值
    // 由这份正文投影而来，与 daemon 的 `pending_permission_view` 同一套映射，因此
    // 不可能再出现"样例写着 daemon 永远发不出的值"那种自相矛盾的夹具（旧夹具写过
    // category="filesystem"、level="tier-2"，而 `is_valid()` 只接受
    // category ∈ read|write|exec|net、level ∈ 1..=4）。
    let details = PermissionRequestDetails {
        tool_name: "fs.write".into(),
        action_summary: Some("write synthetic file".into()),
        reason: "synthetic reason".into(),
        paths: vec!["/synthetic/workspace/README.md".into()],
        category: "write".into(),
        level: 2,
        risk: "medium".into(),
        consequence: "overwrites the file".into(),
    };
    // 导出器拒绝发布 daemon 自己会判为不合法的样例 —— 夹具由此变成机器闸而不是抄写。
    if !details.is_valid() {
        return Err(
            "approvals fixture is not a body the daemon could emit (is_valid() = false)".into(),
        );
    }
    super::write_fixture(stage, "pending-permission-details.json", &details)?;
    let pending = RingingV2PendingApprovals {
        pending_permission: Some(RingingV2PendingPermissionView {
            tool_call_id: "call_fixture".into(),
            tool_name: Some(serde_json::json!(&details.tool_name)),
            action_summary: details
                .action_summary
                .as_ref()
                .map(|value| serde_json::json!(value)),
            reason: Some(serde_json::json!(&details.reason)),
            paths: serde_json::json!(&details.paths),
            category: Some(serde_json::json!(&details.category)),
            level: Some(serde_json::json!(details.level)),
            risk: Some(serde_json::json!(&details.risk)),
            consequence: Some(serde_json::json!(&details.consequence)),
            details_unavailable: false,
        }),
        pending_interaction: Some(RingingV2PendingInteractionView {
            id: "int_fixture".into(),
            kind: InteractionKind::Ask,
            details: Some(serde_json::json!({"question": "synthetic question"})),
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
                kind: InteractionKind::Ask,
                request: ContentValue::Inline {
                    text: "a synthetic question".into(),
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
