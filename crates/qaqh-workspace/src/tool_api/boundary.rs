//! 工具 ↔ agent loop 执行边界（09-19 补充稿 §4）。
//!
//! 契约：loop 只发 [`ExecuteBatch`]、只收 [`BatchOutcome`]；执行细节
//! （线程/超时/取消/审计/事件发射）全部在 tool runtime 内，loop 不再搬运。
//!
//! - 挂起决策（何时 YieldToUser）归 **loop**——只消费 `pending_interactions`；
//! - 恢复经 [`ResumeInteraction`] 回传 runtime；
//! - v1 的 admit 仍在 loop 侧完成（base spec §2.2），v2.0 迁入 runtime
//!   （对齐 codex `Approvable`）。
//!
//! 迁移期说明：本模块类型**尚未接线生产路径**（plan P1 只定义类型）；
//! `engine_tool.rs` 现有路径保持不变。

use std::path::PathBuf;

use crate::authorization::AuthorizedToolCall;
use crate::permission::PermissionLevel;

use super::context::{AgentMode, CancellationToken};
use super::error::FatalToolError;
use super::output::ToolOutcome;

/// 批次执行请求（loop → tool runtime）。
pub struct ExecuteBatch {
    /// 所属回合 id。
    pub turn_id: String,
    /// 回合内轮次。
    pub round_num: u32,
    /// 已授权调用（v1：admit 在 loop 侧完成；v2.0 迁入 runtime）。
    pub calls: Vec<AuthorizedToolCall>,
    /// 批次上下文。
    pub context: BatchContext,
}

/// 批次上下文（会话级，显式传递）。
#[derive(Debug, Clone)]
pub struct BatchContext {
    /// 会话 seed。
    pub session_id: String,
    /// 工作区根。
    pub workspace_root: PathBuf,
    /// 运行模式。
    pub mode: AgentMode,
    /// 生效权限档位。
    pub permission_level: PermissionLevel,
    /// 取消信号。
    pub cancellation: CancellationToken,
}

/// 批次结果（tool runtime → loop）。
pub struct BatchOutcome {
    /// 与 `ExecuteBatch.calls` 一一对应，每项都是终态（含 Cancelled/Error）。
    pub results: Vec<CallOutcome>,
    /// 需要用户裁决的阻塞交互（挂起来源）。
    pub pending_interactions: Vec<PendingInteraction>,
    /// 批次级 fatal（已按 base spec §11 规则 seal 对应 timeline 块）。
    pub fatal: Option<FatalToolError>,
}

/// 单调用终态。
pub struct CallOutcome {
    /// 调用 id。
    pub call_id: String,
    /// 执行结果。
    pub outcome: ToolOutcome,
}

/// 阻塞交互（由 loop 决定挂起/恢复）。
pub struct PendingInteraction {
    /// 调用 id。
    pub call_id: String,
    /// 交互类型。
    pub kind: PendingKind,
    /// 裁决载荷（runtime 适配层负责投影到 wire；只含裁决必需字段）。
    pub request: serde_json::Value,
}

/// 阻塞交互类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PendingKind {
    /// 工具权限审批。
    Permission,
    /// ask_user 问答。
    Ask,
    /// plan 评审。
    PlanApproval,
}

/// 裁决回传（loop → tool runtime）。
pub struct ResumeInteraction {
    /// 调用 id。
    pub call_id: String,
    /// 裁决结果。
    pub decision: InteractionDecision,
}

/// 裁决结果（与 [`PendingKind`] 对应）。
#[derive(Debug, Clone, PartialEq)]
pub enum InteractionDecision {
    /// 权限裁决。
    Permission {
        /// 是否批准。
        approved: bool,
        /// 是否信任该文件夹（"记住这个决定"）。
        trust_folder: bool,
    },
    /// ask_user 回答（结构化载荷）。
    Ask {
        /// 回答内容。
        answers: serde_json::Value,
    },
    /// plan 评审结果。
    Plan {
        /// 是否批准。
        approved: bool,
        /// 可选反馈。
        feedback: Option<String>,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn batch_context_and_outcome_are_constructible() {
        let context = BatchContext {
            session_id: "d9a1a320".to_owned(),
            workspace_root: PathBuf::from("/tmp/ws"),
            mode: AgentMode::Plan,
            permission_level: PermissionLevel::MaxLockdown,
            cancellation: CancellationToken::new(),
        };
        assert_eq!(context.mode, AgentMode::Plan);

        let outcome = BatchOutcome {
            results: Vec::new(),
            pending_interactions: vec![PendingInteraction {
                call_id: "call_1".to_owned(),
                kind: PendingKind::Permission,
                request: serde_json::json!({"tool": "exec"}),
            }],
            fatal: None,
        };
        assert_eq!(outcome.pending_interactions.len(), 1);
        assert_eq!(outcome.pending_interactions[0].kind, PendingKind::Permission);
    }

    #[test]
    fn interaction_decisions_cover_all_pending_kinds() {
        let permission = InteractionDecision::Permission {
            approved: true,
            trust_folder: false,
        };
        let ask = InteractionDecision::Ask {
            answers: serde_json::json!([{"id": "q1", "answer": "yes"}]),
        };
        let plan = InteractionDecision::Plan {
            approved: false,
            feedback: Some("改一下".to_owned()),
        };
        assert_ne!(permission, ask);
        assert_ne!(ask, plan);
    }
}
