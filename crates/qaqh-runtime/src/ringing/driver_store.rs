//! Ringing v2 driver registry.
//!
//! The driver is the client session allowed to mutate a session (composer,
//! cancel, undo, workspace/session control). Identity is the daemon lease's
//! `client_session_id`; `driver_epoch` is bumped on every handover so a stale
//! client cannot keep driving after it lost the seat.
//!
//! The registry is daemon-local: leases are a daemon concept, not a canonical
//! session fact. Canonical `DriverChanged` delivery is tracked as an alpha item
//! (see the v2 handoff) because it needs a single-writer decision.

use std::collections::HashMap;

#[derive(Debug, Default)]
pub struct RingingDriverStore {
    entries: HashMap<String, DriverEntry>,
}

#[derive(Debug, Clone, Default)]
struct DriverEntry {
    holder: Option<String>,
    epoch: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriverState {
    pub holder: Option<String>,
    pub driver_epoch: u64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriverClaimOutcome {
    /// Seat was free (or its previous holder's lease had expired).
    Claimed { driver_epoch: u64 },
    /// The caller already holds the seat; idempotent, epoch unchanged.
    AlreadyHeld { driver_epoch: u64 },
    /// Another live lease holds the seat.
    Busy { holder: String, driver_epoch: u64 },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum DriverReleaseOutcome {
    Released {
        driver_epoch: u64,
    },
    NotDriver {
        holder: Option<String>,
        driver_epoch: u64,
    },
}

impl RingingDriverStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn state(&self, seed: &str) -> DriverState {
        self.entries
            .get(seed)
            .map(|entry| DriverState {
                holder: entry.holder.clone(),
                driver_epoch: entry.epoch,
            })
            .unwrap_or(DriverState {
                holder: None,
                driver_epoch: 0,
            })
    }

    pub fn holder(&self, seed: &str) -> Option<String> {
        self.entries
            .get(seed)
            .and_then(|entry| entry.holder.clone())
    }

    pub fn is_driver(&self, seed: &str, client_session_id: &str) -> bool {
        self.entries
            .get(seed)
            .and_then(|entry| entry.holder.as_deref())
            == Some(client_session_id)
    }

    /// Claim the seat.
    ///
    /// `holder_active` reports whether the current holder's lease is still
    /// alive; a dead holder is treated as vacant and the epoch still advances
    /// so the old client's commands cannot be replayed as current.
    pub fn claim(
        &mut self,
        seed: &str,
        client_session_id: &str,
        holder_active: bool,
    ) -> DriverClaimOutcome {
        let entry = self.entries.entry(seed.to_string()).or_default();
        match entry.holder.as_deref() {
            Some(current) if current == client_session_id => DriverClaimOutcome::AlreadyHeld {
                driver_epoch: entry.epoch,
            },
            Some(current) if holder_active => DriverClaimOutcome::Busy {
                holder: current.to_string(),
                driver_epoch: entry.epoch,
            },
            _ => {
                entry.epoch = entry.epoch.saturating_add(1);
                entry.holder = Some(client_session_id.to_string());
                DriverClaimOutcome::Claimed {
                    driver_epoch: entry.epoch,
                }
            }
        }
    }

    pub fn release(&mut self, seed: &str, client_session_id: &str) -> DriverReleaseOutcome {
        let entry = self.entries.entry(seed.to_string()).or_default();
        if entry.holder.as_deref() == Some(client_session_id) {
            entry.holder = None;
            entry.epoch = entry.epoch.saturating_add(1);
            DriverReleaseOutcome::Released {
                driver_epoch: entry.epoch,
            }
        } else {
            DriverReleaseOutcome::NotDriver {
                holder: entry.holder.clone(),
                driver_epoch: entry.epoch,
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claim_is_monotonic_and_idempotent_for_the_same_holder() {
        let mut store = RingingDriverStore::new();
        assert_eq!(
            store.claim("seed-1", "cs-a", false),
            DriverClaimOutcome::Claimed { driver_epoch: 1 }
        );
        assert_eq!(
            store.claim("seed-1", "cs-a", true),
            DriverClaimOutcome::AlreadyHeld { driver_epoch: 1 }
        );
        assert_eq!(
            store.claim("seed-1", "cs-b", true),
            DriverClaimOutcome::Busy {
                holder: "cs-a".into(),
                driver_epoch: 1,
            }
        );
        assert_eq!(store.holder("seed-1").as_deref(), Some("cs-a"));
    }

    #[test]
    fn expired_holder_is_replaced_and_epoch_advances() {
        let mut store = RingingDriverStore::new();
        assert_eq!(
            store.claim("seed-1", "cs-a", false),
            DriverClaimOutcome::Claimed { driver_epoch: 1 }
        );
        // cs-a's lease is gone: the seat is vacant again.
        assert_eq!(
            store.claim("seed-1", "cs-b", false),
            DriverClaimOutcome::Claimed { driver_epoch: 2 }
        );
        assert!(!store.is_driver("seed-1", "cs-a"));
        assert!(store.is_driver("seed-1", "cs-b"));
    }

    #[test]
    fn release_only_succeeds_for_the_holder_and_bumps_epoch() {
        let mut store = RingingDriverStore::new();
        assert!(matches!(
            store.release("seed-1", "cs-a"),
            DriverReleaseOutcome::NotDriver { .. }
        ));
        assert!(matches!(
            store.claim("seed-1", "cs-a", false),
            DriverClaimOutcome::Claimed { driver_epoch: 1 }
        ));
        assert!(matches!(
            store.release("seed-1", "cs-b"),
            DriverReleaseOutcome::NotDriver {
                holder: Some(_),
                ..
            }
        ));
        assert_eq!(
            store.release("seed-1", "cs-a"),
            DriverReleaseOutcome::Released { driver_epoch: 2 }
        );
        assert_eq!(store.state("seed-1").holder, None);
    }
}
