//! Process registry — registration, heartbeat, liveness polling, GC.
//!
//! The registry is the single source of truth for which processes are alive.
//! Components call `register()` at startup, `heartbeat()` on a timer, and
//! `unregister()` on graceful shutdown. The pipeline's `judge()` calls
//! `check_all()` to detect stale entries.
//!
//! Reuses `liveness::LivenessState` for per-entry timeout tracking.

use std::collections::HashMap;
use std::time::Instant;

use crate::liveness::{self, LivenessResult, LivenessState};
use crate::types::{HpError, ProcessHealth, ProcessKind, ProcessSummary};

/// A registered process entry.
#[derive(Debug, Clone)]
pub struct Registration {
    pub pid: u32,
    pub kind: ProcessKind,
    pub name: String,
    pub started_at: Instant,
    pub liveness: LivenessState,
    pub metadata: HashMap<String, String>,
}

impl Registration {
    fn new(kind: ProcessKind, name: String, pid: u32, timeout_secs: u64) -> Self {
        Self {
            pid,
            kind,
            name,
            started_at: Instant::now(),
            liveness: LivenessState::new(timeout_secs),
            metadata: HashMap::new(),
        }
    }
}

/// The process registry, owned by the HP daemon main loop.
#[derive(Debug, Clone)]
pub struct ProcessRegistry {
    entries: HashMap<u32, Registration>,
    default_timeout_secs: u64,
}

impl ProcessRegistry {
    /// Create a new empty registry.
    ///
    /// `default_timeout_secs` is the heartbeat interval used for new registrations
    /// that don't specify a custom timeout.
    pub fn new(default_timeout_secs: u64) -> Self {
        Self {
            entries: HashMap::new(),
            default_timeout_secs,
        }
    }

    /// Register a new process.
    ///
    /// Returns `Err(DuplicateRegistration)` if `pid` is already tracked.
    pub fn register(
        &mut self,
        kind: ProcessKind,
        name: &str,
        pid: u32,
    ) -> Result<(), HpError> {
        if self.entries.contains_key(&pid) {
            return Err(HpError::DuplicateRegistration(pid));
        }
        self.entries.insert(
            pid,
            Registration::new(kind, name.to_string(), pid, self.default_timeout_secs),
        );
        Ok(())
    }

    /// Remove a process from the registry.
    ///
    /// Returns `Err(ProcessNotFound)` if `pid` was never registered.
    pub fn unregister(&mut self, pid: u32) -> Result<(), HpError> {
        if self.entries.remove(&pid).is_none() {
            return Err(HpError::ProcessNotFound(pid));
        }
        Ok(())
    }

    /// Record a heartbeat for a registered process.
    ///
    /// Resets the per-entry liveness timer and missed-heartbeat count.
    pub fn heartbeat(&mut self, pid: u32) -> Result<(), HpError> {
        let entry = self
            .entries
            .get_mut(&pid)
            .ok_or(HpError::ProcessNotFound(pid))?;
        liveness::heartbeat(&mut entry.liveness);
        Ok(())
    }

    /// Look up a registration by PID.
    pub fn query(&self, pid: u32) -> Option<&Registration> {
        self.entries.get(&pid)
    }

    /// List all registered processes.
    pub fn list(&self) -> Vec<&Registration> {
        self.entries.values().collect()
    }

    /// Number of registered processes.
    pub fn len(&self) -> usize {
        self.entries.len()
    }

    /// Check liveness for every registered process.
    ///
    /// Returns a vector of `(pid, result)` pairs for entries that are not `Alive`.
    /// Call `gc()` to remove dead entries.
    pub fn check_all(&self) -> Vec<(u32, LivenessResult)> {
        let mut results = Vec::new();
        for (&pid, entry) in &self.entries {
            let result = liveness::check_liveness(&entry.liveness);
            if !matches!(result, LivenessResult::Alive) {
                results.push((pid, result));
            }
        }
        results
    }

    /// Remove all dead entries and return their metadata.
    ///
    /// An entry is dead when `check_liveness` returns `LivenessResult::Dead`.
    pub fn gc(&mut self) -> Vec<(u32, String)> {
        let dead: Vec<u32> = self
            .entries
            .iter()
            .filter(|(_, e)| {
                matches!(liveness::check_liveness(&e.liveness), LivenessResult::Dead { .. })
            })
            .map(|(&pid, _)| pid)
            .collect();

        let mut removed = Vec::with_capacity(dead.len());
        for pid in dead {
            if let Some(entry) = self.entries.remove(&pid) {
                removed.push((pid, entry.name.clone()));
            }
        }
        removed
    }

