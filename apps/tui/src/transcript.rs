//! Transcript model: renders the Ringing V1 timeline (authoritative
//! snapshot + incremental SSE entries) into flat display lines.
//!
//! Snapshot application is a full rebuild (idempotent — `activate_timeline`
//! pushes the same page through the handler and the return value), while
//! incremental entries mutate the line addressed by `block_id`.

use std::collections::HashMap;

use qaqh_client::types::{TimelineEntry, TimelinePage};
use qaqh_domain::{
    TimelineBlock, TimelineBlockKind, TimelineEvent, TimelineTool, TimelineToolState,
    TimelineTurnState,
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    User,
    Assistant,
    Tool,
    Notice,
}

#[derive(Debug, Clone)]
pub struct Line {
    pub role: Role,
    pub text: String,
}

#[derive(Debug, Default)]
pub struct Transcript {
    pub lines: Vec<Line>,
    block_line: HashMap<String, usize>,
}

impl Transcript {
    pub fn new() -> Self {
        Self::default()
    }

    /// Rebuild from the authoritative snapshot (also used on gap recovery).
    pub fn apply_snapshot(&mut self, page: &TimelinePage) {
        self.lines.clear();
        self.block_line.clear();
        for turn in &page.snapshot.turns {
            if !turn.user_text.is_empty() {
                self.push(Role::User, format!("❯ {}", turn.user_text));
            }
            for round in &turn.rounds {
                for block in &round.blocks {
                    self.open_block(block);
                }
            }
            if turn.state != TimelineTurnState::Completed {
                self.push(Role::Notice, turn_state_line(turn.state, turn.failure.as_ref()));
            }
        }
    }

    /// Apply one incremental timeline entry.
    pub fn apply_entry(&mut self, entry: &TimelineEntry) {
        match &entry.event {
            TimelineEvent::TurnOpened { user_text } => {
                self.push(Role::User, format!("❯ {user_text}"));
            }
            TimelineEvent::BlockOpened { block } => self.open_block(block),
            TimelineEvent::TextDelta { block_id, delta, .. } => self.append_to(block_id, delta),
            TimelineEvent::BlockCheckpoint { block_id, text } => self.replace(block_id, text),
            TimelineEvent::ToolUpdated { block_id, tool } => {
                self.replace(block_id, &tool_line(tool));
            }
            TimelineEvent::ToolProgress { block_id, chunk } => {
                self.append_to(block_id, chunk);
            }
            TimelineEvent::BlockSealed { block_id } => {
                if let Some(index) = self.block_line.get(block_id).copied() {
                    if self.lines[index].role == Role::Tool
                        && !self.lines[index].text.ends_with(" ✓")
                    {
                        self.lines[index].text.push_str(" ✓");
                    }
                }
            }
            TimelineEvent::RoundSealed { .. } => {}
            TimelineEvent::TurnSealed { state, failure } => {
                self.push(Role::Notice, turn_state_line(*state, failure.as_ref()));
            }
        }
    }

    fn open_block(&mut self, block: &TimelineBlock) {
        match block.kind {
            TimelineBlockKind::Text | TimelineBlockKind::Reasoning => {
                self.upsert(&block.block_id, Role::Assistant, block.text.clone());
            }
            TimelineBlockKind::Tool => {
                let text = match &block.tool {
                    Some(tool) => tool_line(tool),
                    None => "⚙ tool".to_string(),
                };
                self.upsert(&block.block_id, Role::Tool, text);
            }
            TimelineBlockKind::Notice => {
                self.upsert(&block.block_id, Role::Notice, block.text.clone());
            }
        }
    }

    fn push(&mut self, role: Role, text: String) {
        self.lines.push(Line { role, text });
    }

    /// Insert or replace the line addressed by `block_id`.
    fn upsert(&mut self, block_id: &str, role: Role, text: String) {
        if let Some(index) = self.block_line.get(block_id).copied() {
            let line = &mut self.lines[index];
            line.role = role;
            line.text = text;
        } else {
            self.block_line.insert(block_id.to_string(), self.lines.len());
            self.push(role, text);
        }
    }

    fn append_to(&mut self, block_id: &str, delta: &str) {
        if let Some(index) = self.block_line.get(block_id).copied() {
            self.lines[index].text.push_str(delta);
        }
    }

    fn replace(&mut self, block_id: &str, text: &str) {
        if let Some(index) = self.block_line.get(block_id).copied() {
            self.lines[index].text = text.to_string();
        }
    }
}

fn tool_state(state: TimelineToolState) -> &'static str {
    match state {
        TimelineToolState::Prepared => "prepared",
        TimelineToolState::Running => "running",
        TimelineToolState::Succeeded => "succeeded",
        TimelineToolState::Failed => "failed",
    }
}

fn tool_line(tool: &TimelineTool) -> String {
    let mut line = format!("⚙ {} · {}", tool.name, tool_state(tool.state));
    if let Some(summary) = &tool.summary {
        line.push_str(" · ");
        line.push_str(summary);
    }
    if let Some(output) = &tool.output {
        line.push_str("\n");
        line.push_str(output);
    }
    line
}

fn turn_state_line(
    state: TimelineTurnState,
    failure: Option<&qaqh_domain::TimelineFailure>,
) -> String {
    let base = match state {
        TimelineTurnState::Running => "⟳ running",
        TimelineTurnState::Completed => "✓ completed",
        TimelineTurnState::Failed => "✗ failed",
        TimelineTurnState::Cancelled => "⊘ cancelled",
    };
    match failure {
        Some(failure) => format!("{base} — {}", failure.message),
        None => base.to_string(),
    }
}
