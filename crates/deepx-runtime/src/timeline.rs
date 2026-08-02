//! Single-writer native transcript timeline.
//!
//! The appender owns sequence allocation and materializes snapshots from the
//! same records it returns to transport. It intentionally does not depend on
//! `Agent2Ui` or the legacy Ringing conversation/tool projections.

use std::collections::{BTreeMap, HashMap};
use std::fmt;

use deepx_domain::{
    TimelineBlock, TimelineBlockKind, TimelineBlockState, TimelineEntry, TimelineEvent,
    TimelineIntent, TimelineRound, TimelineSnapshot, TimelineTool, TimelineToolState, TimelineTurn,
    TimelineTurnState,
};

/// A live v3 delivery record. `entry.timeline_seq` is the sole SSE cursor for
/// this seed; no per-channel sequence is exposed to a transcript consumer.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TimelineLiveEntry {
    pub seed: String,
    pub entry: TimelineEntry,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum TimelineError {
    DuplicateTurn(String),
    MissingTurn(String),
    MissingRound {
        turn_id: String,
        round_num: u32,
    },
    DuplicateBlock(String),
    MissingBlock(String),
    InvalidBlockShape(String),
    InvalidBlockKind(String),
    InvalidToolIdentity(String),
    SealedBlock(String),
    SealedRound {
        turn_id: String,
        round_num: u32,
    },
    SealedTurn(String),
    RoundOutOfOrder {
        turn_id: String,
        expected: u32,
        received: u32,
    },
    FragmentOutOfOrder {
        block_id: String,
        expected: u64,
        received: u64,
    },
    RoundNotReady {
        turn_id: String,
        round_num: u32,
    },
    TurnNotReady(String),
}

impl fmt::Display for TimelineError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::DuplicateTurn(turn_id) => write!(f, "timeline turn already exists: {turn_id}"),
            Self::MissingTurn(turn_id) => write!(f, "timeline turn does not exist: {turn_id}"),
            Self::MissingRound { turn_id, round_num } => {
                write!(f, "timeline round does not exist: {turn_id}/{round_num}")
            }
            Self::DuplicateBlock(block_id) => {
                write!(f, "timeline block already exists: {block_id}")
            }
            Self::MissingBlock(block_id) => write!(f, "timeline block does not exist: {block_id}"),
            Self::InvalidBlockShape(block_id) => {
                write!(f, "timeline block kind and payload disagree: {block_id}")
            }
            Self::InvalidBlockKind(block_id) => {
                write!(f, "timeline block cannot receive text: {block_id}")
            }
            Self::InvalidToolIdentity(block_id) => {
                write!(f, "timeline tool identity changed: {block_id}")
            }
            Self::SealedBlock(block_id) => write!(f, "timeline block is sealed: {block_id}"),
            Self::SealedRound { turn_id, round_num } => {
                write!(f, "timeline round is sealed: {turn_id}/{round_num}")
            }
            Self::SealedTurn(turn_id) => write!(f, "timeline turn is sealed: {turn_id}"),
            Self::RoundOutOfOrder {
                turn_id,
                expected,
                received,
            } => write!(
                f,
                "timeline round out of order for {turn_id}: expected {expected}, got {received}"
            ),
            Self::FragmentOutOfOrder {
                block_id,
                expected,
                received,
            } => write!(
                f,
                "timeline fragment out of order for {block_id}: expected {expected}, got {received}"
            ),
            Self::RoundNotReady { turn_id, round_num } => {
                write!(
                    f,
                    "timeline round has unsealed blocks: {turn_id}/{round_num}"
                )
            }
            Self::TurnNotReady(turn_id) => {
                write!(f, "timeline turn has unsealed rounds: {turn_id}")
            }
        }
    }
}

impl std::error::Error for TimelineError {}

#[derive(Debug, Default)]
struct SeedTimeline {
    next_seq: u64,
    turns: BTreeMap<String, TimelineTurn>,
    journal: Vec<TimelineEntry>,
    next_fragment: HashMap<(String, u32, String), u64>,
}

