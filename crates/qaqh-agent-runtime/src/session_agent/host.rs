//! AgentHost and AgentHandle lifecycle manager (RLC-3.2).
//!
//! Replaces per-session physical threads with supervised Tokio async tasks.
//! Exposes non-blocking ingress lanes for application commands and high-priority control.

use std::sync::Arc;

use qaqh_agentloop_core::{
    ActorIdentity, driver,
    mailbox::{LaneBudget, Mailbox, MailboxError, MailboxHandle},
    supervisor::Supervisor,
};
use qaqh_session::{
    canonical::generate_ulid,
    session_commit::{CommittedExecution, SessionCommit},
    session_fact_v2::{ContentRef, InputId, InteractionId, ToolCallId, TurnId},
};
use std::sync::Mutex;
use tokio::task::JoinHandle;

use crate::{provider_job::ProviderRequestSpec, tool_job::WorkspaceToolPort};

use super::{
    agent::{ActivityTap, SessionAgent},
    types::{AgentCommand, AgentCompletion, AgentControl, AgentOutcome, SessionAgentError},
};

/// Client-facing handle to a running SessionAgent task.
#[derive(Clone)]
pub struct AgentHandle {
    pub session_id: String,
    pub(crate) mailbox: MailboxHandle<AgentCommand, AgentControl, AgentCompletion>,
}

impl AgentHandle {
    /// 测试 / 诊断用：用已有的 mailbox 句柄装配一个 handle。
    ///
    /// 生产路径只有 [`AgentHost::spawn_with_activity`]；跨 crate 的 registry 测试
    /// 需要构造一个只做命令路由的假实例，mailbox 字段本身保持私有。
    #[doc(hidden)]
    pub fn from_mailbox(
        session_id: String,
        mailbox: MailboxHandle<AgentCommand, AgentControl, AgentCompletion>,
    ) -> Self {
        Self {
            session_id,
            mailbox,
        }
    }

    /// Submit a user message into the normal command lane.
    pub fn submit_user_input(
        &self,
        text: &str,
        attachments: Vec<ContentRef>,
        client_request_id: Option<String>,
    ) -> Result<InputId, MailboxError> {
        // Time-and-random ULID: unique across restarts and within one
        // millisecond, unlike a bare wall-clock millisecond stamp.
        let input_id = InputId::new(format!("input_{}", generate_ulid()));
        self.mailbox.try_command(AgentCommand::UserInput {
            input_id: input_id.clone(),
            text: text.to_string(),
            attachments,
            client_request_id,
        })?;
        Ok(input_id)
    }

    /// Commit a session metadata patch through the actor (the journal's sole
    /// writer). Falls back to a direct journal write only when no actor runs.
    pub fn set_metadata(
        &self,
        patch: qaqh_session::session_fact_v2::SessionMetadataPatch,
    ) -> Result<(), MailboxError> {
        self.mailbox
            .try_command(AgentCommand::SetMetadata { patch })
    }

    /// Commit a session title change through the actor.
    pub fn set_title(
        &self,
        title: &str,
        source: qaqh_session::session_fact_v2::TitleSource,
    ) -> Result<(), MailboxError> {
        self.mailbox.try_command(AgentCommand::SetTitle {
            title: title.to_string(),
            source,
        })
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
        self.mailbox
            .try_control(AgentControl::ToolPermissionRespond {
                tool_call_id,
                approved,
                trust_folder,
            })
    }

    /// Replace the provider parameters, and optionally the tool policy, after a
    /// configuration reload.
    ///
    /// A reload that changes the working directory or the sandbox switch must
    /// reach the actor through the same boundary: the provider swap alone
    /// leaves the existing port dispatching under the old root and policy.
    pub fn reload_provider(
        &self,
        spec: ProviderRequestSpec,
        policy: Option<crate::tool_job::ToolPolicySnapshot>,
    ) -> Result<(), MailboxError> {
        self.mailbox.try_control(AgentControl::ReloadProvider {
            spec: Box::new(spec),
            policy: policy.map(Box::new),
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
        Self::spawn_with_activity(session_id, commit, provider_spec, tool_port, None, None)
    }

    #[allow(clippy::too_many_arguments)]
    pub fn spawn_with_activity(
        session_id: String,
        commit: SessionCommit,
        provider_spec: ProviderRequestSpec,
        tool_port: Arc<WorkspaceToolPort>,
        activity: Option<ActivityTap>,
        progress: Option<Arc<dyn crate::progress::ProgressSink>>,
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
            activity,
            progress,
        );

        let handle = AgentHandle {
            session_id: session_id.clone(),
            mailbox: mailbox_handle,
        };

        let driver = async move {
            match driver::run(mailbox, agent).await {
                Ok(_) => Ok(()),
                Err(failure) => {
                    // The driver hands back the owner, the unconsumed mailbox
                    // and the failing input: that is the recovery material.
                    // Keep it alive (and logged) instead of dropping it.
                    let retained = RetainedDriverFailure {
                        session_id: session_id.clone(),
                        input_kind: describe_input(&failure.input),
                        error: failure.error.to_string(),
                        phase: format!("{:?}", failure.handler.phase()),
                    };
                    log::error!(
                        "[session-agent] {} driver exited with open recovery material: {:?}",
                        session_id,
                        retained
                    );
                    Err(failure.error)
                }
            }
        };
        // 生产路径（daemon）总在 tokio 上下文里；CLI 与集成测试可能在纯同步
        // 上下文中调用 registry，此时 `tokio::spawn` 会 panic（no reactor）。
        let task = match tokio::runtime::Handle::try_current() {
            Ok(handle) => handle.spawn(driver),
            Err(_) => driver_runtime().spawn(driver),
        };

        Ok((handle, task))
    }
}

/// 无 reactor 上下文时的兜底执行器。
///
/// 只在同步调用方（CLI、集成测试）触发；按需创建一次并常驻进程——这些调用点
/// 本来就需要一个后台执行器，只是没人替它们建运行时。
fn driver_runtime() -> &'static tokio::runtime::Runtime {
    static RUNTIME: std::sync::OnceLock<tokio::runtime::Runtime> = std::sync::OnceLock::new();
    RUNTIME.get_or_init(|| {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .thread_name("qaqh-session-driver")
            .build()
            .expect("build session driver runtime")
    })
}
/// Enough shape of a failed driver to keep the recovery material observable.
#[derive(Debug)]
pub struct RetainedDriverFailure {
    pub session_id: String,
    pub input_kind: &'static str,
    pub error: String,
    pub phase: String,
}

fn describe_input(
    input: &qaqh_agentloop_core::mailbox::Input<AgentCommand, AgentControl, AgentCompletion>,
) -> &'static str {
    match input {
        qaqh_agentloop_core::mailbox::Input::Command(_) => "command",
        qaqh_agentloop_core::mailbox::Input::Control(_) => "control",
        qaqh_agentloop_core::mailbox::Input::Completion(_) => "completion",
    }
}
