//! Task board tools backed by the in-process [`TaskBoardHost`].

use std::time::Duration;

use qaqh_tool_core::ToolRisk;
use qaqh_tool_core::tool_api::{
    ToolCallContext, ToolContentBlock, ToolDisplay, ToolError, ToolErrorCode, ToolErrorKind,
    ToolExecutionError, ToolMeta, ToolProjection, TypedTool,
};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::host::{
    TaskBoardHost, TaskBoardTask, TaskClaimAction, TaskClaimRequest, TaskCloseAction,
    TaskCloseRequest, TaskCreateRequest, TaskListRequest, TaskUpdateAction, TaskUpdateRequest,
    task_host,
};

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TaskCreateArgs {
    /// Short task title.
    title: String,
    /// Optional sha256 content id for the full description.
    #[serde(default)]
    description_ref: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TaskClaimArgs {
    task_id: String,
    action: TaskClaimAction,
    /// Required for release.
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TaskUpdateArgs {
    task_id: String,
    action: TaskUpdateAction,
    /// Task id this task depends on (action=add_dependency).
    #[serde(default)]
    depends_on: Option<String>,
    /// Content id of the artifact to attach (action=attach_artifact).
    #[serde(default)]
    artifact_ref: Option<String>,
    /// Media type of the attached artifact.
    #[serde(default)]
    media_type: Option<String>,
    /// Replacement acceptance criteria (action=set_acceptance).
    #[serde(default)]
    acceptance: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TaskCloseArgs {
    task_id: String,
    action: TaskCloseAction,
    /// Content id of the completion result.
    #[serde(default)]
    result_ref: Option<String>,
    /// Close/cancel reason.
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TaskListArgs {
    /// Filter by state: open | claimed | completed | closed | cancelled.
    #[serde(default)]
    state: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct TaskOutput {
    task: TaskBoardTask,
}

impl TaskOutput {
    fn summary_text(&self) -> String {
        format!(
            "{} [{}] epoch={}",
            self.task.task_id, self.task.state, self.task.claim_epoch
        )
    }
}

impl ToolProjection for TaskOutput {
    fn model_blocks(&self) -> Vec<ToolContentBlock> {
        vec![ToolContentBlock::Text {
            text: serde_json::to_string(self).unwrap_or_default(),
        }]
    }

    fn display(&self, _args: &serde_json::Value) -> ToolDisplay {
        let summary = self.summary_text();
        ToolDisplay::new(
            qaqh_tool_core::tool_api::ToolHeader::Other {
                label: "tasks".to_string(),
            },
            qaqh_tool_core::tool_api::ToolBody::Text {
                text: summary.clone(),
                truncated: false,
            },
        )
        .with_summary(summary)
    }
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct TaskListOutput {
    tasks: Vec<TaskBoardTask>,
}

impl TaskListOutput {
    fn summary_text(&self) -> String {
        format!("{} task(s)", self.tasks.len())
    }
}

impl ToolProjection for TaskListOutput {
    fn model_blocks(&self) -> Vec<ToolContentBlock> {
        vec![ToolContentBlock::Text {
            text: serde_json::to_string(self).unwrap_or_default(),
        }]
    }

    fn display(&self, _args: &serde_json::Value) -> ToolDisplay {
        let summary = self.summary_text();
        ToolDisplay::new(
            qaqh_tool_core::tool_api::ToolHeader::Other {
                label: "tasks".to_string(),
            },
            qaqh_tool_core::tool_api::ToolBody::Text {
                text: summary.clone(),
                truncated: false,
            },
        )
        .with_summary(summary)
    }
}

pub struct TaskCreateTool;

impl TypedTool for TaskCreateTool {
    type Args = TaskCreateArgs;
    type Output = TaskOutput;

    fn meta(&self) -> ToolMeta {
        ToolMeta::new(
            "task_create",
            "Create a task in the current root tree's shared task board.",
            qaqh_workspace::permission::ToolCategory::Exec,
            ToolRisk::Administrative,
            Duration::from_secs(30),
        )
    }

    fn run(
        &self,
        ctx: &ToolCallContext,
        args: TaskCreateArgs,
    ) -> Result<Self::Output, ToolExecutionError> {
        let task = task_host_or_error("task_create")?
            .task_create(TaskCreateRequest {
                caller_session_id: &ctx.session_id,
                title: &args.title,
                description_ref: args.description_ref.as_deref(),
            })
            .map_err(|error| task_error("task_create_failed", error))?;
        Ok(TaskOutput { task })
    }
}

pub struct TaskClaimTool;

impl TypedTool for TaskClaimTool {
    type Args = TaskClaimArgs;
    type Output = TaskOutput;

    fn meta(&self) -> ToolMeta {
        ToolMeta::new(
            "task_claim",
            "Claim or release a task with compare-and-set claim epochs.",
            qaqh_workspace::permission::ToolCategory::Exec,
            ToolRisk::Administrative,
            Duration::from_secs(30),
        )
    }

    fn run(
        &self,
        ctx: &ToolCallContext,
        args: TaskClaimArgs,
    ) -> Result<Self::Output, ToolExecutionError> {
        let task = task_host_or_error("task_claim")?
            .task_claim(TaskClaimRequest {
                caller_session_id: &ctx.session_id,
                task_id: &args.task_id,
                action: args.action,
                reason: args.reason.as_deref(),
            })
            .map_err(|error| task_error("task_claim_failed", error))?;
        Ok(TaskOutput { task })
    }
}

pub struct TaskUpdateTool;

impl TypedTool for TaskUpdateTool {
    type Args = TaskUpdateArgs;
    type Output = TaskOutput;

    fn meta(&self) -> ToolMeta {
        ToolMeta::new(
            "task_update",
            "Add a dependency, attach an artifact, or replace acceptance criteria.",
            qaqh_workspace::permission::ToolCategory::Exec,
            ToolRisk::Administrative,
            Duration::from_secs(30),
        )
    }

    fn run(
        &self,
        ctx: &ToolCallContext,
        args: TaskUpdateArgs,
    ) -> Result<Self::Output, ToolExecutionError> {
        let task = task_host_or_error("task_update")?
            .task_update(TaskUpdateRequest {
                caller_session_id: &ctx.session_id,
                task_id: &args.task_id,
                action: args.action,
                depends_on: args.depends_on.as_deref(),
                artifact_ref: args.artifact_ref.as_deref(),
                media_type: args.media_type.as_deref(),
                acceptance: args.acceptance.as_deref(),
            })
            .map_err(|error| task_error("task_update_failed", error))?;
        Ok(TaskOutput { task })
    }
}

pub struct TaskCloseTool;

impl TypedTool for TaskCloseTool {
    type Args = TaskCloseArgs;
    type Output = TaskOutput;

    fn meta(&self) -> ToolMeta {
        ToolMeta::new(
            "task_close",
            "Complete, close, or cancel a task.",
            qaqh_workspace::permission::ToolCategory::Exec,
            ToolRisk::Administrative,
            Duration::from_secs(30),
        )
    }

    fn run(
        &self,
        ctx: &ToolCallContext,
        args: TaskCloseArgs,
    ) -> Result<Self::Output, ToolExecutionError> {
        let task = task_host_or_error("task_close")?
            .task_close(TaskCloseRequest {
                caller_session_id: &ctx.session_id,
                task_id: &args.task_id,
                action: args.action,
                result_ref: args.result_ref.as_deref(),
                reason: args.reason.as_deref(),
            })
            .map_err(|error| task_error("task_close_failed", error))?;
        Ok(TaskOutput { task })
    }
}

pub struct TaskListTool;

impl TypedTool for TaskListTool {
    type Args = TaskListArgs;
    type Output = TaskListOutput;

    fn meta(&self) -> ToolMeta {
        ToolMeta::new(
            "task_list",
            "List tasks in the current root tree's shared task board.",
            qaqh_workspace::permission::ToolCategory::Read,
            ToolRisk::ReadOnly,
            Duration::from_secs(30),
        )
    }

    fn run(
        &self,
        ctx: &ToolCallContext,
        args: TaskListArgs,
    ) -> Result<Self::Output, ToolExecutionError> {
        let tasks = task_host_or_error("task_list")?
            .task_list(TaskListRequest {
                caller_session_id: &ctx.session_id,
                state: args.state.as_deref(),
            })
            .map_err(|error| task_error("task_list_failed", error))?;
        Ok(TaskListOutput { tasks })
    }
}

fn task_host_or_error(tool: &str) -> Result<std::sync::Arc<dyn TaskBoardHost>, ToolExecutionError> {
    task_host().ok_or_else(|| {
        task_error(
            "host_unavailable",
            format!("{tool} requires the in-process task board host"),
        )
    })
}

fn task_error(code: &str, message: impl Into<String>) -> ToolExecutionError {
    let kind = match code {
        "host_unavailable" => ToolErrorKind::Unavailable,
        "task_create_failed" | "task_claim_failed" | "task_update_failed" | "task_close_failed"
        | "task_list_failed" => ToolErrorKind::Execution,
        _ => ToolErrorKind::Custom,
    };
    let mut error = ToolError::new(kind, message)
        .with_hint("Check the task board state and retry with a valid transition.");
    error.code = ToolErrorCode::parse_or_builtin(code, kind);
    ToolExecutionError::Recoverable(error)
}