/// The only component allowed to allocate timeline sequences.
///
/// A future transport actor owns this mutably; keeping its API on `&mut self`
/// makes accidental concurrent producers impossible without an explicit queue.
#[derive(Debug, Default)]
pub struct TimelineAppender {
    seeds: HashMap<String, SeedTimeline>,
}

impl TimelineAppender {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn open_turn(
        &mut self,
        seed: &str,
        turn_id: impl Into<String>,
        user_text: impl Into<String>,
    ) -> Result<TimelineEntry, TimelineError> {
        let turn_id = turn_id.into();
        let timeline = self.seeds.entry(seed.to_string()).or_default();
        if timeline.turns.contains_key(&turn_id) {
            return Err(TimelineError::DuplicateTurn(turn_id));
        }
        let user_text = user_text.into();
        timeline.turns.insert(
            turn_id.clone(),
            TimelineTurn {
                turn_id: turn_id.clone(),
                user_text: user_text.clone(),
                sealed: false,
                state: TimelineTurnState::Running,
                failure: None,
                rounds: vec![],
            },
        );
        Ok(next_entry(
            timeline,
            turn_id,
            None,
            TimelineEvent::TurnOpened { user_text },
        ))
    }

    pub fn open_block(
        &mut self,
        seed: &str,
        turn_id: &str,
        round_num: u32,
        block_id: impl Into<String>,
        kind: TimelineBlockKind,
        tool: Option<TimelineTool>,
    ) -> Result<TimelineEntry, TimelineError> {
        let block_id = block_id.into();
        if (kind == TimelineBlockKind::Tool) != tool.is_some() {
            return Err(TimelineError::InvalidBlockShape(block_id));
        }
        let timeline = self.timeline_mut(seed)?;
        let round = ensure_round_mut(timeline, turn_id, round_num)?;
        if round.sealed {
            return Err(TimelineError::SealedRound {
                turn_id: turn_id.to_string(),
                round_num,
            });
        }
        if round.blocks.iter().any(|block| block.block_id == block_id) {
            return Err(TimelineError::DuplicateBlock(block_id));
        }
        let block = TimelineBlock {
            block_id: block_id.clone(),
            block_order: u32::try_from(round.blocks.len()).unwrap_or(u32::MAX),
            kind,
            state: TimelineBlockState::Open,
            text: String::new(),
            tool,
        };
        round.blocks.push(block.clone());
        Ok(next_entry(
            timeline,
            turn_id.to_string(),
            Some(round_num),
            TimelineEvent::BlockOpened { block },
        ))
    }

    pub fn append_text(
        &mut self,
        seed: &str,
        turn_id: &str,
        round_num: u32,
        block_id: &str,
        fragment_seq: u64,
        delta: impl Into<String>,
    ) -> Result<TimelineEntry, TimelineError> {
        let timeline = self.timeline_mut(seed)?;
        let key = (turn_id.to_string(), round_num, block_id.to_string());
        let expected = *timeline.next_fragment.get(&key).unwrap_or(&0);
        if fragment_seq != expected {
            return Err(TimelineError::FragmentOutOfOrder {
                block_id: block_id.to_string(),
                expected,
                received: fragment_seq,
            });
        }
        let delta = delta.into();
        let round = existing_round_mut(timeline, turn_id, round_num)?;
        let block = block_mut(round, block_id)?;
        if block.state == TimelineBlockState::Sealed {
            return Err(TimelineError::SealedBlock(block_id.to_string()));
        }
        if !matches!(
            block.kind,
            TimelineBlockKind::Reasoning | TimelineBlockKind::Text
        ) {
            return Err(TimelineError::InvalidBlockKind(block_id.to_string()));
        }
        block.text.push_str(&delta);
        timeline
            .next_fragment
            .insert(key, expected.saturating_add(1));
        Ok(next_entry(
            timeline,
            turn_id.to_string(),
            Some(round_num),
            TimelineEvent::TextDelta {
                block_id: block_id.to_string(),
                fragment_seq,
                delta,
            },
        ))
    }

