//! Optional process-wide sink for committed canonical projection events.
//!
//! The daemon installs one sink at startup. Session/tool code publishes after
//! the durable append succeeds, so consumers never observe an event before its
//! canonical fact is committed.

use std::path::Path;
use std::sync::{Arc, OnceLock};

use crate::session_fact_v2::{ProjectionEvent, SessionFact};

pub trait ProjectionSink: Send + Sync {
    fn publish(&self, session_dir: &Path, fact: &SessionFact, events: &[ProjectionEvent]);
}

static PROJECTION_SINK: OnceLock<Arc<dyn ProjectionSink>> = OnceLock::new();

/// Install the process projection sink once.
///
/// Returns the existing sink if one was already installed.
pub fn install_projection_sink(
    sink: Arc<dyn ProjectionSink>,
) -> Result<(), Arc<dyn ProjectionSink>> {
    PROJECTION_SINK.set(sink)
}

pub(crate) fn publish_projection(
    session_dir: &Path,
    fact: &SessionFact,
    events: &[ProjectionEvent],
) {
    if let Some(sink) = PROJECTION_SINK.get() {
        sink.publish(session_dir, fact, events);
    }
}
