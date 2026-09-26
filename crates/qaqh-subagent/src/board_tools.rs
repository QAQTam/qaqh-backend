//! Message board tools backed by the in-process [`BoardHost`].

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
    BoardChannel, BoardChannelCreateRequest, BoardHost, BoardListRequest, BoardNotificationSkip,
    BoardPost, BoardPostOutcome, BoardPostRequest, BoardSnapshot, BoardSubscription,
    BoardSubscriptionAction, BoardSubscriptionRequest, BoardSubscriptionTargetKind, BoardThread,
    BoardThreadCreateRequest, board_host,
};

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BoardChannelCreateArgs {
    name: String,
    #[serde(default)]
    topic: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BoardThreadCreateArgs {
    channel_id: String,
    title: String,
    #[serde(default)]
    task_id: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BoardPostArgs {
    thread_id: String,
    body: String,
    #[serde(default)]
    task_id: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BoardSubscribeArgs {
    target_kind: BoardSubscriptionTargetKind,
    target_id: String,
    action: BoardSubscriptionAction,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BoardListArgs {
    #[serde(default)]
    channel_id: Option<String>,
    #[serde(default)]
    thread_id: Option<String>,
    #[serde(default)]
    include_posts: bool,
    #[serde(default)]
    post_limit: Option<usize>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct BoardChannelOutput {
    channel: BoardChannel,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct BoardThreadOutput {
    thread: BoardThread,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct BoardPostOutput {
    post: BoardPost,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    notified: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    skipped: Vec<BoardNotificationSkip>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct BoardSubscriptionOutput {
    subscription: BoardSubscription,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct BoardListOutput {
    board: BoardSnapshot,
}

macro_rules! simple_projection {
    ($type:ty, $label:literal) => {
        impl ToolProjection for $type {
            fn model_blocks(&self) -> Vec<ToolContentBlock> {
                vec![ToolContentBlock::Text {
                    text: serde_json::to_string(self).unwrap_or_default(),
                }]
            }

            fn summary(&self) -> Option<String> {
                Some($label.to_string())
            }

            fn display(&self, _args: &serde_json::Value) -> ToolDisplay {
                let summary = self.summary().unwrap_or_default();
                ToolDisplay::new(
                    qaqh_workspace::tool_api::ToolHeader::Other {
                        label: "board".to_string(),
                    },
                    qaqh_workspace::tool_api::ToolBody::Text {
                        text: summary.clone(),
                        truncated: false,
                    },
                )
                .with_summary(summary)
            }
        }
    };
}

simple_projection!(BoardChannelOutput, "channel created");
simple_projection!(BoardThreadOutput, "thread created");
simple_projection!(BoardSubscriptionOutput, "subscription updated");

impl ToolProjection for BoardPostOutput {
    fn model_blocks(&self) -> Vec<ToolContentBlock> {
        vec![ToolContentBlock::Text {
            text: serde_json::to_string(self).unwrap_or_default(),
        }]
    }

    fn summary(&self) -> Option<String> {
        Some(format!("posted {}", self.post.post_id))
    }

    fn display(&self, _args: &serde_json::Value) -> ToolDisplay {
        let summary = self.summary().unwrap_or_default();
        ToolDisplay::new(
            qaqh_workspace::tool_api::ToolHeader::Other {
                label: "board".to_string(),
            },
            qaqh_workspace::tool_api::ToolBody::Text {
                text: summary.clone(),
                truncated: false,
            },
        )
        .with_summary(summary)
    }
}

impl ToolProjection for BoardListOutput {
    fn model_blocks(&self) -> Vec<ToolContentBlock> {
        vec![ToolContentBlock::Text {
            text: serde_json::to_string(self).unwrap_or_default(),
        }]
    }

    fn summary(&self) -> Option<String> {
        Some(format!(
            "{} channel(s), {} thread(s), {} post(s)",
            self.board.channels.len(),
            self.board.threads.len(),
            self.board.posts.len()
        ))
    }

    fn display(&self, _args: &serde_json::Value) -> ToolDisplay {
        let summary = self.summary().unwrap_or_default();
        ToolDisplay::new(
            qaqh_workspace::tool_api::ToolHeader::Other {
                label: "board".to_string(),
            },
            qaqh_workspace::tool_api::ToolBody::Text {
                text: summary.clone(),
                truncated: false,
            },
        )
        .with_summary(summary)
    }
}

pub struct BoardChannelCreateTool;

impl TypedTool for BoardChannelCreateTool {
    type Args = BoardChannelCreateArgs;
    type Output = BoardChannelOutput;

    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: ToolName::new("board_channel_create").expect("valid board tool name"),
            display_name: None,
            description: "Create a persistent channel on the current root tree's message board."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Lowercase channel slug." },
                    "topic": { "type": "string", "description": "Optional channel topic." }
                },
                "required": ["name"],
                "additionalProperties": false
            }),
            output_schema: serde_json::to_value(schemars::schema_for!(BoardChannelOutput))
                .expect("board_channel_create output schema"),
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
        args: BoardChannelCreateArgs,
    ) -> Result<Self::Output, ToolExecutionError> {
        let channel = board_host_or_error("board_channel_create")?
            .board_channel_create(BoardChannelCreateRequest {
                caller_session_id: &ctx.session_id,
                name: &args.name,
                topic: args.topic.as_deref(),
            })
            .map_err(|error| board_error("BOARD_CHANNEL_CREATE_FAILED", error))?;
        Ok(BoardChannelOutput { channel })
    }
}

pub struct BoardThreadCreateTool;

impl TypedTool for BoardThreadCreateTool {
    type Args = BoardThreadCreateArgs;
    type Output = BoardThreadOutput;

    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: ToolName::new("board_thread_create").expect("valid board tool name"),
            display_name: None,
            description: "Create a persistent thread in a message board channel.".to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "channel_id": { "type": "string" },
                    "title": { "type": "string" },
                    "task_id": { "type": "string", "description": "Optional task id in the same root tree." }
                },
                "required": ["channel_id", "title"],
                "additionalProperties": false
            }),
            output_schema: serde_json::to_value(schemars::schema_for!(BoardThreadOutput))
                .expect("board_thread_create output schema"),
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
        args: BoardThreadCreateArgs,
    ) -> Result<Self::Output, ToolExecutionError> {
        let thread = board_host_or_error("board_thread_create")?
            .board_thread_create(BoardThreadCreateRequest {
                caller_session_id: &ctx.session_id,
                channel_id: &args.channel_id,
                title: &args.title,
                task_id: args.task_id.as_deref(),
            })
            .map_err(|error| board_error("BOARD_THREAD_CREATE_FAILED", error))?;
        Ok(BoardThreadOutput { thread })
    }
}

pub struct BoardPostTool;

impl TypedTool for BoardPostTool {
    type Args = BoardPostArgs;
    type Output = BoardPostOutput;

    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: ToolName::new("board_post").expect("valid board tool name"),
            display_name: None,
            description:
                "Append a persistent post to a message board thread and best-effort notify running subscribers."
                    .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "thread_id": { "type": "string" },
                    "body": { "type": "string", "description": "Post body, at most 16 KiB." },
                    "task_id": { "type": "string", "description": "Optional task id in the same root tree." }
                },
                "required": ["thread_id", "body"],
                "additionalProperties": false
            }),
            output_schema: serde_json::to_value(schemars::schema_for!(BoardPostOutput))
                .expect("board_post output schema"),
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
        args: BoardPostArgs,
    ) -> Result<Self::Output, ToolExecutionError> {
        let outcome = board_host_or_error("board_post")?
            .board_post(BoardPostRequest {
                caller_session_id: &ctx.session_id,
                thread_id: &args.thread_id,
                body: &args.body,
                task_id: args.task_id.as_deref(),
            })
            .map_err(|error| board_error("BOARD_POST_FAILED", error))?;
        let BoardPostOutcome {
            post,
            notified,
            skipped,
        } = outcome;
        Ok(BoardPostOutput {
            post,
            notified,
            skipped,
        })
    }
}

