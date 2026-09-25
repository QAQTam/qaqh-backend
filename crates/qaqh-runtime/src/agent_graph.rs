//! Canonical agent-graph loader.
//!
//! A root graph is reconstructed by walking parent logs and then each direct
//! child log recursively. Any missing log, missing path, or conflicting edge
//! aborts the load; callers must not fall back to an in-memory guess.

use std::collections::{HashSet, VecDeque};

use qaqh_session::SessionManager;
use qaqh_session::canonical::{CanonicalSessionIdentity, CommittedFactReader};
use qaqh_session::projection::AgentGraphStore;
use qaqh_session::session_fact_v2::{FactPayload, SessionFact, SessionId};

pub(crate) fn load_agent_graph(
    sessions: &SessionManager,
    root_seed: &str,
) -> Result<AgentGraphStore, String> {
    let root_dir = sessions.session_path_dir(root_seed);
    let root_identity = CanonicalSessionIdentity::open(&root_dir)
        .map_err(|error| format!("open root canonical identity for {root_seed}: {error}"))?;
    let root_id = root_identity.session_id;
    let mut graph = AgentGraphStore::new(root_id.clone());
    let mut pending = VecDeque::from([root_id.clone()]);
    let mut visited = HashSet::new();

    while let Some(session_id) = pending.pop_front() {
        if !visited.insert(session_id.clone()) {
            return Err(format!(
                "agent graph cycle while loading session {session_id}"
            ));
        }
        let expected_parent = graph
            .get_edge(&session_id)
            .map(|edge| edge.parent_agent_id.clone());
        let facts = read_canonical_facts(sessions, &session_id)?;
        validate_parent_hint(&session_id, expected_parent.as_ref(), &facts)?;
        for fact in &facts {
            graph
                .apply_fact(fact)
                .map_err(|error| format!("apply agent graph fact: {error}"))?;
        }
        for child_id in graph.children_of(&session_id) {
            if !visited.contains(&child_id) {
                pending.push_back(child_id);
            }
        }
    }

    Ok(graph)
}

fn read_canonical_facts(
    sessions: &SessionManager,
    session_id: &SessionId,
) -> Result<Vec<SessionFact>, String> {
    let session_dir = sessions
        .session_dir_for_id(session_id.as_str())?
        .ok_or_else(|| format!("agent graph session {session_id} has no canonical directory"))?;
    let identity = CanonicalSessionIdentity::open(&session_dir)
        .map_err(|error| format!("open canonical identity for {session_id}: {error}"))?;
    if &identity.session_id != session_id {
        return Err(format!(
            "agent graph session {session_id} identity is {}",
            identity.session_id
        ));
    }
    let reader = CommittedFactReader::open(
        &session_dir,
        identity.session_id.clone(),
        identity.log_id.clone(),
    )
    .map_err(|error| format!("open canonical facts for {session_id}: {error}"))?;
    reader
        .read_all()
        .map_err(|error| format!("read canonical facts for {session_id}: {error}"))
}

