//! Rebuildable parent-child graph over canonical subagent facts.
//!
//! The store is an index, not a second source of truth. It is rebuilt from
//! committed `SubagentSpawned` / `SubagentFinished` facts and fails closed on
//! missing or conflicting topology instead of guessing an edge.

use std::collections::{HashMap, HashSet};
use std::fmt;

use serde::{Deserialize, Serialize};

use crate::session_fact_v2::{
    AgentPath, FactPayload, SessionFact, SessionId, SubagentFinished, SubagentSpawned,
    SubagentTerminalStatus, ToolCallId,
};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AgentGraphEdgeStatus {
    Open,
    Closed { terminal: SubagentTerminalStatus },
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentGraphNode {
    pub agent_id: SessionId,
    pub agent_path: AgentPath,
    pub parent_agent_path: Option<AgentPath>,
    pub role: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentGraphEdge {
    pub parent_agent_id: SessionId,
    pub child_agent_id: SessionId,
    pub parent_call_id: ToolCallId,
    pub status: AgentGraphEdgeStatus,
    pub created_at_ms: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub closed_at_ms: Option<i64>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentGraphSnapshot {
    pub root_session_id: Option<SessionId>,
    pub nodes: Vec<AgentGraphNode>,
    pub edges: Vec<AgentGraphEdge>,
    pub revision: u64,
    pub last_fact_seq: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentGraphError {
    MissingAgentPath {
        child_session_id: SessionId,
    },
    ParentMissing {
        parent_agent_id: SessionId,
    },
    ParentPathMismatch {
        parent_agent_id: SessionId,
        expected: AgentPath,
        actual: AgentPath,
    },
    ChildPathConflict {
        agent_id: SessionId,
        existing: AgentPath,
        incoming: AgentPath,
    },
    AgentPathConflict {
        agent_path: AgentPath,
        existing_agent_id: SessionId,
        incoming_agent_id: SessionId,
    },
    EdgeConflict {
        child_agent_id: SessionId,
    },
    EdgeMissing {
        child_agent_id: SessionId,
    },
    ParentCallMismatch {
        child_agent_id: SessionId,
    },
    Cycle {
        parent_agent_id: SessionId,
        child_agent_id: SessionId,
    },
    ClosedBeforeOpen {
        child_agent_id: SessionId,
    },
    Canonical {
        message: String,
    },
}

impl AgentGraphError {
    pub const fn code(&self) -> &'static str {
        match self {
            Self::MissingAgentPath { .. } => "agent_graph_missing_agent_path",
            Self::ParentMissing { .. } => "agent_graph_parent_missing",
            Self::ParentPathMismatch { .. } => "agent_graph_parent_path_mismatch",
            Self::ChildPathConflict { .. } => "agent_graph_child_path_conflict",
            Self::AgentPathConflict { .. } => "agent_graph_agent_path_conflict",
            Self::EdgeConflict { .. } => "agent_graph_edge_conflict",
            Self::EdgeMissing { .. } => "agent_graph_edge_missing",
            Self::ParentCallMismatch { .. } => "agent_graph_parent_call_mismatch",
            Self::Cycle { .. } => "agent_graph_cycle",
            Self::ClosedBeforeOpen { .. } => "agent_graph_closed_before_open",
            Self::Canonical { .. } => "agent_graph_canonical_read_failed",
        }
    }
}

impl fmt::Display for AgentGraphError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        let code = self.code();
        match self {
            Self::MissingAgentPath { child_session_id } => write!(
                formatter,
                "{code}: spawn fact for {child_session_id} has no canonical AgentPath"
            ),
            Self::ParentMissing { parent_agent_id } => {
                write!(
                    formatter,
                    "{code}: parent agent {parent_agent_id} is not in the graph"
                )
            }
            Self::ParentPathMismatch {
                parent_agent_id,
                expected,
                actual,
            } => write!(
                formatter,
                "{code}: parent {parent_agent_id} path is {actual}, expected {expected}"
            ),
            Self::ChildPathConflict {
                agent_id,
                existing,
                incoming,
            } => write!(
                formatter,
                "{code}: child {agent_id} path is {existing}, incoming path is {incoming}"
            ),
            Self::AgentPathConflict {
                agent_path,
                existing_agent_id,
                incoming_agent_id,
            } => write!(
                formatter,
                "{code}: path {agent_path} belongs to {existing_agent_id}, not {incoming_agent_id}"
            ),
            Self::EdgeConflict { child_agent_id } => write!(
                formatter,
                "{code}: child {child_agent_id} already has a different parent edge"
            ),
            Self::EdgeMissing { child_agent_id } => write!(
                formatter,
                "{code}: finish fact for {child_agent_id} has no open parent edge"
            ),
            Self::ParentCallMismatch { child_agent_id } => write!(
                formatter,
                "{code}: parent call id does not match the edge for {child_agent_id}"
            ),
            Self::Cycle {
                parent_agent_id,
                child_agent_id,
            } => write!(
                formatter,
                "{code}: edge {parent_agent_id} -> {child_agent_id} would create a cycle"
            ),
            Self::ClosedBeforeOpen { child_agent_id } => write!(
                formatter,
                "{code}: edge for {child_agent_id} was closed before it was opened"
            ),
            Self::Canonical { message } => {
                write!(formatter, "{code}: {message}")
            }
        }
    }
}

impl std::error::Error for AgentGraphError {}

/// Rebuildable parent-child graph for one root tree.
#[derive(Debug, Default)]
pub struct AgentGraphStore {
    snapshot: AgentGraphSnapshot,
    by_id: HashMap<SessionId, usize>,
    by_path: HashMap<AgentPath, SessionId>,
    edge_by_child: HashMap<SessionId, usize>,
}

impl AgentGraphStore {
    /// Create an empty graph and register the root `/root` node.
    pub fn new(root_session_id: SessionId) -> Self {
        let mut store = Self::default();
        store.snapshot.root_session_id = Some(root_session_id.clone());
        let root = AgentGraphNode {
            agent_id: root_session_id.clone(),
            agent_path: AgentPath::root(),
            parent_agent_path: None,
            role: None,
        };
        store
            .by_path
            .insert(root.agent_path.clone(), root.agent_id.clone());
        store.by_id.insert(root.agent_id.clone(), 0);
        store.snapshot.nodes.push(root);
        store
    }

    /// Rebuild from committed facts belonging to one root tree.
    pub fn rebuild(
        root_session_id: SessionId,
        facts: impl IntoIterator<Item = SessionFact>,
    ) -> Result<Self, AgentGraphError> {
        let mut store = Self::new(root_session_id);
        for fact in facts {
            store.apply_fact(&fact)?;
        }
        Ok(store)
    }

    /// Rebuild from the committed prefix of one canonical session log.
    pub fn rebuild_from_reader(
        reader: &crate::canonical::CommittedFactReader,
    ) -> Result<Self, AgentGraphError> {
        let facts = reader
            .read_all()
            .map_err(|error| AgentGraphError::Canonical {
                message: error.to_string(),
            })?;
        Self::rebuild(reader.session_id().clone(), facts)
    }

    /// Apply one committed fact. Repeating an identical fact is idempotent.
    pub fn apply_fact(&mut self, fact: &SessionFact) -> Result<(), AgentGraphError> {
        self.snapshot.last_fact_seq = self.snapshot.last_fact_seq.max(fact.fact_seq);
        match &fact.payload {
            FactPayload::SubagentSpawned(payload) => self.apply_spawned(fact, payload),
            FactPayload::SubagentFinished(payload) => self.apply_finished(fact, payload),
            _ => Ok(()),
        }
    }

    pub fn snapshot(&self) -> AgentGraphSnapshot {
        let mut snapshot = self.snapshot.clone();
        snapshot
            .nodes
            .sort_by(|left, right| left.agent_path.as_str().cmp(right.agent_path.as_str()));
        snapshot.edges.sort_by(|left, right| {
            left.child_agent_id
                .as_str()
                .cmp(right.child_agent_id.as_str())
        });
        snapshot
    }

    pub fn root_session_id(&self) -> Option<&SessionId> {
        self.snapshot.root_session_id.as_ref()
    }

    pub fn get_node(&self, agent_id: &SessionId) -> Option<&AgentGraphNode> {
        self.by_id
            .get(agent_id)
            .and_then(|index| self.snapshot.nodes.get(*index))
    }

    pub fn get_node_by_path(&self, agent_path: &AgentPath) -> Option<&AgentGraphNode> {
        self.by_path
            .get(agent_path)
            .and_then(|agent_id| self.get_node(agent_id))
    }

    pub fn get_edge(&self, child_agent_id: &SessionId) -> Option<&AgentGraphEdge> {
        self.edge_by_child
            .get(child_agent_id)
            .and_then(|index| self.snapshot.edges.get(*index))
    }

    /// Direct children ordered by canonical path.
    pub fn children_of(&self, parent_agent_id: &SessionId) -> Vec<SessionId> {
        let mut children = self
            .snapshot
            .edges
            .iter()
            .filter(|edge| &edge.parent_agent_id == parent_agent_id)
            .map(|edge| edge.child_agent_id.clone())
            .collect::<Vec<_>>();
        children.sort_by(|left, right| {
            self.get_node(left)
                .map(|node| node.agent_path.as_str())
                .cmp(&self.get_node(right).map(|node| node.agent_path.as_str()))
        });
        children
    }

    /// Follow parent edges to the root node.
    pub fn root_of(&self, agent_id: &SessionId) -> Option<SessionId> {
        let mut current = agent_id.clone();
        let mut seen = HashSet::new();
        loop {
            if !seen.insert(current.clone()) {
                return None;
            }
            match self.get_edge(&current) {
                Some(edge) => current = edge.parent_agent_id.clone(),
                None => return Some(current),
            }
        }
    }

    /// Return descendants in post-order (children before their parent).
    ///
    /// The input agent itself is not included.
    pub fn descendants_postorder(
        &self,
        agent_id: &SessionId,
    ) -> Result<Vec<SessionId>, AgentGraphError> {
        let mut ordered = Vec::new();
        let mut visiting = HashSet::new();
        for child in self.children_of(agent_id) {
            self.visit_postorder(&child, &mut visiting, &mut ordered)?;
        }
        Ok(ordered)
    }

    /// Nodes at or below `prefix`, ordered by canonical path.
    pub fn list_prefix(&self, prefix: &AgentPath) -> Vec<AgentGraphNode> {
        let mut nodes = self
            .snapshot
            .nodes
            .iter()
            .filter(|node| node.agent_path.is_at_or_below(prefix))
            .cloned()
            .collect::<Vec<_>>();
        nodes.sort_by(|left, right| left.agent_path.as_str().cmp(right.agent_path.as_str()));
        nodes
    }

    fn apply_spawned(
        &mut self,
        fact: &SessionFact,
        payload: &SubagentSpawned,
    ) -> Result<(), AgentGraphError> {
        let parent_agent_path =
            payload
                .parent_agent_path
                .clone()
                .ok_or_else(|| AgentGraphError::MissingAgentPath {
                    child_session_id: payload.child_session_id.clone(),
                })?;
        let child_agent_path =
            payload
                .child_agent_path
                .clone()
                .ok_or_else(|| AgentGraphError::MissingAgentPath {
                    child_session_id: payload.child_session_id.clone(),
                })?;
        let parent_agent_id = fact.session_id.clone();
        let Some(parent) = self.get_node(&parent_agent_id) else {
            return Err(AgentGraphError::ParentMissing { parent_agent_id });
        };
        if parent.agent_path != parent_agent_path {
            return Err(AgentGraphError::ParentPathMismatch {
                parent_agent_id,
                expected: parent.agent_path.clone(),
                actual: parent_agent_path,
            });
        }
        if child_agent_path.parent().as_ref() != Some(&parent.agent_path) {
            return Err(AgentGraphError::ParentPathMismatch {
                parent_agent_id,
                expected: parent.agent_path.clone(),
                actual: child_agent_path,
            });
        }
        if self.is_ancestor(&payload.child_session_id, &parent_agent_id) {
            return Err(AgentGraphError::Cycle {
                parent_agent_id,
                child_agent_id: payload.child_session_id.clone(),
            });
        }

        if let Some(existing_id) = self.by_path.get(&child_agent_path)
            && existing_id != &payload.child_session_id
        {
            return Err(AgentGraphError::AgentPathConflict {
                agent_path: child_agent_path,
                existing_agent_id: existing_id.clone(),
                incoming_agent_id: payload.child_session_id.clone(),
            });
        }

        if let Some(existing_node) = self.get_node(&payload.child_session_id) {
            if existing_node.agent_path != child_agent_path {
                return Err(AgentGraphError::ChildPathConflict {
                    agent_id: payload.child_session_id.clone(),
                    existing: existing_node.agent_path.clone(),
                    incoming: child_agent_path,
                });
            }
        } else {
            let node = AgentGraphNode {
                agent_id: payload.child_session_id.clone(),
                agent_path: child_agent_path.clone(),
                parent_agent_path: Some(parent.agent_path.clone()),
                role: payload.role.clone(),
            };
            let index = self.snapshot.nodes.len();
            self.by_path
                .insert(child_agent_path, payload.child_session_id.clone());
            self.by_id.insert(payload.child_session_id.clone(), index);
            self.snapshot.nodes.push(node);
        }

        if let Some(existing) = self.get_edge(&payload.child_session_id) {
            if existing.parent_agent_id != parent_agent_id
                || existing.parent_call_id != payload.parent_call_id
            {
                return Err(AgentGraphError::EdgeConflict {
                    child_agent_id: payload.child_session_id.clone(),
                });
            }
            return Ok(());
        }

        let edge = AgentGraphEdge {
            parent_agent_id,
            child_agent_id: payload.child_session_id.clone(),
            parent_call_id: payload.parent_call_id.clone(),
            status: AgentGraphEdgeStatus::Open,
            created_at_ms: payload.spawned_at_ms,
            closed_at_ms: None,
        };
        let index = self.snapshot.edges.len();
        self.edge_by_child
            .insert(payload.child_session_id.clone(), index);
        self.snapshot.edges.push(edge);
        self.snapshot.revision = self.snapshot.revision.saturating_add(1);
        Ok(())
    }

    fn apply_finished(
        &mut self,
        fact: &SessionFact,
        payload: &SubagentFinished,
    ) -> Result<(), AgentGraphError> {
        let Some(index) = self.edge_by_child.get(&payload.child_session_id).copied() else {
            return Err(AgentGraphError::EdgeMissing {
                child_agent_id: payload.child_session_id.clone(),
            });
        };
        let edge = self
            .snapshot
            .edges
            .get_mut(index)
            .expect("edge index must point at an edge");
        if edge.parent_agent_id != fact.session_id {
            return Err(AgentGraphError::EdgeConflict {
                child_agent_id: payload.child_session_id.clone(),
            });
        }
        if edge.parent_call_id != payload.parent_call_id {
            return Err(AgentGraphError::ParentCallMismatch {
                child_agent_id: payload.child_session_id.clone(),
            });
        }
        match edge.status {
            AgentGraphEdgeStatus::Open => {
                edge.status = AgentGraphEdgeStatus::Closed {
                    terminal: payload.status,
                };
                edge.closed_at_ms = Some(payload.finished_at_ms);
                self.snapshot.revision = self.snapshot.revision.saturating_add(1);
                Ok(())
            }
            AgentGraphEdgeStatus::Closed { terminal } if terminal == payload.status => Ok(()),
            AgentGraphEdgeStatus::Closed { .. } => Err(AgentGraphError::EdgeConflict {
                child_agent_id: payload.child_session_id.clone(),
            }),
        }
    }

    fn is_ancestor(&self, ancestor: &SessionId, node: &SessionId) -> bool {
        let mut current = node.clone();
        let mut seen = HashSet::new();
        while seen.insert(current.clone()) {
            let Some(edge) = self.get_edge(&current) else {
                return false;
            };
            if &edge.parent_agent_id == ancestor {
                return true;
            }
            current = edge.parent_agent_id.clone();
        }
        false
    }

    fn visit_postorder(
        &self,
        agent_id: &SessionId,
        visiting: &mut HashSet<SessionId>,
        ordered: &mut Vec<SessionId>,
    ) -> Result<(), AgentGraphError> {
        if !visiting.insert(agent_id.clone()) {
            return Err(AgentGraphError::Cycle {
                parent_agent_id: agent_id.clone(),
                child_agent_id: agent_id.clone(),
            });
        }
        for child in self.children_of(agent_id) {
            self.visit_postorder(&child, visiting, ordered)?;
        }
        visiting.remove(agent_id);
        ordered.push(agent_id.clone());
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::session_fact_v2::{
        EventId, FactSchema, LogId, SubagentFinished, SubagentSpawned, ToolCallId,
    };

    fn session_id(raw: &str) -> SessionId {
        SessionId::new(raw)
    }

    fn path(raw: &str) -> AgentPath {
        AgentPath::parse_absolute(raw).expect("valid path")
    }

    fn fact(session_id: &SessionId, seq: u64, payload: FactPayload) -> SessionFact {
        SessionFact {
            schema: FactSchema::v2(),
            session_id: session_id.clone(),
            log_id: LogId::new("log"),
            fact_seq: seq,
            event_id: EventId::new(format!("event-{seq}")),
            ts_ms: seq as i64,
            causation_id: None,
            turn_id: None,
            call_id: None,
            interaction_id: None,
            payload,
        }
    }

    fn spawned(
        parent: &SessionId,
        child: &SessionId,
        parent_path: &str,
        child_path: &str,
    ) -> SessionFact {
        fact(
            parent,
            2,
            FactPayload::SubagentSpawned(SubagentSpawned {
                child_session_id: child.clone(),
                parent_call_id: ToolCallId::new("call-parent"),
                parent_agent_path: Some(path(parent_path)),
                child_agent_path: Some(path(child_path)),
                role: Some("worker".to_string()),
                spawn_config: None,
                spawned_at_ms: 10,
            }),
        )
    }

    fn finished(
        parent: &SessionId,
        child: &SessionId,
        status: SubagentTerminalStatus,
    ) -> SessionFact {
        fact(
            parent,
            3,
            FactPayload::SubagentFinished(SubagentFinished {
                child_session_id: child.clone(),
                parent_call_id: ToolCallId::new("call-parent"),
                status,
                result_ref: None,
                finished_at_ms: 20,
                recovery_ref: None,
            }),
        )
    }

    #[test]
    fn rebuilds_open_edge_and_finish_status() {
        let root = session_id("root");
        let child = session_id("child");
        let store = AgentGraphStore::rebuild(
            root.clone(),
            [
                spawned(&root, &child, "/root", "/root/review"),
                finished(&root, &child, SubagentTerminalStatus::Completed),
            ],
        )
        .expect("graph rebuild");

        let edge = store.get_edge(&child).expect("edge");
        assert_eq!(edge.parent_agent_id, root);
        assert_eq!(
            edge.status,
            AgentGraphEdgeStatus::Closed {
                terminal: SubagentTerminalStatus::Completed
            }
        );
        assert_eq!(
            store.get_node(&child).expect("child").agent_path,
            path("/root/review")
        );
    }

    #[test]
    fn duplicate_spawn_is_idempotent_but_conflict_is_rejected() {
        let root = session_id("root");
        let child = session_id("child");
        let spawn = spawned(&root, &child, "/root", "/root/review");
        let mut store = AgentGraphStore::new(root.clone());
        store.apply_fact(&spawn).expect("first spawn");
        store.apply_fact(&spawn).expect("replay spawn");

        let conflict = spawned(&root, &child, "/root", "/root/other");
        assert!(matches!(
            store.apply_fact(&conflict),
            Err(AgentGraphError::ChildPathConflict { .. })
        ));
    }

    #[test]
    fn finish_requires_an_existing_edge() {
        let root = session_id("root");
        let child = session_id("child");
        let mut store = AgentGraphStore::new(root.clone());
        assert!(matches!(
            store.apply_fact(&finished(&root, &child, SubagentTerminalStatus::Failed)),
            Err(AgentGraphError::EdgeMissing { .. })
        ));
    }

    #[test]
    fn missing_paths_fail_closed() {
        let root = session_id("root");
        let child = session_id("child");
        let spawn = fact(
            &root,
            1,
            FactPayload::SubagentSpawned(SubagentSpawned {
                child_session_id: child.clone(),
                parent_call_id: ToolCallId::new("call-parent"),
                parent_agent_path: None,
                child_agent_path: None,
                role: None,
                spawn_config: None,
                spawned_at_ms: 1,
            }),
        );
        let mut store = AgentGraphStore::new(root);
        assert!(matches!(
            store.apply_fact(&spawn),
            Err(AgentGraphError::MissingAgentPath { .. })
        ));
    }

    #[test]
    fn cascade_is_postorder() {
        let root = session_id("root");
        let child = session_id("child");
        let grandchild = session_id("grandchild");
        let store = AgentGraphStore::rebuild(
            root.clone(),
            [
                spawned(&root, &child, "/root", "/root/review"),
                spawned(&child, &grandchild, "/root/review", "/root/review/tests"),
            ],
        )
        .expect("graph rebuild");

        assert_eq!(
            store.descendants_postorder(&root).expect("cascade"),
            vec![grandchild, child]
        );
    }

    #[test]
    fn prefix_listing_is_component_aware() {
        let root = session_id("root");
        let child = session_id("child");
        let sibling = session_id("sibling");
        let store = AgentGraphStore::rebuild(
            root.clone(),
            [
                spawned(&root, &child, "/root", "/root/review"),
                spawned(&root, &sibling, "/root", "/root/review_extra"),
            ],
        )
        .expect("graph rebuild");

        let listed = store.list_prefix(&path("/root/review"));
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].agent_id, child);
    }

    #[test]
    fn root_of_walks_to_root() {
        let root = session_id("root");
        let child = session_id("child");
        let grandchild = session_id("grandchild");
        let store = AgentGraphStore::rebuild(
            root.clone(),
            [
                spawned(&root, &child, "/root", "/root/review"),
                spawned(&child, &grandchild, "/root/review", "/root/review/tests"),
            ],
        )
        .expect("graph rebuild");
        assert_eq!(store.root_of(&grandchild), Some(root));
    }
}