pub struct BoardSubscribeTool;

impl TypedTool for BoardSubscribeTool {
    type Args = BoardSubscribeArgs;
    type Output = BoardSubscriptionOutput;

    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: ToolName::new("board_subscribe").expect("valid board tool name"),
            display_name: None,
            description: "Subscribe or unsubscribe the caller from a board channel or thread."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "target_kind": { "type": "string", "enum": ["channel", "thread"] },
                    "target_id": { "type": "string" },
                    "action": { "type": "string", "enum": ["subscribe", "unsubscribe"] }
                },
                "required": ["target_kind", "target_id", "action"],
                "additionalProperties": false
            }),
            output_schema: serde_json::to_value(schemars::schema_for!(BoardSubscriptionOutput))
                .expect("board_subscribe output schema"),
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
        args: BoardSubscribeArgs,
    ) -> Result<Self::Output, ToolExecutionError> {
        let subscription = board_host_or_error("board_subscribe")?
            .board_subscribe(BoardSubscriptionRequest {
                caller_session_id: &ctx.session_id,
                target_kind: args.target_kind,
                target_id: &args.target_id,
                action: args.action,
            })
            .map_err(|error| board_error("BOARD_SUBSCRIBE_FAILED", error))?;
        Ok(BoardSubscriptionOutput { subscription })
    }
}