fn validate_parent_hint(
    session_id: &SessionId,
    expected_parent: Option<&SessionId>,
    facts: &[SessionFact],
) -> Result<(), String> {
    if expected_parent.is_none() {
        return Ok(());
    }
    let observed = facts.iter().find_map(|fact| match &fact.payload {
        FactPayload::SessionCreated(created) => created.parent_session_id.clone(),
        _ => None,
    });
    match (observed, expected_parent) {
        (Some(observed), Some(expected)) if &observed == expected => Ok(()),
        (Some(observed), Some(expected)) => Err(format!(
            "agent graph session {session_id} records parent {observed}, expected {expected}"
        )),
        (None, Some(expected)) => Err(format!(
            "agent graph session {session_id} has no canonical SessionCreated parent hint for {expected}"
        )),
        (_, None) => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qaqh_session::canonical::{CanonicalLog, WriterId, generate_ulid};
    use qaqh_session::session_fact_v2::{
        AgentPath, EventId, FactSchema, SessionCreated, SubagentSpawned, ToolCallId,
    };

    const NOW_MS: i64 = 1_789_830_000_000;
    const LEASE_MS: i64 = 10_000;

    #[test]
    fn rebuilds_from_legacy_seed_directories_via_identity_sidecars() {
        let temp = tempfile::tempdir().expect("tempdir");
        let sessions_dir = temp.path().join("sessions");
        std::fs::create_dir_all(&sessions_dir).expect("sessions dir");
        let manager =
            SessionManager::new_for_test(sessions_dir.clone(), temp.path().join("active"));

        let root_dir = sessions_dir.join("root-seed");
        let root_identity =
            CanonicalSessionIdentity::open_or_create(&root_dir).expect("root identity");
        let child_dir = sessions_dir.join("child-seed");
        let child_identity =
            CanonicalSessionIdentity::open_or_create(&child_dir).expect("child identity");

        let mut root_log = CanonicalLog::open(
            &root_dir,
            root_identity.session_id.clone(),
            root_identity.log_id.clone(),
        )
        .expect("root log");
        let root_lease = root_log
            .acquire_writer(WriterId::new("graph-root"), NOW_MS, LEASE_MS)
            .expect("root lease");
        root_log
            .append(
                &root_lease,
                fact(
                    &root_identity,
                    1,
                    FactPayload::SessionCreated(SessionCreated {
                        created_at_ms: NOW_MS,
                        cwd: "/tmp/root".to_string(),
                        model: "test".to_string(),
                        parent_session_id: None,
                        schema_caps: Vec::new(),
                    }),
                ),
                NOW_MS,
            )
            .expect("root created");
        root_log
            .append(
                &root_lease,
                fact(
                    &root_identity,
                    2,
                    FactPayload::SubagentSpawned(SubagentSpawned {
                        child_session_id: child_identity.session_id.clone(),
                        parent_call_id: ToolCallId::new("call_01J00000000000000000000000"),
                        parent_agent_path: Some(AgentPath::root()),
                        child_agent_path: Some(
                            AgentPath::parse_absolute("/root/review").expect("child path"),
                        ),
                        role: Some("review".to_string()),
                        spawned_at_ms: NOW_MS,
                    }),
                ),
                NOW_MS,
            )
            .expect("spawn edge");

        let mut child_log = CanonicalLog::open(
            &child_dir,
            child_identity.session_id.clone(),
            child_identity.log_id.clone(),
        )
        .expect("child log");
        let child_lease = child_log
            .acquire_writer(WriterId::new("graph-child"), NOW_MS, LEASE_MS)
            .expect("child lease");
        child_log
            .append(
                &child_lease,
                fact(
                    &child_identity,
                    1,
                    FactPayload::SessionCreated(SessionCreated {
                        created_at_ms: NOW_MS,
                        cwd: "/tmp/root".to_string(),
                        model: "test".to_string(),
                        parent_session_id: Some(root_identity.session_id.clone()),
                        schema_caps: Vec::new(),
                    }),
                ),
                NOW_MS,
            )
            .expect("child created");

        let graph = load_agent_graph(&manager, "root-seed").expect("load graph");
        let snapshot = graph.snapshot();
        assert_eq!(snapshot.root_session_id, Some(root_identity.session_id));
        assert_eq!(snapshot.nodes.len(), 2);
        assert_eq!(snapshot.edges.len(), 1);
        assert_eq!(snapshot.edges[0].child_agent_id, child_identity.session_id);
    }

    fn fact(
        identity: &CanonicalSessionIdentity,
        fact_seq: u64,
        payload: FactPayload,
    ) -> SessionFact {
        SessionFact {
            schema: FactSchema::v2(),
            session_id: identity.session_id.clone(),
            log_id: identity.log_id.clone(),
            fact_seq,
            event_id: EventId::new(generate_ulid()),
            ts_ms: NOW_MS,
            causation_id: None,
            turn_id: None,
            call_id: None,
            interaction_id: None,
            payload,
        }
    }
}
