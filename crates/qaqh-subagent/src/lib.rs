//! qaqh-subagent — spawn sub-agent tool for the QAQ-Harness agent.
//!
//! The subagent is an **isolated Ringing session**, not a raw child process:
//!
//! 1. `spawn_subagent` runs the subagent as an in-process actor on a daemon
//!    thread. When the daemon installs an in-process [`SubagentHost`]
//!    (Knife-1 step-2: `QaqhService` via `qaqh_subagent::install_host`), the
//!    tool drives the actor directly through the host handle — no daemon
//!    HTTP/SSE loopback. (The legacy HTTP/SSE fallback was removed in PR-4-2;
//!    a host handle is required.)
//! 2. The parent attaches (or directly addresses) the sub-seed and sends the
//!    task via the ordinary `ConversationSendMessage` Ringing command.
//! 3. A background collector thread watches the event stream for
//!    `TurnCompleted` / `TurnFailed` / `ConversationCancelled` and records the
//!    child's terminal outcome through [`SubagentHost::finish_subagent`], i.e.
//!    on the **agent status plane** (`list_agents`) — not in the exec process
//!    registry. The child is addressed by `child_session_id` / `agent_path`,
//!    never by a process id.
//!
//! Supports model override (different model/provider per subagent), context
//! sharing, per-instance naming, and timeout/cancel semantics.
//!
//! ## Registration
//!
//! Call `qaqh_subagent::register(&mut tool_manager)` during agent
//! initialization (the subagent worker itself does this via
//! `AgentState::init_subagent`) to register the `spawn_subagent` tool.

#![allow(clippy::result_large_err)] // TypedTool's frozen public error boundary.

use std::sync::Arc;
use std::sync::mpsc;
use std::time::{Duration, Instant};

use qaqh_domain::ConversationCommand;
use qaqh_ringing::RingingCommand;
use qaqh_tool_core::tool_api::{
    ToolCallContext, ToolContentBlock, ToolDisplay, ToolError, ToolErrorCode, ToolErrorKind,
    ToolExecutionError, ToolMeta, ToolProjection, TypedTool,
};
use qaqh_workspace::{ToolManager, ToolRisk};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

mod board_tools;
mod host;
mod task_tools;
pub use host::{
    AgentOutcome, ArmSubagentCollectorRequest, BoardChannel, BoardChannelCreateRequest, BoardHost,
    BoardListRequest, BoardNotificationSkip, BoardPost, BoardPostOutcome, BoardPostRequest,
    BoardSnapshot, BoardSubscription, BoardSubscriptionAction, BoardSubscriptionRequest,
    BoardSubscriptionTarget, BoardSubscriptionTargetKind, BoardThread, BoardThreadCreateRequest,
    CollectorBatch, CollectorEvent, ContentRef, InterruptAgentRequest, InterruptedAgent,
    ListedAgent, ListedAgentResidency, ListedAgentStatus, SendAgentMessageRequest,
    SentAgentMessage, SpawnSubagentRequest, SpawnedSubagent, StartSubagentRequest, SubagentHost,
    TaskBoardArtifact, TaskBoardHost, TaskBoardTask, TaskClaimAction, TaskClaimRequest,
    TaskCloseAction, TaskCloseRequest, TaskCreateRequest, TaskListRequest, TaskUpdateAction,
    TaskUpdateRequest, WaitAgentOutcome, WaitAgentRequest, board_host, host, install_board_host,
    install_host, install_task_host, task_host,
};

/// 子代理固定身份提示：注入到子代理任务文本的 `[SYSTEM]` 段。
/// 子代理的 base system prompt（`backend_prompt.md`）与主代理同源（同 config
/// 加载），前缀天然一致、可命中 provider 前缀缓存；本段补充子代理专属身份约束。
const SUBAGENT_IDENTITY_PROMPT: &str = "\
You are a subagent engineer working in QAQ-Harness. Follow the main coding agent's \
instructions exactly, never take unauthorized actions, and complete the assigned task faithfully.";

const WAIT_AGENT_MIN_TIMEOUT_MS: u64 = 1_000;
const WAIT_AGENT_DEFAULT_TIMEOUT_MS: u64 = 30_000;
const WAIT_AGENT_MAX_TIMEOUT_MS: u64 = 3_600_000;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct SpawnSubagentArgs {
    /// Short description of the task for the subagent.
    task_description: String,
    /// Name for this subagent, verb+task phrase (e.g. 'explore_task', 'review_code').
    #[serde(default)]
    agent_name: Option<String>,
    /// Optional background context to hand to the subagent before the task.
    #[serde(default)]
    context: Option<String>,
    /// Maximum time in seconds before the subagent is cancelled. Default 120.
    #[serde(default)]
    timeout_secs: Option<u64>,
}

#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct SpawnSubagentOutput {
    timeis: String,
    status: String,
    session_id: String,
    #[serde(skip)]
    #[schemars(skip)]
    child_session_id: String,
    name: String,
    parent_agent_path: String,
    child_agent_path: String,
    content: String,
    #[serde(skip)]
    #[schemars(skip)]
    task_text: String,
    #[serde(skip)]
    #[schemars(skip)]
    timeout_secs: u64,
    #[serde(skip)]
    #[schemars(skip)]
    parent_session_id: String,
    #[serde(skip)]
    #[schemars(skip)]
    spawn_tools: Vec<String>,
    #[serde(skip)]
    #[schemars(skip)]
    spawn_model: Option<String>,
    #[serde(skip)]
    #[schemars(skip)]
    spawn_base_url: Option<String>,
    #[serde(skip)]
    #[schemars(skip)]
    spawn_max_tokens: Option<u32>,
    #[serde(skip)]
    #[schemars(skip)]
    spawn_ephemeral: bool,
    #[serde(skip)]
    #[schemars(skip)]
    spawn_timeout_secs: u64,
}

impl ToolProjection for SpawnSubagentOutput {
    fn model_blocks(&self) -> Vec<ToolContentBlock> {
        vec![ToolContentBlock::Text {
            text: serde_json::to_string(self).unwrap_or_default(),
        }]
    }

    fn display(&self, _args: &serde_json::Value) -> ToolDisplay {
        ToolDisplay::new(
            qaqh_tool_core::tool_api::ToolHeader::Other {
                label: "subagent".to_string(),
            },
            qaqh_tool_core::tool_api::ToolBody::Subagent {
                name: self.name.clone(),
                session_id: self.session_id.clone(),
            },
        )
        .with_summary(self.content.clone())
    }

