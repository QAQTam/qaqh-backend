//! Daemon-level parent/child lifecycle supervisor.
//!
//! The supervisor owns the authoritative in-memory parent/child edge state and
//! makes the unload order explicit:
//!
//! `child terminal -> parent SubagentFinished -> child join -> parent unload ack`
//!
//! Registry remains the owner of process handles. This module deliberately does
//! not know how to spawn, signal, or join workers; it only tracks edges and
//! records the lifecycle transitions the registry observes.

use std::collections::{BTreeSet, HashMap, HashSet};

const MAX_TRACE_EVENTS: usize = 4096;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChildState {
    Running,
    Terminal,
    Joined,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum LifecycleEvent {
    EdgeLinked { parent: String, child: String },
    EdgeUnlinked { parent: String, child: String },
    ParentUnloadRequested { parent: String },
    ChildCancelSent { parent: String, child: String },
    ChildTerminal { parent: String, child: String },
    ParentSubagentFinished { parent: String, child: String },
    ChildJoined { parent: String, child: String },
    ParentUnloadAck { parent: String },
}

impl LifecycleEvent {
    #[cfg(test)]
    pub(crate) fn kind(&self) -> &'static str {
        match self {
            Self::EdgeLinked { .. } => "edge_linked",
            Self::EdgeUnlinked { .. } => "edge_unlinked",
            Self::ParentUnloadRequested { .. } => "parent_unload_requested",
            Self::ChildCancelSent { .. } => "child_cancel_sent",
            Self::ChildTerminal { .. } => "child_terminal",
            Self::ParentSubagentFinished { .. } => "parent_subagent_finished",
            Self::ChildJoined { .. } => "child_joined",
            Self::ParentUnloadAck { .. } => "parent_unload_ack",
        }
    }
}

/// In-memory parent/child edge state used by [`AgentRegistry`](crate::AgentRegistry).
#[derive(Default)]
pub(crate) struct SubagentSupervisor {
    parent_of: HashMap<String, String>,
    children_of: HashMap<String, BTreeSet<String>>,
    state: HashMap<String, ChildState>,
    trace: Vec<LifecycleEvent>,
}

impl SubagentSupervisor {
    /// Link one parent/child edge idempotently.
    pub(crate) fn link(&mut self, parent: &str, child: &str) -> Result<(), String> {
        if parent.is_empty() || child.is_empty() || parent == child {
            return Err("subagent edge requires distinct non-empty parent/child".into());
        }
        if let Some(existing) = self.parent_of.get(child) {
            if existing == parent {
                return Ok(());
            }
            return Err(format!(
                "subagent child {child} already belongs to parent {existing}"
            ));
        }
        if self
            .descendants_postorder(child)
            .iter()
            .any(|seed| seed == parent)
        {
            return Err(format!(
                "subagent edge would create a cycle: {parent} -> {child}"
            ));
        }
        self.parent_of.insert(child.to_string(), parent.to_string());
        self.children_of
            .entry(parent.to_string())
            .or_default()
            .insert(child.to_string());
        self.state
            .entry(child.to_string())
            .or_insert(ChildState::Running);
        self.push_trace(LifecycleEvent::EdgeLinked {
            parent: parent.to_string(),
            child: child.to_string(),
        });
        Ok(())
    }

    /// Remove one child edge. Removing an unknown edge is idempotent.
    pub(crate) fn unlink(&mut self, child: &str) {
        let Some(parent) = self.parent_of.remove(child) else {
            self.children_of.remove(child);
            self.state.remove(child);
            return;
        };
        if let Some(children) = self.children_of.get_mut(&parent) {
            children.remove(child);
            if children.is_empty() {
                self.children_of.remove(&parent);
            }
        }
        self.children_of.remove(child);
        self.state.remove(child);
        self.push_trace(LifecycleEvent::EdgeUnlinked {
            parent,
            child: child.to_string(),
        });
    }

    pub(crate) fn parent_of(&self, child: &str) -> Option<String> {
        self.parent_of.get(child).cloned()
    }

    pub(crate) fn root_of(&self, seed: &str) -> String {
        let mut current = seed.to_string();
        let mut seen = HashSet::new();
        while seen.insert(current.clone()) {
            let Some(parent) = self.parent_of.get(&current) else {
                break;
            };
            current = parent.clone();
        }
        current
    }

