//! GoalEngine: autonomous plan execution with compact-aware scheduling.
//!
//! GoalEngine drives the Loop through autonomous turns using the
//! unified `todo.json` store. It loads a TodoStore on activation,
//! advances through items sequentially, and supports mid-execution
//! CRUD via pending changes that merge on turn boundaries.

use deepx_workspace::todo::{TodoItem, TodoMode, TodoStatus, TodoStore, load_todo, save_todo};

/// Task complexity — maintained locally since the TodoItem model no longer stores it.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub(crate) enum Complexity {
    Small,
    Medium,
    Large,
}

/// A buffered mutation applied on the next turn boundary.
#[derive(Debug, Clone)]
enum PendingChange {
    CreateItem(TodoItem),
    UpdateItem {
        id: String,
        status: Option<TodoStatus>,
        title: Option<String>,
        description: Option<String>,
        evidence: Option<String>,
    },
    DeleteItem(String),
}

pub struct GoalEngine {
    store: Option<TodoStore>,
    turns_since_compact: usize,
    compact_needed: bool,
    pending_changes: Vec<PendingChange>,
}

impl GoalEngine {
    pub fn new() -> Self {
        Self {
            store: None,
            turns_since_compact: 0,
            compact_needed: false,
            pending_changes: Vec::new(),
        }
    }

    // ═══════════════════════════════════════════
    // Goal activation
    // ═══════════════════════════════════════════

    /// Activate goal mode from the current todo.json.
    /// If `ids` are specified, only those items are activated (in order).
    /// Otherwise all pending/in_progress items are activated.
    pub fn activate(&mut self, ids: Option<&[String]>) -> Result<(), String> {
        let mut store = load_todo()?;

        if store.mode == TodoMode::Goal {
            return Err(
                "A goal is already active. Stop it first with todo(action=\"cancel\").".into(),
            );
        }

        // Filter items to activate
        let active_items: Vec<TodoItem> = if let Some(ids) = ids {
            ids.iter()
                .filter_map(|id| store.items.iter().find(|item| &item.id == id).cloned())
                .collect()
        } else {
            store
                .items
                .iter()
                .filter(|item| {
                    item.status == TodoStatus::Pending || item.status == TodoStatus::InProgress
                })
                .cloned()
                .collect()
        };

        if active_items.is_empty() {
            return Err(
                "No items to activate. Use todo(action=\"create\") first or specify ids.".into(),
            );
        }

        // Items are activated in their natural order (complexity sorting removed).
        let sorted = active_items;

        let first_id = sorted[0].id.clone();
        store.items = sorted;
        store.mode = TodoMode::Goal;
        store.current_id = Some(first_id);
        store.auto_turns = 0;

        save_todo(&store)?;
        self.store = Some(store);
        self.turns_since_compact = 0;
        self.compact_needed = false;
        self.pending_changes.clear();

        Ok(())
    }

    /// Stop (cancel) the current goal. Restores manual mode.
    pub fn stop(&mut self) -> Result<(), String> {
        let Some(mut store) = self.store.take() else {
            return Err("No active goal to stop.".into());
        };
        store.mode = TodoMode::Manual;
        store.current_id = None;
        save_todo(&store)?;
        self.pending_changes.clear();
        Ok(())
    }

    // ═══════════════════════════════════════════
    // Step advancement
    // ═══════════════════════════════════════════

    /// Mark the current step as complete and advance to the next.
    pub fn step_complete(&mut self, summary: &str) -> Result<Option<String>, String> {
        let Some(ref mut store) = self.store else {
            return Err("No active goal.".into());
        };

        let current_id = store.current_id.clone().ok_or("No current step.")?;

        // Mark current item completed
        if let Some(item) = store.items.iter_mut().find(|item| item.id == current_id) {
            item.status = TodoStatus::Completed;
            item.evidence = Some(summary.to_string());
        }

        // Find next pending/in_progress item
        let next = store
            .items
            .iter()
            .find(|item| {
                item.id != current_id
                    && (item.status == TodoStatus::Pending || item.status == TodoStatus::InProgress)
            })
            .cloned();

        if let Some(ref next_item) = next {
            // Mark next item in_progress
            if let Some(item) = store.items.iter_mut().find(|item| item.id == next_item.id) {
                item.status = TodoStatus::InProgress;
            }
            store.current_id = Some(next_item.id.clone());
            store.auto_turns += 1;
        } else {
            store.current_id = None;
            store.mode = TodoMode::Manual;
        }

        save_todo(store)?;
        Ok(next.map(|item| item.id))
    }

