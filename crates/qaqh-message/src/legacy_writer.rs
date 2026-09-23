//! Temporary process-wide serialization for legacy persistence writers.
//!
//! Canonical `events.lock` will eventually become the only cross-process
//! writer fence. Until then, all legacy message/WAL/journal/timeline writes
//! share this facade so they cannot interleave with each other.
//!
//! This is process-local by design; the existing daemon single-instance lock
//! remains the cross-process guard during the observation period.

use std::sync::{Mutex, MutexGuard};

static LEGACY_WRITER: Mutex<()> = Mutex::new(());

pub struct LegacyWriterFacade;

impl LegacyWriterFacade {
    pub fn lock() -> MutexGuard<'static, ()> {
        LEGACY_WRITER
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    pub fn with_lock<R>(write: impl FnOnce() -> R) -> R {
        let _guard = Self::lock();
        write()
    }
}
