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
    let root_id = SessionId::new(root_seed);
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
    let session_dir = sessions.session_path_dir(session_id.as_str());
    if !session_dir.exists() {
        return Err(format!(
            "agent graph session {} is missing at {}",
            session_id,
            session_dir.display()
        ));
    }
    let identity = CanonicalSessionIdentity::open_or_create(&session_dir)
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
