//! Team task board canonical aggregate contract tests.

use std::fs::OpenOptions;
use std::io::Write;

use qaqh_session::canonical::generate_ulid;
use qaqh_session::session_fact_v2::{
    AgentPath, ContentHash, ContentRef, EventId, LogId, SessionId,
};
use qaqh_session::team::{
    TaskAcceptanceSet, TaskArtifactAttached, TaskCancelled, TaskClaimed, TaskClosed, TaskCompleted,
    TaskCreated, TaskDependencyAdded, TaskId, TaskReleased, TaskState, TeamActor, TeamCreated,
    TeamFact, TeamId, TeamPayload, TeamStore, new_team_schema,
};

fn team_id() -> TeamId {
    TeamId::new(SessionId::new("0198f0a0-0000-7000-8000-000000000001"))
}

fn actor(path: &str) -> TeamActor {
    TeamActor::new(AgentPath::parse_absolute(path).expect("agent path"), None)
}

fn fact(team_id: &TeamId, actor: TeamActor, payload: TeamPayload) -> TeamFact {
    TeamFact {
        schema: new_team_schema(),
        team_id: team_id.clone(),
        log_id: LogId::new("team-test-log"),
        fact_seq: 0,
        event_id: EventId::new(generate_ulid()),
        ts_ms: 1_789_830_000_000,
        causation_id: None,
        actor,
        payload,
    }
}

fn created(team_id: &TeamId) -> TeamFact {
    fact(
        team_id,
        actor("/root"),
        TeamPayload::TeamCreated(TeamCreated {
            root_session_id: team_id.0.clone(),
            created_at_ms: 1_789_830_000_000,
        }),
    )
}

fn task_created(team_id: &TeamId, task_id: &TaskId, title: &str) -> TeamFact {
    fact(
        team_id,
        actor("/root"),
        TeamPayload::TaskCreated(TaskCreated {
            task_id: task_id.clone(),
            title: title.to_string(),
            description_ref: None,
            created_by: actor("/root"),
            created_at_ms: 1_789_830_000_001,
        }),
    )
}

fn open(temp: &tempfile::TempDir) -> TeamStore {
    let id = team_id();
    TeamStore::open_or_create(temp.path(), id, 1_789_830_000_000).expect("open team store")
}

#[test]
fn create_claim_release_and_replay() {
    let temp = tempfile::tempdir().expect("tempdir");
    let id = team_id();
    let task = TaskId::generate();
    let worker = actor("/root/worker");

    let mut store = open(&temp);
    store.append(created(&id)).expect("team created");
    store
        .append(task_created(&id, &task, "first task"))
        .expect("task created");
    store
        .append(fact(
            &id,
            worker.clone(),
            TeamPayload::TaskClaimed(TaskClaimed {
                task_id: task.clone(),
                owner: worker.clone(),
                claim_epoch: 1,
                claimed_at_ms: 1_789_830_000_002,
            }),
        ))
        .expect("claim");
    assert_eq!(store.snapshot().tasks[0].state, TaskState::Claimed);
    assert_eq!(store.snapshot().tasks[0].claim_epoch, 1);

    store
        .append(fact(
            &id,
            worker.clone(),
            TeamPayload::TaskReleased(TaskReleased {
                task_id: task.clone(),
                owner: worker.clone(),
                claim_epoch: 1,
                reason: "yielding".into(),
                released_at_ms: 1_789_830_000_003,
            }),
        ))
        .expect("release");
    assert_eq!(store.snapshot().tasks[0].state, TaskState::Open);
    assert!(store.snapshot().tasks[0].owner.is_none());

    let online = store.snapshot();
    let reopened = open(&temp);
    assert_eq!(reopened.snapshot(), online, "replay must be equivalent");
}