pub struct BoardListTool;

impl TypedTool for BoardListTool {
    type Args = BoardListArgs;
    type Output = BoardListOutput;

    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: ToolName::new("board_list").expect("valid board tool name"),
            display_name: None,
            description: "List message board channels, threads, posts, and subscriptions."
                .to_string(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "channel_id": { "type": "string" },
                    "thread_id": { "type": "string" },
                    "include_posts": { "type": "boolean", "default": false },
                    "post_limit": { "type": "integer", "minimum": 1, "maximum": 200 }
                },
                "additionalProperties": false
            }),
            output_schema: serde_json::to_value(schemars::schema_for!(BoardListOutput))
                .expect("board_list output schema"),
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
        args: BoardListArgs,
    ) -> Result<Self::Output, ToolExecutionError> {
        let board = board_host_or_error("board_list")?
            .board_list(BoardListRequest {
                caller_session_id: &ctx.session_id,
                channel_id: args.channel_id.as_deref(),
                thread_id: args.thread_id.as_deref(),
                include_posts: args.include_posts,
                post_limit: args.post_limit,
            })
            .map_err(|error| board_error("BOARD_LIST_FAILED", error))?;
        Ok(BoardListOutput { board })
    }
}

fn board_host_or_error(tool: &str) -> Result<std::sync::Arc<dyn BoardHost>, ToolExecutionError> {
    board_host().ok_or_else(|| {
        board_error(
            "HOST_UNAVAILABLE",
            format!("{tool} requires the in-process message board host"),
        )
    })
}

fn board_error(code: &str, message: impl Into<String>) -> ToolExecutionError {
    let kind = match code {
        "HOST_UNAVAILABLE" => ToolErrorKind::Unavailable,
        "BOARD_CHANNEL_CREATE_FAILED"
        | "BOARD_THREAD_CREATE_FAILED"
        | "BOARD_POST_FAILED"
        | "BOARD_SUBSCRIBE_FAILED"
        | "BOARD_LIST_FAILED" => ToolErrorKind::Execution,
        _ => ToolErrorKind::Custom,
    };
    let mut error = ToolError::new(kind, message)
        .with_hint("Check the message board state and retry with valid references.");
    error.code = ToolErrorCode::from_legacy(code);
    ToolExecutionError::Recoverable(error)
}
