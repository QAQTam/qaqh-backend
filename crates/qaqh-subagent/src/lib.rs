//! qaqh-subagent — 模型可控的新对话（子代理）工具面。
//!
//! ADR-0014：子代理**就是会话**——与普通会话同一条装配路径，血缘
//! （`parent_session_id`）、工具白名单与 Subagent sandbox 角色由会话行承载。
//!
//! 1. `spawn_subagent` 经进程内 [`SubagentHost`] 创建子会话并立即返回；
//!    任务以普通 inter-agent `ConversationSendMessage` 投递。
//! 2. 子 loop 回合终态经 registry 的终态路由回传 canonical 结果消息到父
//!    会话持久 mailbox（父 idle 开新回合、忙则回合边界合流）。没有
//!    collector 线程，没有重试循环。
//! 3. ADR-0018：子代理=会话，生命周期预算与父同律（上下文压力 → handoff）。
//!    独立的 120s 墙钟超时清扫已退役；模型选择继承父会话行，工具面与父
//!    会话同律（STANDARD），不再有配置面白名单/超时旋钮。
//!
//! ## Registration
//!
//! ADR-0014 切片 D：最小工具集（spawn_subagent / send_message / wait_agent /
//! list_agents / interrupt_agent）经 `SpawnInput.tool_registrars` 进入每个
//! 会话。切片 E：协作工具面（task_*/board_*，`register_collaboration_tools`）
//! 同路进入每个会话——任务板按根树键控，lead 建任务、子级认领干活。

#![cfg_attr(not(test), forbid(unsafe_code))]
#![allow(clippy::result_large_err)] // TypedTool's frozen public error boundary.

use std::sync::Arc;
use std::time::Duration;

use qaqh_ringing_contract::fact::InterAgentDelivery;
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
    AgentOutcome, BoardChannel, BoardChannelCreateRequest, BoardHost,
    BoardListRequest, BoardNotificationSkip, BoardPost, BoardPostOutcome, BoardPostRequest,
    BoardSnapshot, BoardSubscription, BoardSubscriptionAction, BoardSubscriptionRequest,
    BoardSubscriptionTarget, BoardSubscriptionTargetKind, BoardThread, BoardThreadCreateRequest,
    InterruptAgentRequest,
    InterruptedAgent, ListedAgent, ListedAgentResidency, ListedAgentStatus,
    SendAgentMessageRequest, SentAgentMessage, SpawnSubagentRequest, SpawnedSubagent,
    SubagentHost, TaskBoardArtifact, TaskBoardHost, TaskBoardTask,
    TaskClaimAction, TaskClaimRequest, TaskCloseAction, TaskCloseRequest, TaskCreateRequest,
    TaskListRequest, TaskUpdateAction, TaskUpdateRequest, WaitAgentOutcome, WaitAgentRequest,
    board_host, host, install_board_host, install_host, install_task_host, task_host,
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
    /// Context inheritance: fresh (default) = child does not see this
    /// conversation; fork = the parent's completed turns are seeded into the
    /// child's log (the current in-flight turn is not included).
    #[serde(default)]
    mode: Option<SpawnMode>,
}

