//! Parent/child topology for cancellation and descendant-first unloading.

use std::collections::{BTreeSet, HashMap, HashSet};

/// In-memory parent/child edge state used by [`AgentRegistry`](crate::AgentRegistry).
#[derive(Default)]
pub(crate) struct SubagentSupervisor {
    parent_of: HashMap<String, String>,
    children_of: HashMap<String, BTreeSet<String>>,
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
            .any(|session_id| session_id == parent)
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
        Ok(())
    }

    /// Remove one child edge. Removing an unknown edge is idempotent.
    pub(crate) fn unlink(&mut self, child: &str) {
        let Some(parent) = self.parent_of.remove(child) else {
            self.children_of.remove(child);
            return;
        };
        if let Some(children) = self.children_of.get_mut(&parent) {
            children.remove(child);
            if children.is_empty() {
                self.children_of.remove(&parent);
            }
        }
        self.children_of.remove(child);
    }

    pub(crate) fn parent_of(&self, child: &str) -> Option<String> {
        self.parent_of.get(child).cloned()
    }

    pub(crate) fn root_of(&self, session_id: &str) -> String {
        let mut current = session_id.to_string();
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
