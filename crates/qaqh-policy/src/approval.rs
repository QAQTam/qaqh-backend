//! Approval lifecycle registry.
//!
//! The registry owns the first-answer-wins decision for pending approvals.
//! The first resolution consumes the pending challenge; later responses return
//! the stored decision without producing another grant.

use std::collections::{HashMap, VecDeque};

const MAX_RESOLVED_TOMBSTONES: usize = 1024;

/// Stable decision recorded for a resolved approval.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ApprovalDecision {
    Approved,
    Rejected,
    Expired,
}

impl ApprovalDecision {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Approved => "approved",
            Self::Rejected => "rejected",
            Self::Expired => "expired",
        }
    }
}

/// Result of looking up a pending approval.
#[derive(Debug, PartialEq, Eq)]
pub enum ApprovalTake<T> {
    Pending(T),
    AlreadyResolved(ApprovalDecision),
    Missing,
}

/// First-answer-wins registry for pending approvals.
pub struct ApprovalRegistry<T> {
    pending: HashMap<String, T>,
    resolved: HashMap<String, ApprovalDecision>,
    resolved_order: VecDeque<String>,
}

impl<T> Default for ApprovalRegistry<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T> ApprovalRegistry<T> {
    pub fn new() -> Self {
        Self {
            pending: HashMap::new(),
            resolved: HashMap::new(),
            resolved_order: VecDeque::new(),
        }
    }

    /// Register a new pending approval. Reusing a key starts a new interaction.
    pub fn insert(&mut self, call_id: impl Into<String>, pending: T) -> Option<T> {
        let call_id = call_id.into();
        if self.resolved.remove(&call_id).is_some() {
            self.resolved_order.retain(|existing| existing != &call_id);
        }
        self.pending.insert(call_id, pending)
    }

    /// Consume a pending approval or return the stable first-answer decision.
    pub fn take(&mut self, call_id: &str) -> ApprovalTake<T> {
        if let Some(pending) = self.pending.remove(call_id) {
            return ApprovalTake::Pending(pending);
        }
        self.resolved
            .get(call_id)
            .copied()
            .map(ApprovalTake::AlreadyResolved)
            .unwrap_or(ApprovalTake::Missing)
    }

    /// 把入站 id 归一到 registry 的 key：先精确匹配；否则由调用方注入的
    /// 匹配器在挂起/已解决项里找。
    ///
    /// v2 投影（`ControlDelta::InteractionRequested.call_id`）只暴露 canonical
    /// call_id，而本表按 wire id 记账——不归一的话 v2 壳层的答复会落成
    /// `unknown permission response`。匹配语义（如 wire id ↔ canonical id）
    /// 由调用方注入，本 crate 保持零领域依赖。
    pub fn resolve_key_with(
        &self,
        incoming: &str,
        matches: impl Fn(&str, &str) -> bool,
    ) -> Option<String> {
        if self.pending.contains_key(incoming) || self.resolved.contains_key(incoming) {
            return Some(incoming.to_string());
        }
        self.pending
            .keys()
            .chain(self.resolved.keys())
            .find(|key| matches(key, incoming))
            .cloned()
    }

    /// Record the first answer for a consumed pending approval.
    pub fn mark_resolved(&mut self, call_id: impl Into<String>, decision: ApprovalDecision) {
        let call_id = call_id.into();
        if self.resolved.insert(call_id.clone(), decision).is_none() {
            self.resolved_order.push_back(call_id);
        }
        while self.resolved.len() > MAX_RESOLVED_TOMBSTONES {
            let Some(oldest) = self.resolved_order.pop_front() else {
                break;
            };
            self.resolved.remove(&oldest);
        }
    }

    pub fn clear(&mut self) {
        self.pending.clear();
        self.resolved.clear();
        self.resolved_order.clear();
    }

    #[cfg(test)]
    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    #[cfg(test)]
    pub fn resolved_len(&self) -> usize {
        self.resolved.len()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_answer_wins_and_duplicate_returns_stable_decision() {
        let mut registry = ApprovalRegistry::new();
        assert!(registry.insert("call-1", "challenge").is_none());

        let pending = match registry.take("call-1") {
            ApprovalTake::Pending(pending) => pending,
            other => panic!("expected pending approval, got {other:?}"),
        };
        assert_eq!(pending, "challenge");
        registry.mark_resolved("call-1", ApprovalDecision::Approved);

        assert_eq!(
            registry.take("call-1"),
            ApprovalTake::AlreadyResolved(ApprovalDecision::Approved)
        );
        assert_eq!(registry.pending_len(), 0);
        assert_eq!(registry.resolved_len(), 1);
    }

    #[test]
    fn missing_approval_is_not_reported_as_resolved() {
        let mut registry = ApprovalRegistry::<&str>::new();
        assert_eq!(registry.take("missing"), ApprovalTake::Missing);
    }

    #[test]
    fn clear_drops_pending_and_resolved_state() {
        let mut registry = ApprovalRegistry::new();
        registry.insert("call-1", "pending");
        registry.mark_resolved("call-2", ApprovalDecision::Rejected);

        registry.clear();

        assert_eq!(registry.pending_len(), 0);
        assert_eq!(registry.resolved_len(), 0);
        assert_eq!(registry.take("call-1"), ApprovalTake::Missing);
        assert_eq!(registry.take("call-2"), ApprovalTake::Missing);
    }

    #[test]
    fn reusing_a_key_starts_a_new_interaction() {
        let mut registry = ApprovalRegistry::new();
        registry.mark_resolved("call-1", ApprovalDecision::Approved);
        assert!(registry.insert("call-1", "new challenge").is_none());

        assert_eq!(
            registry.take("call-1"),
            ApprovalTake::Pending("new challenge")
        );
    }

    #[test]
    fn resolved_tombstones_are_bounded() {
        let mut registry = ApprovalRegistry::<&str>::new();
        for index in 0..=MAX_RESOLVED_TOMBSTONES {
            registry.mark_resolved(format!("call-{index}"), ApprovalDecision::Approved);
        }

        assert_eq!(registry.resolved_len(), MAX_RESOLVED_TOMBSTONES);
        assert_eq!(registry.take("call-0"), ApprovalTake::Missing);
    }

    #[test]
    fn resolve_key_uses_injected_matcher_for_canonical_ids() {
        let mut registry = ApprovalRegistry::new();
        registry.insert("toolu_01ABC", "pending");

        let key = registry.resolve_key_with("call_01ABC", |stored, incoming| {
            stored == incoming
                || stored
                    .trim_start_matches("toolu_")
                    .eq_ignore_ascii_case(incoming.trim_start_matches("call_"))
        });
        assert_eq!(key.as_deref(), Some("toolu_01ABC"));
    }
}