/// ADR-0014 切片G：子代理上下文继承模式（dsh subagent/subagent_fork 同语义，
/// 本仓收进单一工具的 mode 参数，不另立工具）。
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum SpawnMode {
    #[default]
    Fresh,
    Fork,
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
    parent_session_id: String,
    #[serde(skip)]
    #[schemars(skip)]
    spawn_tools: Vec<String>,
    #[serde(skip)]
    #[schemars(skip)]
    spawn_ephemeral: bool,
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
            parent_session_id: self.parent_session_id.clone(),
            parent_agent_path: self.parent_agent_path.clone(),
            child_agent_path: self.child_agent_path.clone(),
            spawn_tools: self.spawn_tools.clone(),
            spawn_ephemeral: self.spawn_ephemeral,
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
    /// Recipient: AgentPath (absolute or caller-relative) or a session id
    /// (cross-session chat, ADR-0014).
    to: String,
    /// Message body to deliver to the recipient.
    message: String,
    /// Delivery mode: queue (default, merge at turn boundary), trigger
    /// (start a turn when idle), steer / interject (merge into the running
    /// turn at the next safe point).
    #[serde(default)]
    mode: Option<String>,
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
            "Send a canonical message to another conversation (agent path or session id; cross-session allowed). mode: queue (default, merged at the turn boundary), trigger (start a turn when idle), steer/interject (merge into the running turn).",
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
        let delivery = match args.mode.as_deref().map(str::trim) {
            None | Some("") | Some("queue") => InterAgentDelivery::Queue,
            Some("trigger") => InterAgentDelivery::Trigger,
            Some("steer") => InterAgentDelivery::Steer,
            Some("interject") => InterAgentDelivery::Interject,
            Some(other) => {
                return Err(subagent_error(
                    "invalid_mode",
                    format!("send_message: unknown mode {other:?}"),
                    "Use queue, trigger, steer or interject.",
                ));
            }
        };
        handle_agent_message(ctx, args, delivery)
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
    delivery: InterAgentDelivery,
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
            InterAgentDelivery::Queue => "queue",
            InterAgentDelivery::Trigger => "trigger",
            InterAgentDelivery::Interrupt => "interrupt",
            InterAgentDelivery::Steer => "steer",
            InterAgentDelivery::Interject => "interject",
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
            "Spawn a child conversation (subagent) for a focused task and deliver the task immediately. Returns the child session id and agent path; its final answer arrives as a canonical message when the turn ends - do not poll. Track with list_agents, chat with send_message, cancel with interrupt_agent. agent_name = verb+task phrase (e.g. 'explore_task'). mode: fresh (default) = the child does not see this conversation; fork = the child inherits your completed turns (not the current in-flight turn) - use fork when the task needs the history you already built.",
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
    mgr.register_display("wait_agent", project_wait_agent_display);
    mgr.register_typed(WaitAgentTool);
    mgr.register_display("interrupt_agent", project_interrupt_agent_display);
    mgr.register_typed(InterruptAgentTool);
}