#[test]
fn stale_claim_and_complete_are_rejected() {
    let temp = tempfile::tempdir().expect("tempdir");
    let id = team_id();
    let task = TaskId::generate();
    let worker = actor("/root/worker");

    let mut store = open(&temp);
    store.append(created(&id)).expect("team created");
    store
        .append(task_created(&id, &task, "task"))
        .expect("task created");
    store
        .append(fact(
            &id,
            worker.clone(),
            TeamPayload::TaskClaimed(TaskClaimed {
                task_id: task.clone(),
                owner: worker.clone(),
                claim_epoch: 1,
                claimed_at_ms: 1_789_830_000_002,
            }),
        ))
        .expect("claim");

    let stale_claim = store.append(fact(
        &id,
        worker.clone(),
        TeamPayload::TaskClaimed(TaskClaimed {
            task_id: task.clone(),
            owner: worker.clone(),
            claim_epoch: 3,
            claimed_at_ms: 1_789_830_000_003,
        }),
    ));
    assert!(stale_claim.is_err(), "stale claim epoch must reject");

    let stale_complete = store.append(fact(
        &id,
        worker.clone(),
        TeamPayload::TaskCompleted(TaskCompleted {
            task_id: task.clone(),
            owner: worker,
            claim_epoch: 2,
            result_ref: None,
            completed_at_ms: 1_789_830_000_004,
        }),
    ));
    assert!(stale_complete.is_err(), "stale complete epoch must reject");
    assert_eq!(store.snapshot().tasks[0].state, TaskState::Claimed);
}

#[test]
fn dependency_gate_and_cycle_are_enforced() {
    let temp = tempfile::tempdir().expect("tempdir");
    let id = team_id();
    let task_a = TaskId::generate();
    let task_b = TaskId::generate();
    let worker = actor("/root/worker");

    let mut store = open(&temp);
    store.append(created(&id)).expect("team created");
    store
        .append(task_created(&id, &task_a, "A"))
        .expect("task A");
    store
        .append(task_created(&id, &task_b, "B"))
        .expect("task B");
    store
        .append(fact(
            &id,
            worker.clone(),
            TeamPayload::TaskDependencyAdded(TaskDependencyAdded {
                task_id: task_b.clone(),
                depends_on: task_a.clone(),
                added_at_ms: 1_789_830_000_002,
            }),
        ))
        .expect("B depends on A");

    // B cannot complete while A is open.
    store
        .append(fact(
            &id,
            worker.clone(),
            TeamPayload::TaskClaimed(TaskClaimed {
                task_id: task_b.clone(),
                owner: worker.clone(),
                claim_epoch: 1,
                claimed_at_ms: 1_789_830_000_003,
            }),
        ))
        .expect("claim B");
    let blocked = store.append(fact(
        &id,
        worker.clone(),
        TeamPayload::TaskCompleted(TaskCompleted {
            task_id: task_b.clone(),
            owner: worker.clone(),
            claim_epoch: 1,
            result_ref: None,
            completed_at_ms: 1_789_830_000_004,
        }),
    ));
    assert!(blocked.is_err(), "dependency must gate completion");

    // A -> B would create a cycle.
    let cycle = store.append(fact(
        &id,
        worker,
        TeamPayload::TaskDependencyAdded(TaskDependencyAdded {
            task_id: task_a,
            depends_on: task_b,
            added_at_ms: 1_789_830_000_005,
        }),
    ));
    assert!(cycle.is_err(), "dependency cycle must reject");
}

