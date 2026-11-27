use qaqh_core_vocab::{ContentRef, SessionId};
use qaqh_ringing_contract::fact::{ContentRef as StoredContentRef, InterAgentActor};
use qaqh_ringing_runtime::RingingHub;
use qaqh_ringing_v3::{
    RpcError, TaskState, TaskView, TeamAgentResidency, TeamAgentSnapshot, TeamAgentStatus,
    TeamInboxSummary, TeamOverview, TeamSnapshot,
};

use super::super::{RealBackend, internal, projection};
use crate::v3_server::state::ServerState;

pub(super) async fn team(
    backend: &RealBackend,
    session_id: SessionId,
) -> Result<TeamOverview, RpcError> {
    let service = backend.service.clone();
    let raw = session_id.as_str().to_string();
    let (children, names, envelopes, tasks) = RealBackend::blocking({
        let raw = raw.clone();
        move || {
            let children = service
                .owners()
                .session_children(&raw)
                .map_err(|error| error.to_string())?;
            let names = service
                .owners()
                .db()
                .read_blocking(qaqh_session::db::view::spawn_names)
                .map_err(|error| error.to_string())?;
            let mut ids = vec![raw.clone()];
            ids.extend(children.iter().cloned());
            let envelopes = service
                .owners()
                .db()
                .read_blocking(|conn| {
                    qaqh_session::db::view::inter_agent_deliveries(conn, &ids, 64)
                })
                .map_err(|error| error.to_string())?;
            let tasks = service
                .task_board_snapshot(&raw)
                .map_err(|error| error.to_string())?;
            Ok::<_, String>((children, names, envelopes, tasks))
        }
    })
    .await?;
    let mut agents = vec![team_agent(backend, &session_id, "/root", None, None).await?];
    for child in children {
        let child = SessionId::new(child).map_err(|error| internal(error.to_string()))?;
        let path = format!("/root/{child}");
        agents.push(
            team_agent(
                backend,
                &child,
                &path,
                Some("/root"),
                names.get(child.as_str()),
            )
            .await?,
        );
    }
    let mut unread_messages = Vec::with_capacity(envelopes.len());
    for envelope in envelopes {
        let author = InterAgentActor::from_signature(&envelope.author)
            .ok_or_else(|| internal("corrupt inter-agent author"))?;
        let recipient = InterAgentActor::from_signature(&envelope.recipient)
            .ok_or_else(|| internal("corrupt inter-agent recipient"))?;
        unread_messages.push(TeamInboxSummary {
            message_id: envelope.message_id,
            author: actor_signature(author),
            recipient: actor_signature(recipient),
            task_id: envelope.task_id,
            delivery: delivery_name(envelope.delivery).to_string(),
            created_at_ms: envelope.created_at_ms,
        });
    }
    let mut task_views = Vec::with_capacity(tasks.tasks.len());
    for task in tasks.tasks {
        let description_ref = match task.description_ref {
            Some(value) => Some(stored_ref(&backend.hub, &backend.state, &raw, value).await?),
            None => None,
        };
        let result_ref = match task.result_ref {
            Some(value) => Some(stored_ref(&backend.hub, &backend.state, &raw, value).await?),
            None => None,
        };
        task_views.push(TaskView {
            task_id: task.task_id.to_string(),
            title: task.title,
            description_ref,
            state: match task.state {
                qaqh_ringing_contract::team::TaskState::Open => TaskState::Open,
                qaqh_ringing_contract::team::TaskState::Claimed => TaskState::Claimed,
                qaqh_ringing_contract::team::TaskState::Completed => TaskState::Completed,
                qaqh_ringing_contract::team::TaskState::Closed
                | qaqh_ringing_contract::team::TaskState::Cancelled => TaskState::Cancelled,
            },
            owner: task
                .owner
                .map(|owner| owner.agent_path.as_str().to_string()),
            claim_epoch: task.claim_epoch,
            depends_on: task
                .depends_on
                .into_iter()
                .map(|id| id.to_string())
                .collect(),
            artifacts: task
                .artifacts
                .into_iter()
                .map(|artifact| artifact.artifact_ref.hash().as_str().to_string())
                .collect(),
            acceptance: task.acceptance,
            write_scopes: task.write_scopes,
            ready: task.ready,
            write_scope_warnings: task.write_scope_warnings,
            result_ref,
            created_by: task.created_by.agent_path.as_str().to_string(),
            created_at_ms: task.created_at_ms,
            updated_at_ms: task.updated_at_ms,
        });
    }
    Ok(TeamOverview {
        session_id: session_id.clone(),
        team: TeamSnapshot {
            root_session_id: Some(session_id.clone()),
            agents,
            unread_messages,
            revision: 0,
            last_fact_seq: 0,
        },
        tasks: qaqh_ringing_v3::TaskBoardSnapshot {
            team_id: tasks.team_id.map(|id| id.as_str().to_string()),
            revision: tasks.revision,
            last_fact_seq: tasks.last_fact_seq,
            tasks: task_views,
        },
    })
}

async fn stored_ref(
    hub: &RingingHub,
    state: &ServerState,
    session_id: &str,
    value: StoredContentRef,
) -> Result<ContentRef, RpcError> {
    projection::content_ref(
        hub,
        state,
        session_id,
        qaqh_ringing_contract::fact::ContentValue::Ref { content_ref: value },
    )
    .await
    .map_err(internal)
}

async fn team_agent(
    backend: &RealBackend,
    id: &SessionId,
    path: &str,
    parent: Option<&str>,
    nickname: Option<&String>,
) -> Result<TeamAgentSnapshot, RpcError> {
    let service = backend.service.clone();
    let raw = id.as_str().to_string();
    let (status, loaded) = RealBackend::blocking(move || {
        let status = match service
            .owners()
            .session_run_state(&raw)
            .map_err(|error| error.to_string())?
        {
            qaqh_session::owner::SessionRunState::Running => TeamAgentStatus::Running,
            qaqh_session::owner::SessionRunState::Waiting => TeamAgentStatus::WaitingUser,
            qaqh_session::owner::SessionRunState::Cancelling => TeamAgentStatus::Running,
            qaqh_session::owner::SessionRunState::Idle => TeamAgentStatus::Idle,
            qaqh_session::owner::SessionRunState::NeverStarted => TeamAgentStatus::PendingInit,
        };
        Ok::<_, String>((
            status,
            service
                .agent_residency_loaded(&raw)
                .map_err(|error| error.to_string())?,
        ))
    })
    .await?;
    Ok(TeamAgentSnapshot {
        agent_id: id.clone(),
        agent_path: path.into(),
        nickname: nickname.cloned(),
        status,
        residency: if loaded {
            TeamAgentResidency::Loaded
        } else {
            TeamAgentResidency::Unloaded
        },
        parent_agent_path: parent.map(str::to_string),
    })
}

fn actor_signature(actor: InterAgentActor) -> String {
    match actor {
        InterAgentActor::Path(path) => path.as_str().to_string(),
        InterAgentActor::Session(id) => id.as_str().to_string(),
    }
}

fn delivery_name(delivery: qaqh_ringing_contract::fact::InterAgentDelivery) -> &'static str {
    match delivery {
        qaqh_ringing_contract::fact::InterAgentDelivery::Queue => "queue",
        qaqh_ringing_contract::fact::InterAgentDelivery::Trigger => "trigger",
        qaqh_ringing_contract::fact::InterAgentDelivery::Interrupt => "interrupt",
        qaqh_ringing_contract::fact::InterAgentDelivery::Steer => "steer",
        qaqh_ringing_contract::fact::InterAgentDelivery::Interject => "interject",
    }
}