/// Register the collaboration surface (task_*/board_*) alongside the agent
/// tools. Production sessions receive both registrars (ADR-0014 切片E); the
/// board is keyed by the caller's root tree, so lead and teammates share it.
pub fn register_collaboration_tools(mgr: &mut ToolManager) {
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

/// 构造子代理任务文本：固定身份提示（`[SYSTEM]`）+ 显式包裹的上下文
/// （`<main_subagent_message>`，防止子代理把传入内容当作自己的 user 消息
/// 而直接动项目）+ 任务（`[TASK]`）。
/// 上下文可见性措辞由 fork 派生（dsh inheritsParentContext 同口径）：
/// fresh 不见父对话；fork 继承已完成回合、不见父级在飞回合。
fn build_subagent_task(task_description: &str, context: &str, fork: bool) -> String {
    let visibility = if fork {
        "You are a forked subagent: the parent's completed turns are seeded into your context; you do not see the parent's current in-flight turn."
    } else {
        "You are a fresh subagent: you do not see the parent conversation."
    };
    let mut parts = vec![format!("[SYSTEM]\n{SUBAGENT_IDENTITY_PROMPT}\n{visibility}")];
    if !context.trim().is_empty() {
        parts.push(format!(
            "[CONTEXT]\n<main_subagent_message>\n{}\n</main_subagent_message>",
            context.trim()
        ));
    }
    parts.push(format!("[TASK]\n{}", task_description.trim()));
    parts.join("\n\n")
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
    let fork = args.mode.unwrap_or_default() == SpawnMode::Fork;

    // ADR-0018：子代理=会话——工具面与父会话同律（空白名单 = STANDARD，
    // 装配时从有效 config 解析），模型选择继承父会话行，生命周期预算与父
    // 同律。不再有配置面白名单/超时旋钮，也不在工具路径读全局 config。
    let tools: Vec<String> = Vec::new();

    if task.trim().is_empty() {
        return Err(subagent_error(
            "missing_task",
            "spawn_subagent: task_description is required",
            "Provide a task description.",
        ));
    }
    let task_text = build_subagent_task(&task, &context, fork);

    // 子代理继承主代理的工作区。workspace 由显式 ToolCallContext 注入，
    // 派生工具线程不再读取线程局部状态。为空/`.` 时不传。
    let parent_workspace = ctx.workspace_root.to_string_lossy();
    let workspace = if parent_workspace.is_empty() || parent_workspace == "." {
        None
    } else {
        Some(parent_workspace.into_owned())
    };
    // ── 1. Create the child conversation (ADR-0014: subagent = session). ──
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
            selection: None,
            workspace: workspace.as_deref(),
            fork,
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

    // ── 2. Deliver the task as a canonical inter-agent message. The child's
    // turn terminal routes the answer back to this session's mailbox. ──
    host.send_agent_message(SendAgentMessageRequest {
        caller_session_id: &ctx.session_id,
        target: &child_session_id,
        text: &task_text,
        delivery: InterAgentDelivery::Trigger,
    })
    .map_err(|error| {
        subagent_error(
            "send_error",
            format!("spawn_subagent: task delivery rejected: {error}"),
            "The child session exists; retry with send_message(mode=trigger).",
        )
    })?;
    log::info!(
        "[SUBAGENT] '{name}' child conversation created (seed={session_id}, path={child_agent_path}); task delivered"
    );
    let content = format!(
        "Subagent '{name}' spawned at {child_agent_path} (session {child_session_id}); task delivered. Its final answer arrives as a message when the turn ends — do not poll. Track with list_agents; chat with send_message; cancel with interrupt_agent(target=\"{child_agent_path}\")."
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
        parent_session_id: ctx.session_id.clone(),
        spawn_tools: tools.clone(),
        spawn_ephemeral: false,
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

        for name in ["send_message"] {
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
        register_collaboration_tools(&mut manager);

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

        // ADR-0014：模型面 4 个参数（切片G 增 mode）；ADR-0018：预算/工具面
        // 与父会话同律，system_prompt / tools / model / base_url /
        // max_tokens / timeout 不暴露。
        let expected: Vec<&str> = vec!["task_description", "agent_name", "context", "mode"];
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
        let bare = build_subagent_task("do the thing", "", false);
        assert!(bare.contains("[SYSTEM]"));
        assert!(bare.contains(SUBAGENT_IDENTITY_PROMPT));
        assert!(bare.contains("[TASK]\ndo the thing"));
        assert!(!bare.contains("[CONTEXT]"));
        assert!(bare.contains("you do not see the parent conversation"));

        // 有 context：必须包裹在 <main_subagent_message> 内，防止子代理把
        // 传入内容误判为自身 user 消息而直接修改项目。
        let with_ctx = build_subagent_task("review the diff", "repo at F:\\proj\nbranch main", false);
        assert!(with_ctx.contains(
            "<main_subagent_message>\nrepo at F:\\proj\nbranch main\n</main_subagent_message>"
        ));
        assert!(with_ctx.contains("[TASK]\nreview the diff"));
        // 身份提示在前，任务在后。
        assert!(with_ctx.find("[SYSTEM]").unwrap() < with_ctx.find("[TASK]").unwrap());

        // fork 措辞：继承已完成回合、不见在飞回合（切片G）。
        let forked = build_subagent_task("continue", "", true);
        assert!(forked.contains("completed turns are seeded into your context"));
        assert!(forked.contains("do not see the parent's current in-flight turn"));
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
            parent_session_id: "root-seed".to_string(),
            spawn_tools: vec![],
            spawn_ephemeral: false,
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
            exec_host: None,
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
                    mode: None,
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

    fn listed_agent(path: &str, id: &str, answer: Option<&str>) -> ListedAgent {
        ListedAgent {
            root_session_id: "root".to_string(),
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
