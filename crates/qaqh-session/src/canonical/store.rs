//! Durable canonical store facade with projection updates.

use std::path::Path;

use super::{
    CanonicalError, CanonicalLog, CommittedFactReader, EventsCommit, WriterFence, WriterId,
    WriterLease,
};
use crate::{
    projection::{
        ProjectionSet, ProjectionSetSnapshot, projection_events_for_fact,
        projection_replaceable_events_for_fact,
    },
    session_fact_v2::{LogId, ProjectionEvent, SessionFact, SessionId},
};

/// Result of one durable canonical append.
#[derive(Debug)]
pub struct AppendOutcome {
    pub fact: SessionFact,
    pub events: Vec<ProjectionEvent>,
}

/// Canonical log plus the projections rebuilt from its committed prefix.
///
/// The store is the first production-facing facade over [`CanonicalLog`]:
/// appends become durable before projections or events are exposed. It does
/// not yet route legacy message/journal writers through this owner.
#[derive(Debug)]
pub struct CanonicalSessionStore {
    log: CanonicalLog,
    projections: ProjectionSet,
}

impl CanonicalSessionStore {
    pub fn open(
        session_dir: impl AsRef<Path>,
        session_id: SessionId,
        log_id: LogId,
    ) -> Result<Self, CanonicalError> {
        let log = CanonicalLog::open(session_dir.as_ref(), session_id, log_id)?;
        let reader = CommittedFactReader::open(
            session_dir.as_ref(),
            log.session_id().clone(),
            log.log_id().clone(),
        )?;
        let projections = ProjectionSet::rebuild(reader.read_all()?.into_iter());
        Ok(Self { log, projections })
    }

    pub fn session_id(&self) -> &SessionId {
        self.log.session_id()
    }

    pub fn log_id(&self) -> &LogId {
        self.log.log_id()
    }

    pub fn committed(&self) -> &EventsCommit {
        self.log.committed()
    }

    pub fn writer_fence(&self) -> Result<Option<WriterFence>, CanonicalError> {
        self.log.writer_fence()
    }

    pub fn acquire_writer(
        &mut self,
        writer_id: WriterId,
        now_ms: i64,
        lease_duration_ms: i64,
    ) -> Result<WriterLease, CanonicalError> {
        self.log
            .acquire_writer(writer_id, now_ms, lease_duration_ms)
    }

    pub fn renew_writer(
        &mut self,
        lease: &WriterLease,
        now_ms: i64,
        lease_duration_ms: i64,
    ) -> Result<WriterLease, CanonicalError> {
        self.log.renew_writer(lease, now_ms, lease_duration_ms)
    }

    /// Give up a writer lease early (see [`CanonicalLog::release_writer`]).
    pub fn release_writer(&self, lease: &WriterLease, now_ms: i64) -> Result<(), CanonicalError> {
        self.log.release_writer(lease, now_ms)
    }

    /// Durably append one fact, then update projections and publishable events.
    pub fn append(
        &mut self,
        lease: &WriterLease,
        fact: SessionFact,
        now_ms: i64,
    ) -> Result<AppendOutcome, CanonicalError> {
        let fact = self.log.append(lease, fact, now_ms)?;
        let deltas = self.projections.apply(&fact);
        let mut events = projection_events_for_fact(&fact, &deltas)?;
        events.extend(projection_replaceable_events_for_fact(&fact, &deltas)?);
        Ok(AppendOutcome { fact, events })
    }

    pub fn projections(&self) -> &ProjectionSet {
        &self.projections
    }

    pub fn snapshot(&self) -> ProjectionSetSnapshot {
        self.projections.snapshot()
    }

    pub fn last_fact_seq(&self) -> u64 {
        self.projections.last_fact_seq()
    }
}