    pub fn update_tool(
        &mut self,
        seed: &str,
        turn_id: &str,
        round_num: u32,
        block_id: &str,
        state: TimelineToolState,
        summary: Option<String>,
    ) -> Result<TimelineEntry, TimelineError> {
        let timeline = self.timeline_mut(seed)?;
        let round = existing_round_mut(timeline, turn_id, round_num)?;
        let block = block_mut(round, block_id)?;
        if block.state == TimelineBlockState::Sealed {
            return Err(TimelineError::SealedBlock(block_id.to_string()));
        }
        let Some(tool) = block.tool.as_mut() else {
            return Err(TimelineError::InvalidBlockKind(block_id.to_string()));
        };
        tool.state = state;
        tool.summary = summary.or_else(|| tool.summary.clone());
        let tool = tool.clone();
        Ok(next_entry(
            timeline,
            turn_id.to_string(),
            Some(round_num),
            TimelineEvent::ToolUpdated {
                block_id: block_id.to_string(),
                tool,
            },
        ))
    }

    /// Updates mutable presentation fields while preserving the identity and
    /// any durable detail omitted by a lifecycle producer (notably retained
    /// execution progress and a pending permission record).
    pub fn replace_tool(
        &mut self,
        seed: &str,
        turn_id: &str,
        round_num: u32,
        block_id: &str,
        mut next_tool: TimelineTool,
    ) -> Result<TimelineEntry, TimelineError> {
        let timeline = self.timeline_mut(seed)?;
        let round = existing_round_mut(timeline, turn_id, round_num)?;
        let block = block_mut(round, block_id)?;
        if block.state == TimelineBlockState::Sealed {
            return Err(TimelineError::SealedBlock(block_id.to_string()));
        }
        let Some(tool) = block.tool.as_mut() else {
            return Err(TimelineError::InvalidBlockKind(block_id.to_string()));
        };
        if tool.tool_call_id != next_tool.tool_call_id || tool.name != next_tool.name {
            return Err(TimelineError::InvalidToolIdentity(block_id.to_string()));
        }
        if next_tool.progress.is_empty() {
            next_tool.progress = tool.progress.clone();
        }
        if next_tool.permission.is_none() {
            next_tool.permission = tool.permission.clone();
        }
        *tool = next_tool.clone();
        Ok(next_entry(
            timeline,
            turn_id.to_string(),
            Some(round_num),
            TimelineEvent::ToolUpdated {
                block_id: block_id.to_string(),
                tool: next_tool,
            },
        ))
    }

    /// Applies an append-only execution-output patch to an existing tool
    /// block. Identity, arguments, terminal output, and permission state stay
    /// untouched until their explicit lifecycle update arrives.
    pub fn append_tool_progress(
        &mut self,
        seed: &str,
        turn_id: &str,
        round_num: u32,
        block_id: &str,
        chunk: String,
    ) -> Result<TimelineEntry, TimelineError> {
        let timeline = self.timeline_mut(seed)?;
        let round = existing_round_mut(timeline, turn_id, round_num)?;
        let block = block_mut(round, block_id)?;
        if block.state == TimelineBlockState::Sealed {
            return Err(TimelineError::SealedBlock(block_id.to_string()));
        }
        let Some(tool) = block.tool.as_mut() else {
            return Err(TimelineError::InvalidBlockKind(block_id.to_string()));
        };
        tool.progress.push_str(&chunk);
        Ok(next_entry(
            timeline,
            turn_id.to_string(),
            Some(round_num),
            TimelineEvent::ToolProgress {
                block_id: block_id.to_string(),
                chunk,
            },
        ))
    }