    /// Get the prompt for the current step.
    pub fn current_step_prompt(&self) -> Option<String> {
        let store = self.store.as_ref()?;
        let current_id = store.current_id.as_ref()?;
        let item = store.items.iter().find(|item| &item.id == current_id)?;

        let progress = store
            .items
            .iter()
            .filter(|item| item.status == TodoStatus::Completed)
            .count();

        Some(format!(
            "[自动执行计划 / 目标模式]\n\n\
             T{}: {}\n{}\n\n\
             进度: {}/{} 已完成\n\n\
             完成此步骤后，必须调用 todo(action=\\\"update\\\", id=\\\"{}\\\", status=\\\"completed\\\", evidence=\\\"...\\\")。\n\
             如果遇到无法自行安全解决的阻塞，调用 todo(action=\\\"cancel\\\", reason=\\\"...\\\") 或 ask。",
            item.id,
            item.title,
            item.description,
            progress + 1,
            store.items.len(),
            item.id,
        ))
    }

    // ═══════════════════════════════════════════
    // Pending changes (mid-execution CRUD)
    // ═══════════════════════════════════════════

    /// Buffer a create mutation, applied on next turn boundary.
    pub fn pending_create(&mut self, item: TodoItem) {
        self.pending_changes.push(PendingChange::CreateItem(item));
    }

    /// Buffer an update mutation.
    pub fn pending_update(
        &mut self,
        id: String,
        status: Option<TodoStatus>,
        title: Option<String>,
        description: Option<String>,
        evidence: Option<String>,
    ) {
        self.pending_changes.push(PendingChange::UpdateItem {
            id,
            status,
            title,
            description,
            evidence,
        });
    }

    /// Buffer a delete mutation.
    pub fn pending_delete(&mut self, id: String) {
        self.pending_changes.push(PendingChange::DeleteItem(id));
    }

    /// Apply all buffered changes to the store and persist.
    /// Called on turn boundaries (after TurnComplete).
    fn merge_pending(&mut self) -> Result<(), String> {
        let Some(ref mut store) = self.store else {
            self.pending_changes.clear();
            return Ok(());
        };

        for change in self.pending_changes.drain(..).collect::<Vec<_>>() {
            match change {
                PendingChange::CreateItem(item) => {
                    store.items.push(item);
                }
                PendingChange::UpdateItem {
                    id,
                    status,
                    title,
                    description,
                    evidence,
                } => {
                    if let Some(item) = store.items.iter_mut().find(|item| item.id == id) {
                        if let Some(s) = status {
                            item.status = s;
                        }
                        if let Some(t) = title {
                            item.title = t;
                        }
                        if let Some(d) = description {
                            item.description = d;
                        }
                        if let Some(e) = evidence {
                            item.evidence = Some(e);
                        }
                    }
                }
                PendingChange::DeleteItem(id) => {
                    store.items.retain(|item| item.id != id);
                    // If the deleted item was the current step, advance
                    if store.current_id.as_deref() == Some(&id) {
                        let next = store
                            .items
                            .iter()
                            .find(|item| {
                                item.status == TodoStatus::Pending
                                    || item.status == TodoStatus::InProgress
                            })
                            .cloned();
                        store.current_id = next.map(|item| item.id);
                    }
                }
            }
        }

        // Re-sort after merges (complexity sorting removed).
        let items = store.items.clone();
        store.items = items;

        save_todo(store)
    }

    // ═══════════════════════════════════════════
    // Compact integration
    // ═══════════════════════════════════════════

