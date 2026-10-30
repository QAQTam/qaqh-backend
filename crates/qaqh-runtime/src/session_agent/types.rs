//! Types and state machine definitions for SessionAgent (RLC-3).

use qaqh_agentloop_core::{
    mailbox::FrameSize,
    supervisor::TaskCompletion,
};
use qaqh_session::session_fact_v2::{
    ContentRef, InputId, InteractionId, ToolCallId, TurnId,
};

use crate::provider_job::ProviderOutcome;
use crate::tool_job::ToolOutcome;

/// Ingress application commands.
#[derive(Debug, Clone)]
pub enum AgentCommand {
    UserInput {
        input_id: InputId,
        text: String,
        attachments: Vec<ContentRef>,
        client_request_id: Option<String>,
    },
    QueueInput {
        input_id: InputId,
        text: String,
        attachments: Vec<ContentRef>,
        client_request_id: Option<String>,
    },
}

impl FrameSize for AgentCommand {
    fn retained_bytes(&self) -> usize {
        match self {
            AgentCommand::UserInput { text, .. } => std::mem::size_of::<Self>() + text.len() + 64,
            AgentCommand::QueueInput { text, .. } => std::mem::size_of::<Self>() + text.len() + 64,
        }
    }
}

/// Control lane signals that have scheduling priority over commands.
#[derive(Debug, Clone)]
pub enum AgentControl {
    CancelTurn {
        turn_id: Option<TurnId>,
        reason: String,
    },
    ResolveInteraction {
        interaction_id: InteractionId,
        answers: Vec<(String, String)>,
    },
    ToolPermissionRespond {
        tool_call_id: ToolCallId,
        approved: bool,
        trust_folder: bool,
    },
    Shutdown,
}

impl FrameSize for AgentControl {
    fn retained_bytes(&self) -> usize {
        match self {
            AgentControl::CancelTurn { reason, .. } => std::mem::size_of::<Self>() + reason.len() + 32,
            AgentControl::ResolveInteraction { answers, .. } => {
                let ans_len: usize = answers.iter().map(|(q, a)| q.len() + a.len() + 16).sum();
                std::mem::size_of::<Self>() + ans_len + 32
            }
            AgentControl::ToolPermissionRespond { .. } => std::mem::size_of::<Self>() + 32,
            AgentControl::Shutdown => std::mem::size_of::<Self>(),
        }
    }
}

/// Unified task outcome from background workers.
#[derive(Debug, Clone)]
pub enum AgentOutcome {
    Provider(ProviderOutcome),
    Tool(ToolOutcome),
}

impl FrameSize for AgentOutcome {
    fn retained_bytes(&self) -> usize {
        match self {
            AgentOutcome::Provider(p) => {
                let err_len = p.error.as_ref().map(|s| s.len()).unwrap_or(0);
                let text_len = p.message.as_ref().map(|m| m.content.len() * 128).unwrap_or(0);
                std::mem::size_of::<Self>() + err_len + text_len + 256
            }
            AgentOutcome::Tool(t) => std::mem::size_of::<Self>() + t.output.len() + 128,
        }
    }
}

pub type AgentCompletion = TaskCompletion<AgentOutcome>;

/// SessionAgent lifecycle phase.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SessionPhase {
    Ready,
    Active(ActivePhase),
    Closing,
    Unloaded,
    Blocked(String),
}

/// Sub-phases during an active turn.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ActivePhase {
    PreparingContext,
    AwaitingProvider { round: u32, attempt: u32 },
    AwaitingTools { active_calls: Vec<ToolCallId> },
    AwaitingUser { interaction_id: InteractionId },
    Finalizing,
}

#[derive(Debug)]
pub enum SessionAgentError {
    Commit(qaqh_session::session_commit::SessionCommitError),
    Supervisor(qaqh_agentloop_core::supervisor::SupervisorError),
    Mailbox(qaqh_agentloop_core::mailbox::MailboxError),
    Internal(String),
}

impl std::fmt::Display for SessionAgentError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Commit(e) => write!(f, "commit error: {e}"),
            Self::Supervisor(e) => write!(f, "supervisor error: {e:?}"),
            Self::Mailbox(e) => write!(f, "mailbox error: {e:?}"),
            Self::Internal(s) => write!(f, "internal error: {s}"),
        }
    }
}

impl std::error::Error for SessionAgentError {}

impl From<qaqh_session::session_commit::SessionCommitError> for SessionAgentError {
    fn from(err: qaqh_session::session_commit::SessionCommitError) -> Self {
        Self::Commit(err)
    }
}

impl From<qaqh_agentloop_core::supervisor::SupervisorError> for SessionAgentError {
    fn from(err: qaqh_agentloop_core::supervisor::SupervisorError) -> Self {
        Self::Supervisor(err)
    }
}

impl From<qaqh_agentloop_core::mailbox::MailboxError> for SessionAgentError {
    fn from(err: qaqh_agentloop_core::mailbox::MailboxError) -> Self {
        Self::Mailbox(err)
    }
}
