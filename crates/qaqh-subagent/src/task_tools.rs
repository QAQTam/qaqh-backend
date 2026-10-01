//! Task board tools backed by the in-process [`TaskBoardHost`].

use std::time::Duration;

use qaqh_workspace::ToolRisk;
use qaqh_workspace::tool_api::{
    OutputBudget, ToolCallContext, ToolContentBlock, ToolDescriptor, ToolDisplay, ToolError,
    ToolErrorCode, ToolErrorKind, ToolExecutionError, ToolExposure, ToolName, ToolProjection,
    ToolSource, TypedTool,
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
    title: String,
    #[serde(default)]
    description_ref: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TaskClaimArgs {
    task_id: String,
    action: TaskClaimAction,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TaskUpdateArgs {
    task_id: String,
    action: TaskUpdateAction,
    #[serde(default)]
    depends_on: Option<String>,
    #[serde(default)]
    artifact_ref: Option<String>,
    #[serde(default)]
    media_type: Option<String>,
    #[serde(default)]
    acceptance: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TaskCloseArgs {
    task_id: String,
    action: TaskCloseAction,
    #[serde(default)]
    result_ref: Option<String>,
    #[serde(default)]
    reason: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct TaskListArgs {
    #[serde(default)]
    state: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct TaskOutput {
    task: TaskBoardTask,
}

impl ToolProjection for TaskOutput {
    fn model_blocks(&self) -> Vec<ToolContentBlock> {
        vec![ToolContentBlock::Text {
            text: serde_json::to_string(self).unwrap_or_default(),
        }]
    }

    fn summary(&self) -> Option<String> {
        Some(format!(
            "{} [{}] epoch={}",
            self.task.task_id, self.task.state, self.task.claim_epoch
        ))
    }

    fn display(&self, _args: &serde_json::Value) -> ToolDisplay {
        let summary = self.summary().unwrap_or_default();
        ToolDisplay::new(
            qaqh_workspace::tool_api::ToolHeader::Other {
                label: "tasks".to_string(),
            },
            qaqh_workspace::tool_api::ToolBody::Text {
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

impl ToolProjection for TaskListOutput {
    fn model_blocks(&self) -> Vec<ToolContentBlock> {
        vec![ToolContentBlock::Text {
            text: serde_json::to_string(self).unwrap_or_default(),
        }]
    }

    fn summary(&self) -> Option<String> {
        Some(format!("{} task(s)", self.tasks.len()))
    }

    fn display(&self, _args: &serde_json::Value) -> ToolDisplay {
        let summary = self.summary().unwrap_or_default();
        ToolDisplay::new(
            qaqh_workspace::tool_api::ToolHeader::Other {
                label: "tasks".to_string(),
            },
            qaqh_workspace::tool_api::ToolBody::Text {
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

    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: ToolName::new("task_create").expect("valid task_create tool name"),
            display_name: None,
            description: "Create a task in the current root tree's shared task board.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "title": { "type": "string", "description": "Short task title." },
                    "description_ref": {
                        "type": "string",
                        "description": "Optional sha256 content id for the full description."
                    }
                },
                "required": ["title"],
                "additionalProperties": false
            }),
            output_schema: serde_json::to_value(schemars::schema_for!(TaskOutput))
                .expect("task_create output schema"),
            category: qaqh_workspace::permission::ToolCategory::Exec,
            risk: ToolRisk::Administrative,
            default_timeout: Duration::from_secs(30),
            exposure: ToolExposure::Direct,
            source: ToolSource::Builtin,
            output_budget: OutputBudget::default(),
            capabilities: qaqh_workspace::tool_api::ToolCapabilities::default(),
        }
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

    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: ToolName::new("task_claim").expect("valid task_claim tool name"),
            display_name: None,
            description: "Claim or release a task with compare-and-set claim epochs.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "task_id": { "type": "string" },
                    "action": { "type": "string", "enum": ["claim", "release"] },
                    "reason": { "type": "string", "description": "Required for release." }
                },
                "required": ["task_id", "action"],
                "additionalProperties": false
            }),
            output_schema: serde_json::to_value(schemars::schema_for!(TaskOutput))
                .expect("task_claim output schema"),
            category: qaqh_workspace::permission::ToolCategory::Exec,
            risk: ToolRisk::Administrative,
            default_timeout: Duration::from_secs(30),
            exposure: ToolExposure::Direct,
            source: ToolSource::Builtin,
            output_budget: OutputBudget::default(),
            capabilities: qaqh_workspace::tool_api::ToolCapabilities::default(),
        }
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

    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: ToolName::new("task_update").expect("valid task_update tool name"),
            display_name: None,
            description: "Add a dependency, attach an artifact, or replace acceptance criteria."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "task_id": { "type": "string" },
                    "action": {
                        "type": "string",
                        "enum": ["add_dependency", "attach_artifact", "set_acceptance"]
                    },
                    "depends_on": { "type": "string" },
                    "artifact_ref": { "type": "string" },
                    "media_type": { "type": "string" },
                    "acceptance": { "type": "array", "items": { "type": "string" } }
                },
                "required": ["task_id", "action"],
                "additionalProperties": false
            }),
            output_schema: serde_json::to_value(schemars::schema_for!(TaskOutput))
                .expect("task_update output schema"),
            category: qaqh_workspace::permission::ToolCategory::Exec,
            risk: ToolRisk::Administrative,
            default_timeout: Duration::from_secs(30),
            exposure: ToolExposure::Direct,
            source: ToolSource::Builtin,
            output_budget: OutputBudget::default(),
            capabilities: qaqh_workspace::tool_api::ToolCapabilities::default(),
        }
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

    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: ToolName::new("task_close").expect("valid task_close tool name"),
            display_name: None,
            description: "Complete, close, or cancel a task.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "task_id": { "type": "string" },
                    "action": { "type": "string", "enum": ["complete", "close", "cancel"] },
                    "result_ref": { "type": "string" },
                    "reason": { "type": "string" }
                },
                "required": ["task_id", "action"],
                "additionalProperties": false
            }),
            output_schema: serde_json::to_value(schemars::schema_for!(TaskOutput))
                .expect("task_close output schema"),
            category: qaqh_workspace::permission::ToolCategory::Exec,
            risk: ToolRisk::Administrative,
            default_timeout: Duration::from_secs(30),
            exposure: ToolExposure::Direct,
            source: ToolSource::Builtin,
            output_budget: OutputBudget::default(),
            capabilities: qaqh_workspace::tool_api::ToolCapabilities::default(),
        }
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

    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: ToolName::new("task_list").expect("valid task_list tool name"),
            display_name: None,
            description: "List tasks in the current root tree's shared task board.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "state": {
                        "type": "string",
                        "enum": ["open", "claimed", "completed", "closed", "cancelled"]
                    }
                },
                "additionalProperties": false
            }),
            output_schema: serde_json::to_value(schemars::schema_for!(TaskListOutput))
                .expect("task_list output schema"),
            category: qaqh_workspace::permission::ToolCategory::Read,
            risk: ToolRisk::ReadOnly,
            default_timeout: Duration::from_secs(30),
            exposure: ToolExposure::Direct,
            source: ToolSource::Builtin,
            output_budget: OutputBudget::default(),
            capabilities: qaqh_workspace::tool_api::ToolCapabilities::default(),
        }
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