    /// Parse complexity-labeled tasks from a compact summary.
    pub(crate) fn parse_compact_tasks(summary: &str) -> Vec<(Complexity, String)> {
        let mut tasks = Vec::new();
        let mut in_remaining = false;

        for line in summary.lines() {
            let trimmed = line.trim();
            if trimmed.starts_with("### Remaining Work") || trimmed == "Remaining Work" {
                in_remaining = true;
                continue;
            }
            if trimmed.starts_with("### ") || trimmed.starts_with("## ") {
                if in_remaining && !trimmed.starts_with("### Remaining") {
                    break;
                }
                continue;
            }
            if !in_remaining {
                continue;
            }

            let lower = trimmed.to_lowercase();
            let (complexity, desc_start) =
                if lower.starts_with("[small]") || lower.starts_with("- [small]") {
                    (Complexity::Small, find_after_label(trimmed, "[small]"))
                } else if lower.starts_with("[medium]") || lower.starts_with("- [medium]") {
                    (Complexity::Medium, find_after_label(trimmed, "[medium]"))
                } else if lower.starts_with("[large]") || lower.starts_with("- [large]") {
                    (Complexity::Large, find_after_label(trimmed, "[large]"))
                } else {
                    continue;
                };

            let desc = trimmed[desc_start..].trim().to_string();
            if !desc.is_empty() && desc != "None" {
                tasks.push((complexity, desc));
            }
        }

        tasks
    }

    /// Sync compact summary back to todo.json items (for the next LLM).
    pub fn sync_from_compact(&mut self, summary: &str) -> Result<(), String> {
        let tasks = Self::parse_compact_tasks(summary);
        if tasks.is_empty() {
            return Ok(());
        }

        let Some(ref mut store) = self.store else {
            return Ok(());
        };

        for (_complexity, desc) in tasks {
            // Avoid duplicates: check if a similar task already exists
            if store
                .items
                .iter()
                .any(|item| item.description.contains(&desc) || desc.contains(&item.description))
            {
                continue;
            }
            // Generate a new ID
            let next = store
                .items
                .iter()
                .filter_map(|item| item.id.strip_prefix('T')?.parse::<u32>().ok())
                .max()
                .unwrap_or(0)
                + 1;
            store.items.push(TodoItem {
                id: format!("T{next}"),
                title: desc.chars().take(60).collect(),
                description: desc,
                status: TodoStatus::Pending,
                evidence: None,
            });
        }

        save_todo(store)
    }

    // ═══════════════════════════════════════════
    // Lifecycle hooks
    // ═══════════════════════════════════════════

    /// Called after each turn in goal mode.
    pub fn on_turn_complete(&mut self) {
        self.turns_since_compact += 1;
        if self.turns_since_compact >= 8 {
            self.compact_needed = true;
        }
        let _ = self.merge_pending();
    }

    pub fn should_refresh_compact(&self) -> bool {
        self.compact_needed || self.turns_since_compact >= 8
    }

    pub fn is_active(&self) -> bool {
        self.store
            .as_ref()
            .map(|s| s.mode == TodoMode::Goal)
            .unwrap_or(false)
    }

    pub fn store_ref(&self) -> Option<&TodoStore> {
        self.store.as_ref()
    }
}

fn find_after_label(line: &str, label: &str) -> usize {
    let lower = line.to_lowercase();
    if let Some(idx) = lower.find(&label.to_lowercase()) {
        idx + label.len()
    } else {
        if line.starts_with("- ") || line.starts_with("* ") {
            2
        } else {
            0
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_parse_compact_tasks() {
        let summary = "\
### Decision Log
- **Decision**: refactored X

### State Snapshot
- file.rs(123)

### Remaining Work
- [small] fix typo in config
- [medium] refactor handler
- [large] split plan/mod.rs

### Thinking Appendix
None";

        let tasks = GoalEngine::parse_compact_tasks(summary);
        assert_eq!(tasks.len(), 3);
        assert_eq!(tasks[0].0, Complexity::Small);
        assert_eq!(tasks[1].0, Complexity::Medium);
        assert_eq!(tasks[2].0, Complexity::Large);
        assert_eq!(tasks[0].1, "fix typo in config");
    }
}