    /// Applies one producer intent. The method is the only place that turns a
    /// producer's ordered intent into a numbered transcript record.
    pub fn apply_intent(
        &mut self,
        seed: &str,
        intent: TimelineIntent,
    ) -> Result<TimelineEntry, TimelineError> {
        match intent {
            TimelineIntent::TurnOpened { turn_id, user_text } => {
                self.open_turn(seed, turn_id, user_text)
            }
            TimelineIntent::BlockOpened {
                turn_id,
                round_num,
                block_id,
                kind,
                tool,
            } => self.open_block(seed, &turn_id, round_num, block_id, kind, tool),
            TimelineIntent::TextDelta {
                turn_id,
                round_num,
                block_id,
                delta,
            } => {
                let fragment_seq = self
                    .seeds
                    .get(seed)
                    .and_then(|timeline| {
                        timeline
                            .next_fragment
                            .get(&(turn_id.clone(), round_num, block_id.clone()))
                    })
                    .copied()
                    .unwrap_or(0);
                self.append_text(seed, &turn_id, round_num, &block_id, fragment_seq, delta)
            }
            TimelineIntent::ToolUpdated {
                turn_id,
                round_num,
                block_id,
                tool,
            } => self.replace_tool(seed, &turn_id, round_num, &block_id, tool),
            TimelineIntent::ToolProgress {
                turn_id,
                round_num,
                block_id,
                chunk,
            } => self.append_tool_progress(seed, &turn_id, round_num, &block_id, chunk),
            TimelineIntent::BlockSealed {
                turn_id,
                round_num,
                block_id,
            } => self.seal_block(seed, &turn_id, round_num, &block_id),
            TimelineIntent::RoundSealed {
                turn_id,
                round_num,
                is_final,
            } => self.seal_round(seed, &turn_id, round_num, is_final),
            TimelineIntent::TurnSealed {
                turn_id,
                state,
                failure,
            } => self.seal_turn_with_state(seed, &turn_id, state, failure),
        }
    }

    pub fn seal_block(
        &mut self,
        seed: &str,
        turn_id: &str,
        round_num: u32,
        block_id: &str,
    ) -> Result<TimelineEntry, TimelineError> {
        let timeline = self.timeline_mut(seed)?;
        let round = existing_round_mut(timeline, turn_id, round_num)?;
        let block = block_mut(round, block_id)?;
        block.state = TimelineBlockState::Sealed;
        Ok(next_entry(
            timeline,
            turn_id.to_string(),
            Some(round_num),
            TimelineEvent::BlockSealed {
                block_id: block_id.to_string(),
            },
        ))
    }

    pub fn seal_round(
        &mut self,
        seed: &str,
        turn_id: &str,
        round_num: u32,
        is_final: bool,
    ) -> Result<TimelineEntry, TimelineError> {
        let timeline = self.timeline_mut(seed)?;
        let round = existing_round_mut(timeline, turn_id, round_num)?;
        if round
            .blocks
            .iter()
            .any(|block| block.state != TimelineBlockState::Sealed)
        {
            return Err(TimelineError::RoundNotReady {
                turn_id: turn_id.to_string(),
                round_num,
            });
        }
        round.sealed = true;
        round.is_final = is_final;
        Ok(next_entry(
            timeline,
            turn_id.to_string(),
            Some(round_num),
            TimelineEvent::RoundSealed { is_final },
        ))
    }

    pub fn seal_turn(&mut self, seed: &str, turn_id: &str) -> Result<TimelineEntry, TimelineError> {
        self.seal_turn_with_state(seed, turn_id, TimelineTurnState::Completed, None)
    }

    pub fn seal_turn_with_state(
        &mut self,
        seed: &str,
        turn_id: &str,
        state: TimelineTurnState,
        failure: Option<deepx_domain::TimelineFailure>,
    ) -> Result<TimelineEntry, TimelineError> {
        let timeline = self.timeline_mut(seed)?;
        let turn = timeline
            .turns
            .get_mut(turn_id)
            .ok_or_else(|| TimelineError::MissingTurn(turn_id.into()))?;
        if turn.rounds.iter().any(|round| !round.sealed) {
            return Err(TimelineError::TurnNotReady(turn_id.to_string()));
        }
        turn.sealed = true;
        turn.state = state;
        turn.failure = failure.clone();
        Ok(next_entry(
            timeline,
            turn_id.to_string(),
            None,
            TimelineEvent::TurnSealed { state, failure },
        ))
    }

