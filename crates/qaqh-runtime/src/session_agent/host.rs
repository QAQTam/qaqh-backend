//! AgentHost and AgentHandle lifecycle manager (RLC-3.2).
//!
//! Replaces per-session physical threads with supervised Tokio async tasks.
//! Exposes non-blocking ingress lanes for application commands and high-priority control.

use std::sync::Arc;

use qaqh_agentloop_core::{
    ActorIdentity,
    driver,
    mailbox::{LaneBudget, Mailbox, MailboxError, MailboxHandle},
    supervisor::Supervisor,
};
use qaqh_session::{
    session_commit::{CommittedExecution, SessionCommit},
    session_fact_v2::{ContentRef, InputId, InteractionId, ToolCallId, TurnId},
};
use tokio::{sync::Mutex, task::JoinHandle};

use crate::{
    provider_job::ProviderRequestSpec,
    tool_job::WorkspaceToolPort,
};

use super::{
    agent::SessionAgent,
    types::{
        AgentCommand, AgentCompletion, AgentControl, AgentOutcome, SessionAgentError,
    },
};

/// Client-facing handle to a running SessionAgent task.
#[derive(Clone)]
pub struct AgentHandle {
    pub session_id: String,
    pub(crate) mailbox: MailboxHandle<AgentCommand, AgentControl, AgentCompletion>,
}

impl AgentHandle {
    /// Submit a user message into the normal command lane.
    pub fn submit_user_input(
        &self,
        text: &str,
        attachments: Vec<ContentRef>,
        client_request_id: Option<String>,
    ) -> Result<InputId, MailboxError> {
        let input_id = InputId::new(format!(
            "input_01J{:023}",
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap_or_default()
                .as_millis()
        ));
        self.mailbox.try_command(AgentCommand::UserInput {
            input_id: input_id.clone(),
            text: text.to_string(),
            attachments,
            client_request_id,
        })?;
        Ok(input_id)
    }

    /// Submit a high-priority cancellation request.
    pub fn cancel(&self, turn_id: Option<TurnId>, reason: &str) -> Result<(), MailboxError> {
        self.mailbox.try_control(AgentControl::CancelTurn {
            turn_id,
            reason: reason.to_string(),
        })
    }

    /// Resolve a suspended interaction (such as an ask question).
    pub fn resolve_interaction(
        &self,
        interaction_id: InteractionId,
        answers: Vec<(String, String)>,
    ) -> Result<(), MailboxError> {
        self.mailbox.try_control(AgentControl::ResolveInteraction {
            interaction_id,
            answers,
        })
    }

    /// Respond to an individual tool permission challenge.
    pub fn respond_tool_permission(
        &self,
        tool_call_id: ToolCallId,
        approved: bool,
        trust_folder: bool,
    ) -> Result<(), MailboxError> {
        self.mailbox.try_control(AgentControl::ToolPermissionRespond {
            tool_call_id,
            approved,
            trust_folder,
        })
    }

    /// Request clean shutdown of the agent task.
    pub fn shutdown(&self) -> Result<(), MailboxError> {
        self.mailbox.try_control(AgentControl::Shutdown)
    }
}

/// Host responsible for spawning and supervising the async SessionAgent task.
pub struct AgentHost;

impl AgentHost {
    pub fn spawn(
        session_id: String,
        commit: SessionCommit,
        provider_spec: ProviderRequestSpec,
        tool_port: Arc<WorkspaceToolPort>,
    ) -> Result<(AgentHandle, JoinHandle<Result<(), SessionAgentError>>), SessionAgentError> {
        let (mailbox_handle, mailbox) = Mailbox::new(
            LaneBudget::COMMANDS,
            LaneBudget::CONTROL,
            LaneBudget::COMPLETIONS,
        )
        .map_err(SessionAgentError::Mailbox)?;

        let incarnation = commit.incarnation();
        let mut supervisor: Supervisor<AgentOutcome, CommittedExecution> =
            Supervisor::new(16).map_err(SessionAgentError::Supervisor)?;
        let actor = ActorIdentity {
            session_id: session_id.clone(),
            incarnation,
        };
        supervisor.adopt_actor(&actor);

        let sup_arc = Arc::new(Mutex::new(supervisor));
        let agent = SessionAgent::new(
            session_id.clone(),
            incarnation,
            commit,
            Arc::clone(&sup_arc),
            mailbox_handle.clone(),
            provider_spec,
            tool_port,
        );

        let handle = AgentHandle {
            session_id,
            mailbox: mailbox_handle,
        };

        let task = tokio::spawn(async move {
            driver::run(mailbox, agent)
                .await
                .map(|_| ())
                .map_err(|failure| failure.error)
        });

        Ok((handle, task))
    }
}