    fn effects(&self) -> Vec<qaqh_tool_core::ToolEffect> {
        vec![qaqh_tool_core::ToolEffect::SubagentSpawned {
            session_id: self.session_id.clone(),
            child_session_id: self.child_session_id.clone(),
            name: self.name.clone(),
            task_text: self.task_text.clone(),
            timeout_secs: self.timeout_secs,
            parent_session_id: self.parent_session_id.clone(),
            parent_agent_path: self.parent_agent_path.clone(),
            child_agent_path: self.child_agent_path.clone(),
            spawn_tools: self.spawn_tools.clone(),
            spawn_model: self.spawn_model.clone(),
            spawn_base_url: self.spawn_base_url.clone(),
            spawn_max_tokens: self.spawn_max_tokens,
            spawn_ephemeral: self.spawn_ephemeral,
            spawn_timeout_secs: self.spawn_timeout_secs,
        }]
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ListAgentsArgs {
    /// Absolute or caller-relative AgentPath prefix. Defaults to /root.
    #[serde(default)]
    path_prefix: Option<String>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct ListAgentsOutput {
    agents: Vec<ListedAgent>,
}

impl ListAgentsOutput {
    fn summary_text(&self) -> String {
        format!("{} agent(s)", self.agents.len())
    }
}

impl ToolProjection for ListAgentsOutput {
    fn model_blocks(&self) -> Vec<ToolContentBlock> {
        vec![ToolContentBlock::Text {
            text: serde_json::to_string(self).unwrap_or_default(),
        }]
    }

    fn display(&self, _args: &serde_json::Value) -> ToolDisplay {
        let summary = self.summary_text();
        ToolDisplay::new(
            qaqh_tool_core::tool_api::ToolHeader::Other {
                label: "agents".to_string(),
            },
            qaqh_tool_core::tool_api::ToolBody::Text {
                text: summary.clone(),
                truncated: false,
            },
        )
        .with_summary(summary)
    }
}

pub struct ListAgentsTool;

impl TypedTool for ListAgentsTool {
    type Args = ListAgentsArgs;
    type Output = ListAgentsOutput;

    fn meta(&self) -> ToolMeta {
        ToolMeta::new(
            "list_agents",
            "List logical agents at or below a path prefix in the current root tree. Defaults to /root. Relative prefixes resolve below the caller.",
            qaqh_workspace::permission::ToolCategory::Read,
            ToolRisk::ReadOnly,
            Duration::from_secs(30),
        )
    }

    fn run(
        &self,
        ctx: &ToolCallContext,
        args: ListAgentsArgs,
    ) -> Result<Self::Output, ToolExecutionError> {
        let path_prefix = args
            .path_prefix
            .as_deref()
            .map(str::trim)
            .filter(|prefix| !prefix.is_empty())
            .unwrap_or("/root");
        let host = host().ok_or_else(|| {
            subagent_error(
                "host_unavailable",
                "list_agents: no in-process subagent host installed",
                "Agent discovery requires the daemon host (install_host).",
            )
        })?;
        let agents = host
            .list_agents(&ctx.session_id, path_prefix)
            .map_err(|error| {
                subagent_error(
                    "list_error",
                    format!("list_agents: {error}"),
                    "Check that the path prefix names a valid AgentPath in the current root tree.",
                )
            })?;
        Ok(ListAgentsOutput { agents })
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct AgentMessageArgs {
    /// Absolute AgentPath or caller-relative path of the recipient.
    to: String,
    /// Message body to deliver to the recipient.
    message: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct AgentMessageOutput {
    message_id: String,
    recipient: String,
    delivery: String,
}

impl AgentMessageOutput {
    fn summary_text(&self) -> String {
        match self.delivery.as_str() {
            "queue" => format!("message queued to {}", self.recipient),
            "trigger" => format!("task triggered at {}", self.recipient),
            "interrupt" => format!("interrupt requested for {}", self.recipient),
            other => format!("{other} message for {}", self.recipient),
        }
    }
}

impl ToolProjection for AgentMessageOutput {
    fn model_blocks(&self) -> Vec<ToolContentBlock> {
        vec![ToolContentBlock::Text {
            text: serde_json::to_string(self).unwrap_or_default(),
        }]
    }

    fn display(&self, _args: &serde_json::Value) -> ToolDisplay {
        let summary = self.summary_text();
        ToolDisplay::new(
            qaqh_tool_core::tool_api::ToolHeader::Other {
                label: "agent-message".to_string(),
            },
            qaqh_tool_core::tool_api::ToolBody::Text {
                text: summary.clone(),
                truncated: false,
            },
        )
        .with_summary(summary)
    }
}

pub struct SendMessageTool;

impl TypedTool for SendMessageTool {
    type Args = AgentMessageArgs;
    type Output = AgentMessageOutput;

    fn meta(&self) -> ToolMeta {
        ToolMeta::new(
            "send_message",
            "Queue a canonical message to another agent in the current root tree without starting a turn.",
            qaqh_workspace::permission::ToolCategory::Exec,
            ToolRisk::Administrative,
            Duration::from_secs(30),
        )
    }

    fn run(
        &self,
        ctx: &ToolCallContext,
        args: AgentMessageArgs,
    ) -> Result<Self::Output, ToolExecutionError> {
        handle_agent_message(ctx, args, qaqh_domain::InterAgentDelivery::Queue)
    }
}

pub struct FollowupTaskTool;

impl TypedTool for FollowupTaskTool {
    type Args = AgentMessageArgs;
    type Output = AgentMessageOutput;

    fn meta(&self) -> ToolMeta {
        ToolMeta::new(
            "followup_task",
            "Queue a canonical task to another agent and trigger its turn when idle.",
            qaqh_workspace::permission::ToolCategory::Exec,
            ToolRisk::Administrative,
            Duration::from_secs(30),
        )
    }

    fn run(
        &self,
        ctx: &ToolCallContext,
        args: AgentMessageArgs,
    ) -> Result<Self::Output, ToolExecutionError> {
        handle_agent_message(ctx, args, qaqh_domain::InterAgentDelivery::Trigger)
    }
}

pub struct SteerAgentTool;

impl TypedTool for SteerAgentTool {
    type Args = AgentMessageArgs;
    type Output = AgentMessageOutput;

    fn meta(&self) -> ToolMeta {
        ToolMeta::new(
            "steer_agent",
            "Merge a steering message into the target's current turn at the next safe point. Does not start an idle turn.",
            qaqh_workspace::permission::ToolCategory::Exec,
            ToolRisk::Administrative,
            Duration::from_secs(30),
        )
    }

    fn run(
        &self,
        ctx: &ToolCallContext,
        args: AgentMessageArgs,
    ) -> Result<Self::Output, ToolExecutionError> {
        handle_agent_message(ctx, args, qaqh_domain::InterAgentDelivery::Steer)
    }
}

pub struct InterjectAgentTool;

impl TypedTool for InterjectAgentTool {
    type Args = AgentMessageArgs;
    type Output = AgentMessageOutput;

    fn meta(&self) -> ToolMeta {
        ToolMeta::new(
            "interject_agent",
            "Merge an urgent correction into the target's current turn at the next safe point. Does not cancel the turn or start an idle turn.",
            qaqh_workspace::permission::ToolCategory::Exec,
            ToolRisk::Administrative,
            Duration::from_secs(30),
        )
    }

    fn run(
        &self,
        ctx: &ToolCallContext,
        args: AgentMessageArgs,
    ) -> Result<Self::Output, ToolExecutionError> {
        handle_agent_message(ctx, args, qaqh_domain::InterAgentDelivery::Interject)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WaitAgentArgs {
    /// Timeout in milliseconds. Defaults to 30000.
    #[serde(default)]
    timeout_ms: Option<u64>,
    /// Agent paths or ids to wait on. Omit to wait on the caller's mailbox.
    #[serde(default)]
    targets: Option<Vec<String>>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct WaitAgentOutput {
    message: String,
    timed_out: bool,
    /// `targets` 模式：目标 agent 的当前状态与终态摘要（结果面）。
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    agents: Vec<WaitAgentTarget>,
}

/// `wait_agent(targets=...)` 的单个目标快照。
#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct WaitAgentTarget {
    pub agent_path: String,
    pub status: ListedAgentStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub outcome: Option<AgentOutcome>,
}

impl WaitAgentOutput {
    fn summary_text(&self) -> String {
        self.message.clone()
    }
}

impl ToolProjection for WaitAgentOutput {
    fn model_blocks(&self) -> Vec<ToolContentBlock> {
        vec![ToolContentBlock::Text {
            text: serde_json::to_string(self).unwrap_or_default(),
        }]
    }

    fn display(&self, _args: &serde_json::Value) -> ToolDisplay {
        let summary = self.summary_text();
        ToolDisplay::new(
            qaqh_tool_core::tool_api::ToolHeader::Other {
                label: "agents".to_string(),
            },
            qaqh_tool_core::tool_api::ToolBody::Text {
                text: summary.clone(),
                truncated: false,
            },
        )
        .with_summary(summary)
    }
}

pub struct WaitAgentTool;

impl TypedTool for WaitAgentTool {
    type Args = WaitAgentArgs;
    type Output = WaitAgentOutput;

    fn meta(&self) -> ToolMeta {
        // The typed handler enforces the tighter mailbox wait bound. The
        // descriptor must not kill a valid long wait before it returns.
        ToolMeta::new(
            "wait_agent",
            "Wait for agents to finish. With `targets` (agent paths or ids) the call returns each target's status and terminal outcome (final answer) once every target finished — use this instead of polling. Without `targets` it waits for a mailbox update and returns no message content; queued communications are merged by the runtime at the next turn boundary.",
            qaqh_workspace::permission::ToolCategory::Read,
            ToolRisk::ReadOnly,
            Duration::from_millis(WAIT_AGENT_MAX_TIMEOUT_MS),
        )
    }

    fn run(
        &self,
        ctx: &ToolCallContext,
        args: WaitAgentArgs,
    ) -> Result<Self::Output, ToolExecutionError> {
        let timeout_ms = args.timeout_ms.unwrap_or(WAIT_AGENT_DEFAULT_TIMEOUT_MS);
        if !(WAIT_AGENT_MIN_TIMEOUT_MS..=WAIT_AGENT_MAX_TIMEOUT_MS).contains(&timeout_ms) {
            return Err(subagent_error(
                "invalid_timeout",
                format!(
                    "wait_agent timeout_ms must be between {WAIT_AGENT_MIN_TIMEOUT_MS} and \
                     {WAIT_AGENT_MAX_TIMEOUT_MS}"
                ),
                "Use a bounded wait timeout.",
            ));
        }
        let host = host().ok_or_else(|| {
            subagent_error(
                "host_unavailable",
                "wait_agent requires the in-process subagent host",
                "Check that the daemon installed the subagent host.",
            )
        })?;
        let should_cancel = || ctx.cancellation.is_cancelled();
        let targets = args.targets.unwrap_or_default();
        match host
            .wait_agent(WaitAgentRequest {
                caller_session_id: &ctx.session_id,
                timeout: Duration::from_millis(timeout_ms),
                should_cancel: &should_cancel,
                targets: &targets,
            })
            .map_err(|error| {
                subagent_error(
                    "wait_rejected",
                    format!("wait_agent rejected: {error}"),
                    "Check that the caller has a committed canonical mailbox.",
                )
            })? {
            WaitAgentOutcome::Activity { .. } => {
                let agents = wait_agent_snapshot(&host, &ctx.session_id, &targets);
                Ok(WaitAgentOutput {
                    message: if targets.is_empty() {
                        "Wait completed. Mailbox activity detected.".to_string()
                    } else {
                        format!("Wait completed: {} target(s) finished.", agents.len())
                    },
                    timed_out: false,
                    agents,
                })
            }
            WaitAgentOutcome::TimedOut { .. } => {
                let agents = wait_agent_snapshot(&host, &ctx.session_id, &targets);
                Ok(WaitAgentOutput {
                    message: "Wait timed out.".to_string(),
                    timed_out: true,
                    agents,
                })
            }
            WaitAgentOutcome::Cancelled => Err(subagent_error(
                "wait_cancelled",
                "wait_agent was cancelled",
                "The surrounding turn was cancelled.",
            )),
        }
    }
}

/// `targets` 模式的快照：一次 `list_agents` 读，读失败退化为空快照。
fn wait_agent_snapshot(
    host: &Arc<dyn SubagentHost>,
    caller_session_id: &str,
    targets: &[String],
) -> Vec<WaitAgentTarget> {
    if targets.is_empty() {
        return Vec::new();
    }
    let listed = host
        .list_agents(caller_session_id, "/root")
        .unwrap_or_default();
    wait_targets_from_listing(&listed, targets)
}

/// 把 `targets` 解析成状态快照（目标可以是 agent_path 或 agent_id）。
fn wait_targets_from_listing(listed: &[ListedAgent], targets: &[String]) -> Vec<WaitAgentTarget> {
    if targets.is_empty() {
        return Vec::new();
    }
    targets
        .iter()
        .filter_map(|target| {
            listed
                .iter()
                .find(|agent| agent.agent_path == *target || agent.agent_id == *target)
                .map(|agent| WaitAgentTarget {
                    agent_path: agent.agent_path.clone(),
                    status: agent.status,
                    outcome: agent.outcome.clone(),
                })
        })
        .collect()
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct InterruptAgentArgs {
    /// Absolute AgentPath or caller-relative target.
    target: String,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct InterruptAgentOutput {
    recipient: String,
    previous_status: String,
}

impl InterruptAgentOutput {
    fn summary_text(&self) -> String {
        format!(
            "interrupt requested for {} (was {})",
            self.recipient, self.previous_status
        )
    }
}

impl ToolProjection for InterruptAgentOutput {
    fn model_blocks(&self) -> Vec<ToolContentBlock> {
        vec![ToolContentBlock::Text {
            text: serde_json::to_string(self).unwrap_or_default(),
        }]
    }

    fn display(&self, _args: &serde_json::Value) -> ToolDisplay {
        let summary = self.summary_text();
        ToolDisplay::new(
            qaqh_tool_core::tool_api::ToolHeader::Other {
                label: "agents".to_string(),
            },
            qaqh_tool_core::tool_api::ToolBody::Text {
                text: summary.clone(),
                truncated: false,
            },
        )
        .with_summary(summary)
    }
}

pub struct InterruptAgentTool;

impl TypedTool for InterruptAgentTool {
    type Args = InterruptAgentArgs;
    type Output = InterruptAgentOutput;

    fn meta(&self) -> ToolMeta {
        ToolMeta::new(
            "interrupt_agent",
            "Interrupt an agent's current turn without deleting its identity. The agent remains available for later messages and follow-up tasks.",
            qaqh_workspace::permission::ToolCategory::Exec,
            ToolRisk::Administrative,
            Duration::from_secs(30),
        )
    }

    fn run(
        &self,
        ctx: &ToolCallContext,
        args: InterruptAgentArgs,
    ) -> Result<Self::Output, ToolExecutionError> {
        if args.target.trim().is_empty() {
            return Err(subagent_error(
                "missing_target",
                "interrupt_agent target is required",
                "Provide an AgentPath such as /root/review_code.",
            ));
        }
        let host = host().ok_or_else(|| {
            subagent_error(
                "host_unavailable",
                "interrupt_agent requires the in-process subagent host",
                "Check that the daemon installed the subagent host.",
            )
        })?;
        let interrupted = host
            .interrupt_agent(InterruptAgentRequest {
                caller_session_id: &ctx.session_id,
                target: args.target.trim(),
            })
            .map_err(|error| {
                subagent_error(
                    "interrupt_rejected",
                    format!("interrupt_agent rejected: {error}"),
                    "Root and self interrupts are forbidden; check the target path.",
                )
            })?;
        Ok(InterruptAgentOutput {
            recipient: interrupted.recipient,
            previous_status: interrupted.previous_status,
        })
    }
}

fn handle_agent_message(
    ctx: &ToolCallContext,
    args: AgentMessageArgs,
    delivery: qaqh_domain::InterAgentDelivery,
) -> Result<AgentMessageOutput, ToolExecutionError> {
    if args.to.trim().is_empty() {
        return Err(subagent_error(
            "missing_target",
            "agent message target is required",
            "Provide an AgentPath such as /root/review_code.",
        ));
    }
    if args.message.trim().is_empty() {
        return Err(subagent_error(
            "missing_message",
            "agent message body is required",
            "Provide a non-empty message.",
        ));
    }
    let host = host().ok_or_else(|| {
        subagent_error(
            "host_unavailable",
            "agent messaging requires the in-process subagent host",
            "Check that the daemon installed the subagent host.",
        )
    })?;
    let sent = host
        .send_agent_message(SendAgentMessageRequest {
            caller_session_id: &ctx.session_id,
            target: args.to.trim(),
            text: &args.message,
            delivery,
        })
        .map_err(|error| {
            subagent_error(
                "send_rejected",
                format!("agent message rejected: {error}"),
                "Check the recipient path, root ownership and message size.",
            )
        })?;
    Ok(AgentMessageOutput {
        message_id: sent.message_id,
        recipient: sent.recipient,
        delivery: match sent.delivery {
            qaqh_domain::InterAgentDelivery::Queue => "queue",
            qaqh_domain::InterAgentDelivery::Trigger => "trigger",
            qaqh_domain::InterAgentDelivery::Interrupt => "interrupt",
            qaqh_domain::InterAgentDelivery::Steer => "steer",
            qaqh_domain::InterAgentDelivery::Interject => "interject",
        }
        .to_string(),
    })
}

pub struct SpawnSubagentTool;

impl TypedTool for SpawnSubagentTool {
    type Args = SpawnSubagentArgs;
    type Output = SpawnSubagentOutput;

    fn meta(&self) -> ToolMeta {
        ToolMeta::new(
            "spawn_subagent",
            "Spawn an isolated subagent for a focused task. Returns the child agent path; its final answer is injected as a [SUBAGENT] message when done - do not poll. Track it with list_agents (outcome carries the final answer) and cancel it with interrupt_agent. agent_name = verb+task phrase (e.g. 'explore_task').",
            qaqh_workspace::permission::ToolCategory::Exec,
            ToolRisk::Administrative,
            Duration::from_secs(180),
        )
    }

    fn run(
        &self,
        ctx: &ToolCallContext,
        args: SpawnSubagentArgs,
    ) -> Result<Self::Output, ToolExecutionError> {
        handle_spawn_subagent(ctx, args)
    }
}

pub fn register(mgr: &mut ToolManager) {
    mgr.register_display("spawn_subagent", project_subagent_display);
    mgr.register_typed(SpawnSubagentTool);
    mgr.register_display("list_agents", project_list_agents_display);
    mgr.register_typed(ListAgentsTool);
    mgr.register_display("send_message", project_agent_message_display);
    mgr.register_typed(SendMessageTool);
    mgr.register_display("followup_task", project_agent_message_display);
    mgr.register_typed(FollowupTaskTool);
    mgr.register_display("steer_agent", project_agent_message_display);
    mgr.register_typed(SteerAgentTool);
    mgr.register_display("interject_agent", project_agent_message_display);
    mgr.register_typed(InterjectAgentTool);
    mgr.register_display("wait_agent", project_wait_agent_display);
    mgr.register_typed(WaitAgentTool);
    mgr.register_display("interrupt_agent", project_interrupt_agent_display);
    mgr.register_typed(InterruptAgentTool);
    mgr.register_typed(task_tools::TaskCreateTool);
    mgr.register_typed(task_tools::TaskClaimTool);
    mgr.register_typed(task_tools::TaskUpdateTool);
    mgr.register_typed(task_tools::TaskCloseTool);
    mgr.register_typed(task_tools::TaskListTool);
    mgr.register_typed(board_tools::BoardChannelCreateTool);
    mgr.register_typed(board_tools::BoardThreadCreateTool);
    mgr.register_typed(board_tools::BoardPostTool);
    mgr.register_typed(board_tools::BoardSubscribeTool);
    mgr.register_typed(board_tools::BoardListTool);
}

fn unix_ms() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(i64::MAX as u128) as i64
}

/// 构造子代理任务文本：固定身份提示（`[SYSTEM]`）+ 显式包裹的上下文
/// （`<main_subagent_message>`，防止子代理把传入内容当作自己的 user 消息
/// 而直接动项目）+ 任务（`[TASK]`）。
fn build_subagent_task(task_description: &str, context: &str) -> String {
    let mut parts = vec![format!("[SYSTEM]\n{SUBAGENT_IDENTITY_PROMPT}")];
    if !context.trim().is_empty() {
        parts.push(format!(
            "[CONTEXT]\n<main_subagent_message>\n{}\n</main_subagent_message>",
            context.trim()
        ));
    }
    parts.push(format!("[TASK]\n{}", task_description.trim()));
    parts.join("\n\n")
}

/// 子代理命令/事件传输抽象（PR-4-2：legacy HTTP/SSE 回连已删除，仅宿主直连）。
///
/// - [`HostTransport`]：进程内直连（无 lease / 无 HTTP），由 daemon 装配宿主。
trait SubagentTransport: Send {
    /// 向某 seed 发送命令。返回是否被 accepted。
    fn send_command(&self, session_id: &str, command: RingingCommand) -> Result<bool, String>;
    /// 读取外置大内容。
    fn download_content(&self, session_id: &str, reference: &ContentRef)
    -> Result<Vec<u8>, String>;
    /// 该 seed 的实时事件批次流。
    fn events(&self) -> &mpsc::Receiver<CollectorBatch>;
}

/// 宿主直连传输：直接调用进程内宿主（ActorRegistry + RingingHub）。
struct HostTransport {
    host: Arc<dyn SubagentHost>,
    batch_rx: mpsc::Receiver<CollectorBatch>,
}

impl SubagentTransport for HostTransport {
    fn send_command(&self, session_id: &str, command: RingingCommand) -> Result<bool, String> {
        // SessionClose 由 daemon registry 拦截处理（loop_core 会忽略该命令），
        // 进程内宿主直接执行 close（registry + 临时会话清理），语义一致。
        if matches!(
            &command,
            RingingCommand::Control(qaqh_domain::ControlCommand::SessionClose { .. })
        ) {
            self.host.close(session_id)?;
            return Ok(true);
        }
        self.host.send_ringing(session_id, command)?;
        Ok(true)
    }

    fn download_content(
        &self,
        session_id: &str,
        reference: &ContentRef,
    ) -> Result<Vec<u8>, String> {
        self.host.download_content(session_id, reference)
    }

    fn events(&self) -> &mpsc::Receiver<CollectorBatch> {
        &self.batch_rx
    }
}

fn project_subagent_display(
    args: &serde_json::Value,
    output: &str,
) -> qaqh_tool_core::tool_api::ToolDisplay {
    if let Ok(output) = serde_json::from_str::<SpawnSubagentOutput>(output) {
        return output.display(args);
    }
    let name = args
        .get("agent_name")
        .and_then(|value| value.as_str())
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("sub");
    qaqh_tool_core::tool_api::ToolDisplay::new(
        qaqh_tool_core::tool_api::ToolHeader::Other {
            label: "subagent".to_string(),
        },
        qaqh_tool_core::tool_api::ToolBody::Subagent {
            name: name.to_string(),
            session_id: String::new(),
        },
    )
}

fn project_list_agents_display(
    args: &serde_json::Value,
    output: &str,
) -> qaqh_tool_core::tool_api::ToolDisplay {
    if let Ok(output) = serde_json::from_str::<ListAgentsOutput>(output) {
        return output.display(args);
    }
    qaqh_tool_core::tool_api::ToolDisplay::new(
        qaqh_tool_core::tool_api::ToolHeader::Other {
            label: "agents".to_string(),
        },
        qaqh_tool_core::tool_api::ToolBody::Text {
            text: "0 agent(s)".to_string(),
            truncated: false,
        },
    )
}

fn project_agent_message_display(
    args: &serde_json::Value,
    output: &str,
) -> qaqh_tool_core::tool_api::ToolDisplay {
    if let Ok(output) = serde_json::from_str::<AgentMessageOutput>(output) {
        return output.display(args);
    }
    qaqh_tool_core::tool_api::ToolDisplay::new(
        qaqh_tool_core::tool_api::ToolHeader::Other {
            label: "agent-message".to_string(),
        },
        qaqh_tool_core::tool_api::ToolBody::Text {
            text: "agent message rejected".to_string(),
            truncated: false,
        },
    )
}

fn project_wait_agent_display(
    args: &serde_json::Value,
    output: &str,
) -> qaqh_tool_core::tool_api::ToolDisplay {
    if let Ok(output) = serde_json::from_str::<WaitAgentOutput>(output) {
        return output.display(args);
    }
    qaqh_tool_core::tool_api::ToolDisplay::new(
        qaqh_tool_core::tool_api::ToolHeader::Other {
            label: "agents".to_string(),
        },
        qaqh_tool_core::tool_api::ToolBody::Text {
            text: "wait failed".to_string(),
            truncated: false,
        },
    )
}

fn project_interrupt_agent_display(
    args: &serde_json::Value,
    output: &str,
) -> qaqh_tool_core::tool_api::ToolDisplay {
    if let Ok(output) = serde_json::from_str::<InterruptAgentOutput>(output) {
        return output.display(args);
    }
    qaqh_tool_core::tool_api::ToolDisplay::new(
        qaqh_tool_core::tool_api::ToolHeader::Other {
            label: "agents".to_string(),
        },
        qaqh_tool_core::tool_api::ToolBody::Text {
            text: "interrupt rejected".to_string(),
            truncated: false,
        },
    )
}

fn handle_spawn_subagent(
    ctx: &ToolCallContext,
    args: SpawnSubagentArgs,
) -> Result<SpawnSubagentOutput, ToolExecutionError> {
    let name = args
        .agent_name
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or("sub")
        .to_string();
    validate_agent_name(&name)?;
    let task = args.task_description;
    let context = args.context.unwrap_or_default();

    // 模型面只暴露 4 个参数；工具白名单 / 模型 / base-url / max-tokens /
    // 超时默认值一律取自用户设置（cfg.subagent.*，前端设置页可调），
    // 空值=继承主代理。
    let (tools, model_override, base_url_override, max_tokens, cfg_timeout) =
        qaqh_config::Config::load()
            .ok()
            .map(|cfg| {
                (
                    cfg.subagent.default_tools.clone(),
                    cfg.subagent.model.clone(),
                    cfg.subagent.base_url.clone(),
                    cfg.subagent.max_tokens,
                    cfg.subagent.timeout_secs,
                )
            })
            .unwrap_or_default();
    let timeout_secs = args
        .timeout_secs
        .unwrap_or(cfg_timeout.max(1))
        .clamp(1, 3600);

    if task.trim().is_empty() {
        return Err(subagent_error(
            "missing_task",
            "spawn_subagent: task_description is required",
            "Provide a task description.",
        ));
    }
    let task_text = build_subagent_task(&task, &context);

    // 子代理继承主代理的工作区。workspace 由显式 ToolCallContext 注入，
    // 派生工具线程不再读取线程局部状态。为空/`.` 时不传。
    let parent_workspace = ctx.workspace_root.to_string_lossy();
    let workspace = if parent_workspace.is_empty() || parent_workspace == "." {
        None
    } else {
        Some(parent_workspace.into_owned())
    };
    let model = if model_override.is_empty() {
        None
    } else {
        Some(model_override.as_str())
    };
    let base_url = if base_url_override.is_empty() {
        None
    } else {
        Some(base_url_override.as_str())
    };
    let max_tokens_opt = if max_tokens == 0 || max_tokens == 4096 {
        None
    } else {
        Some(max_tokens)
    };

    // ── 1. Create the child actor only. The task is delivered by the runtime
    // after the canonical `SubagentSpawned` edge is committed. ──
    let host = host().ok_or_else(|| {
        subagent_error(
            "host_unavailable",
            "spawn_subagent: no in-process subagent host installed",
            "Subagent spawning requires the daemon host (install_host).",
        )
    })?;
    let spawned = host
        .spawn_subagent(SpawnSubagentRequest {
            parent_session_id: &ctx.session_id,
            requested_name: &name,
            tools: &tools,
            model,
            base_url,
            max_tokens: max_tokens_opt,
            workspace: workspace.as_deref(),
            // P1-⑤：子代理档位继承父会话，不再回落全局 permission_tier。
            permission_level: ctx.permission_level as u8,
        })
        .map_err(|error| {
            subagent_error(
                "spawn_error",
                format!("spawn_subagent: host rejected spawn: {error}"),
                "Check that the daemon can start subagent actors.",
            )
        })?;
    if spawned.session_id.is_empty() {
        return Err(subagent_error(
            "spawn_error",
            "spawn_subagent: host returned empty seed",
            "Check host/daemon logs.",
        ));
    }
    let session_id = spawned.session_id;
    let child_session_id = spawned.child_session_id;
    let parent_agent_path = spawned.parent_agent_path;
    let child_agent_path = spawned.child_agent_path;

    // ── 2. Hand the child over to the runtime. The collector starts only after
    // the canonical spawn edge is committed; the child is addressed by its
    // canonical ids from here on. ──
    log::info!(
        "[SUBAGENT] '{name}' actor created (seed={session_id}, path={child_agent_path}); awaiting canonical edge"
    );
    let content = format!(
        "Subagent '{name}' spawned at {child_agent_path}; the task starts after the canonical spawn edge is committed. Track it with list_agents; cancel it with interrupt_agent(target=\"{child_agent_path}\")."
    );
    Ok(SpawnSubagentOutput {
        timeis: qaqh_workspace::now_utc8(),
        status: "ok".to_string(),
        session_id,
        child_session_id,
        name,
        parent_agent_path,
        child_agent_path,
        content,
        task_text,
        timeout_secs,
        parent_session_id: ctx.session_id.clone(),
        spawn_tools: tools.clone(),
        spawn_model: model.map(str::to_string),
        spawn_base_url: base_url.map(str::to_string),
        spawn_max_tokens: max_tokens_opt,
        spawn_ephemeral: false,
        spawn_timeout_secs: timeout_secs,
    })
}

fn validate_agent_name(name: &str) -> Result<(), ToolExecutionError> {
    if name.is_empty()
        || name.len() > 64
        || matches!(name, "root" | "." | "..")
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    {
        return Err(subagent_error(
            "invalid_agent_name",
            format!(
                "spawn_subagent: invalid agent_name {name:?}; use lowercase [a-z0-9_] segments"
            ),
            "Use a verb+task name such as 'review_code' or 'explore_task'.",
        ));
    }
    Ok(())
}

fn subagent_error(
    code: &str,
    message: impl Into<String>,
    hint: impl Into<String>,
) -> ToolExecutionError {
    let kind = match code {
        "missing_task" => ToolErrorKind::InvalidArguments,
        "host_unavailable" | "send_error" => ToolErrorKind::Unavailable,
        "send_rejected" | "spawn_error" => ToolErrorKind::Execution,
        _ => ToolErrorKind::Custom,
    };
    let mut error = ToolError::new(kind, message).with_hint(hint);
    error.code = ToolErrorCode::parse_or_builtin(code, kind);
    ToolExecutionError::Recoverable(error)
}

/// Start task delivery and the background result collector.
///
/// The caller must have committed the canonical `SubagentSpawned` edge first.
/// The collector reports the child's terminal outcome through
/// [`SubagentHost::finish_subagent`] keyed by `child_session_id`, so a failed
/// edge commit can abort through [`SubagentHost::abort_subagent`] without
/// leaking a running task.
pub fn start_subagent_collector(
    host: Arc<dyn SubagentHost>,
    request: StartSubagentRequest<'_>,
) -> Result<(), String> {
    let StartSubagentRequest {
        session_id,
        child_session_id,
        name,
        task_text,
        timeout_secs,
        parent_session_id,
        parent_call_id,
        inter_agent,
    } = request;
    let batch_rx = host.subscribe(session_id);
    let transport = Box::new(HostTransport {
        host: host.clone(),
        batch_rx,
    }) as Box<dyn SubagentTransport>;
    let message_id = inter_agent
        .as_ref()
        .map(|envelope| envelope.message_id.clone())
        .unwrap_or_else(|| format!("subagent-task:{session_id}"));
    let completion_route = inter_agent.as_ref().map(|envelope| CompletionRoute {
        root_session_id: envelope.root_session_id.clone(),
        parent_agent_path: envelope.author.clone(),
        child_agent_path: envelope.recipient.clone(),
    });
    let send = RingingCommand::Conversation(ConversationCommand::ConversationSendMessage {
        text: task_text.to_string(),
        images: vec![],
        attachments: None,
        message_id: Some(message_id),
        input_purpose: qaqh_domain::ConversationInputPurpose::TriggerTurn,
        as_system: false,
        inter_agent,
        subagent_terminal: None,
    });
    match transport.send_command(session_id, send) {
        Ok(true) => {}
        Ok(false) => {
            return Err("daemon rejected subagent task send".to_string());
        }
        Err(error) => {
            return Err(format!("send subagent task: {error}"));
        }
    }

    let collector_host = host.clone();
    let session_id = session_id.to_string();
    let child_session_id = child_session_id.to_string();
    let name = name.to_string();
    let parent_session = parent_session_id.to_string();
    let parent_call_id = parent_call_id.to_string();
    std::thread::spawn(move || {
        collect_subagent_result(
            transport,
            collector_host,
            &session_id,
            &child_session_id,
            &name,
            timeout_secs,
            &parent_session,
            &parent_call_id,
            completion_route,
        );
    });
    Ok(())
}

#[derive(Debug, Clone)]
struct CompletionRoute {
    root_session_id: String,
    parent_agent_path: String,
    child_agent_path: String,
}

/// Re-arm result collection before a Trigger delivery to a reloaded child.
///
/// The original collector exits with the previous turn and closes the child.
/// Reload restores the actor, so a new collector must subscribe before the
/// triggering command is written or a fast completion can be missed.
pub fn arm_subagent_collector(
    host: Arc<dyn SubagentHost>,
    request: ArmSubagentCollectorRequest<'_>,
) -> Result<(), String> {
    let batch_rx = host.subscribe(request.session_id);
    let collector_host = host.clone();
    let transport = Box::new(HostTransport { host, batch_rx }) as Box<dyn SubagentTransport>;
    let session_id = request.session_id.to_string();
    let child_session_id = request.child_session_id.to_string();
    let name = request.name.to_string();
    let parent_session = request.parent_session_id.to_string();
    let parent_call_id = request.parent_call_id.to_string();
    let timeout_secs = request.timeout_secs;
    let route = CompletionRoute {
        root_session_id: request.root_session_id.to_string(),
        parent_agent_path: request.parent_agent_path.to_string(),
        child_agent_path: request.child_agent_path.to_string(),
    };
    std::thread::spawn(move || {
        collect_subagent_result(
            transport,
            collector_host,
            &session_id,
            &child_session_id,
            &name,
            timeout_secs,
            &parent_session,
            &parent_call_id,
            Some(route),
        );
    });
    Ok(())
}

/// Background collector: watches the sub-seed's event stream (process-local,
/// direct host connection) until a terminal event or the
/// timeout — mirroring the old stdout-frame collector, but over the Ringing
/// event plane.
/// P1-⑥：终态标签不再整段覆盖已收到的部分答案。超时/失败路径会把
/// `<partial_answer>` 随注入带给父代理；取消路径正文被 T-1-2 压制，但
/// 状态面（`list_agents` 的 `outcome`）仍可取回部分工作成果。
fn terminal_answer_with_partial(tag: String, partial: &str) -> String {
    if partial.trim().is_empty() {
        tag
    } else {
        format!("{tag}\n\n<partial_answer>\n{partial}\n</partial_answer>")
    }
}

#[allow(clippy::too_many_arguments)]
fn collect_subagent_result(
    transport: Box<dyn SubagentTransport>,
    host: Arc<dyn SubagentHost>,
    session_id: &str,
    child_session_id: &str,
    name: &str,
    timeout_secs: u64,
    parent_session: &str,
    parent_call_id: &str,
    completion_route: Option<CompletionRoute>,
) {
    let deadline = Instant::now() + Duration::from_secs(timeout_secs);
    let mut final_answer = String::new();
    let mut exit_code: i32 = 0;
    let mut did_finish = false;
    let mut did_cancel = false;
    let mut terminal = qaqh_domain::SubagentTerminalKind::Completed;
    // 诊断：是否收到过子 seed 的任意事件（用于区分"子代理一开始就死了"
    // 与"中途卡死"——worker 侧 [SUBAGENT-WORKER] 日志 + 落盘开关配合）。
    let mut first_event_logged = false;

    while !did_finish && !did_cancel {
        // P0-①：deadline 检查必须在每轮循环执行。此前只在 recv_timeout
        // 返回 Timeout 时检查——子代理事件持续流出时每个 300ms 轮询都命中
        // Ok(batch) 分支，deadline 永不被评估，timeout_secs 形同虚设
        // （实例：会话 01a13dd7 配置 120s 实跑 847s）。
        if Instant::now() >= deadline {
            log::warn!(
                "[SUBAGENT] '{name}' timeout after {timeout_secs}s (first_event={first_event_logged}) — cancelling sub turn"
            );
            let cancel = RingingCommand::Conversation(
                qaqh_domain::ConversationCommand::ConversationCancel { turn_id: None },
            );
            if let Err(e) = transport.send_command(session_id, cancel) {
                log::warn!("[SUBAGENT] '{name}' timeout cancel send failed: {e}");
            }
            final_answer = terminal_answer_with_partial(
                format!("[SUBAGENT '{name}' TIMEOUT after {timeout_secs}s]"),
                &final_answer,
            );
            exit_code = 1;
            terminal = qaqh_domain::SubagentTerminalKind::TimedOut;
            break;
        }
        match transport.events().recv_timeout(Duration::from_millis(300)) {
            Ok(batch) => {
                if batch.session_id != session_id {
                    continue;
                }
                if !first_event_logged && !batch.events.is_empty() {
                    first_event_logged = true;
                    log::info!(
                        "[SUBAGENT] '{name}' first event received ({} events)",
                        batch.events.len()
                    );
                }
                for event in batch.events {
                    match event {
                        CollectorEvent::AnswerSealed { text, output_ref } => {
                            // Prefer the authoritative full answer; fall back to
                            // externalized content when the body is large.
                            if let Some(answer) = text {
                                if !answer.is_empty() {
                                    final_answer = answer;
                                }
                            } else if let Some(reference) = output_ref
                                && let Ok(bytes) =
                                    transport.download_content(session_id, &reference)
                            {
                                final_answer = String::from_utf8_lossy(&bytes).to_string();
                            }
                        }
                        CollectorEvent::TurnFinished {
                            failed,
                            cancelled,
                            error,
                        } => {
                            if failed {
                                log::warn!("[SUBAGENT] '{name}' turn failed: {error:?}");
                                final_answer = terminal_answer_with_partial(
                                    format!(
                                        "[SUBAGENT '{name}' ERROR] {}",
                                        error.unwrap_or_else(|| "unknown".into())
                                    ),
                                    &final_answer,
                                );
                                exit_code = 1;
                                terminal = qaqh_domain::SubagentTerminalKind::Failed;
                            } else if cancelled {
                                log::info!("[SUBAGENT] '{name}' conversation cancelled");
                                final_answer = terminal_answer_with_partial(
                                    format!("[SUBAGENT '{name}' CANCELLED]"),
                                    &final_answer,
                                );
                                terminal = qaqh_domain::SubagentTerminalKind::Cancelled;
                                did_cancel = true;
                            } else {
                                log::info!("[SUBAGENT] '{name}' turn completed");
                            }
                            did_finish = true;
                        }
                    }
                    if did_finish || did_cancel {
                        break;
                    }
                }
            }
            Err(mpsc::RecvTimeoutError::Timeout) => {
                // 本 300ms 无事件；deadline 已在循环顶部统一检查。
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                // Event stream closed before a terminal event (daemon gone?).
                log::warn!(
                    "[SUBAGENT] '{name}' event stream closed, partial answer_len={}",
                    final_answer.len()
                );
                terminal = qaqh_domain::SubagentTerminalKind::Failed;
                did_finish = true;
            }
        }
    }

    let answer_len = final_answer.len();

    // P0-②：先落**状态面**再注入。注入可能全部被拒（父会话忙碌 / lease 竞争），
    // 此时 `list_agents` 是父代理唯一可自助取回结果的通道，必须在任何注入
    // 尝试之前就绪。结果面挂在 agent 上（child_session_id 为键），不再借用
    // exec 的进程注册表。
    host.finish_subagent(child_session_id, &final_answer, exit_code);

    // ── 结果回传：注入主代理会话。 ──
    // 主代理 idle 时这条消息触发新回合（模型自动看到子代理结果并继续）；
    // 主代理仍在运行中则进入回合 lap 边界的见缝插针通道。注入被 daemon 拒绝
    // （lease/compact 等）时重试一次并告警，避免静默丢失。
    let (state_tag, header) = match terminal {
        qaqh_domain::SubagentTerminalKind::Completed => {
            ("completed", format!("subagent '{name}' completed"))
        }
        qaqh_domain::SubagentTerminalKind::Failed => (
            "error",
            format!("subagent '{name}' failed (exit={exit_code})"),
        ),
        qaqh_domain::SubagentTerminalKind::Cancelled => {
            ("cancelled", format!("subagent '{name}' cancelled"))
        }
        qaqh_domain::SubagentTerminalKind::TimedOut => {
            ("timeout", format!("subagent '{name}' timed out"))
        }
    };
    // T-1-2：被取消的子代理不得把结果注入父会话。取消是终态——父会话可能
    // 正是被用户取消（或本 collector 的 kill/timeout 路径）而停下的，注入会
    // 触发一个 `TurnStart` 把已判定 cancel 的会话复活，并把一段早已作废的
    // `final_answer` 当作模型输入。仅留日志痕迹，不注入、不留存正文。
    if did_cancel {
        log::info!(
            "[SUBAGENT] '{name}' cancelled — result body suppressed (answer_len={answer_len}); terminal edge will still be reported"
        );
    }
    if !parent_session.is_empty() {
        let terminal_notification = qaqh_domain::SubagentTerminalNotification {
            child_session_id: child_session_id.to_string(),
            parent_call_id: parent_call_id.to_string(),
            terminal,
        };
        let completion_message_id = if did_cancel {
            format!("subagent-terminal:{session_id}")
        } else {
            format!("subagent-result:{session_id}")
        };
        let completion_inter_agent =
            completion_route
                .as_ref()
                .map(|route| qaqh_domain::InterAgentEnvelope {
                    message_id: completion_message_id.clone(),
                    root_session_id: route.root_session_id.clone(),
                    author: route.child_agent_path.clone(),
                    recipient: route.parent_agent_path.clone(),
                    other_recipients: vec![],
                    task_id: None,
                    reply_to: None,
                    causation_id: None,
                    delivery: qaqh_domain::InterAgentDelivery::Queue,
                    created_at_ms: unix_ms(),
                });
        let v2_delivery = completion_inter_agent.is_some();
        let inject = RingingCommand::Conversation(
            qaqh_domain::ConversationCommand::ConversationSendMessage {
                text: if did_cancel {
                    String::new()
                } else {
                    format!(
                        "<qaqh_subagent_result name=\"{name}\" state=\"{state_tag}\" exit=\"{exit_code}\">\n{header}\n{final_answer}\n</qaqh_subagent_result>"
                    )
                },
                images: vec![],
                attachments: None,
                message_id: Some(completion_message_id),
                input_purpose: if did_cancel || v2_delivery {
                    qaqh_domain::ConversationInputPurpose::QueueOnly
                } else {
                    qaqh_domain::ConversationInputPurpose::TriggerTurn
                },
                // V2 results are attributed inter-agent communications and are
                // queue-only. The legacy fallback keeps its system injection
                // shape until all callers carry a canonical route.
                as_system: !v2_delivery,
                inter_agent: completion_inter_agent,
                subagent_terminal: Some(terminal_notification),
            },
        );
        let mut accepted = false;
        let mut last_rejected: Option<String> = None;
        const INJECT_ATTEMPTS: usize = 5;
        for attempt in 0..INJECT_ATTEMPTS {
            if attempt > 0 {
                // 线性退避：300ms → 600ms → 1200ms → 2400ms
                std::thread::sleep(std::time::Duration::from_millis(
                    300 * (1 << attempt.min(3)),
                ));
            }
            match transport.send_command(parent_session, inject.clone()) {
                Ok(true) => {
                    accepted = true;
                    break;
                }
                other => {
                    last_rejected = Some(format!("{other:?}"));
                    log::warn!(
                        "[SUBAGENT] '{name}' inject attempt {} not accepted: {:?}",
                        attempt + 1,
                        last_rejected
                    );
                }
            }
        }
        if accepted {
            log::info!("[SUBAGENT] '{name}' inject accepted ({} bytes)", answer_len);
        } else {
            // P0-② 兜底：原 purpose（TriggerTurn 在父会话忙碌时被拒）失败后，
            // 改用 QueueOnly 重投——不触发新回合，仅在下一轮 lap 边界合并进
            // 父会话。message_id 不变（幂等去重）；非取消场景在正文末尾追加
            // 结果指针（子会话 id），状态面此时已 record 可查。
            let mut fallback = inject.clone();
            if let RingingCommand::Conversation(
                qaqh_domain::ConversationCommand::ConversationSendMessage {
                    input_purpose,
                    text,
                    ..
                },
            ) = &mut fallback
            {
                *input_purpose = qaqh_domain::ConversationInputPurpose::QueueOnly;
                if !did_cancel && !text.is_empty() {
                    text.push_str(&format!(
                        "\n[result-pointer] child session {child_session_id} · read it with list_agents"
                    ));
                }
            }
            const FALLBACK_ATTEMPTS: usize = 3;
            for attempt in 0..FALLBACK_ATTEMPTS {
                if attempt > 0 {
                    // 线性退避：300ms → 600ms → 1200ms
                    std::thread::sleep(std::time::Duration::from_millis(
                        300 * (1 << attempt.min(2)),
                    ));
                }
                match transport.send_command(parent_session, fallback.clone()) {
                    Ok(true) => {
                        accepted = true;
                        log::info!(
                            "[SUBAGENT] '{name}' queue-only fallback accepted ({} bytes) after primary inject rejected: {:?}",
                            answer_len,
                            last_rejected
                        );
                        break;
                    }
                    other => {
                        last_rejected = Some(format!("{other:?}"));
                    }
                }
            }
            if !accepted {
                log::error!(
                    "[SUBAGENT] '{name}' inject FAILED after {INJECT_ATTEMPTS}+{FALLBACK_ATTEMPTS} attempts; result recoverable via list_agents (child session {child_session_id}): {:?}",
                    last_rejected
                );
            }
        }
    }

    // ── 自动卸载：终态后关闭子 agent（actor / worker 进程），释放后台资源。──
    // SessionClose 语义由进程内宿主直接执行（registry.close + 临时会话清理）。
    // 失败仅告警：结果已注入主会话 + 终态已回写注册表，残留不丢数据。
    if let Err(e) = transport.send_command(
        session_id,
        RingingCommand::Control(qaqh_domain::ControlCommand::SessionClose {
            session_id: session_id.to_string(),
        }),
    ) {
        log::warn!("[SUBAGENT] '{name}' close worker {session_id} failed: {e}");
    } else {
        log::info!("[SUBAGENT] '{name}' sub agent {session_id} closed (auto-unload)");
    }

    log::info!(
        "[SUBAGENT] '{name}' collector complete (seed={session_id}), answer_len={answer_len}, exit={exit_code}, cancelled={did_cancel}, first_event={first_event_logged}"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawn_subagent_schema_never_accepts_api_keys() {
        let mut manager = ToolManager::new();
        register(&mut manager);

        let handler = manager
            .lookup("spawn_subagent")
            .expect("spawn_subagent should be registered");
        let properties = handler.input_schema["properties"]
            .as_object()
            .expect("tool properties should be an object");

        assert!(!properties.contains_key("api_key"));
        assert!(!handler.input_schema.to_string().contains("--api-key"));
    }

    #[test]
    fn list_agents_schema_is_registered_and_path_prefix_is_optional() {
        let mut manager = ToolManager::new();
        register(&mut manager);

        let handler = manager
            .lookup("list_agents")
            .expect("list_agents should be registered");
        assert_eq!(handler.input_schema["type"], "object");
        assert!(
            handler.input_schema["properties"]
                .as_object()
                .expect("properties")
                .contains_key("path_prefix")
        );
        assert!(
            handler.input_schema.get("required").is_none(),
            "path_prefix must default to /root"
        );
    }

    #[test]
    fn agent_message_tools_are_registered_with_required_fields() {
        let mut manager = ToolManager::new();
        register(&mut manager);

        for name in ["send_message", "followup_task"] {
            let handler = manager.lookup(name).expect("agent message tool registered");
            let properties = handler.input_schema["properties"]
                .as_object()
                .expect("properties");
            assert!(properties.contains_key("to"));
            assert!(properties.contains_key("message"));
            assert_eq!(
                handler.input_schema["required"].as_array().map(Vec::len),
                Some(2)
            );
        }
    }

    #[test]
    fn wait_agent_schema_is_registered_with_optional_timeout() {
        let mut manager = ToolManager::new();
        register(&mut manager);

        let handler = manager
            .lookup("wait_agent")
            .expect("wait_agent should be registered");
        assert!(
            handler.input_schema["properties"]
                .as_object()
                .expect("properties")
                .contains_key("timeout_ms")
        );
        assert!(
            handler.input_schema.get("required").is_none(),
            "timeout_ms must default to the bounded 30s wait"
        );
    }

    #[test]
    fn interrupt_agent_schema_requires_target() {
        let mut manager = ToolManager::new();
        register(&mut manager);

        assert!(
            manager.lookup("close_agent").is_none(),
            "V2 must not expose the legacy close_agent delete tool"
        );
        let handler = manager
            .lookup("interrupt_agent")
            .expect("interrupt_agent should be registered");
        assert!(
            handler.input_schema["properties"]
                .as_object()
                .expect("properties")
                .contains_key("target")
        );
        assert_eq!(
            handler.input_schema["required"].as_array().map(Vec::len),
            Some(1)
        );
    }

    #[test]
    fn task_tools_are_registered_with_closed_schemas() {
        let mut manager = ToolManager::new();
        register(&mut manager);

        for name in [
            "task_create",
            "task_claim",
            "task_update",
            "task_close",
            "task_list",
            "board_channel_create",
            "board_thread_create",
            "board_post",
            "board_subscribe",
            "board_list",
            "steer_agent",
            "interject_agent",
        ] {
            let handler = manager
                .lookup(name)
                .unwrap_or_else(|| panic!("{name} should be registered"));
            assert_eq!(
                handler.input_schema["additionalProperties"],
                serde_json::json!(false),
                "{name} must reject unknown fields"
            );
        }
        assert!(
            manager.lookup("task_list").is_some(),
            "task_list read surface must be registered"
        );
    }

    #[test]
    fn spawn_subagent_schema_exposes_only_the_llm_facing_params() {
        let mut manager = ToolManager::new();
        register(&mut manager);
        let handler = manager
            .lookup("spawn_subagent")
            .expect("spawn_subagent should be registered");
        let properties = handler.input_schema["properties"]
            .as_object()
            .expect("tool properties should be an object");

        // 模型面只有 4 个参数；system_prompt / tools / model / base_url /
        // max_tokens 不再暴露（由设置页配置或内置身份提示提供）。
        let expected: Vec<&str> = vec!["task_description", "agent_name", "context", "timeout_secs"];
        assert_eq!(properties.len(), expected.len());
        for key in expected {
            assert!(properties.contains_key(key), "missing {key}");
        }
        for legacy in [
            "system_prompt",
            "tools",
            "model",
            "base_url",
            "max_tokens",
            "name",
            "task",
        ] {
            assert!(
                !properties.contains_key(legacy),
                "{legacy} must not be exposed"
            );
        }
        assert_eq!(
            handler.input_schema["required"][0], "task_description",
            "task_description must be the only required param"
        );
    }

    #[test]
    fn task_builder_injects_identity_and_wraps_context() {
        // 无 context：身份提示 + 任务，无 context 段。
        let bare = build_subagent_task("do the thing", "");
        assert!(bare.contains("[SYSTEM]"));
        assert!(bare.contains(SUBAGENT_IDENTITY_PROMPT));
        assert!(bare.contains("[TASK]\ndo the thing"));
        assert!(!bare.contains("[CONTEXT]"));

        // 有 context：必须包裹在 <main_subagent_message> 内，防止子代理把
        // 传入内容误判为自身 user 消息而直接修改项目。
        let with_ctx = build_subagent_task("review the diff", "repo at F:\\proj\nbranch main");
        assert!(with_ctx.contains(
            "<main_subagent_message>\nrepo at F:\\proj\nbranch main\n</main_subagent_message>"
        ));
        assert!(with_ctx.contains("[TASK]\nreview the diff"));
        // 身份提示在前，任务在后。
        assert!(with_ctx.find("[SYSTEM]").unwrap() < with_ctx.find("[TASK]").unwrap());
    }

    #[test]
    fn typed_output_model_and_display_share_the_same_payload() {
        let output = SpawnSubagentOutput {
            timeis: "UTC+8 2026-09-23 12:00".to_string(),
            status: "ok".to_string(),
            session_id: "sub-seed".to_string(),
            child_session_id: "0198f1a0-0000-7000-8000-000000000003".to_string(),
            name: "review_code".to_string(),
            parent_agent_path: "/root".to_string(),
            child_agent_path: "/root/review_code".to_string(),
            content: "Subagent 'review_code' spawned at /root/review_code".to_string(),
            task_text: "[TASK]\nreview".to_string(),
            timeout_secs: 120,
            parent_session_id: "root-seed".to_string(),
            spawn_tools: vec![],
            spawn_model: None,
            spawn_base_url: None,
            spawn_max_tokens: None,
            spawn_ephemeral: false,
            spawn_timeout_secs: 120,
        };
        let model = match output.model_blocks().into_iter().next() {
            Some(ToolContentBlock::Text { text }) => text,
            _ => panic!("subagent output must have a text model block"),
        };
        assert!(
            !model.contains("process_id"),
            "subagent must not expose an exec process handle: {model}"
        );
        assert!(model.contains("\"child_agent_path\":\"/root/review_code\""));
        assert!(model.contains("\"session_id\":\"sub-seed\""));
        assert!(model.contains("\"content\":\"Subagent 'review_code' spawned"));
        assert!(
            !model.contains("child_session_id"),
            "canonical child identity is an internal effect, not model-visible JSON"
        );
        let display = output.display(&serde_json::json!({}));
        assert_eq!(
            display.summary.as_deref(),
            Some("Subagent 'review_code' spawned at /root/review_code")
        );
        match display.body {
            qaqh_tool_core::tool_api::ToolBody::Subagent { name, session_id } => {
                assert_eq!(name, "review_code");
                assert_eq!(session_id, "sub-seed");
            }
            other => panic!("unexpected subagent display body: {other:?}"),
        }
    }

    #[test]
    fn missing_task_keeps_the_legacy_error_code() {
        let ctx = ToolCallContext {
            call_id: "call-subagent".to_string(),
            session_id: "parent-seed".to_string(),
            workspace_root: std::path::PathBuf::from("/tmp/workspace"),
            mode: qaqh_tool_core::tool_api::AgentMode::Code,
            permission_level: qaqh_workspace::permission::PermissionLevel::SkipPermissions,
            sandbox: qaqh_tool_core::tool_api::SandboxMode::Main,
            sandbox_spec: qaqh_tool_core::tool_api::SandboxSpec::workspace_write(
                std::path::PathBuf::from("/tmp/workspace"),
            ),
            exec_default_shell: None,
            timeout: Duration::from_secs(180),
            cancellation: qaqh_tool_core::tool_api::CancellationToken::new(),
            progress: None,
            exec_progress: None,
            official_web: None,
            source: qaqh_tool_core::tool_api::ToolCallSource::Model,
        };
        let error = SpawnSubagentTool
            .run(
                &ctx,
                SpawnSubagentArgs {
                    task_description: "   ".to_string(),
                    agent_name: None,
                    context: None,
                    timeout_secs: None,
                },
            )
            .expect_err("empty task must fail before host lookup");
        match error {
            ToolExecutionError::Recoverable(error) => {
                assert_eq!(error.code.as_str(), "missing_task");
            }
            ToolExecutionError::Fatal(error) => {
                panic!(
                    "missing task must be recoverable, got fatal: {}",
                    error.message
                )
            }
        }
    }

    // ── T-1-2：取消后不得注入父会话 ──────────────────────────────────────

    /// 记录投递命令的 mock 传输：事件流由测试预置（这里只放一条
    /// `ConversationCancelled`，让 collector 立即进取消终态）。
    struct RecordingTransport {
        batch_rx: mpsc::Receiver<CollectorBatch>,
        sent: Arc<std::sync::Mutex<Vec<(String, RingingCommand)>>>,
    }

    impl SubagentTransport for RecordingTransport {
        fn send_command(&self, session_id: &str, command: RingingCommand) -> Result<bool, String> {
            self.sent
                .lock()
                .expect("test mutex must not be poisoned")
                .push((session_id.to_string(), command));
            Ok(true)
        }

        fn download_content(
            &self,
            _session: &str,
            _reference: &ContentRef,
        ) -> Result<Vec<u8>, String> {
            Err("no externalized content in this test".into())
        }

        fn events(&self) -> &mpsc::Receiver<CollectorBatch> {
            &self.batch_rx
        }
    }

    #[derive(Default)]
    struct CollectorTestHost {
        sent: Arc<std::sync::Mutex<Vec<(String, RingingCommand)>>>,
        event_tx: std::sync::Mutex<Option<mpsc::Sender<CollectorBatch>>>,
        /// `finish_subagent` 记录：(child_session_id, answer, exit_code)。
        finished: Arc<std::sync::Mutex<Vec<(String, String, i32)>>>,
    }

    impl SubagentHost for CollectorTestHost {
        fn spawn_subagent(
            &self,
            _request: SpawnSubagentRequest<'_>,
        ) -> Result<SpawnedSubagent, String> {
            Err("not used".to_string())
        }

        fn list_agents(
            &self,
            _caller_session_id: &str,
            _path_prefix: &str,
        ) -> Result<Vec<ListedAgent>, String> {
            Ok(Vec::new())
        }

        fn start_subagent(&self, _request: StartSubagentRequest<'_>) -> Result<(), String> {
            Err("not used".to_string())
        }

        fn send_agent_message(
            &self,
            _request: SendAgentMessageRequest<'_>,
        ) -> Result<SentAgentMessage, String> {
            Err("not used".to_string())
        }

        fn wait_agent(&self, _request: WaitAgentRequest<'_>) -> Result<WaitAgentOutcome, String> {
            Ok(WaitAgentOutcome::TimedOut {
                activity_fact_seq: 0,
            })
        }

        fn interrupt_agent(
            &self,
            _request: InterruptAgentRequest<'_>,
        ) -> Result<InterruptedAgent, String> {
            Err("not used".to_string())
        }

        fn rollback_subagent(&self, _session: &str, _child_session_id: &str) {}

        fn abort_subagent(&self, _session: &str) {}

        fn finish_subagent(&self, child_session_id: &str, answer: &str, exit_code: i32) {
            self.finished
                .lock()
                .expect("test mutex must not be poisoned")
                .push((child_session_id.to_string(), answer.to_string(), exit_code));
        }

        fn send_ringing(&self, session_id: &str, command: RingingCommand) -> Result<(), String> {
            self.sent
                .lock()
                .expect("test mutex must not be poisoned")
                .push((session_id.to_string(), command));
            Ok(())
        }

        fn subscribe(&self, _session: &str) -> mpsc::Receiver<CollectorBatch> {
            let (tx, rx) = mpsc::channel();
            *self
                .event_tx
                .lock()
                .expect("test mutex must not be poisoned") = Some(tx);
            rx
        }

        fn download_content(
            &self,
            _session: &str,
            _reference: &ContentRef,
        ) -> Result<Vec<u8>, String> {
            Err("not used".to_string())
        }

        fn close(&self, _session: &str) -> Result<(), String> {
            Ok(())
        }
    }

    fn cancelled_batch(session_id: &str) -> CollectorBatch {
        CollectorBatch {
            session_id: session_id.to_string(),
            events: vec![CollectorEvent::TurnFinished {
                failed: false,
                cancelled: true,
                error: None,
            }],
        }
    }

    fn completed_batch(session_id: &str) -> CollectorBatch {
        CollectorBatch {
            session_id: session_id.to_string(),
            events: vec![CollectorEvent::TurnFinished {
                failed: false,
                cancelled: false,
                error: None,
            }],
        }
    }

    /// T-1-2 回归：collector 收到 `ConversationCancelled` 后**不得**把
    /// `final_answer` 注入父会话。未修复时本测试红：仍向父 seed 发
    /// `ConversationSendMessage { as_system: true }`，父会话被重新开回合
    /// （TurnStart）——「已判定 cancel 的子代理复活」的第二段根因。
    #[test]
    fn cancelled_collector_does_not_inject() {
        let child = "sub-cancel-inject-child";
        let parent = "sub-cancel-inject-parent";
        let (tx, rx) = mpsc::channel::<CollectorBatch>();
        tx.send(cancelled_batch(child))
            .expect("test channel must not fail");

        let sent: Arc<std::sync::Mutex<Vec<(String, RingingCommand)>>> = Arc::default();
        let transport = Box::new(RecordingTransport {
            batch_rx: rx,
            sent: Arc::clone(&sent),
        });
        let host = Arc::new(CollectorTestHost::default());

        collect_subagent_result(
            transport,
            host.clone(),
            child,
            "0198f1a0-0000-7000-8000-000000000003",
            "cancelled_task",
            5,
            parent,
            "call_01J00000000000000000000000",
            None,
        );

        // 取消路径的结果仍落在状态面（正文被 T-1-2 压制为标签）。
        let finished = host
            .finished
            .lock()
            .expect("test mutex must not be poisoned")
            .clone();
        assert_eq!(finished.len(), 1, "collector must report one outcome");
        assert_eq!(finished[0].0, "0198f1a0-0000-7000-8000-000000000003");
        assert!(
            finished[0].1.contains("CANCELLED"),
            "outcome must carry the cancelled tag, got {:?}",
            finished[0].1
        );

        let sent = sent.lock().expect("test mutex must not be poisoned");
        let injected: Vec<_> = sent
            .iter()
            .filter(|(session_id, _)| session_id == parent)
            .collect();
        assert_eq!(
            injected.len(),
            1,
            "cancelled child must send exactly one terminal notification"
        );
        assert!(
            matches!(
                &injected[0].1,
                RingingCommand::Conversation(
                    qaqh_domain::ConversationCommand::ConversationSendMessage {
                        text,
                        subagent_terminal: Some(terminal),
                        ..
                    }
                ) if text.is_empty()
                    && terminal.terminal == qaqh_domain::SubagentTerminalKind::Cancelled
                    && terminal.child_session_id
                        == "0198f1a0-0000-7000-8000-000000000003"
            ),
            "cancelled child must send an empty structured terminal notification, got {:?}",
            injected[0].1
        );
        assert!(
            sent.iter().any(|(session_id, command)| {
                session_id == child
                    && matches!(
                        command,
                        RingingCommand::Control(qaqh_domain::ControlCommand::SessionClose { .. })
                    )
            }),
            "子 worker 的自动卸载（SessionClose）不受抑制影响，实测: {sent:?}"
        );
    }

    #[test]
    fn completed_collector_delivers_queue_only_inter_agent_result() {
        let child = "0198f1a0-0000-7000-8000-000000000004";
        let parent = "sub-complete-parent";
        let (tx, rx) = mpsc::channel::<CollectorBatch>();
        tx.send(completed_batch(child))
            .expect("test channel must not fail");

        let sent: Arc<std::sync::Mutex<Vec<(String, RingingCommand)>>> = Arc::default();
        let transport = Box::new(RecordingTransport {
            batch_rx: rx,
            sent: Arc::clone(&sent),
        });
        let host = Arc::new(CollectorTestHost::default());

        collect_subagent_result(
            transport,
            host.clone(),
            child,
            child,
            "complete_task",
            5,
            parent,
            "call_01J00000000000000000000001",
            Some(CompletionRoute {
                root_session_id: "root-session".to_string(),
                parent_agent_path: "/root".to_string(),
                child_agent_path: "/root/complete_task".to_string(),
            }),
        );

        let finished = host
            .finished
            .lock()
            .expect("test mutex must not be poisoned")
            .clone();
        assert_eq!(finished.len(), 1, "collector must report one outcome");
        assert_eq!(finished[0].0, child, "outcome is keyed by child_session_id");
        assert_eq!(finished[0].2, 0, "completed child reports exit code 0");

        let sent = sent.lock().expect("test mutex must not be poisoned");
        let injected = sent
            .iter()
            .find(|(session_id, _)| session_id == parent)
            .expect("completion must be delivered to the parent");
        let RingingCommand::Conversation(ConversationCommand::ConversationSendMessage {
            message_id,
            input_purpose,
            as_system,
            inter_agent: Some(envelope),
            subagent_terminal: Some(terminal),
            ..
        }) = &injected.1
        else {
            panic!("unexpected completion command: {:?}", injected.1);
        };
        assert_eq!(
            *input_purpose,
            qaqh_domain::ConversationInputPurpose::QueueOnly
        );
        assert!(
            !*as_system,
            "V2 completion must use inter-agent attribution"
        );
        assert_eq!(envelope.delivery, qaqh_domain::InterAgentDelivery::Queue);
        assert_eq!(envelope.author, "/root/complete_task");
        assert_eq!(envelope.recipient, "/root");
        assert_eq!(message_id.as_deref(), Some(envelope.message_id.as_str()));
        assert_eq!(terminal.child_session_id, child);
    }

    #[test]
    fn reload_collector_routes_completion_before_trigger_delivery() {
        let host = Arc::new(CollectorTestHost::default());
        arm_subagent_collector(
            host.clone(),
            ArmSubagentCollectorRequest {
                session_id: "reload-child",
                child_session_id: "reload-child",
                name: "reload_task",
                parent_session_id: "reload-parent",
                parent_call_id: "call_01J00000000000000000000002",
                timeout_secs: 5,
                root_session_id: "reload-root",
                parent_agent_path: "/root",
                child_agent_path: "/root/reload_task",
            },
        )
        .expect("arm reload collector");
        let event_tx = host
            .event_tx
            .lock()
            .expect("test mutex must not be poisoned")
            .take()
            .expect("collector subscription");
        event_tx
            .send(completed_batch("reload-child"))
            .expect("send completion event");

        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let sent = host.sent.lock().expect("test mutex must not be poisoned");
            if sent.iter().any(|(session_id, command)| {
                session_id == "reload-parent"
                    && matches!(
                        command,
                        RingingCommand::Conversation(
                            ConversationCommand::ConversationSendMessage {
                                input_purpose: qaqh_domain::ConversationInputPurpose::QueueOnly,
                                inter_agent: Some(envelope),
                                ..
                            }
                        ) if envelope.author == "/root/reload_task"
                            && envelope.recipient == "/root"
                    )
            }) {
                break;
            }
            drop(sent);
            assert!(
                Instant::now() < deadline,
                "reloaded collector did not route queue-only completion"
            );
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    fn listed_agent(path: &str, id: &str, answer: Option<&str>) -> ListedAgent {
        ListedAgent {
            root_session_id: "root-session".to_string(),
            agent_id: id.to_string(),
            agent_path: path.to_string(),
            parent_agent_path: Some("/root".to_string()),
            nickname: None,
            role: None,
            status: ListedAgentStatus::Completed,
            residency: ListedAgentResidency::Unloaded,
            created_at_ms: 0,
            outcome: answer.map(|answer| AgentOutcome {
                answer: Some(answer.to_string()),
                exit_code: 0,
            }),
        }
    }

    /// P2：`wait_agent(targets=...)` 的目标既可用路径也可用 id 寻址，
    /// 且快照带上状态面的终态摘要（替代 `process check`）。
    #[test]
    fn wait_targets_resolve_by_path_or_id_and_carry_the_outcome() {
        let listed = vec![
            listed_agent("/root/review", "child-a", Some("done")),
            listed_agent("/root/explore", "child-b", None),
        ];

        let by_path = wait_targets_from_listing(&listed, &["/root/review".to_string()]);
        assert_eq!(by_path.len(), 1);
        assert_eq!(by_path[0].agent_path, "/root/review");
        assert_eq!(by_path[0].status, ListedAgentStatus::Completed);
        assert_eq!(
            by_path[0]
                .outcome
                .as_ref()
                .and_then(|outcome| outcome.answer.as_deref()),
            Some("done")
        );

        let by_id = wait_targets_from_listing(
            &listed,
            &["child-b".to_string(), "/root/missing".to_string()],
        );
        assert_eq!(by_id.len(), 1, "未知目标被忽略");
        assert_eq!(by_id[0].agent_path, "/root/explore");
        assert!(by_id[0].outcome.is_none(), "未结束的目标没有终态摘要");
    }
}
