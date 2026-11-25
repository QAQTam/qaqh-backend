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

mod host;
pub mod goal;
mod task_tools;
#[cfg(test)]
mod host_isolation_tests;
pub use host::{
    AgentOutcome, InterruptAgentRequest, InterruptedAgent, ListedAgent, ListedAgentResidency,
    ListedAgentStatus, SendAgentMessageRequest, SentAgentMessage, SpawnSubagentRequest,
    SpawnedSubagent, SubagentHost, TaskBoardArtifact, TaskBoardHost, TaskBoardTask,
    TaskClaimAction, TaskClaimRequest, TaskCloseAction, TaskCloseRequest, TaskCreateRequest,
    TaskListPage, TaskListRequest, TaskUpdateAction, TaskUpdateRequest, WaitAgentOutcome,
    WaitAgentRequest, normalize_write_scope,
    task_is_ready, write_scope_warnings,
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

/// ADR-0033：模型可见 JSON 恒为 `{session_id, name, content}` 三键——session id
/// 是同事的唯一寻址（send/wait/interrupt 一律用它），不再暴露 timeis/status
/// 信封遗留与 `session_id==child_session_id` 双 ID。`task_text` 仅供内部 effect。
#[derive(Debug, Deserialize, Serialize, JsonSchema)]
pub struct SpawnSubagentOutput {
    session_id: String,
    name: String,
    content: String,
    #[serde(skip)]
    #[schemars(skip)]
    task_text: String,
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
            child_session_id: self.session_id.clone(),
            name: self.name.clone(),
            task_text: self.task_text.clone(),
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

pub struct ListAgentsTool {
    pub host: Arc<dyn SubagentHost>,
}

impl TypedTool for ListAgentsTool {
    type Args = ListAgentsArgs;
    type Output = ListAgentsOutput;

    fn meta(&self) -> ToolMeta {
        ToolMeta::new(
            "list_agents",
            "List logical agents at or below a path prefix in the current root tree: status \
             (pending_init/running/waiting_user/interrupted/completed/errored), residency \
             and terminal outcome — check what a teammate is doing before steering it. \
             Defaults to /root. `agent_id` IS the session id — that is how you address the \
             agent (send_message / wait_agent / interrupt_agent). Parallel sessions \
             in OTHER root trees are not enumerated here; reach them by session id \
             (send_message; ids come from the journal tool's file attribution).",
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
        let host = self.host.as_ref();
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

pub struct SendMessageTool {
    pub host: Arc<dyn SubagentHost>,
}

impl TypedTool for SendMessageTool {
    type Args = AgentMessageArgs;
    type Output = AgentMessageOutput;

    fn meta(&self) -> ToolMeta {
        ToolMeta::new(
            "send_message",
            "Send a canonical message to another conversation. to: a session id (primary, \
              ADR-0033 — the id returned by spawn_subagent or listed by list_agents) or an \
              AgentPath; cross-root delivery requires the session id. mode: queue (default, \
              merged at the turn boundary), trigger (start a turn when idle), steer/interject \
              (merge into the running turn). The recipient sees your text as a plain user \
              message without sender metadata — open with who you are (your session id) \
              and what you ask, so it can answer or reply back to you.",
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
        handle_agent_message(&*self.host, ctx, args, delivery)
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WaitAgentArgs {
    /// Timeout in milliseconds. Defaults to 30000.
    #[serde(default)]
    timeout_ms: Option<u64>,
    /// Session ids (primary) or paths to wait on. Required — the call returns
    /// each target's status and terminal outcome once every target finished.
    targets: Vec<String>,
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

pub struct WaitAgentTool {
    pub host: Arc<dyn SubagentHost>,
}

impl TypedTool for WaitAgentTool {
    type Args = WaitAgentArgs;
    type Output = WaitAgentOutput;

    fn meta(&self) -> ToolMeta {
        // The typed handler enforces the tighter mailbox wait bound. The
        // descriptor must not kill a valid long wait before it returns.
        ToolMeta::new(
            "wait_agent",
            "Wait for the given colleagues to finish. Returns each target's status and terminal outcome (final answer) once every target finished — use this instead of polling. Targets are session ids (from spawn_subagent/list_agents).",
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
        let host = self.host.as_ref();
        let should_cancel = || ctx.cancellation.is_cancelled();
        let targets = args.targets;
        if targets.is_empty() {
            // ADR-0033：wait 单语义——只等目标终态；mailbox 水位模式随 50ms
            // 内存轮询一起退役，消息到达靠回合边界合流，无需模型显式等待。
            return Err(subagent_error(
                "missing_targets",
                "wait_agent: at least one target session id is required",
                "Pass the session ids returned by spawn_subagent or list_agents.",
            ));
        }
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
            WaitAgentOutcome::Settled => {
                let agents = wait_agent_snapshot(host, &ctx.session_id, &targets);
                Ok(WaitAgentOutput {
                    message: format!("Wait completed: {} target(s) finished.", agents.len()),
                    timed_out: false,
                    agents,
                })
            }
            WaitAgentOutcome::TimedOut => {
                let agents = wait_agent_snapshot(host, &ctx.session_id, &targets);
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
    host: &dyn SubagentHost,
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
    /// Session id (primary, from spawn_subagent/list_agents) or AgentPath.
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

pub struct InterruptAgentTool {
    pub host: Arc<dyn SubagentHost>,
}

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
                "Provide the target session id (from spawn_subagent/list_agents) or an AgentPath.",
            ));
        }
        let host = self.host.as_ref();
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
    host: &dyn SubagentHost,
    ctx: &ToolCallContext,
    args: AgentMessageArgs,
    delivery: InterAgentDelivery,
) -> Result<AgentMessageOutput, ToolExecutionError> {
    if args.to.trim().is_empty() {
        return Err(subagent_error(
            "missing_target",
            "agent message target is required",
            "Provide the target session id (from spawn_subagent/list_agents) or an AgentPath.",
        ));
    }
    if args.message.trim().is_empty() {
        return Err(subagent_error(
            "missing_message",
            "agent message body is required",
            "Provide a non-empty message.",
        ));
    }
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

pub struct SpawnSubagentTool {
    pub host: Arc<dyn SubagentHost>,
}

impl TypedTool for SpawnSubagentTool {
    type Args = SpawnSubagentArgs;
    type Output = SpawnSubagentOutput;

    fn meta(&self) -> ToolMeta {
        ToolMeta::new(
            "spawn_subagent",
            "Spawn a colleague conversation for a focused task and deliver the task immediately. Returns the child session id and name — the session id is the ONLY address you need: send_message(to=<session id>), wait_agent(targets=[<session id>]), interrupt_agent(target=<session id>). Its final answer arrives as a canonical message when the turn ends - do not poll. agent_name = verb+task phrase (e.g. 'explore_task'). mode: fresh (default) = the colleague does not see this conversation; fork = it inherits your completed turns (not the current in-flight turn) - use fork when the task needs the history you already built.",
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
        handle_spawn_subagent(&*self.host, ctx, args)
    }
}

/// 注册会话工具面（agent 五件）；`SubagentHost` 在注册期显式注入工具实例。
pub fn register(mgr: &mut ToolManager, host: Arc<dyn SubagentHost>) {
    mgr.register_display("spawn_subagent", project_subagent_display);
    mgr.register_typed(SpawnSubagentTool { host: Arc::clone(&host) });
    mgr.register_display("list_agents", project_list_agents_display);
    mgr.register_typed(ListAgentsTool { host: Arc::clone(&host) });
    mgr.register_display("send_message", project_agent_message_display);
    mgr.register_typed(SendMessageTool { host: Arc::clone(&host) });
    mgr.register_display("wait_agent", project_wait_agent_display);
    mgr.register_typed(WaitAgentTool { host: Arc::clone(&host) });
    mgr.register_display("interrupt_agent", project_interrupt_agent_display);
    mgr.register_typed(InterruptAgentTool { host });
}

/// Register the collaboration surface (task_*). ADR-0033 切片D:留言板词表
/// 退役——通信只走 canonical mailbox(send_message),协调走任务板。
/// v4.5 P2-1b：`TaskBoardHost` 注册期注入。
pub fn register_collaboration_tools(mgr: &mut ToolManager, host: Arc<dyn TaskBoardHost>) {
    mgr.register_typed(task_tools::TaskCreateTool { host: Arc::clone(&host) });
    mgr.register_typed(task_tools::TaskClaimTool { host: Arc::clone(&host) });
    mgr.register_typed(task_tools::TaskUpdateTool { host: Arc::clone(&host) });
    mgr.register_typed(task_tools::TaskCloseTool { host: Arc::clone(&host) });
    mgr.register_typed(task_tools::TaskListTool { host });
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
    let mut parts = vec![format!(
        "[SYSTEM]\n{SUBAGENT_IDENTITY_PROMPT}\n{visibility}"
    )];
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
    host: &dyn SubagentHost,
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
    // ADR-0033：seed 与 child 是同一个 UUID（历史双 ID 收编）——模型面只暴露
    // 子会话 id 一个名字。
    let SpawnedSubagent {
        session_id,
        child_session_id,
        child_agent_path,
        ..
    } = spawned;
    debug_assert_eq!(
        session_id, child_session_id,
        "spawn seed 与 child session id 必须同值（ADR-0033 双 ID 收编）"
    );

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
        "Colleague '{name}' spawned (session {child_session_id}); task delivered. Address it by session id: send_message(to=\"{child_session_id}\"), wait_agent(targets=[\"{child_session_id}\"]), interrupt_agent(target=\"{child_session_id}\"). Its final answer arrives as a message when the turn ends — do not poll."
    );
    Ok(SpawnSubagentOutput {
        session_id: child_session_id,
        name,
        content,
        task_text,
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

    /// 注册冒烟用桩宿主（schema 面断言不触达执行路径；执行面的宿主隔离
    /// 反证在 `src/host_isolation_tests.rs`）。
    struct StubHost;
    impl SubagentHost for StubHost {
        fn spawn_subagent(
            &self,
            _request: SpawnSubagentRequest<'_>,
        ) -> Result<SpawnedSubagent, String> {
            Err("stub".to_string())
        }
        fn list_agents(
            &self,
            _caller_session_id: &str,
            _path_prefix: &str,
        ) -> Result<Vec<ListedAgent>, String> {
            Err("stub".to_string())
        }
        fn send_agent_message(
            &self,
            _request: SendAgentMessageRequest<'_>,
        ) -> Result<SentAgentMessage, String> {
            Err("stub".to_string())
        }
        fn wait_agent(&self, _request: WaitAgentRequest<'_>) -> Result<WaitAgentOutcome, String> {
            Err("stub".to_string())
        }
        fn interrupt_agent(
            &self,
            _request: InterruptAgentRequest<'_>,
        ) -> Result<InterruptedAgent, String> {
            Err("stub".to_string())
        }
        fn send_ringing(&self, _session_id: &str, _command: qaqh_ringing::RingingCommand) -> Result<(), String> {
            Err("stub".to_string())
        }
    }

    fn stub_subagent_host() -> Arc<dyn SubagentHost> {
        Arc::new(StubHost)
    }

    struct StubTaskHost;
    impl TaskBoardHost for StubTaskHost {
        fn task_create(&self, _r: TaskCreateRequest<'_>) -> Result<TaskBoardTask, String> {
            Err("stub".to_string())
        }
        fn task_claim(&self, _r: TaskClaimRequest<'_>) -> Result<TaskBoardTask, String> {
            Err("stub".to_string())
        }
        fn task_update(&self, _r: TaskUpdateRequest<'_>) -> Result<TaskBoardTask, String> {
            Err("stub".to_string())
        }
        fn task_close(&self, _r: TaskCloseRequest<'_>) -> Result<TaskBoardTask, String> {
            Err("stub".to_string())
        }
        fn task_list(&self, _r: TaskListRequest<'_>) -> Result<TaskListPage, String> {
            Err("stub".to_string())
        }
    }

    #[test]
    fn spawn_subagent_schema_never_accepts_api_keys() {
        let mut manager = ToolManager::new();
        register(&mut manager, stub_subagent_host());

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
        register(&mut manager, stub_subagent_host());

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
        register(&mut manager, stub_subagent_host());

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
    fn wait_agent_schema_requires_targets_and_optional_timeout() {
        let mut manager = ToolManager::new();
        register(&mut manager, stub_subagent_host());

        let handler = manager
            .lookup("wait_agent")
            .expect("wait_agent should be registered");
        assert!(
            handler.input_schema["properties"]
                .as_object()
                .expect("properties")
                .contains_key("timeout_ms")
        );
        // ADR-0033 闸门：targets 必选（单语义 = 只等目标终态）。
        let required = handler.input_schema["required"]
            .as_array()
            .expect("targets must be a required param");
        assert_eq!(required.len(), 1);
        assert_eq!(required[0], "targets");
    }

    #[test]
    fn interrupt_agent_schema_requires_target() {
        let mut manager = ToolManager::new();
        register(&mut manager, stub_subagent_host());

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
        register_collaboration_tools(&mut manager, Arc::new(StubTaskHost));

        for name in [
            "task_create",
            "task_claim",
            "task_update",
            "task_close",
            "task_list",
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
        register(&mut manager, stub_subagent_host());
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
        let with_ctx =
            build_subagent_task("review the diff", "repo at F:\\proj\nbranch main", false);
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
            session_id: "0198f1a0-0000-7000-8000-000000000003".to_string(),
            name: "review_code".to_string(),
            content: "Colleague 'review_code' spawned (session 0198f1a0); task delivered."
                .to_string(),
            task_text: "[TASK]\nreview".to_string(),
        };
        let model = match output.model_blocks().into_iter().next() {
            Some(ToolContentBlock::Text { text }) => text,
            _ => panic!("subagent output must have a text model block"),
        };
        assert!(
            !model.contains("process_id"),
            "subagent must not expose an exec process handle: {model}"
        );
        // ADR-0033 闸门：模型可见 JSON 恰三键（session_id/name/content）——
        // timeis/status 信封遗留、双 ID、path 词汇一律不进模型面。
        let parsed: serde_json::Map<String, serde_json::Value> =
            serde_json::from_str(&model).expect("spawn output model block must be valid JSON");
        assert_eq!(
            parsed.len(),
            3,
            "spawn output model JSON must carry exactly three keys: {model}"
        );
        for key in ["session_id", "name", "content"] {
            assert!(parsed.contains_key(key), "missing {key}: {model}");
        }
        assert!(model.contains("\"session_id\":\"0198f1a0"));
        assert!(
            !model.contains("child_session_id")
                && !model.contains("timeis")
                && !model.contains("agent_path"),
            "内部身份/信封遗留/path 词汇不得出现在模型面: {model}"
        );
        let display = output.display(&serde_json::json!({}));
        assert_eq!(
            display.summary.as_deref(),
            Some("Colleague 'review_code' spawned (session 0198f1a0); task delivered.")
        );
        match display.body {
            qaqh_tool_core::tool_api::ToolBody::Subagent { name, session_id } => {
                assert_eq!(name, "review_code");
                assert_eq!(session_id, "0198f1a0-0000-7000-8000-000000000003");
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
            exec_max_output_chars: None,
            timeout: Duration::from_secs(180),
            cancellation: qaqh_tool_core::tool_api::CancellationToken::new(),
            progress: None,
            exec_progress: None,
            exec_host: None,
            official_web: None,
            source: qaqh_tool_core::tool_api::ToolCallSource::Model,
        };
        let error = SpawnSubagentTool { host: stub_subagent_host() }
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

    // ── ADR-0033 切片B 闸门:write_scopes 归一化/就绪派生/重叠警告 ──

    fn board_task(task_id: &str, state: &str, scopes: &[&str], deps: &[&str]) -> TaskBoardTask {
        TaskBoardTask {
            task_id: task_id.to_string(),
            title: String::new(),
            state: state.to_string(),
            owner: None,
            claim_epoch: 0,
            depends_on: deps.iter().map(|dep| dep.to_string()).collect(),
            artifacts: Vec::new(),
            acceptance: Vec::new(),
            write_scopes: scopes.iter().map(|scope| scope.to_string()).collect(),
            ready: false,
            write_scope_warnings: Vec::new(),
            owner_session_id: None,
            result_ref: None,
            created_at_ms: 0,
            updated_at_ms: 0,
        }
    }

    #[test]
    fn write_scope_normalization_is_lenient_but_rejects_garbage() {
        assert_eq!(
            normalize_write_scope("./src/api\\v2/").unwrap(),
            "src/api/v2",
            "反斜杠归一 + ./ 剥除 + 尾斜杠剥除"
        );
        assert_eq!(normalize_write_scope("/abs/path").unwrap(), "abs/path");
        assert!(normalize_write_scope("").is_err());
        assert!(normalize_write_scope("  . ").is_err());
        assert!(normalize_write_scope(&"x".repeat(257)).is_err());
    }

    #[test]
    fn task_readiness_requires_open_and_all_deps_terminal() {
        let done = board_task("dep-1", "completed", &[], &[]);
        let wip = board_task("dep-2", "claimed", &[], &[]);
        let missing = board_task("dep-3", "open", &[], &[]);
        let index = std::collections::HashMap::from([
            ("dep-1".to_string(), &done),
            ("dep-2".to_string(), &wip),
        ]);

        let ready = board_task("t", "open", &[], &["dep-1"]);
        assert!(task_is_ready(&ready, &index));

        let blocked = board_task("t", "open", &[], &["dep-2"]);
        assert!(!task_is_ready(&blocked, &index), "未完成依赖阻塞就绪");

        let dangling = board_task("t", "open", &[], &["dep-3"]);
        assert!(!task_is_ready(&dangling, &index), "依赖缺失 fail-closed");

        let claimed = board_task("t", "claimed", &[], &[]);
        assert!(!task_is_ready(&claimed, &index), "claimed 不是 ready");
    }

    #[test]
    fn scope_warnings_flag_only_claimed_overlaps_and_respect_prefix_boundary() {
        let claimed_a = board_task("a", "claimed", &["src"], &[]);
        let claimed_b = board_task("b", "claimed", &["src/api"], &[]);
        let claimed_far = board_task("c", "claimed", &["src2"], &[]);
        let open_task = board_task("d", "open", &["src/tests"], &[]);
        let board = vec![&claimed_a, &claimed_b, &claimed_far, &open_task];

        let warnings = write_scope_warnings(&claimed_a, &board);
        assert_eq!(
            warnings,
            vec!["write scope \"src\" overlaps claimed task b"],
            "真前缀相交才警告;src2 不算(src 边界),open 任务不作为冲突方"
        );
        assert!(write_scope_warnings(&claimed_far, &board).is_empty());
        // open 任务也携带警告:认领前即可预判与在飞工作的冲突。
        assert_eq!(
            write_scope_warnings(&open_task, &board),
            vec!["write scope \"src/tests\" overlaps claimed task a"]
        );
    }
}
