//! Root-session quota ledger.
//!
//! The ledger is deliberately append-only and protected by a dedicated
//! `quota.lock`. A reservation is durable before it is returned to the
//! caller; `committed` and `released` records are terminal for that
//! reservation.

use std::collections::{HashMap, HashSet};
use std::fs::{self, File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use serde::{Deserialize, Serialize};

static NEXT_RESERVATION: AtomicU64 = AtomicU64::new(1);

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum QuotaKind {
    Spawn,
    Message,
    Content,
    Tool,
}

impl QuotaKind {
    fn gated_at_soft_limit(self) -> bool {
        matches!(self, Self::Spawn | Self::Tool)
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReleaseReason {
    Completed,
    Cancelled,
    NoCanonicalEdge,
    Reconciliation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct QuotaLimits {
    pub soft: u64,
    pub hard: u64,
}

impl QuotaLimits {
    pub fn new(soft: u64, hard: u64) -> Result<Self, String> {
        if soft > hard {
            return Err(format!(
                "quota soft limit {soft} must not exceed hard limit {hard}"
            ));
        }
        Ok(Self { soft, hard })
    }

    pub fn unlimited() -> Self {
        Self {
            soft: u64::MAX,
            hard: u64::MAX,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaReservation {
    pub reservation_id: String,
    pub kind: QuotaKind,
    pub amount: u64,
    pub canonical_key: String,
    pub child_session_id: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct QuotaSnapshot {
    pub held: u64,
    pub committed: u64,
    pub reservations: usize,
    pub soft_limit: u64,
    pub hard_limit: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
enum QuotaRecord {
    Reserved {
        reservation_id: String,
        root_session_id: String,
        child_session_id: Option<String>,
        kind: QuotaKind,
        amount: u64,
        canonical_key: String,
        at_ms: i64,
    },
    Committed {
        reservation_id: String,
        amount: u64,
        at_ms: i64,
    },
    Released {
        reservation_id: String,
        reason: ReleaseReason,
        at_ms: i64,
    },
}

#[derive(Debug, Clone)]
struct HeldReservation {
    kind: QuotaKind,
    amount: u64,
    child_session_id: Option<String>,
}

#[derive(Debug, Default)]
struct LedgerState {
    held: HashMap<String, HeldReservation>,
    settled: HashSet<String>,
    committed: u64,
}

impl LedgerState {
    fn held_total(&self) -> u64 {
        self.held.values().map(|held| held.amount).sum()
    }

    fn total_used(&self) -> u64 {
        self.committed.saturating_add(self.held_total())
    }
}

/// One root session's durable quota ledger.
#[derive(Debug)]
pub struct QuotaLedger {
    root_session_id: String,
    limits: QuotaLimits,
    ledger_path: PathBuf,
    lock_path: PathBuf,
    state: LedgerState,
}

impl QuotaLedger {
    /// Open under `{data_dir}/quota/{root_session_id}`.
    pub fn open(root_session_id: &str, limits: QuotaLimits) -> Result<Self, String> {
        Self::open_at(qaqh_types::platform::data_dir(), root_session_id, limits)
    }

    /// Open under an explicit base directory (tests and embedding).
    pub fn open_at(
        data_dir: impl AsRef<Path>,
        root_session_id: &str,
        limits: QuotaLimits,
    ) -> Result<Self, String> {
        if root_session_id.is_empty() {
            return Err("quota root_session_id must not be empty".into());
        }
        let dir = data_dir.as_ref().join("quota").join(root_session_id);
        fs::create_dir_all(&dir).map_err(|error| format!("quota create_dir_all: {error}"))?;
        let ledger_path = dir.join("ledger.jsonl");
        let lock_path = dir.join("quota.lock");
        let mut ledger = Self {
            root_session_id: root_session_id.to_string(),
            limits,
            ledger_path,
            lock_path,
            state: LedgerState::default(),
        };
        ledger.refresh()?;
        Ok(ledger)
    }

    pub fn root_session_id(&self) -> &str {
        &self.root_session_id
    }

    pub fn ledger_path(&self) -> &Path {
        &self.ledger_path
    }

    pub fn lock_path(&self) -> &Path {
        &self.lock_path
    }

    pub fn limits(&self) -> QuotaLimits {
        self.limits
    }

    /// Durably reserve capacity. The returned reservation is the ack the
    /// caller may use before performing the protected side effect.
    pub fn reserve(
        &mut self,
        kind: QuotaKind,
        amount: u64,
        canonical_key: impl Into<String>,
        child_session_id: Option<String>,
    ) -> Result<QuotaReservation, String> {
        let _guard = self.lock_exclusive()?;
        self.refresh()?;
        let used = self.state.total_used();
        let total = used.saturating_add(amount);
        if total > self.limits.hard {
            return Err(format!(
                "quota hard limit exceeded: {} + {amount} > {}",
                used, self.limits.hard
            ));
        }
        if total > self.limits.soft && kind.gated_at_soft_limit() {
            return Err(format!(
                "quota soft limit exceeded for {kind:?}: {} + {amount} > {}",
                used, self.limits.soft
            ));
        }

        let reservation_id = format!(
            "q-{}-{:x}",
            NEXT_RESERVATION.fetch_add(1, Ordering::Relaxed),
            now_ms()
        );
        let canonical_key = canonical_key.into();
        let record = QuotaRecord::Reserved {
            reservation_id: reservation_id.clone(),
            root_session_id: self.root_session_id.clone(),
            child_session_id: child_session_id.clone(),
            kind,
            amount,
            canonical_key: canonical_key.clone(),
            at_ms: now_ms(),
        };
        self.append(&record)?;
        self.state.held.insert(
            reservation_id.clone(),
            HeldReservation {
                kind,
                amount,
                child_session_id: child_session_id.clone(),
            },
        );
        Ok(QuotaReservation {
            reservation_id,
            kind,
            amount,
            canonical_key,
            child_session_id,
        })
    }

    /// Commit a held reservation. Duplicate commit is idempotent.
    pub fn commit(&mut self, reservation_id: &str) -> Result<(), String> {
        let _guard = self.lock_exclusive()?;
        self.refresh()?;
        if self.state.settled.contains(reservation_id) {
            return Ok(());
        }
        let Some(held) = self.state.held.remove(reservation_id) else {
            return Err(format!("quota reservation {reservation_id} is not held"));
        };
        let record = QuotaRecord::Committed {
            reservation_id: reservation_id.to_string(),
            amount: held.amount,
            at_ms: now_ms(),
        };
        self.append(&record)?;
        self.state.committed = self.state.committed.saturating_add(held.amount);
        self.state.settled.insert(reservation_id.to_string());
        Ok(())
    }

    /// Release a held reservation. Duplicate release is idempotent.
    pub fn release(&mut self, reservation_id: &str, reason: ReleaseReason) -> Result<(), String> {
        let _guard = self.lock_exclusive()?;
        self.refresh()?;
        if self.state.settled.contains(reservation_id) {
            return Ok(());
        }
        if self.state.held.remove(reservation_id).is_none() {
            return Err(format!("quota reservation {reservation_id} is not held"));
        }
        let record = QuotaRecord::Released {
            reservation_id: reservation_id.to_string(),
            reason,
            at_ms: now_ms(),
        };
        self.append(&record)?;
        self.state.settled.insert(reservation_id.to_string());
        Ok(())
    }

    /// Release spawn reservations whose child has no canonical edge.
    pub fn reconcile_spawns(
        &mut self,
        canonical_children: &HashSet<String>,
    ) -> Result<usize, String> {
        let _guard = self.lock_exclusive()?;
        self.refresh()?;
        let stale: Vec<(String, String)> = self
            .state
            .held
            .iter()
            .filter_map(|(id, held)| match (&held.kind, &held.child_session_id) {
                (QuotaKind::Spawn, Some(child)) if !canonical_children.contains(child) => {
                    Some((id.clone(), child.clone()))
                }
                _ => None,
            })
            .collect();
        for (reservation_id, child) in &stale {
            let record = QuotaRecord::Released {
                reservation_id: reservation_id.clone(),
                reason: ReleaseReason::NoCanonicalEdge,
                at_ms: now_ms(),
            };
            self.append(&record)?;
            self.state.held.remove(reservation_id);
            self.state.settled.insert(reservation_id.clone());
            log::warn!(
                "[quota] released unbacked spawn reservation {reservation_id} for child {child}"
            );
        }
        Ok(stale.len())
    }

    pub fn snapshot(&mut self) -> Result<QuotaSnapshot, String> {
        let _guard = self.lock_exclusive()?;
        self.refresh()?;
        Ok(QuotaSnapshot {
            held: self.state.held_total(),
            committed: self.state.committed,
            reservations: self.state.held.len(),
            soft_limit: self.limits.soft,
            hard_limit: self.limits.hard,
        })
    }

    fn refresh(&mut self) -> Result<(), String> {
        let mut state = LedgerState::default();
        match File::open(&self.ledger_path) {
            Ok(file) => {
                for (line_number, line) in BufReader::new(file).lines().enumerate() {
                    let line = line.map_err(|error| format!("quota ledger read line: {error}"))?;
                    if line.trim().is_empty() {
                        continue;
                    }
                    let record: QuotaRecord = serde_json::from_str(&line).map_err(|error| {
                        format!(
                            "quota ledger parse {}:{}: {error}",
                            self.ledger_path.display(),
                            line_number + 1
                        )
                    })?;
                    match record {
                        QuotaRecord::Reserved {
                            reservation_id,
                            kind,
                            amount,
                            canonical_key: _,
                            child_session_id,
                            ..
                        } => {
                            if state.held.contains_key(&reservation_id)
                                || state.settled.contains(&reservation_id)
                            {
                                return Err(format!(
                                    "quota ledger has duplicate reservation {reservation_id}"
                                ));
                            }
                            state.held.insert(
                                reservation_id,
                                HeldReservation {
                                    kind,
                                    amount,
                                    child_session_id,
                                },
                            );
                        }
                        QuotaRecord::Committed {
                            reservation_id,
                            amount,
                            ..
                        } => {
                            if state.settled.contains(&reservation_id) {
                                continue;
                            }
                            if state.held.remove(&reservation_id).is_none() {
                                return Err(format!(
                                    "quota ledger commits unknown reservation {reservation_id}"
                                ));
                            }
                            state.committed = state.committed.saturating_add(amount);
                            state.settled.insert(reservation_id);
                        }
                        QuotaRecord::Released { reservation_id, .. } => {
                            if state.settled.contains(&reservation_id) {
                                continue;
                            }
                            if state.held.remove(&reservation_id).is_none() {
                                return Err(format!(
                                    "quota ledger releases unknown reservation {reservation_id}"
                                ));
                            }
                            state.settled.insert(reservation_id);
                        }
                    }
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
            Err(error) => return Err(format!("quota ledger open: {error}")),
        }
        self.state = state;
        Ok(())
    }

    fn append(&self, record: &QuotaRecord) -> Result<(), String> {
        let mut file = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.ledger_path)
            .map_err(|error| format!("quota ledger open append: {error}"))?;
        serde_json::to_writer(&mut file, record)
            .map_err(|error| format!("quota ledger serialize: {error}"))?;
        file.write_all(b"\n")
            .map_err(|error| format!("quota ledger newline: {error}"))?;
        file.sync_all()
            .map_err(|error| format!("quota ledger fsync: {error}"))?;
        Ok(())
    }

    fn lock_exclusive(&self) -> Result<File, String> {
        let file = OpenOptions::new()
            .create(true)
            .read(true)
            .write(true)
            .truncate(false)
            .open(&self.lock_path)
            .map_err(|error| format!("quota lock open: {error}"))?;
        file.lock()
            .map_err(|error| format!("quota lock: {error}"))?;
        Ok(file)
    }
}

fn now_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as i64
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_ledger(dir: &tempfile::TempDir, soft: u64, hard: u64) -> QuotaLedger {
        QuotaLedger::open_at(
            dir.path(),
            "root-session",
            QuotaLimits::new(soft, hard).expect("limits"),
        )
        .expect("ledger")
    }

    #[test]
    fn paths_are_fixed_under_data_dir() {
        let dir = tempfile::tempdir().unwrap();
        let ledger = open_ledger(&dir, 100, 200);
        assert!(
            ledger
                .ledger_path()
                .ends_with("quota/root-session/ledger.jsonl")
        );
        assert!(
            ledger
                .lock_path()
                .ends_with("quota/root-session/quota.lock")
        );
    }

    #[test]
    fn reservation_is_durable_before_ack() {
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = open_ledger(&dir, 100, 200);
        let reservation = ledger
            .reserve(QuotaKind::Message, 7, "message:1", None)
            .expect("reserve");
        drop(ledger);

        let mut reopened = open_ledger(&dir, 100, 200);
        let snapshot = reopened.snapshot().expect("snapshot");
        assert_eq!(snapshot.held, 7);
        assert_eq!(snapshot.reservations, 1);
        assert_eq!(reservation.amount, 7);
    }

    #[test]
    fn commit_and_release_are_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = open_ledger(&dir, 100, 200);
        let reservation = ledger
            .reserve(QuotaKind::Content, 11, "content:1", None)
            .expect("reserve");
        ledger.commit(&reservation.reservation_id).expect("commit");
        ledger
            .commit(&reservation.reservation_id)
            .expect("commit replay");
        let snapshot = ledger.snapshot().expect("snapshot");
        assert_eq!(snapshot.held, 0);
        assert_eq!(snapshot.committed, 11);
    }

    #[test]
    fn hard_limit_rejects_without_appending() {
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = open_ledger(&dir, 5, 10);
        ledger
            .reserve(QuotaKind::Message, 10, "message:1", None)
            .expect("first reservation");
        assert!(
            ledger
                .reserve(QuotaKind::Message, 1, "message:2", None)
                .is_err()
        );
        assert_eq!(ledger.snapshot().expect("snapshot").held, 10);
    }

    #[test]
    fn committed_usage_still_counts_toward_limit() {
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = open_ledger(&dir, 10, 10);
        let reservation = ledger
            .reserve(QuotaKind::Message, 10, "message:1", None)
            .expect("first reservation");
        ledger.commit(&reservation.reservation_id).expect("commit");
        assert!(
            ledger
                .reserve(QuotaKind::Message, 1, "message:2", None)
                .is_err(),
            "committed usage must remain part of the quota watermark"
        );
    }

    #[test]
    fn soft_limit_blocks_spawn_but_allows_message() {
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = open_ledger(&dir, 10, 20);
        ledger
            .reserve(QuotaKind::Message, 10, "message:1", None)
            .expect("message at soft limit");
        assert!(
            ledger
                .reserve(QuotaKind::Spawn, 1, "spawn:1", Some("child".into()))
                .is_err()
        );
        ledger
            .reserve(QuotaKind::Message, 5, "message:2", None)
            .expect("message below hard limit");
    }

    #[test]
    fn reconciliation_releases_unbacked_spawn() {
        let dir = tempfile::tempdir().unwrap();
        let mut ledger = open_ledger(&dir, 100, 200);
        ledger
            .reserve(QuotaKind::Spawn, 3, "spawn:child", Some("child".into()))
            .expect("reserve spawn");
        assert_eq!(
            ledger.reconcile_spawns(&HashSet::new()).expect("reconcile"),
            1
        );
        let snapshot = ledger.snapshot().expect("snapshot");
        assert_eq!(snapshot.held, 0);
    }
}
