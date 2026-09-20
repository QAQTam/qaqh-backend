//! Rebuildable workspace resource and subagent graph projection.

use serde::{Deserialize, Serialize};

use crate::session_fact_v2::{
    ContentRef, ContentValue, FactPayload, ResourceDelta, ResourceId, ResourceKind, SessionFact,
    SessionId, SubagentTerminalStatus, ToolCallId, WorkspaceResourceChanged,
};

use super::Projection;

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WorkspaceResourceState {
    pub resource_kind: ResourceKind,
    pub resource_id: ResourceId,
    pub source_call_id: Option<ToolCallId>,
    pub resource_revision: u64,
    pub summary_ref: ContentRef,
    pub deleted: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct GraphEdgeState {
    pub child_session_id: SessionId,
    pub parent_call_id: ToolCallId,
    pub status: Option<SubagentTerminalStatus>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct ResourceSnapshot {
    pub workspace: Vec<WorkspaceResourceState>,
    pub graph: Vec<GraphEdgeState>,
    pub revision: u64,
    pub last_fact_seq: u64,
}

#[derive(Debug, Default)]
pub struct ResourceProjection {
    snapshot: ResourceSnapshot,
}

impl Projection for ResourceProjection {
    type Snapshot = ResourceSnapshot;
    type Delta = ResourceDelta;

    fn apply(&mut self, fact: &SessionFact) -> Option<Self::Delta> {
        self.snapshot.last_fact_seq = fact.fact_seq;
        match &fact.payload {
            FactPayload::WorkspaceResourceChanged(payload) => {
                self.upsert_workspace(payload);
                Some(ResourceDelta::WorkspaceResourceChanged {
                    revision: self.next_revision(),
                    resource_kind: payload.resource_kind,
                    resource_id: payload.resource_id.clone(),
                    source_call_id: payload.source_call_id.clone(),
                    summary: ContentValue::Ref {
                        content_ref: payload.summary_ref.clone(),
                    },
                    deleted: payload.deleted,
                })
            }
            FactPayload::SubagentSpawned(payload) => {
                self.upsert_graph_edge(
                    payload.child_session_id.clone(),
                    payload.parent_call_id.clone(),
                    None,
                );
                Some(ResourceDelta::GraphEdge {
                    revision: self.next_revision(),
                    child_session_id: payload.child_session_id.clone(),
                    parent_call_id: payload.parent_call_id.clone(),
                    status: None,
                })
            }
            FactPayload::SubagentFinished(payload) => {
                self.upsert_graph_edge(
                    payload.child_session_id.clone(),
                    payload.parent_call_id.clone(),
                    Some(payload.status),
                );
                Some(ResourceDelta::GraphEdge {
                    revision: self.next_revision(),
                    child_session_id: payload.child_session_id.clone(),
                    parent_call_id: payload.parent_call_id.clone(),
                    status: Some(payload.status),
                })
            }
            _ => None,
        }
    }

    fn snapshot(&self) -> Self::Snapshot {
        self.snapshot.clone()
    }

    fn last_fact_seq(&self) -> u64 {
        self.snapshot.last_fact_seq
    }
}

impl ResourceProjection {
    fn upsert_workspace(&mut self, payload: &WorkspaceResourceChanged) {
        let state = WorkspaceResourceState {
            resource_kind: payload.resource_kind,
            resource_id: payload.resource_id.clone(),
            source_call_id: payload.source_call_id.clone(),
            resource_revision: payload.revision,
            summary_ref: payload.summary_ref.clone(),
            deleted: payload.deleted,
        };
        if let Some(existing) = self.snapshot.workspace.iter_mut().find(|existing| {
            existing.resource_kind == state.resource_kind
                && existing.resource_id == state.resource_id
        }) {
            *existing = state;
        } else {
            self.snapshot.workspace.push(state);
        }
    }

    fn upsert_graph_edge(
        &mut self,
        child_session_id: SessionId,
        parent_call_id: ToolCallId,
        status: Option<SubagentTerminalStatus>,
    ) {
        if let Some(existing) = self
            .snapshot
            .graph
            .iter_mut()
            .find(|existing| existing.child_session_id == child_session_id)
        {
            existing.parent_call_id = parent_call_id;
            existing.status = status;
        } else {
            self.snapshot.graph.push(GraphEdgeState {
                child_session_id,
                parent_call_id,
                status,
            });
        }
    }

    fn next_revision(&mut self) -> u64 {
        self.snapshot.revision = self.snapshot.revision.saturating_add(1);
        self.snapshot.revision
    }
}
