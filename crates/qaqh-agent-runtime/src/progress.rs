//! In-flight display channel for generation.
//!
//! Fragments here are display only. They never become facts, never authorize a
//! tool and never feed the model context: the durable truth is still the sealed
//! response and tool result. Updates carry the cumulative value for one stable
//! identity, so a dropped frame self-heals and a duplicate is idempotent.

use std::sync::Arc;

/// Which assistant block an update belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProgressBlockKind {
    Reasoning,
    Answer,
}

/// One cumulative assistant block update.
pub struct AssistantProgress<'a> {
    pub turn_id: &'a str,
    pub block_id: &'a str,
    pub kind: ProgressBlockKind,
    pub model: &'a str,
    pub text: &'a str,
}

/// One cumulative tool-argument update.
pub struct ToolInputProgress<'a> {
    pub turn_id: &'a str,
    pub call_id: &'a str,
    pub tool_name: &'a str,
    pub args: &'a str,
}

/// Host-owned destination for in-flight display updates.
pub trait ProgressSink: Send + Sync + 'static {
    fn assistant_block(&self, update: AssistantProgress<'_>);
    fn tool_input(&self, update: ToolInputProgress<'_>);
    /// The attempt ended (sealed, failed, cancelled or superseded). Any
    /// in-flight identity must be dropped so a reconnect cannot resurrect it.
    fn attempt_finished(&self, turn_id: &str, block_ids: &[String], call_ids: &[String]);
}

/// Block identities allocated before generation starts.
///
/// The same ids are reused when the response seals, so streaming text and the
/// durable block are one identity instead of two blocks that appear to append.
#[derive(Clone, Debug)]
pub struct AttemptBlocks {
    pub turn_id: String,
    pub model: String,
    pub reasoning_block_id: String,
    pub answer_block_id: String,
}

impl AttemptBlocks {
    pub fn new(turn_id: &str, model: &str) -> Self {
        Self {
            turn_id: turn_id.to_string(),
            model: model.to_string(),
            reasoning_block_id: format!(
                "block_{}",
                qaqh_session::canonical::generate_ulid()
            ),
            answer_block_id: format!("block_{}", qaqh_session::canonical::generate_ulid()),
        }
    }

    pub fn all_block_ids(&self) -> Vec<String> {
        vec![
            self.reasoning_block_id.clone(),
            self.answer_block_id.clone(),
        ]
    }
}

/// Everything one provider attempt needs to publish display progress.
#[derive(Clone)]
pub struct ProgressStream {
    pub sink: Arc<dyn ProgressSink>,
    pub blocks: AttemptBlocks,
}
