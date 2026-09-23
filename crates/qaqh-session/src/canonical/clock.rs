//! Durable logical clock shared by replay window and content GC.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::session_fact_v2::{EventId, SessionFact};

use super::CanonicalError;
use super::log::write_json_atomic;

pub const CONTENT_DIR: &str = "content";
pub const CONTENT_CLOCK_FILE: &str = "content/clock.json";
pub const CONTENT_CLOCK_SCHEMA: &str = "qaqh.content-clock/v1";

const ZERO_EVENT_ID: &str = "00000000000000000000000000";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ContentClockRecord {
    pub schema: String,
    pub logical_now_ms: i64,
    pub source_fact_seq: u64,
    pub source_event_id: EventId,
    /// Diagnostic only; never participates in expiry decisions.
    pub updated_at_ms: i64,
}

/// Session-local logical clock with atomic persistence.
#[derive(Debug, Clone)]
pub struct ContentClock {
    session_dir: PathBuf,
    record: ContentClockRecord,
}

impl ContentClock {
    /// Recover the clock from committed facts and the optional manifest value.
    ///
    /// A valid clock is ignored when its source fact is beyond the committed
    /// high-water. Missing, corrupt, or behind clocks are rebuilt atomically.
    pub fn open_or_recover(
        session_dir: impl AsRef<Path>,
        committed_facts: &[SessionFact],
        manifest_logical_now_ms: Option<i64>,
    ) -> Result<Self, CanonicalError> {
        let session_dir = session_dir.as_ref().to_path_buf();
        let clock_path = session_dir.join(CONTENT_CLOCK_FILE);
        let current = read_clock(&clock_path)?;
        let max_fact = committed_facts
            .iter()
            .max_by_key(|fact| (fact.ts_ms, fact.fact_seq));
        let committed_high_water = committed_facts
            .iter()
            .map(|fact| fact.fact_seq)
            .max()
            .unwrap_or(0);
        let max_fact_ts = max_fact.map(|fact| fact.ts_ms).unwrap_or(0);
        let clock_value = current
            .as_ref()
            .filter(|record| record.source_fact_seq <= committed_high_water)
            .map(|record| record.logical_now_ms)
            .unwrap_or(0);
        let logical_now_ms = clock_value
            .max(manifest_logical_now_ms.unwrap_or(0))
            .max(max_fact_ts)
            .max(0);
        let needs_rebuild = current.as_ref().is_none_or(|record| {
            record.logical_now_ms < logical_now_ms || record.source_fact_seq > committed_high_water
        });

        let record = if needs_rebuild {
            let record = rebuilt_record(logical_now_ms, max_fact);
            write_json_atomic(&clock_path, &record)?;
            record
        } else {
            current.expect("clock exists when no rebuild is required")
        };

        Ok(Self {
            session_dir,
            record,
        })
    }

    pub fn record(&self) -> &ContentClockRecord {
        &self.record
    }

    pub fn logical_now_ms(&self) -> i64 {
        self.record.logical_now_ms
    }

    pub fn clock_path(&self) -> PathBuf {
        self.session_dir.join(CONTENT_CLOCK_FILE)
    }

    /// Advance the clock after a canonical fact is durable.
    ///
    /// Returns `true` only when the clock was rewritten.
    pub fn advance(&mut self, fact: &SessionFact) -> Result<bool, CanonicalError> {
        if fact.ts_ms <= self.record.logical_now_ms {
            return Ok(false);
        }
        self.record = ContentClockRecord {
            schema: CONTENT_CLOCK_SCHEMA.into(),
            logical_now_ms: fact.ts_ms,
            source_fact_seq: fact.fact_seq,
            source_event_id: fact.event_id.clone(),
            updated_at_ms: fact.ts_ms,
        };
        write_json_atomic(&self.clock_path(), &self.record)?;
        Ok(true)
    }
}

fn rebuilt_record(logical_now_ms: i64, source: Option<&SessionFact>) -> ContentClockRecord {
    let (source_fact_seq, source_event_id) = source
        .map(|fact| (fact.fact_seq, fact.event_id.clone()))
        .unwrap_or_else(|| (0, EventId::new(ZERO_EVENT_ID)));
    ContentClockRecord {
        schema: CONTENT_CLOCK_SCHEMA.into(),
        logical_now_ms,
        source_fact_seq,
        source_event_id,
        updated_at_ms: logical_now_ms,
    }
}

fn read_clock(path: &Path) -> Result<Option<ContentClockRecord>, CanonicalError> {
    let bytes = match fs::read(path) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    match serde_json::from_slice::<ContentClockRecord>(&bytes) {
        Ok(record) if record.schema == CONTENT_CLOCK_SCHEMA => Ok(Some(record)),
        Ok(_) | Err(_) => Ok(None),
    }
}