#[test]
fn artifacts_acceptance_complete_and_close() {
    let temp = tempfile::tempdir().expect("tempdir");
    let id = team_id();
    let task = TaskId::generate();
    let worker = actor("/root/worker");
    let artifact_ref = ContentRef::new(ContentHash::new(format!("sha256:{}", "a".repeat(64))));

    let mut store = open(&temp);
    store.append(created(&id)).expect("team created");
    store
        .append(task_created(&id, &task, "artifact task"))
        .expect("task created");
    store
        .append(fact(
            &id,
            worker.clone(),
            TeamPayload::TaskClaimed(TaskClaimed {
                task_id: task.clone(),
                owner: worker.clone(),
                claim_epoch: 1,
                claimed_at_ms: 1_789_830_000_002,
            }),
        ))
        .expect("claim");
    store
        .append(fact(
            &id,
            worker.clone(),
            TeamPayload::TaskArtifactAttached(TaskArtifactAttached {
                task_id: task.clone(),
                artifact_ref: artifact_ref.clone(),
                media_type: "text/plain".into(),
                added_at_ms: 1_789_830_000_003,
            }),
        ))
        .expect("artifact");
    store
        .append(fact(
            &id,
            worker.clone(),
            TeamPayload::TaskAcceptanceSet(TaskAcceptanceSet {
                task_id: task.clone(),
                acceptance: vec!["tests pass".into()],
                updated_at_ms: 1_789_830_000_004,
            }),
        ))
        .expect("acceptance");
    store
        .append(fact(
            &id,
            worker.clone(),
            TeamPayload::TaskCompleted(TaskCompleted {
                task_id: task.clone(),
                owner: worker.clone(),
                claim_epoch: 1,
                result_ref: Some(artifact_ref.clone()),
                completed_at_ms: 1_789_830_000_005,
            }),
        ))
        .expect("complete");
    assert_eq!(store.snapshot().tasks[0].state, TaskState::Completed);
    assert_eq!(store.snapshot().tasks[0].artifacts.len(), 1);

    store
        .append(fact(
            &id,
            worker.clone(),
            TeamPayload::TaskClosed(TaskClosed {
                task_id: task.clone(),
                closed_by: worker.clone(),
                closed_at_ms: 1_789_830_000_006,
            }),
        ))
        .expect("close");
    assert_eq!(store.snapshot().tasks[0].state, TaskState::Closed);

    let after_terminal = store.append(fact(
        &id,
        worker,
        TeamPayload::TaskArtifactAttached(TaskArtifactAttached {
            task_id: task,
            artifact_ref,
            media_type: "text/plain".into(),
            added_at_ms: 1_789_830_000_007,
        }),
    ));
    assert!(after_terminal.is_err(), "terminal task rejects artifacts");
}

#[test]
fn cancel_is_terminal_and_replay_is_equivalent() {
    let temp = tempfile::tempdir().expect("tempdir");
    let id = team_id();
    let task = TaskId::generate();
    let worker = actor("/root/worker");

    let mut store = open(&temp);
    store.append(created(&id)).expect("team created");
    store
        .append(task_created(&id, &task, "cancel me"))
        .expect("task created");
    store
        .append(fact(
            &id,
            worker.clone(),
            TeamPayload::TaskCancelled(TaskCancelled {
                task_id: task.clone(),
                cancelled_by: worker.clone(),
                reason: "no longer needed".into(),
                cancelled_at_ms: 1_789_830_000_002,
            }),
        ))
        .expect("cancel");
    assert_eq!(store.snapshot().tasks[0].state, TaskState::Cancelled);

    let after_cancel = store.append(fact(
        &id,
        worker,
        TeamPayload::TaskClaimed(TaskClaimed {
            task_id: task,
            owner: actor("/root/other"),
            claim_epoch: 1,
            claimed_at_ms: 1_789_830_000_003,
        }),
    ));
    assert!(after_cancel.is_err(), "cancelled task is terminal");

    let online = store.snapshot();
    let reopened = open(&temp);
    assert_eq!(reopened.snapshot(), online);
}

#[test]
fn torn_tail_is_truncated_on_reopen() {
    let temp = tempfile::tempdir().expect("tempdir");
    let id = team_id();
    let task = TaskId::generate();

    let mut store = open(&temp);
    store.append(created(&id)).expect("team created");
    store
        .append(task_created(&id, &task, "torn tail"))
        .expect("task created");
    let expected = store.snapshot();

    {
        let mut events = OpenOptions::new()
            .append(true)
            .open(temp.path().join("events.jsonl"))
            .expect("open events");
        events.write_all(b"{\"torn\":").expect("write torn tail");
        events.sync_all().expect("sync torn tail");
    }

    let reopened = open(&temp);
    assert_eq!(
        reopened.snapshot(),
        expected,
        "torn tail must not affect committed projection"
    );
    let events = std::fs::read(temp.path().join("events.jsonl")).expect("read events");
    assert!(
        !events.ends_with(b"{\"torn\":"),
        "torn tail must be truncated to committed offset"
    );
}
