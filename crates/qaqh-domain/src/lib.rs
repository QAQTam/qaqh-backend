//! # qaqh-domain — Ringing 共享领域类型
//!
//! 提供运行时和 Ringing 契约共同使用的事件、状态、回合及值对象。这里的
//! 命令 payload 是数据类型；公共命令根和提交信封由 RingingV2 定义。
//!
//! 本 crate 不依赖 wire 或 transport 实现，领域类型也不携带 HTTP、SSE、
//! WebSocket 或 pipe 等传输语义。

#![cfg_attr(not(test), forbid(unsafe_code))]
pub mod channel;
pub mod event;
pub mod interaction_body;
pub mod state;
pub mod turn;

pub use channel::RingingChannel;
pub use qaqh_ringing_contract::commands::{
    AskAnswer, ImageBlock, InterAgentEnvelope, SubagentTerminalKind,
    SubagentTerminalNotification,
};
pub use qaqh_ringing_contract::fact::InterAgentDelivery;
pub use event::{
    ActivityState, AgentLifecycleState, AskMode, AskQuestion, AskResolution, CodeDeltaRecord,
    CompactStatus, ContentRef, ControlEvent, ConversationEvent, DashboardDocument,
    DashboardSnapshot, DashboardTask, DomainError, DomainEvent, ErrorScope, NoticeLevel,
    PermissionCategory, PermissionRisk, PlanReviewItem, ProviderToolState, RoundDeltaKind,
    SessionActivity, SessionState, SkillInfo, SkillRuntimeInfo, SkillsStatus, ToolEvent,
    ToolResult,
};
pub use turn::{
    FileSnapshotInfo, RoundBlock, RoundData, ToolCallDef, ToolResultDef, TurnData,
};
