use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Mutex;

use deepx_proto::{CompanionCommandStatus, CompanionInteraction, CompanionInteractionKey};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum InteractionSource {
    Desktop,
    Pet,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InteractionClaim {
    key: CompanionInteractionKey,
    command_id: String,
    claim_id: u64,
    source: InteractionSource,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum BeginClaim {
    Claimed(InteractionClaim),
    Duplicate(CompanionCommandStatus),
    Rejected(CompanionCommandStatus),
}

#[derive(Debug)]
struct PendingInteraction {
    interaction: CompanionInteraction,
    claim: Option<InteractionClaim>,
}

#[derive(Debug, Default)]
struct CoordinatorState {
    pending: HashMap<CompanionInteractionKey, PendingInteraction>,
    resolved: HashSet<CompanionInteractionKey>,
    resolved_order: VecDeque<CompanionInteractionKey>,
    current_generation: HashMap<String, u64>,
    command_results: HashMap<String, CompanionCommandStatus>,
    command_order: VecDeque<String>,
    next_claim_id: u64,
}

#[derive(Debug, Default)]
pub struct InteractionCoordinator {
    inner: Mutex<CoordinatorState>,
}

impl InteractionCoordinator {
    pub fn register(&self, interaction: CompanionInteraction) {
        let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        let generation = interaction.key.generation;
        let seed = interaction.key.seed.clone();
        let current = inner.current_generation.get(&seed).copied().unwrap_or(0);
        if generation < current {
            return;
        }
        if generation > current {
            invalidate_seed_locked(&mut inner, &seed);
            inner.current_generation.insert(seed, generation);
        }
        if inner.pending.contains_key(&interaction.key) || inner.resolved.contains(&interaction.key)
        {
            return;
        }
        inner.pending.insert(
            interaction.key.clone(),
            PendingInteraction {
                interaction,
                claim: None,
            },
        );
    }

    pub fn advance_generation(&self, seed: &str, generation: u64) -> Vec<CompanionInteractionKey> {
        let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        let current = inner.current_generation.get(seed).copied().unwrap_or(0);
        if generation <= current {
            return Vec::new();
        }
        let expired = invalidate_seed_locked(&mut inner, seed);
        inner
            .current_generation
            .insert(seed.to_string(), generation);
        expired
    }

    pub fn invalidate_generation(
        &self,
        seed: &str,
        generation: u64,
    ) -> Vec<CompanionInteractionKey> {
        let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        if inner.current_generation.get(seed).copied() != Some(generation) {
            return Vec::new();
        }
        invalidate_seed_locked(&mut inner, seed)
    }

    pub fn begin(
        &self,
        key: &CompanionInteractionKey,
        command_id: &str,
        source: InteractionSource,
    ) -> BeginClaim {
        let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        if let Some(status) = inner.command_results.get(command_id).copied() {
            return BeginClaim::Duplicate(status);
        }
        if inner
            .current_generation
            .get(&key.seed)
            .is_some_and(|generation| key.generation != *generation)
        {
            return BeginClaim::Rejected(CompanionCommandStatus::StaleGeneration);
        }
        if inner
            .pending
            .get(key)
            .is_none_or(|pending| pending.claim.is_some())
        {
            return BeginClaim::Rejected(CompanionCommandStatus::AlreadyResolved);
        }
        inner.next_claim_id = inner.next_claim_id.saturating_add(1);
        let claim = InteractionClaim {
            key: key.clone(),
            command_id: command_id.to_string(),
            claim_id: inner.next_claim_id,
            source,
        };
        if let Some(pending) = inner.pending.get_mut(key) {
            pending.claim = Some(claim.clone());
        }
        BeginClaim::Claimed(claim)
    }

    pub fn commit(&self, claim: &InteractionClaim) -> bool {
        let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        let matches = inner
            .pending
            .get(&claim.key)
            .and_then(|pending| pending.claim.as_ref())
            == Some(claim);
        if !matches {
            return false;
        }
        inner.pending.remove(&claim.key);
        remember_resolved(&mut inner, claim.key.clone());
        inner
            .command_results
            .insert(claim.command_id.clone(), CompanionCommandStatus::Accepted);
        inner.command_order.push_back(claim.command_id.clone());
        while inner.command_order.len() > 1024 {
            if let Some(expired) = inner.command_order.pop_front() {
                inner.command_results.remove(&expired);
            }
        }
        true
    }

    pub fn rollback(&self, claim: &InteractionClaim) -> bool {
        let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        let Some(pending) = inner.pending.get_mut(&claim.key) else {
            return false;
        };
        if pending.claim.as_ref() != Some(claim) {
            return false;
        }
        pending.claim = None;
        true
    }

    pub fn resolve(&self, key: &CompanionInteractionKey) -> bool {
        let mut inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        let removed = inner.pending.remove(key).is_some();
        if removed {
            remember_resolved(&mut inner, key.clone());
        }
        removed
    }

    pub fn pending(&self) -> Vec<CompanionInteraction> {
        let inner = self.inner.lock().unwrap_or_else(|error| error.into_inner());
        inner
            .pending
            .values()
            .map(|pending| pending.interaction.clone())
            .collect()
    }
}

fn invalidate_seed_locked(
    inner: &mut CoordinatorState,
    seed: &str,
) -> Vec<CompanionInteractionKey> {
    let mut expired = Vec::new();
    inner.pending.retain(|key, _| {
        if key.seed == seed {
            expired.push(key.clone());
            false
        } else {
            true
        }
    });
    expired.sort_by(|left, right| {
        left.generation
            .cmp(&right.generation)
            .then_with(|| left.request_id.cmp(&right.request_id))
    });
    for key in &expired {
        remember_resolved(inner, key.clone());
    }
    expired
}

fn remember_resolved(inner: &mut CoordinatorState, key: CompanionInteractionKey) {
    if !inner.resolved.insert(key.clone()) {
        return;
    }
    inner.resolved_order.push_back(key);
    while inner.resolved_order.len() > 1024 {
        if let Some(expired) = inner.resolved_order.pop_front() {
            inner.resolved.remove(&expired);
        }
    }
}