    pub fn replay_since(&self, seed: &str, watermark: u64) -> Vec<TimelineEntry> {
        self.seeds.get(seed).map_or_else(Vec::new, |timeline| {
            timeline
                .journal
                .iter()
                .filter(|entry| entry.timeline_seq > watermark)
                .cloned()
                .collect()
        })
    }

    pub fn snapshot(&self, seed: &str) -> Option<TimelineSnapshot> {
        self.seeds.get(seed).map(|timeline| TimelineSnapshot {
            watermark: timeline.next_seq,
            turns: timeline.turns.values().cloned().collect(),
        })
    }

    /// Restores a journal that was previously produced by this appender. The
    /// persisted materialized snapshot is authoritative; the journal is kept
    /// solely for replay after a reconnect watermark.
    pub fn restore(
        &mut self,
        seed: String,
        snapshot: TimelineSnapshot,
        journal: Vec<TimelineEntry>,
    ) {
        let mut next_fragment = HashMap::new();
        for entry in &journal {
            if let TimelineEvent::TextDelta {
                block_id,
                fragment_seq,
                ..
            } = &entry.event
            {
                if let Some(round_num) = entry.round_num {
                    next_fragment.insert(
                        (entry.turn_id.clone(), round_num, block_id.clone()),
                        fragment_seq.saturating_add(1),
                    );
                }
            }
        }
        self.seeds.insert(
            seed,
            SeedTimeline {
                next_seq: snapshot.watermark,
                turns: snapshot
                    .turns
                    .into_iter()
                    .map(|turn| (turn.turn_id.clone(), turn))
                    .collect(),
                journal,
                next_fragment,
            },
        );
    }

    fn timeline_mut(&mut self, seed: &str) -> Result<&mut SeedTimeline, TimelineError> {
        self.seeds
            .get_mut(seed)
            .ok_or_else(|| TimelineError::MissingTurn(format!("seed:{seed}")))
    }
}

fn next_entry(
    timeline: &mut SeedTimeline,
    turn_id: String,
    round_num: Option<u32>,
    event: TimelineEvent,
) -> TimelineEntry {
    timeline.next_seq = timeline.next_seq.saturating_add(1);
    let entry = TimelineEntry {
        timeline_seq: timeline.next_seq,
        turn_id,
        round_num,
        event,
    };
    timeline.journal.push(entry.clone());
    entry
}

fn existing_round_mut<'a>(
    timeline: &'a mut SeedTimeline,
    turn_id: &str,
    round_num: u32,
) -> Result<&'a mut TimelineRound, TimelineError> {
    let turn = timeline
        .turns
        .get_mut(turn_id)
        .ok_or_else(|| TimelineError::MissingTurn(turn_id.into()))?;
    let index = turn
        .rounds
        .iter()
        .position(|round| round.round_num == round_num)
        .ok_or_else(|| TimelineError::MissingRound {
            turn_id: turn_id.to_string(),
            round_num,
        })?;
    Ok(&mut turn.rounds[index])
}

fn ensure_round_mut<'a>(
    timeline: &'a mut SeedTimeline,
    turn_id: &str,
    round_num: u32,
) -> Result<&'a mut TimelineRound, TimelineError> {
    let turn = timeline
        .turns
        .get_mut(turn_id)
        .ok_or_else(|| TimelineError::MissingTurn(turn_id.into()))?;
    if turn.sealed {
        return Err(TimelineError::SealedTurn(turn_id.to_string()));
    }
    let index = match turn
        .rounds
        .iter()
        .position(|round| round.round_num == round_num)
    {
        Some(index) => index,
        None => {
            let expected = turn
                .rounds
                .last()
                .map_or(0, |round| round.round_num.saturating_add(1));
            if round_num != expected {
                return Err(TimelineError::RoundOutOfOrder {
                    turn_id: turn_id.to_string(),
                    expected,
                    received: round_num,
                });
            }
            turn.rounds.push(TimelineRound {
                round_num,
                sealed: false,
                is_final: false,
                blocks: vec![],
            });
            turn.rounds.len() - 1
        }
    };
    Ok(&mut turn.rounds[index])
}

