//! Types and state machine definitions for SessionAgent (RLC-3).

use qaqh_agentloop_core::{
    mailbox::FrameSize,
    supervisor::TaskCompletion,
};
use qaqh_session::session_fact_v2::{
    ContentRef, InputId, InteractionId, ToolCallId, TurnId,
};

use crate::provider_job::{ProviderOutcome, ProviderRequestSpec};
use crate::tool_job::ToolOutcome;

/// One durable input waiting for its turn. `accepted` mirrors the committed
/// `InputAccepted` fact: the queue itself is never the source of truth.
#[derive(Debug, Clone)]
pub struct QueuedUserInput {
    pub input_id: InputId,
    pub text: String,
    pub attachments: Vec<ContentRef>,
    pub client_request_id: Option<String>,
    pub accepted: bool,
}

impl FrameSize for QueuedUserInput {
    fn retained_bytes(&self) -> usize {
        std::mem::size_of::<Self>()
            .saturating_add(self.text.len())
            .saturating_add(self.attachments.len() * 96)
            .saturating_add(
                self.client_request_id
                    .as_ref()
                    .map_or(0, String::capacity),
            )
    }
}

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
    /// Session metadata write (profile / effort / mode / archived / …). The
    /// actor is the only writer of the journal, so meta writes ride the mailbox
    /// instead of opening a second `SessionCommit`.
    SetMetadata {
        patch: qaqh_session::session_fact_v2::SessionMetadataPatch,
    },
    /// Title write, same reasoning as [`Self::SetMetadata`].
    SetTitle {
        title: String,
        source: qaqh_session::session_fact_v2::TitleSource,
    },
}

impl FrameSize for AgentCommand {
    fn retained_bytes(&self) -> usize {
        match self {
            AgentCommand::UserInput {
                text,
                attachments,
                client_request_id,
                ..
            } => std::mem::size_of::<Self>()
                .saturating_add(text.len())
                .saturating_add(attachments.len() * 96)
                .saturating_add(client_request_id.as_ref().map_or(0, String::capacity))
                .saturating_add(64),
            AgentCommand::QueueInput {
                text,
                attachments,
                client_request_id,
                ..
            } => std::mem::size_of::<Self>()
                .saturating_add(text.len())
                .saturating_add(attachments.len() * 96)
                .saturating_add(client_request_id.as_ref().map_or(0, String::capacity))
                .saturating_add(64),
            AgentCommand::SetMetadata { patch } => std::mem::size_of::<Self>()
                .saturating_add(256)
                .saturating_add(patch.skills.as_ref().map_or(0, |s| 64))
                .saturating_add(patch.frozen_annotation.as_ref().map_or(0, String::capacity))
                .saturating_add(patch.custom_tools.as_ref().map_or(0, |t| t.len() * 48)),
            AgentCommand::SetTitle { title, .. } => std::mem::size_of::<Self>()
                .saturating_add(title.len())
                .saturating_add(64),
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
    /// Replace the provider request parameters after a configuration reload.
    ReloadProvider {
        spec: Box<ProviderRequestSpec>,
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
            AgentControl::ReloadProvider { spec } => std::mem::size_of::<Self>()
                + std::mem::size_of::<ProviderRequestSpec>()
                + spec
                    .tools
                    .as_ref()
                    .map_or(0, |tools| tools.len() * 128)
                + spec.provider.model.len()
                + spec.provider.base_url.len()
                + spec.effort.as_ref().map_or(0, String::capacity),
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
            // Delegate to the payload's own measurement: re-estimating here
            // silently under-reports large bodies.
            AgentOutcome::Provider(p) => std::mem::size_of::<Self>().saturating_add(p.retained_bytes()),
            AgentOutcome::Tool(t) => std::mem::size_of::<Self>().saturating_add(t.retained_bytes()),
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
