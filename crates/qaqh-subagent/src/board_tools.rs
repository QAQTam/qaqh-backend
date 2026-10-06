//! Message board tools backed by the in-process [`BoardHost`].

use std::time::Duration;

use qaqh_tool_core::ToolRisk;
use qaqh_tool_core::tool_api::{
    ToolCallContext, ToolContentBlock, ToolDisplay, ToolError, ToolErrorCode, ToolErrorKind,
    ToolExecutionError, ToolMeta, ToolProjection, TypedTool,
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
    /// Lowercase channel slug.
    name: String,
    /// Optional channel topic.
    #[serde(default)]
    topic: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BoardThreadCreateArgs {
    channel_id: String,
    title: String,
    /// Optional task id in the same root tree.
    #[serde(default)]
    task_id: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct BoardPostArgs {
    thread_id: String,
    /// Post body, at most 16 KiB.
    body: String,
    /// Optional task id in the same root tree.
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
    /// Cap on returned posts when include_posts is set (1..=200).
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

            fn display(&self, _args: &serde_json::Value) -> ToolDisplay {
                ToolDisplay::new(
                    qaqh_tool_core::tool_api::ToolHeader::Other {
                        label: "board".to_string(),
                    },
                    qaqh_tool_core::tool_api::ToolBody::Text {
                        text: $label.to_string(),
                        truncated: false,
                    },
                )
                .with_summary($label.to_string())
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

    fn display(&self, _args: &serde_json::Value) -> ToolDisplay {
        let summary = format!("posted {}", self.post.post_id);
        ToolDisplay::new(
            qaqh_tool_core::tool_api::ToolHeader::Other {
                label: "board".to_string(),
            },
            qaqh_tool_core::tool_api::ToolBody::Text {
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

    fn display(&self, _args: &serde_json::Value) -> ToolDisplay {
        let summary = format!(
            "{} channel(s), {} thread(s), {} post(s)",
            self.board.channels.len(),
            self.board.threads.len(),
            self.board.posts.len()
        );
        ToolDisplay::new(
            qaqh_tool_core::tool_api::ToolHeader::Other {
                label: "board".to_string(),
            },
            qaqh_tool_core::tool_api::ToolBody::Text {
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

    fn meta(&self) -> ToolMeta {
        ToolMeta::new(
            "board_channel_create",
            "Create a persistent channel on the current root tree's message board.",
            qaqh_workspace::permission::ToolCategory::Exec,
            ToolRisk::Administrative,
            Duration::from_secs(30),
        )
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
            .map_err(|error| board_error("board_channel_create_failed", error))?;
        Ok(BoardChannelOutput { channel })
    }
}

pub struct BoardThreadCreateTool;

impl TypedTool for BoardThreadCreateTool {
    type Args = BoardThreadCreateArgs;
    type Output = BoardThreadOutput;

    fn meta(&self) -> ToolMeta {
        ToolMeta::new(
            "board_thread_create",
            "Create a persistent thread in a message board channel.",
            qaqh_workspace::permission::ToolCategory::Exec,
            ToolRisk::Administrative,
            Duration::from_secs(30),
        )
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
            .map_err(|error| board_error("board_thread_create_failed", error))?;
        Ok(BoardThreadOutput { thread })
    }
}

pub struct BoardPostTool;

impl TypedTool for BoardPostTool {
    type Args = BoardPostArgs;
    type Output = BoardPostOutput;

    fn meta(&self) -> ToolMeta {
        ToolMeta::new(
            "board_post",
            "Append a persistent post to a message board thread and best-effort notify running subscribers.",
            qaqh_workspace::permission::ToolCategory::Exec,
            ToolRisk::Administrative,
            Duration::from_secs(30),
        )
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
            .map_err(|error| board_error("board_post_failed", error))?;
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

    fn meta(&self) -> ToolMeta {
        ToolMeta::new(
            "board_subscribe",
            "Subscribe or unsubscribe the caller from a board channel or thread.",
            qaqh_workspace::permission::ToolCategory::Exec,
            ToolRisk::Administrative,
            Duration::from_secs(30),
        )
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
            .map_err(|error| board_error("board_subscribe_failed", error))?;
        Ok(BoardSubscriptionOutput { subscription })
    }
}

pub struct BoardListTool;

impl TypedTool for BoardListTool {
    type Args = BoardListArgs;
    type Output = BoardListOutput;

    fn meta(&self) -> ToolMeta {
        ToolMeta::new(
            "board_list",
            "List message board channels, threads, posts, and subscriptions.",
            qaqh_workspace::permission::ToolCategory::Read,
            ToolRisk::ReadOnly,
            Duration::from_secs(30),
        )
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
            .map_err(|error| board_error("board_list_failed", error))?;
        Ok(BoardListOutput { board })
    }
}

fn board_host_or_error(tool: &str) -> Result<std::sync::Arc<dyn BoardHost>, ToolExecutionError> {
    board_host().ok_or_else(|| {
        board_error(
            "host_unavailable",
            format!("{tool} requires the in-process message board host"),
        )
    })
}

fn board_error(code: &str, message: impl Into<String>) -> ToolExecutionError {
    let kind = match code {
        "host_unavailable" => ToolErrorKind::Unavailable,
        "board_channel_create_failed"
        | "board_thread_create_failed"
        | "board_post_failed"
        | "board_subscribe_failed"
        | "board_list_failed" => ToolErrorKind::Execution,
        _ => ToolErrorKind::Custom,
    };
    let mut error = ToolError::new(kind, message)
        .with_hint("Check the message board state and retry with valid references.");
    error.code = ToolErrorCode::parse_or_builtin(code, kind);
    ToolExecutionError::Recoverable(error)
}
