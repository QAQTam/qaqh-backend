//! Rebuildable task board projection over the TeamFact log.

use serde::{Deserialize, Serialize};

use super::error::{TeamError, TeamResult};
use super::types::{
    TaskArtifact, TaskCreated, TaskId, TaskState, TeamActor, TeamFact, TeamId, TeamPayload,
};

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskBoardSnapshot {
    pub team_id: Option<TeamId>,
    pub revision: u64,
    pub last_fact_seq: u64,
    pub tasks: Vec<TaskView>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", content = "data", rename_all = "snake_case")]
pub enum TaskBoardDelta {
    TeamCreated {
        revision: u64,
        team_id: TeamId,
    },
    TaskCreated {
        revision: u64,
        task: Box<TaskView>,
    },
    TaskClaimed {
        revision: u64,
        task_id: TaskId,
        owner: TeamActor,
        claim_epoch: u64,
        claimed_at_ms: i64,
    },
    TaskReleased {
        revision: u64,
        task_id: TaskId,
        previous_owner: TeamActor,
        claim_epoch: u64,
        released_at_ms: i64,
    },
    TaskDependencyAdded {
        revision: u64,
        task_id: TaskId,
        depends_on: TaskId,
        added_at_ms: i64,
    },
    TaskArtifactAttached {
        revision: u64,
        task_id: TaskId,
        artifact: TaskArtifact,
    },
    TaskAcceptanceSet {
        revision: u64,
        task_id: TaskId,
        acceptance: Vec<String>,
        updated_at_ms: i64,
    },
    TaskCompleted {
        revision: u64,
        task_id: TaskId,
        result_ref: Option<crate::session_fact_v2::ContentRef>,
        completed_at_ms: i64,
    },
    TaskClosed {
        revision: u64,
        task_id: TaskId,
        closed_at_ms: i64,
    },
    TaskCancelled {
        revision: u64,
        task_id: TaskId,
        reason: String,
        cancelled_at_ms: i64,
    },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct TaskView {
    pub task_id: TaskId,
    pub title: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description_ref: Option<crate::session_fact_v2::ContentRef>,
    pub state: TaskState,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<TeamActor>,
    pub claim_epoch: u64,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub depends_on: Vec<TaskId>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub artifacts: Vec<TaskArtifact>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub acceptance: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result_ref: Option<crate::session_fact_v2::ContentRef>,
    pub created_by: TeamActor,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

#[derive(Debug, Clone, Default)]
pub struct TaskBoardProjection {
    snapshot: TaskBoardSnapshot,
}

impl TaskBoardProjection {
    pub fn snapshot(&self) -> &TaskBoardSnapshot {
        &self.snapshot
    }

    pub fn into_snapshot(self) -> TaskBoardSnapshot {
        self.snapshot
    }

    pub fn task(&self, task_id: &TaskId) -> Option<&TaskView> {
        self.snapshot
            .tasks
            .iter()
            .find(|task| &task.task_id == task_id)
    }

    /// Validate a fact against the current committed projection. The store
    /// calls this before appending so invalid transitions never enter the log.
    pub fn validate(&self, fact: &TeamFact) -> TeamResult<()> {
        match &fact.payload {
            TeamPayload::TeamCreated(payload) => {
                if self.snapshot.team_id.is_some() {
                    return Err(TeamError::Validation(
                        "TeamCreated may only appear once".into(),
                    ));
                }
                if payload.root_session_id != fact.team_id.0 {
                    return Err(TeamError::Validation(
                        "TeamCreated root_session_id must equal team_id".into(),
                    ));
                }
                Ok(())
            }
            TeamPayload::TaskCreated(payload) => {
                self.require_team(fact)?;
                if self.task(&payload.task_id).is_some() {
                    return Err(TeamError::TaskAlreadyExists(
                        payload.task_id.as_str().to_string(),
                    ));
                }
                Ok(())
            }
            TeamPayload::TaskClaimed(payload) => {
                let task = self.require_task(&payload.task_id)?;
                if task.state != TaskState::Open {
                    return Err(TeamError::Validation(format!(
                        "task {} is {:?}; claim requires Open",
                        payload.task_id, task.state
                    )));
                }
                if payload.owner != fact.actor {
                    return Err(TeamError::Validation(
                        "claim owner must equal the fact actor".into(),
                    ));
                }
                let expected_epoch = task.claim_epoch.saturating_add(1);
                if payload.claim_epoch != expected_epoch {
                    return Err(TeamError::Validation(format!(
                        "stale claim epoch for {}: expected {expected_epoch}, got {}",
                        payload.task_id, payload.claim_epoch
                    )));
                }
                Ok(())
            }
            TeamPayload::TaskReleased(payload) => {
                let task = self.require_claimed_task(payload.task_id.as_str(), &payload.owner)?;
                if payload.claim_epoch != task.claim_epoch {
                    return Err(stale_epoch(
                        &payload.task_id,
                        task.claim_epoch,
                        payload.claim_epoch,
                    ));
                }
                if payload.owner != fact.actor {
                    return Err(TeamError::Validation(
                        "release owner must equal the fact actor".into(),
                    ));
                }
                Ok(())
            }
            TeamPayload::TaskDependencyAdded(payload) => {
                let task = self.require_task(&payload.task_id)?;
                if task.state.is_terminal() {
                    return Err(TeamError::Validation(format!(
                        "task {} is terminal; cannot add dependencies",
                        payload.task_id
                    )));
                }
                if self.task(&payload.depends_on).is_none() {
                    return Err(TeamError::TaskNotFound(
                        payload.depends_on.as_str().to_string(),
                    ));
                }
                if task.depends_on.contains(&payload.depends_on) {
                    return Err(TeamError::Validation(format!(
                        "task {} already depends on {}",
                        payload.task_id, payload.depends_on
                    )));
                }
                if self.depends_on_transitively(&payload.depends_on, &payload.task_id) {
                    return Err(TeamError::Validation(format!(
                        "dependency edge {} -> {} would create a cycle",
                        payload.task_id, payload.depends_on
                    )));
                }
                Ok(())
            }
            TeamPayload::TaskArtifactAttached(payload) => {
                let task = self.require_task(&payload.task_id)?;
                if task.state.is_terminal() {
                    return Err(TeamError::Validation(format!(
                        "task {} is terminal; cannot attach artifacts",
                        payload.task_id
                    )));
                }
                Ok(())
            }
            TeamPayload::TaskAcceptanceSet(payload) => {
                let task = self.require_task(&payload.task_id)?;
                if task.state.is_terminal() {
                    return Err(TeamError::Validation(format!(
                        "task {} is terminal; cannot change acceptance",
                        payload.task_id
                    )));
                }
                Ok(())
            }
            TeamPayload::TaskCompleted(payload) => {
                let task = self.require_claimed_task(payload.task_id.as_str(), &payload.owner)?;
                if payload.claim_epoch != task.claim_epoch {
                    return Err(stale_epoch(
                        &payload.task_id,
                        task.claim_epoch,
                        payload.claim_epoch,
                    ));
                }
                if payload.owner != fact.actor {
                    return Err(TeamError::Validation(
                        "complete owner must equal the fact actor".into(),
                    ));
                }
                for dependency in &task.depends_on {
                    let Some(dep) = self.task(dependency) else {
                        return Err(TeamError::TaskNotFound(dependency.as_str().to_string()));
                    };
                    if !dep.state.satisfies_dependency() {
                        return Err(TeamError::Validation(format!(
                            "task {} cannot complete: dependency {} is {:?}",
                            payload.task_id, dependency, dep.state
                        )));
                    }
                }
                Ok(())
            }
            TeamPayload::TaskClosed(payload) => {
                let task = self.require_task(&payload.task_id)?;
                if task.state != TaskState::Completed {
                    return Err(TeamError::Validation(format!(
                        "task {} is {:?}; close requires Completed",
                        payload.task_id, task.state
                    )));
                }
                Ok(())
            }
            TeamPayload::TaskCancelled(payload) => {
                let task = self.require_task(&payload.task_id)?;
                if task.state.is_terminal() {
                    return Err(TeamError::Validation(format!(
                        "task {} is already {:?}",
                        payload.task_id, task.state
                    )));
                }
                Ok(())
            }
        }
    }

    /// Apply a validated fact. Returns the delta for the append outcome.
    pub fn apply(&mut self, fact: &TeamFact) -> TeamResult<TaskBoardDelta> {
        self.validate(fact)?;
        self.snapshot.last_fact_seq = fact.fact_seq;
        self.snapshot.revision = self.snapshot.revision.saturating_add(1);
        let revision = self.snapshot.revision;
        match &fact.payload {
            TeamPayload::TeamCreated(_) => {
                self.snapshot.team_id = Some(fact.team_id.clone());
                Ok(TaskBoardDelta::TeamCreated {
                    revision,
                    team_id: fact.team_id.clone(),
                })
            }
            TeamPayload::TaskCreated(payload) => {
                let task = TaskView::from_created(payload, fact.ts_ms);
                self.snapshot.tasks.push(task.clone());
                Ok(TaskBoardDelta::TaskCreated {
                    revision,
                    task: Box::new(task),
                })
            }
            TeamPayload::TaskClaimed(payload) => {
                let task = self.task_mut(&payload.task_id)?;
                task.state = TaskState::Claimed;
                task.owner = Some(payload.owner.clone());
                task.claim_epoch = payload.claim_epoch;
                task.updated_at_ms = payload.claimed_at_ms;
                Ok(TaskBoardDelta::TaskClaimed {
                    revision,
                    task_id: payload.task_id.clone(),
                    owner: payload.owner.clone(),
                    claim_epoch: payload.claim_epoch,
                    claimed_at_ms: payload.claimed_at_ms,
                })
            }
            TeamPayload::TaskReleased(payload) => {
                let task = self.task_mut(&payload.task_id)?;
                task.state = TaskState::Open;
                task.owner = None;
                task.updated_at_ms = payload.released_at_ms;
                Ok(TaskBoardDelta::TaskReleased {
                    revision,
                    task_id: payload.task_id.clone(),
                    previous_owner: payload.owner.clone(),
                    claim_epoch: payload.claim_epoch,
                    released_at_ms: payload.released_at_ms,
                })
            }
            TeamPayload::TaskDependencyAdded(payload) => {
                let task = self.task_mut(&payload.task_id)?;
                task.depends_on.push(payload.depends_on.clone());
                task.updated_at_ms = payload.added_at_ms;
                Ok(TaskBoardDelta::TaskDependencyAdded {
                    revision,
                    task_id: payload.task_id.clone(),
                    depends_on: payload.depends_on.clone(),
                    added_at_ms: payload.added_at_ms,
                })
            }
            TeamPayload::TaskArtifactAttached(payload) => {
                let artifact = TaskArtifact {
                    artifact_ref: payload.artifact_ref.clone(),
                    media_type: payload.media_type.clone(),
                    added_at_ms: payload.added_at_ms,
                };
                let task = self.task_mut(&payload.task_id)?;
                task.artifacts.push(artifact.clone());
                task.updated_at_ms = payload.added_at_ms;
                Ok(TaskBoardDelta::TaskArtifactAttached {
                    revision,
                    task_id: payload.task_id.clone(),
                    artifact,
                })
            }
            TeamPayload::TaskAcceptanceSet(payload) => {
                let task = self.task_mut(&payload.task_id)?;
                task.acceptance = payload.acceptance.clone();
                task.updated_at_ms = payload.updated_at_ms;
                Ok(TaskBoardDelta::TaskAcceptanceSet {
                    revision,
                    task_id: payload.task_id.clone(),
                    acceptance: payload.acceptance.clone(),
                    updated_at_ms: payload.updated_at_ms,
                })
            }
            TeamPayload::TaskCompleted(payload) => {
                let task = self.task_mut(&payload.task_id)?;
                task.state = TaskState::Completed;
                task.result_ref = payload.result_ref.clone();
                task.updated_at_ms = payload.completed_at_ms;
                Ok(TaskBoardDelta::TaskCompleted {
                    revision,
                    task_id: payload.task_id.clone(),
                    result_ref: payload.result_ref.clone(),
                    completed_at_ms: payload.completed_at_ms,
                })
            }
            TeamPayload::TaskClosed(payload) => {
                let task = self.task_mut(&payload.task_id)?;
                task.state = TaskState::Closed;
                task.updated_at_ms = payload.closed_at_ms;
                Ok(TaskBoardDelta::TaskClosed {
                    revision,
                    task_id: payload.task_id.clone(),
                    closed_at_ms: payload.closed_at_ms,
                })
            }
            TeamPayload::TaskCancelled(payload) => {
                let task = self.task_mut(&payload.task_id)?;
                task.state = TaskState::Cancelled;
                task.updated_at_ms = payload.cancelled_at_ms;
                Ok(TaskBoardDelta::TaskCancelled {
                    revision,
                    task_id: payload.task_id.clone(),
                    reason: payload.reason.clone(),
                    cancelled_at_ms: payload.cancelled_at_ms,
                })
            }
        }
    }

    fn require_team(&self, fact: &TeamFact) -> TeamResult<()> {
        match &self.snapshot.team_id {
            Some(team_id) if team_id == &fact.team_id => Ok(()),
            Some(team_id) => Err(TeamError::IdentityMismatch {
                expected: team_id.as_str().to_string(),
                actual: fact.team_id.as_str().to_string(),
            }),
            None => Err(TeamError::MissingTeamCreated),
        }
    }

    fn require_task(&self, task_id: &TaskId) -> TeamResult<&TaskView> {
        self.task(task_id)
            .ok_or_else(|| TeamError::TaskNotFound(task_id.as_str().to_string()))
    }

    fn require_claimed_task(&self, task_id: &str, owner: &TeamActor) -> TeamResult<&TaskView> {
        let task_id = TaskId::new(task_id);
        let task = self.require_task(&task_id)?;
        if task.state != TaskState::Claimed {
            return Err(TeamError::Validation(format!(
                "task {task_id} is {:?}; operation requires Claimed",
                task.state
            )));
        }
        if task.owner.as_ref() != Some(owner) {
            return Err(TeamError::Validation(format!(
                "task {task_id} is owned by a different actor"
            )));
        }
        Ok(task)
    }

    fn task_mut(&mut self, task_id: &TaskId) -> TeamResult<&mut TaskView> {
        self.snapshot
            .tasks
            .iter_mut()
            .find(|task| &task.task_id == task_id)
            .ok_or_else(|| TeamError::TaskNotFound(task_id.as_str().to_string()))
    }

    fn depends_on_transitively(&self, from: &TaskId, target: &TaskId) -> bool {
        let mut stack = vec![from.clone()];
        let mut seen = std::collections::HashSet::new();
        while let Some(current) = stack.pop() {
            if &current == target {
                return true;
            }
            if !seen.insert(current.clone()) {
                continue;
            }
            if let Some(task) = self.task(&current) {
                stack.extend(task.depends_on.iter().cloned());
            }
        }
        false
    }
}

impl TaskView {
    fn from_created(payload: &TaskCreated, ts_ms: i64) -> Self {
        Self {
            task_id: payload.task_id.clone(),
            title: payload.title.clone(),
            description_ref: payload.description_ref.clone(),
            state: TaskState::Open,
            owner: None,
            claim_epoch: 0,
            depends_on: Vec::new(),
            artifacts: Vec::new(),
            acceptance: Vec::new(),
            result_ref: None,
            created_by: payload.created_by.clone(),
            created_at_ms: payload.created_at_ms,
            updated_at_ms: ts_ms,
        }
    }
}

fn stale_epoch(task_id: &TaskId, expected: u64, actual: u64) -> TeamError {
    TeamError::Validation(format!(
        "stale claim epoch for {task_id}: expected {expected}, got {actual}"
    ))
}
