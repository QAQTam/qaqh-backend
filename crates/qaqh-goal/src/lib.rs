//! Goal 工具面(ADR-0015 切片 D):`create_goal` / `update_goal` / `get_goal`。
//!
//! Codex goal 语义的本仓落地:模型只能声明 complete/blocked/paused,
//! resume 与预算态由用户命令(`goal.set`)或系统控制。宿主(`GoalHost`)由
//! daemon 装配时安装(`install_host`,subagent 同款先例);未安装即报错,
//! 不做降级。工具仅注入根会话(registry spawn 侧控制)。

#![cfg_attr(not(test), forbid(unsafe_code))]
#![allow(clippy::result_large_err)] // TypedTool's frozen public error boundary.

use std::sync::{Arc, OnceLock};
use std::time::Duration;

use qaqh_ringing_contract::state::{GoalStatus, GoalView};
use qaqh_tool_core::tool_api::{
    ToolCallContext, ToolContentBlock, ToolDisplay, ToolError, ToolErrorCode, ToolErrorKind,
    ToolExecutionError, ToolMeta, ToolProjection, TypedTool,
};
use qaqh_workspace::{ToolManager, ToolRisk};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

// ── Host ────────────────────────────────────────────────────────────────────

/// 宿主边界:runtime 以 QaqhService 实现(DB 权威 + 事件 + 续跑)。
pub trait GoalHost: Send + Sync {
    /// 创建 goal。已有未完成 goal → Err(模型须先 complete/clear)。
    fn create_goal(
        &self,
        session_id: &str,
        objective: &str,
        token_budget: Option<u64>,
    ) -> Result<GoalView, String>;
    /// 状态迁移(模型面只许 complete/blocked/paused,工具侧已校验)。
    fn update_goal_status(&self, session_id: &str, status: GoalStatus) -> Result<GoalView, String>;
    fn get_goal(&self, session_id: &str) -> Result<Option<GoalView>, String>;
}

static HOST: OnceLock<Arc<dyn GoalHost>> = OnceLock::new();

pub fn install_host(host: Arc<dyn GoalHost>) {
    if HOST.set(host).is_ok() {
        log::info!("[goal] in-process goal host installed");
    }
}

pub fn host() -> Option<Arc<dyn GoalHost>> {
    HOST.get().cloned()
}

// ── Output ──────────────────────────────────────────────────────────────────

/// 三工具共用输出(Codex GoalToolResponse 同构)。
#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct GoalToolOutput {
    pub goal: Option<GoalView>,
    pub remaining_tokens: Option<u64>,
    /// complete 且有用量时的汇报指令;其余场合缺席。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub completion_budget_report: Option<String>,
}

impl GoalToolOutput {
    fn new(goal: Option<GoalView>, report_complete: bool) -> Self {
        let remaining_tokens = goal
            .as_ref()
            .and_then(|g| g.token_budget.map(|b| b.saturating_sub(g.tokens_used)));
        let completion_budget_report = if report_complete
            && goal.as_ref().is_some_and(|g| {
                g.status == GoalStatus::Complete
                    && (g.token_budget.is_some() || g.time_used_ms > 0)
            }) {
            Some(
                "Goal achieved. Report final usage from this tool result's goal fields: \
                 if token_budget is present, include tokens_used/token_budget; \
                 if time_used_ms > 0, summarize elapsed time concisely in the response language."
                    .to_string(),
            )
        } else {
            None
        };
        Self {
            goal,
            remaining_tokens,
            completion_budget_report,
        }
    }
}

impl ToolProjection for GoalToolOutput {
    fn model_blocks(&self) -> Vec<ToolContentBlock> {
        vec![ToolContentBlock::Text {
            text: serde_json::to_string(self).unwrap_or_default(),
        }]
    }

    fn display(&self, _args: &serde_json::Value) -> ToolDisplay {
        let summary = match &self.goal {
            Some(goal) => format!(
                "goal {:?} — {} (tokens {}{})",
                goal.status,
                goal.objective,
                goal.tokens_used,
                goal.token_budget
                    .map(|b| format!("/{b}"))
                    .unwrap_or_default()
            ),
            None => "no goal".to_string(),
        };
        ToolDisplay::new(
            qaqh_tool_core::tool_api::ToolHeader::Other {
                label: "goal".to_string(),
            },
            qaqh_tool_core::tool_api::ToolBody::Text {
                text: summary.clone(),
                truncated: false,
            },
        )
        .with_summary(summary)
    }
}

// ── Args ────────────────────────────────────────────────────────────────────

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct CreateGoalArgs {
    /// Required. The concrete objective to start pursuing.
    pub objective: String,
    /// Positive token budget. Omit unless explicitly requested.
    #[serde(default)]
    pub token_budget: Option<u64>,
}

/// 模型面只许三态;resume/budget/usage 态由用户或系统控制。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum UpdateGoalStatus {
    Complete,
    Blocked,
    Paused,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct UpdateGoalArgs {
    pub status: UpdateGoalStatus,
}

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct GetGoalArgs {}

// ── Tools ───────────────────────────────────────────────────────────────────

pub struct CreateGoalTool;
pub struct UpdateGoalTool;
pub struct GetGoalTool;

fn goal_error(
    code: &str,
    message: impl Into<String>,
    hint: impl Into<String>,
) -> ToolExecutionError {
    let kind = match code {
        "missing_objective" | "invalid_status" => ToolErrorKind::InvalidArguments,
        "host_unavailable" => ToolErrorKind::Unavailable,
        _ => ToolErrorKind::Execution,
    };
    let mut error = ToolError::new(kind, message).with_hint(hint);
    error.code = ToolErrorCode::parse_or_builtin(code, kind);
    ToolExecutionError::Recoverable(error)
}