fn block_mut<'a>(
    round: &'a mut TimelineRound,
    block_id: &str,
) -> Result<&'a mut TimelineBlock, TimelineError> {
    round
        .blocks
        .iter_mut()
        .find(|block| block.block_id == block_id)
        .ok_or_else(|| TimelineError::MissingBlock(block_id.to_string()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tool() -> TimelineTool {
        TimelineTool {
            tool_call_id: "call-1".into(),
            name: "read".into(),
            state: TimelineToolState::Prepared,
            summary: None,
            args_json: None,
            output: None,
            progress: String::new(),
            failure: None,
            permission: None,
        }
    }

    #[test]
    fn appender_assigns_a_single_order_across_text_and_tools() {
        let mut appender = TimelineAppender::new();
        appender.open_turn("s", "t", "question").unwrap();
        appender
            .open_block("s", "t", 0, "reasoning", TimelineBlockKind::Reasoning, None)
            .unwrap();
        appender
            .append_text("s", "t", 0, "reasoning", 0, "inspect")
            .unwrap();
        appender.seal_block("s", "t", 0, "reasoning").unwrap();
        appender
            .open_block("s", "t", 0, "tool", TimelineBlockKind::Tool, Some(tool()))
            .unwrap();
        appender
            .update_tool(
                "s",
                "t",
                0,
                "tool",
                TimelineToolState::Succeeded,
                Some("read file".into()),
            )
            .unwrap();
        appender.seal_block("s", "t", 0, "tool").unwrap();
        appender
            .open_block("s", "t", 0, "answer", TimelineBlockKind::Text, None)
            .unwrap();
        appender
            .append_text("s", "t", 0, "answer", 0, "done")
            .unwrap();
        appender.seal_block("s", "t", 0, "answer").unwrap();
        appender.seal_round("s", "t", 0, true).unwrap();
        appender.seal_turn("s", "t").unwrap();

        let snapshot = appender.snapshot("s").unwrap();
        let blocks = &snapshot.turns[0].rounds[0].blocks;
        assert_eq!(
            blocks
                .iter()
                .map(|block| block.block_id.as_str())
                .collect::<Vec<_>>(),
            ["reasoning", "tool", "answer"]
        );
        assert!(
            blocks
                .iter()
                .all(|block| block.state == TimelineBlockState::Sealed)
        );
        assert_eq!(blocks[2].text, "done");
        assert_eq!(
            blocks[1].tool.as_ref().unwrap().state,
            TimelineToolState::Succeeded
        );
        assert_eq!(
            appender.replay_since("s", 0).last().unwrap().timeline_seq,
            snapshot.watermark
        );
    }

    #[test]
    fn appender_rejects_missing_or_reordered_text_fragments() {
        let mut appender = TimelineAppender::new();
        appender.open_turn("s", "t", "question").unwrap();
        appender
            .open_block("s", "t", 0, "answer", TimelineBlockKind::Text, None)
            .unwrap();
        assert!(matches!(
            appender.append_text("s", "t", 0, "answer", 1, "late"),
            Err(TimelineError::FragmentOutOfOrder {
                expected: 0,
                received: 1,
                ..
            })
        ));
        appender
            .append_text("s", "t", 0, "answer", 0, "first")
            .unwrap();
        appender.seal_block("s", "t", 0, "answer").unwrap();
        assert!(matches!(
            appender.append_text("s", "t", 0, "answer", 1, "after seal"),
            Err(TimelineError::SealedBlock(_))
        ));
    }

    #[test]
    fn tool_progress_survives_a_terminal_lifecycle_update() {
        let mut appender = TimelineAppender::new();
        appender.open_turn("s", "t", "question").unwrap();
        appender
            .open_block("s", "t", 0, "tool", TimelineBlockKind::Tool, Some(tool()))
            .unwrap();
        let progress = appender
            .apply_intent(
                "s",
                TimelineIntent::ToolProgress {
                    turn_id: "t".into(),
                    round_num: 0,
                    block_id: "tool".into(),
                    chunk: "executing\\n".into(),
                },
            )
            .unwrap();
        let mut final_tool = tool();
        final_tool.state = TimelineToolState::Succeeded;
        final_tool.output = Some("done".into());
        appender
            .replace_tool("s", "t", 0, "tool", final_tool)
            .unwrap();

        assert!(matches!(progress.event, TimelineEvent::ToolProgress { .. }));
        let snapshot = appender.snapshot("s").unwrap();
        let tool = snapshot.turns[0].rounds[0].blocks[0].tool.as_ref().unwrap();
        assert_eq!(tool.progress, "executing\\n");
        assert_eq!(tool.output.as_deref(), Some("done"));
        assert_eq!(tool.state, TimelineToolState::Succeeded);
    }

    #[test]
    fn appender_rejects_ambiguous_block_shapes_and_late_rounds() {
        let mut appender = TimelineAppender::new();
        appender.open_turn("s", "t", "question").unwrap();
        assert!(matches!(
            appender.open_block("s", "t", 0, "tool", TimelineBlockKind::Tool, None),
            Err(TimelineError::InvalidBlockShape(_))
        ));
        assert!(matches!(
            appender.open_block("s", "t", 2, "text", TimelineBlockKind::Text, None),
            Err(TimelineError::RoundOutOfOrder {
                expected: 0,
                received: 2,
                ..
            })
        ));
    }

    #[test]
    fn snapshot_and_replay_form_a_lossless_recovery_boundary() {
        let mut appender = TimelineAppender::new();
        let opened = appender.open_turn("s", "t", "question").unwrap();
        appender
            .open_block("s", "t", 0, "answer", TimelineBlockKind::Text, None)
            .unwrap();
        let first = appender
            .append_text("s", "t", 0, "answer", 0, "hel")
            .unwrap();
        let snapshot = appender.snapshot("s").unwrap();
        let second = appender
            .append_text("s", "t", 0, "answer", 1, "lo")
            .unwrap();

        assert_eq!(opened.timeline_seq, 1);
        assert!(first.timeline_seq <= snapshot.watermark);
        let tail = appender.replay_since("s", snapshot.watermark);
        assert_eq!(tail, vec![second]);
    }

    #[test]
    fn intents_allocate_fragment_and_timeline_sequences_at_the_single_writer() {
        let mut appender = TimelineAppender::new();
        let intents = [
            TimelineIntent::TurnOpened {
                turn_id: "t".into(),
                user_text: "question".into(),
            },
            TimelineIntent::BlockOpened {
                turn_id: "t".into(),
                round_num: 0,
                block_id: "answer".into(),
                kind: TimelineBlockKind::Text,
                tool: None,
            },
            TimelineIntent::TextDelta {
                turn_id: "t".into(),
                round_num: 0,
                block_id: "answer".into(),
                delta: "hel".into(),
            },
            TimelineIntent::TextDelta {
                turn_id: "t".into(),
                round_num: 0,
                block_id: "answer".into(),
                delta: "lo".into(),
            },
        ];
        let entries: Vec<_> = intents
            .into_iter()
            .map(|intent| appender.apply_intent("s", intent).unwrap())
            .collect();

        assert_eq!(
            entries
                .iter()
                .map(|entry| entry.timeline_seq)
                .collect::<Vec<_>>(),
            [1, 2, 3, 4]
        );
        assert!(matches!(
            &entries[2].event,
            TimelineEvent::TextDelta {
                fragment_seq: 0,
                ..
            }
        ));
        assert!(matches!(
            &entries[3].event,
            TimelineEvent::TextDelta {
                fragment_seq: 1,
                ..
            }
        ));
        assert_eq!(
            appender.snapshot("s").unwrap().turns[0].rounds[0].blocks[0].text,
            "hello"
        );
    }
}
