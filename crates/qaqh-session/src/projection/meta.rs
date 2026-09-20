//! Rebuildable session metadata projection.

use serde::{Deserialize, Serialize};

use crate::session_fact_v2::{
    CompactionApplied, FactPayload, MetaDelta, RecoveryOutcome, RecoveryRef, SearchVisibility,
    SessionCreated, SessionDeleted as SessionDeletedFact, SessionFact, SessionId,
    SessionMetadataPatch,
};

use super::Projection;

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SessionMetaSnapshot {
    pub session_id: Option<SessionId>,
    pub cwd: Option<String>,
    pub model: Option<String>,
    pub parent_session_id: Option<SessionId>,
    pub schema_caps: Vec<String>,
    pub archived: bool,
    pub search_visibility: Option<SearchVisibility>,
    pub title: Option<String>,
    pub deleted: Option<SessionDeletedFact>,
    pub context_revision: Option<u64>,
    pub last_recovery: Option<RecoveryOutcome>,
    pub revision: u64,
    pub last_fact_seq: u64,
}

#[derive(Debug, Default)]
pub struct SessionMetaProjection {
    snapshot: SessionMetaSnapshot,
}

impl Projection for SessionMetaProjection {
    type Snapshot = SessionMetaSnapshot;
    type Delta = MetaDelta;

    fn apply(&mut self, fact: &SessionFact) -> Option<Self::Delta> {
        self.snapshot.last_fact_seq = fact.fact_seq;
        match &fact.payload {
            FactPayload::SessionCreated(payload) => {
                self.apply_created(&fact.session_id, payload);
                Some(MetaDelta::Created {
                    revision: self.next_revision(),
                    cwd: payload.cwd.clone(),
                    model: payload.model.clone(),
                    parent_session_id: payload.parent_session_id.clone(),
                    schema_caps: payload.schema_caps.clone(),
                })
            }
            FactPayload::SessionMetadataChanged(payload) => {
                self.apply_metadata_changed(&payload.patch);
                Some(MetaDelta::MetadataChanged {
                    revision: self.next_revision(),
                    patch: payload.patch.clone(),
                })
            }
            FactPayload::SessionTitleChanged(payload) => {
                self.snapshot.title = Some(payload.title.clone());
                Some(MetaDelta::TitleChanged {
                    revision: self.next_revision(),
                    title: payload.title.clone(),
                    source: payload.source,
                })
            }
            FactPayload::SessionDeleted(payload) => {
                self.snapshot.deleted = Some(payload.clone());
                Some(MetaDelta::Deleted {
                    revision: self.next_revision(),
                    tombstone_at_ms: payload.tombstone_at_ms,
                    reason: payload.reason,
                    purge_after_ms: payload.purge_after_ms,
                })
            }
            FactPayload::CompactionApplied(payload) => {
                self.apply_compaction(payload);
                Some(MetaDelta::ContextRevision {
                    revision: self.next_revision(),
                    checkpoint_id: payload.checkpoint_id.clone(),
                    context_revision: payload.context_revision,
                })
            }
            FactPayload::SessionRecovered(payload) => {
                self.snapshot.last_recovery = Some(payload.outcome);
                Some(MetaDelta::Recovered {
                    revision: self.next_revision(),
                    outcome: payload.outcome,
                    recovery_ref: RecoveryRef {
                        recovery_id: payload.recovery_id.clone(),
                        recovery_event_id: payload.recovery_event_id.clone(),
                        recovery_input_fingerprint: payload.recovery_input_fingerprint.clone(),
                    },
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

impl SessionMetaProjection {
    fn apply_created(&mut self, session_id: &SessionId, payload: &SessionCreated) {
        self.snapshot.session_id = Some(session_id.clone());
        self.snapshot.cwd = Some(payload.cwd.clone());
        self.snapshot.model = Some(payload.model.clone());
        self.snapshot.parent_session_id = payload.parent_session_id.clone();
        self.snapshot.schema_caps = payload.schema_caps.clone();
        self.snapshot.archived = false;
        self.snapshot.search_visibility = None;
        self.snapshot.title = None;
        self.snapshot.deleted = None;
        self.snapshot.context_revision = None;
        self.snapshot.last_recovery = None;
    }

    fn apply_metadata_changed(&mut self, patch: &SessionMetadataPatch) {
        if let Some(cwd) = &patch.cwd {
            self.snapshot.cwd = Some(cwd.clone());
        }
        if let Some(model) = &patch.model {
            self.snapshot.model = Some(model.clone());
        }
        if let Some(archived) = patch.archived {
            self.snapshot.archived = archived;
        }
        if let Some(search_visibility) = patch.search_visibility {
            self.snapshot.search_visibility = Some(search_visibility);
        }
        if let Some(parent_session_id) = &patch.parent_session_id {
            self.snapshot.parent_session_id = Some(parent_session_id.clone());
        }
        if let Some(schema_caps) = &patch.schema_caps {
            self.snapshot.schema_caps = schema_caps.clone();
        }
    }

    fn apply_compaction(&mut self, payload: &CompactionApplied) {
        self.snapshot.context_revision = Some(payload.context_revision);
    }

    fn next_revision(&mut self) -> u64 {
        self.snapshot.revision = self.snapshot.revision.saturating_add(1);
        self.snapshot.revision
    }
}