    /// Build a `ProcessHealth` for a given PID.
    pub fn health(&self, pid: u32) -> Result<ProcessHealth, HpError> {
        let entry = self.entries.get(&pid).ok_or(HpError::ProcessNotFound(pid))?;
        let elapsed = entry.liveness.last_activity.elapsed().as_secs();
        let alive = matches!(
            liveness::check_liveness(&entry.liveness),
            LivenessResult::Alive | LivenessResult::Unresponsive { .. }
        );
        Ok(ProcessHealth {
            pid,
            kind: entry.kind,
            name: entry.name.clone(),
            alive,
            last_heartbeat: elapsed,
            missed_heartbeats: entry.liveness.missed_heartbeats,
        })
    }

    /// Build a `ProcessSummary` for every registered process.
    pub fn summaries(&self) -> Vec<ProcessSummary> {
        self.entries
            .values()
            .map(|e| {
                let alive = matches!(
                    liveness::check_liveness(&e.liveness),
                    LivenessResult::Alive | LivenessResult::Unresponsive { .. }
                );
                ProcessSummary {
                    pid: e.pid,
                    kind: e.kind,
                    name: e.name.clone(),
                    alive,
                    uptime_secs: e.started_at.elapsed().as_secs(),
                }
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::{HpError, ProcessKind};

    #[test]
    fn test_register_and_query() {
        let mut reg = ProcessRegistry::new(30);
        assert!(reg.register(ProcessKind::Agent, "test-agent", 1001).is_ok());
        assert!(matches!(
            reg.register(ProcessKind::Agent, "dup", 1001),
            Err(HpError::DuplicateRegistration(_))
        ));
        assert!(reg.query(1001).is_some());
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn test_unregister() {
        let mut reg = ProcessRegistry::new(30);
        reg.register(ProcessKind::Tools, "test-tools", 2001).unwrap();
        assert!(reg.unregister(2001).is_ok());
        assert!(reg.query(2001).is_none());
        assert_eq!(reg.len(), 0);
    }

    #[test]
    fn test_heartbeat_resets_liveness() {
        let mut reg = ProcessRegistry::new(30);
        reg.register(ProcessKind::Tui, "test-tui", 3001).unwrap();

        // Simulate elapsed time by aging the liveness state
        if let Some(e) = reg.entries.get_mut(&3001) {
            e.liveness.last_activity = Instant::now()
                - std::time::Duration::from_secs(35);
        }

        // Should be unresponsive before heartbeat
        let results = reg.check_all();
        assert!(results.iter().any(|(pid, _)| *pid == 3001));

        // Heartbeat resets
        reg.heartbeat(3001).unwrap();
        let results = reg.check_all();
        assert!(!results.iter().any(|(pid, _)| *pid == 3001));
    }

    #[test]
    fn test_gc_removes_dead_entries() {
        let mut reg = ProcessRegistry::new(30);
        reg.register(ProcessKind::Agent, "alive", 4001).unwrap();
        reg.register(ProcessKind::Tools, "dead", 4002).unwrap();

        // Age the second entry past the dead threshold (3x interval)
        if let Some(e) = reg.entries.get_mut(&4002) {
            e.liveness.last_activity = Instant::now()
                - std::time::Duration::from_secs(120);
        }

        let removed = reg.gc();
        assert_eq!(removed.len(), 1);
        assert_eq!(removed[0].0, 4002);
        assert_eq!(reg.len(), 1);
    }

    #[test]
    fn test_summaries() {
        let mut reg = ProcessRegistry::new(30);
        reg.register(ProcessKind::Agent, "a1", 5001).unwrap();
        reg.register(ProcessKind::Tui, "t1", 5002).unwrap();

        let summaries = reg.summaries();
        assert_eq!(summaries.len(), 2);
        assert!(summaries.iter().all(|s| s.alive));
    }

    #[test]
    fn test_health_builds_correctly() {
        let mut reg = ProcessRegistry::new(30);
        reg.register(ProcessKind::Agent, "h-test", 6001).unwrap();

        let h = reg.health(6001).unwrap();
        assert_eq!(h.pid, 6001);
        assert!(h.alive);

        assert!(matches!(reg.health(9999), Err(HpError::ProcessNotFound(_))));
    }
}