fn require_host() -> Result<Arc<dyn GoalHost>, ToolExecutionError> {
    host().ok_or_else(|| {
        goal_error(
            "host_unavailable",
            "goal tools require the daemon host",
            "Goal tools require the daemon host (install_host).",
        )
    })
}

impl TypedTool for CreateGoalTool {
    type Args = CreateGoalArgs;
    type Output = GoalToolOutput;

    fn meta(&self) -> ToolMeta {
        ToolMeta::new(
            "create_goal",
            "Create a goal only when explicitly requested by the user or system/developer instructions; do not infer goals from ordinary tasks. Set token_budget only when an explicit token budget is requested. Fails if an unfinished goal exists; use update_goal only for status.",
            qaqh_workspace::permission::ToolCategory::Write,
            ToolRisk::Write,
            Duration::from_secs(10),
        )
    }

    fn run(
        &self,
        ctx: &ToolCallContext,
        args: CreateGoalArgs,
    ) -> Result<Self::Output, ToolExecutionError> {
        let objective = args.objective.trim().to_string();
        if objective.is_empty() {
            return Err(goal_error(
                "missing_objective",
                "create_goal: objective is required",
                "Provide the concrete objective to pursue.",
            ));
        }
        let host = require_host()?;
        let goal = host
            .create_goal(&ctx.session_id, &objective, args.token_budget)
            .map_err(|error| {
                goal_error(
                    "goal_create_failed",
                    format!("create_goal: {error}"),
                    "Complete or clear the existing goal first (update_goal / goal.clear).",
                )
            })?;
        Ok(GoalToolOutput::new(Some(goal), false))
    }
}

impl TypedTool for UpdateGoalTool {
    type Args = UpdateGoalArgs;
    type Output = GoalToolOutput;

    fn meta(&self) -> ToolMeta {
        ToolMeta::new(
            "update_goal",
            "Update the existing goal's status. Set `complete` only when the objective has actually been achieved and no required work remains. Set `blocked` only when the same blocking condition has repeated for at least three consecutive goal turns and you cannot make meaningful progress without user input or an external-state change; never merely because work is hard, slow, or uncertain. Set `paused` only at the user's explicit request, never on your own initiative. You cannot resume, budget-limit, or usage-limit a goal; those are controlled by the user or system. Do not mark complete merely because the budget is nearly exhausted. When marking a budgeted goal complete, report the final token usage from the tool result to the user.",
            qaqh_workspace::permission::ToolCategory::Write,
            ToolRisk::Write,
            Duration::from_secs(10),
        )
    }

    fn run(
        &self,
        ctx: &ToolCallContext,
        args: UpdateGoalArgs,
    ) -> Result<Self::Output, ToolExecutionError> {
        let status = match args.status {
            UpdateGoalStatus::Complete => GoalStatus::Complete,
            UpdateGoalStatus::Blocked => GoalStatus::Blocked,
            UpdateGoalStatus::Paused => GoalStatus::Paused,
        };
        let host = require_host()?;
        let goal = host
            .update_goal_status(&ctx.session_id, status)
            .map_err(|error| {
                goal_error(
                    "goal_update_failed",
                    format!("update_goal: {error}"),
                    "Check get_goal for the current goal state.",
                )
            })?;
        Ok(GoalToolOutput::new(
            Some(goal),
            args.status == UpdateGoalStatus::Complete,
        ))
    }
}

impl TypedTool for GetGoalTool {
    type Args = GetGoalArgs;
    type Output = GoalToolOutput;

    fn meta(&self) -> ToolMeta {
        ToolMeta::new(
            "get_goal",
            "Get the current goal for this session, including status, budgets, token and elapsed-time usage, and remaining token budget.",
            qaqh_workspace::permission::ToolCategory::Read,
            ToolRisk::ReadOnly,
            Duration::from_secs(10),
        )
    }

    fn run(
        &self,
        ctx: &ToolCallContext,
        _args: GetGoalArgs,
    ) -> Result<Self::Output, ToolExecutionError> {
        let host = require_host()?;
        let goal = host.get_goal(&ctx.session_id).map_err(|error| {
            goal_error(
                "goal_read_failed",
                format!("get_goal: {error}"),
                "Check daemon logs.",
            )
        })?;
        Ok(GoalToolOutput::new(goal, false))
    }
}

/// 注册器:经 `SpawnInput.tool_registrars` 进入根会话(ADR-0015)。
pub fn register(mgr: &mut ToolManager) {
    mgr.register_typed(CreateGoalTool);
    mgr.register_typed(UpdateGoalTool);
    mgr.register_typed(GetGoalTool);
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 工具面注册冒烟:三件套都在执行面,名字钉住(模型按名调用)。
    #[test]
    fn register_exposes_the_goal_tool_triplet() {
        let mut manager = ToolManager::new();
        register(&mut manager);
        for name in ["create_goal", "update_goal", "get_goal"] {
            assert!(
                manager.builtin(name).is_some(),
                "{name} must be on the typed execution surface"
            );
        }
    }

    /// update_goal 的模型面值域只有三态;resume 等态不在 schema 里。
    #[test]
    fn update_status_schema_is_the_model_vocabulary() {
        let schema = schemars::schema_for!(UpdateGoalStatus);
        let value = serde_json::to_value(&schema).expect("schema json");
        let variants = value["enum"].as_array().expect("string enum");
        let names: Vec<&str> = variants.iter().filter_map(|v| v.as_str()).collect();
        assert_eq!(names, vec!["complete", "blocked", "paused"]);
    }
}