    pub(crate) fn children_of(&self, parent: &str) -> Vec<String> {
        self.children_of
            .get(parent)
            .map(|children| children.iter().cloned().collect())
            .unwrap_or_default()
    }

    /// Post-order descendants: grandchildren before children.
    pub(crate) fn descendants_postorder(&self, parent: &str) -> Vec<String> {
        fn visit(
            supervisor: &SubagentSupervisor,
            parent: &str,
            seen: &mut HashSet<String>,
            out: &mut Vec<String>,
        ) {
            for child in supervisor.children_of(parent) {
                if !seen.insert(child.clone()) {
                    continue;
                }
                visit(supervisor, &child, seen, out);
                out.push(child);
            }
        }

        let mut out = Vec::new();
        visit(self, parent, &mut HashSet::new(), &mut out);
        out
    }

    pub(crate) fn begin_unload(&mut self, parent: &str) -> Vec<String> {
        self.push_trace(LifecycleEvent::ParentUnloadRequested {
            parent: parent.to_string(),
        });
        self.descendants_postorder(parent)
    }

    pub(crate) fn cancel_sent(&mut self, parent: &str, child: &str) {
        self.push_trace(LifecycleEvent::ChildCancelSent {
            parent: parent.to_string(),
            child: child.to_string(),
        });
    }

    pub(crate) fn child_terminal(&mut self, parent: &str, child: &str) {
        self.state.insert(child.to_string(), ChildState::Terminal);
        self.push_trace(LifecycleEvent::ChildTerminal {
            parent: parent.to_string(),
            child: child.to_string(),
        });
    }

    pub(crate) fn parent_subagent_finished(&mut self, parent: &str, child: &str) {
        debug_assert_eq!(
            self.state.get(child),
            Some(&ChildState::Terminal),
            "parent SubagentFinished must follow child terminal"
        );
        self.push_trace(LifecycleEvent::ParentSubagentFinished {
            parent: parent.to_string(),
            child: child.to_string(),
        });
    }

    pub(crate) fn child_joined(&mut self, parent: &str, child: &str) {
        self.state.insert(child.to_string(), ChildState::Joined);
        self.push_trace(LifecycleEvent::ChildJoined {
            parent: parent.to_string(),
            child: child.to_string(),
        });
    }

    pub(crate) fn parent_unload_ack(&mut self, parent: &str) {
        self.push_trace(LifecycleEvent::ParentUnloadAck {
            parent: parent.to_string(),
        });
    }

    pub(crate) fn trace(&self) -> &[LifecycleEvent] {
        &self.trace
    }

    fn push_trace(&mut self, event: LifecycleEvent) {
        if self.trace.len() >= MAX_TRACE_EVENTS {
            self.trace.remove(0);
        }
        self.trace.push(event);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn link_is_idempotent_and_rejects_conflicting_parent() {
        let mut supervisor = SubagentSupervisor::default();
        supervisor.link("parent", "child").unwrap();
        supervisor.link("parent", "child").unwrap();
        assert_eq!(supervisor.children_of("parent"), vec!["child"]);
        assert!(supervisor.link("other", "child").is_err());
    }

    #[test]
    fn unload_order_is_child_terminal_parent_finished_join_ack() {
        let mut supervisor = SubagentSupervisor::default();
        supervisor.link("parent", "child").unwrap();
        let descendants = supervisor.begin_unload("parent");
        assert_eq!(descendants, vec!["child"]);

        supervisor.cancel_sent("parent", "child");
        supervisor.child_terminal("parent", "child");
        supervisor.parent_subagent_finished("parent", "child");
        supervisor.child_joined("parent", "child");
        supervisor.parent_unload_ack("parent");

        let kinds: Vec<&str> = supervisor
            .trace()
            .iter()
            .map(LifecycleEvent::kind)
            .collect();
        assert_eq!(
            kinds,
            vec![
                "edge_linked",
                "parent_unload_requested",
                "child_cancel_sent",
                "child_terminal",
                "parent_subagent_finished",
                "child_joined",
                "parent_unload_ack",
            ]
        );
    }

    #[test]
    fn descendants_are_postorder() {
        let mut supervisor = SubagentSupervisor::default();
        supervisor.link("parent", "child").unwrap();
        supervisor.link("child", "grandchild").unwrap();
        assert_eq!(
            supervisor.descendants_postorder("parent"),
            vec!["grandchild", "child"]
        );
    }
}
