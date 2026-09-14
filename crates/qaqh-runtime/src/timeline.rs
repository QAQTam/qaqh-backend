//! Single-writer native transcript timeline.
//!
//! The appender owns sequence allocation and materializes snapshots from the
//! same records it returns to transport. It intentionally does not depend on
//! `Agent2Ui` or the legacy Ringing conversation/tool projections.

use std::collections::{BTreeMap, HashMap, VecDeque};
use std::fmt;

use qaqh_domain::{
    TimelineBlock, TimelineBlockKind, TimelineBlockState, TimelineEntry, TimelineEvent,
    TimelineIntent, TimelineRound, TimelineSnapshot, TimelineTool, TimelineToolState, TimelineTurn,
    TimelineTurnState,
};

/// A live Ringing V1 timeline delivery record. `entry.timeline_seq` is the sole SSE cursor for
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

/// turn seal 卸载回调：(seed, turn) 由调用方持久化完整文本。
pub type OffloadFn = std::sync::Arc<dyn Fn(&str, &TimelineTurn) + Send + Sync>;

#[derive(Default)]
struct SeedTimeline {
    next_seq: u64,
    turns: BTreeMap<String, TimelineTurn>,
    /// 回放尾：只在尾部追加、只在头部驱逐，故用 `VecDeque` 使两端均摊 O(1)
    /// （BUG-2026-09-12-13：曾用 `Vec::remove(0)`，越过 8192 条上限后每条新
    /// 事件 memmove 整个 journal）。
    journal: VecDeque<TimelineEntry>,
    next_fragment: HashMap<(String, u32, String), u64>,
    /// journal 内滞留的 payload 字节数（text_delta/checkpoint/进度 chunk）。
    /// 驱逐时从头部扣减，O(1) 维护。
    journal_bytes: u64,
    /// True = 已 seal turn 的 blocks 文本被卸载出内存（壳模式），由
    /// `offload` 回调在持久化时补齐快照全文。见 `set_offload`。
    offload_enabled: bool,
    /// 卸载回调（set_offload 注入；Default 的 None = 不卸载）。
    #[allow(clippy::type_complexity)]
    offload: Option<OffloadFn>,
}

/// The only component allowed to allocate timeline sequences.
///
/// A future transport actor owns this mutably; keeping its API on `&mut self`
/// makes accidental concurrent producers impossible without an explicit queue.
#[derive(Default)]
pub struct TimelineAppender {
    seeds: HashMap<String, SeedTimeline>,
}

impl TimelineAppender {
    pub fn new() -> Self {
        Self::default()
    }

    /// 该 seed 的 timeline 是否已在内存（懒加载索引检查用）。
    pub fn contains(&self, seed: &str) -> bool {
        self.seeds.contains_key(seed)
    }

    pub fn open_turn(
        &mut self,
        seed: &str,
        turn_id: impl Into<String>,
        user_text: impl Into<String>,
    ) -> Result<TimelineEntry, TimelineError> {
        let turn_id = turn_id.into();
        let user_text = user_text.into();
        let timeline = self.seeds.entry(seed.to_string()).or_default();
        if timeline.turns.contains_key(&turn_id) {
            // Reopen allowance: the message store is the authoritative history
            // and counts every turn, while meta.turn_count only persists on
            // completion — so a daemon restart can leave the worker's restored
            // counter behind the timeline's recorded turns. The next input then
            // legitimately reuses an id the timeline already sealed, either
            // orphan-Cancelled by the sealer or Completed when the count lagged
            // further (observed: t14 reused as Completed after restart).
            // Rejecting the intent (DuplicateTurn) starves the frontend
            // transcript for that turn forever: every later block/text intent
            // fails against the sealed turn and the resumed stream stays blank.
            // Any sealed turn is terminal history; a fresh TurnOpened for the
            // same id means the worker reuses it for a new input, so reset it
            // in place. Only an unsealed (still running) duplicate is a genuine
            // error.
            let reopenable = {
                let turn = timeline.turns.get(&turn_id).expect("checked above");
                turn.sealed
            };
            if reopenable {
                let reopened_user_text = {
                    let turn = timeline.turns.get_mut(&turn_id).expect("checked above");
                    turn.user_text = user_text;
                    turn.sealed = false;
                    turn.state = TimelineTurnState::Running;
                    turn.failure = None;
                    turn.rounds.clear();
                    turn.user_text.clone()
                };
                // Fragment counters are keyed by (turn, round, block) and the
                // reopened turn may reuse the same round/block ids, so reset
                // them — otherwise the resumed stream's first TextDelta is
                // rejected with FragmentOutOfOrder and the transcript stalls
                // again.
                timeline
                    .next_fragment
                    .retain(|(turn, _, _), _| turn != &turn_id);
                return Ok(next_entry(
                    timeline,
                    turn_id,
                    None,
                    TimelineEvent::TurnOpened {
                        user_text: reopened_user_text,
                    },
                ));
            }
            return Err(TimelineError::DuplicateTurn(turn_id));
        }
        // 预分配：紧接其后的 TurnOpened entry 将占用 next_seq+1，作为该
        // turn 的权威创建序（快照排序依据，不依赖 turn_id 命名格式）。
        let created_seq = timeline.next_seq.saturating_add(1);
        timeline.turns.insert(
            turn_id.clone(),
            TimelineTurn {
                turn_id: turn_id.clone(),
                created_seq,
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

    /// Applies a text-block synchronization for one reasoning/text block.
    ///
    /// The event carries the **increment** since the previous checkpoint
    /// (`arg`) whenever the delivered stream can account for it, so the
    /// payload — and therefore the SSE frame, the journal entry and the
    /// snapshot write amplification — stays proportional to what the block
    /// actually grew by, not to the total block length. When the block text
    /// cannot be derived from what the transport delivered (lost/reordered
    /// `TextDelta`s, or a non-appending rewrite) the event falls back to a
    /// full-value overwrite (`text`), which is self-healing; the next
    /// checkpoint returns to the incremental form.
    ///
    /// The in-memory projection always materializes the full text, so the
    /// snapshot/recovery semantics are unchanged. `next_fragment` accounting
    /// is intentionally left untouched so subsequent `TextDelta`s still
    /// validate against the monotonic counter.
    pub fn checkpoint_block(
        &mut self,
        seed: &str,
        turn_id: &str,
        round_num: u32,
        block_id: &str,
        text: impl Into<String>,
    ) -> Result<TimelineEntry, TimelineError> {
        let timeline = self.timeline_mut(seed)?;
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
        let text = text.into();
        // 增量推导：`block.text` 就是"上一个已交付事件之后客户端持有的文本"
        // （每个 delta / checkpoint 都同时更新它与传输层），因此差值天然是
        // 本帧需要补的余量。文本不是 `block.text` 的追加式延伸时（丢/乱序
        // delta 后的整流、非追加改写），降级为一次性全量覆盖（`text`），
        // 消费侧 replace 语义自愈。
        let (arg, overwrite) = match text.strip_prefix(block.text.as_str()) {
            Some(arg) => (Some(arg.to_string()), String::new()),
            None => (None, text.clone()),
        };
        block.text = text;
        Ok(next_entry(
            timeline,
            turn_id.to_string(),
            Some(round_num),
            TimelineEvent::BlockCheckpoint {
                block_id: block_id.to_string(),
                arg,
                text: overwrite,
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
            TimelineIntent::BlockCheckpoint {
                turn_id,
                round_num,
                block_id,
                text,
            } => self.checkpoint_block(seed, &turn_id, round_num, &block_id, text),
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
        failure: Option<qaqh_domain::TimelineFailure>,
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
        let entry = next_entry(
            timeline,
            turn_id.to_string(),
            None,
            TimelineEvent::TurnSealed { state, failure },
        );
        // seal 即时裁剪：sealed turn 的条目在快照内已物化，回放不再需要
        // （与 persist 侧 prune_sealed_timeline_journal 语义一致）。不裁剪则
        // journal 随会话累积（实测单会话 7.3 万条 / 25 MB）。
        prune_turn_journal(timeline, turn_id);
        // turn-seal 卸载：先回调持久化完整文本，再把 blocks 清成壳。
        // 回调在持有 timeline 锁的状态下执行 append-only 追加（O(文本)
        // 一次写，无每秒重写），不做任何可锁 store 状态访问，无死锁面。
        if let Some(offload) = timeline.offload.clone() {
            if let Some(turn) = timeline.turns.get(turn_id) {
                offload(seed, turn);
            }
            if let Some(turn) = timeline.turns.get_mut(turn_id) {
                offload_turn_blocks(turn);
            }
        }
        Ok(entry)
    }

    /// 开启 turn-seal 卸载：seal 后该 turn 的 blocks 文本移出内存，
    /// `offload` 回调负责持久化完整文本（offload 侧车）。reasoning 链路
    /// 常驻内存是长会话内存增长的主因之一；文本的持久权威由侧车承担，
    /// 内存只保留壳（turn 元数据 + 首块预览）。
    pub fn set_offload(&mut self, seed: &str, offload: Option<OffloadFn>) {
        if let Some(timeline) = self.seeds.get_mut(seed) {
            timeline.offload_enabled = offload.is_some();
            timeline.offload = offload;
        }
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
        self.seeds.get(seed).map(|timeline| {
            let mut turns: Vec<TimelineTurn> = timeline.turns.values().cloned().collect();
            // turns 存于 HashMap（无序）——按 created_seq 排序（TurnOpened
            // entry 的 seq，权威时间序）；旧磁盘数据 created_seq=0 时退化为
            // turn_id 数值序（t1..tN 递增）。两者混合时旧 turn 在前、新 turn
            // 在后，时间序依然正确。快照数组序必须=时间序：前端恢复按"尾部
            // 窗口"取最新回合，顺序错乱会恢复出错误的回合集合（实测：40
            // turns 会话恢复窗口落在旧回合，最新消息缺失）。
            turns.sort_by_key(|t| (t.created_seq, turn_num(&t.turn_id)));
            TimelineSnapshot {
                watermark: timeline.next_seq,
                turns,
            }
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
        let journal: VecDeque<TimelineEntry> = journal.into_iter().collect();
        let mut next_fragment = HashMap::new();
        let mut journal_bytes = 0u64;
        for entry in &journal {
            if let TimelineEvent::TextDelta {
                block_id,
                fragment_seq,
                ..
            } = &entry.event
                && let Some(round_num) = entry.round_num
            {
                next_fragment.insert(
                    (entry.turn_id.clone(), round_num, block_id.clone()),
                    fragment_seq.saturating_add(1),
                );
            }
            journal_bytes += journal_entry_payload_bytes(&entry.event);
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
                journal_bytes,
                offload_enabled: false,
                offload: None,
            },
        );
    }

    fn timeline_mut(&mut self, seed: &str) -> Result<&mut SeedTimeline, TimelineError> {
        self.seeds
            .get_mut(seed)
            .ok_or_else(|| TimelineError::MissingTurn(format!("seed:{seed}")))
    }
}

/// turn_id → 数值序（t1/t10 → 1/10）；无数字后缀按 0（保持原序兜底）。
fn turn_num(id: &str) -> u64 {
    id.trim_start_matches(|c: char| !c.is_ascii_digit())
        .parse()
        .unwrap_or(0)
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
    timeline.journal_bytes += journal_entry_payload_bytes(&entry.event);
    timeline.journal.push_back(entry.clone());
    enforce_journal_budget(timeline);
    entry
}

/// journal 条目的 payload 字节数（内存预算估算；结构事件为 0）。
fn journal_entry_payload_bytes(event: &TimelineEvent) -> u64 {
    match event {
        TimelineEvent::TextDelta { delta, .. } => delta.len() as u64,
        TimelineEvent::BlockCheckpoint { text, .. } => text.len() as u64,
        TimelineEvent::ToolProgress { chunk, .. } => chunk.len() as u64,
        _ => 0,
    }
}

/// 双限（条数 + 字节）头部驱逐：保序移除最老条目直至两项都回到界内。
///
/// 两端均摊 O(1)（`pop_front`），与 journal 的 FIFO 语义一致——`Vec::remove(0)`
/// 会让每条越过上限的事件 memmove 整个窗口，实测 52–83× 阶跃且全程持锁。
fn enforce_journal_budget(timeline: &mut SeedTimeline) {
    let entry_limit = crate::ringing::persistence_policy::MAX_TIMELINE_JOURNAL_ENTRIES;
    let byte_limit = crate::ringing::persistence_policy::journal_byte_limit();
    while timeline.journal.len() > entry_limit || timeline.journal_bytes > byte_limit {
        let Some(oldest) = timeline.journal.pop_front() else {
            break;
        };
        timeline.journal_bytes = timeline
            .journal_bytes
            .saturating_sub(journal_entry_payload_bytes(&oldest.event));
    }
}

/// 把 turn 卸载成壳：清空各 block 文本/进度，保留身份与元数据。
/// 首块保留 512 字符预览，前端列表仍可显示摘要；全文从侧车恢复。
fn offload_turn_blocks(turn: &mut TimelineTurn) {
    for round in &mut turn.rounds {
        for block in &mut round.blocks {
            if block.text.chars().count() > 512 {
                let preview: String = block.text.chars().take(512).collect();
                block.text = preview;
            }
            if let Some(tool) = &mut block.tool {
                if tool.progress.chars().count() > 512 {
                    let preview: String = tool.progress.chars().take(512).collect();
                    tool.progress = preview;
                }
                tool.output = None;
                tool.diff = None;
            }
        }
    }
}

/// 移除某 turn 的全部 journal 条目（seal 即时裁剪）。
fn prune_turn_journal(timeline: &mut SeedTimeline, turn_id: &str) {
    timeline.journal.retain(|entry| {
        let keep = entry.turn_id != turn_id;
        if !keep {
            timeline.journal_bytes = timeline
                .journal_bytes
                .saturating_sub(journal_entry_payload_bytes(&entry.event));
        }
        keep
    });
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

// `materialize_timeline_from_journal` / `apply_journal_entry` / `block_mut_replay`
// 已随 timeline-journal 一并移除（2026-09-10）。重建投影现由
// `ringing::timeline_rebuild` 从 messages.jsonl / compact-context 完成。

#[cfg(test)]
mod tests {
    use super::*;
    use qaqh_domain::TimelineFailure;

    fn tool() -> TimelineTool {
        TimelineTool {
            tool_call_id: "call-1".into(),
            name: "read".into(),
            state: TimelineToolState::Prepared,
            summary: None,
            args_json: None,
            output: None,
            diff: None,
            progress: String::new(),
            failure: None,
            permission: None,
        }
    }

    #[test]
    fn orphan_cancelled_turn_is_reopened_by_the_next_input() {
        // Regression: after a daemon restart the orphan-sealer marks the
        // interrupted turn Cancelled, but the message store still counts it,
        // so the worker's next input reuses the same turn_id. open_turn must
        // reset that placeholder instead of rejecting with DuplicateTurn —
        // otherwise every timeline intent for the turn is dropped and the
        // frontend transcript stays blank.
        let mut appender = TimelineAppender::new();
        appender
            .open_turn("s", "t1", "interrupted question")
            .unwrap();
        appender
            .open_block("s", "t1", 0, "answer", TimelineBlockKind::Text, None)
            .unwrap();
        appender
            .append_text("s", "t1", 0, "answer", 0, "partial")
            .unwrap();
        appender.seal_block("s", "t1", 0, "answer").unwrap();
        appender.seal_round("s", "t1", 0, false).unwrap();
        // Simulate daemon restart orphan seal.
        appender
            .seal_turn_with_state(
                "s",
                "t1",
                TimelineTurnState::Cancelled,
                Some(TimelineFailure {
                    code: "daemon_restart_interrupted".into(),
                    message: "interrupted".into(),
                }),
            )
            .unwrap();

        // The worker reuses t1 for the resumed conversation.
        let reopened = appender
            .open_turn("s", "t1", "next question")
            .expect("orphan cancelled turn must be reopenable");
        assert!(matches!(reopened.event, TimelineEvent::TurnOpened { .. }));

        let snapshot = appender.snapshot("s").unwrap();
        let turn = &snapshot.turns[0];
        assert_eq!(turn.user_text, "next question");
        assert!(!turn.sealed);
        assert_eq!(turn.state, TimelineTurnState::Running);
        assert!(turn.rounds.is_empty(), "stale rounds are cleared");
        assert!(turn.failure.is_none(), "failure marker is cleared");

        // Subsequent intents for the reopened turn flow normally.
        appender
            .open_block("s", "t1", 0, "answer", TimelineBlockKind::Text, None)
            .unwrap();
        appender
            .append_text("s", "t1", 0, "answer", 0, "full reply")
            .unwrap();
        let final_snapshot = appender.snapshot("s").unwrap();
        assert_eq!(
            final_snapshot.turns[0].rounds[0].blocks[0].text,
            "full reply"
        );
    }

    #[test]
    fn completed_turn_is_reopened_when_id_is_reused() {
        // Regression: after a daemon restart the worker's restored turn
        // counter (message store) can lag behind the timeline's recorded
        // turns, so the next input reuses an id the timeline already sealed
        // as Completed (observed: t14 reused after restart). The timeline must
        // reset that terminal turn in place instead of rejecting with
        // DuplicateTurn — otherwise every intent for the resumed turn is
        // dropped and the frontend transcript stays blank.
        let mut appender = TimelineAppender::new();
        appender.open_turn("s", "t1", "question").unwrap();
        appender.seal_turn("s", "t1").unwrap();
        let reopened = appender
            .open_turn("s", "t1", "reused question")
            .expect("sealed turns must be reopenable on id reuse");
        assert!(matches!(reopened.event, TimelineEvent::TurnOpened { .. }));
        let snapshot = appender.snapshot("s").unwrap();
        let turn = &snapshot.turns[0];
        assert_eq!(turn.user_text, "reused question");
        assert!(!turn.sealed);
        assert_eq!(turn.state, TimelineTurnState::Running);
        assert!(turn.failure.is_none());
        assert!(turn.rounds.is_empty(), "stale rounds are cleared");
    }

    #[test]
    fn running_turn_is_not_reopened() {
        // An unsealed (still running) turn is a live producer; a second
        // TurnOpened for the same id is a genuine duplicate and must fail.
        let mut appender = TimelineAppender::new();
        appender.open_turn("s", "t1", "question").unwrap();
        let err = appender
            .open_turn("s", "t1", "duplicate")
            .expect_err("running turns must not be reopened");
        assert!(matches!(err, TimelineError::DuplicateTurn(_)));
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
        // seal 裁剪语义：TurnSealed 会清空该 turn 的回放尾，seq 连续性
        // 必须在 seal 前断言；watermark 含 TurnSealed 占用的下一序号。
        let last_seq_before_seal = appender
            .replay_since("s", 0)
            .last()
            .expect("journal holds entries while the turn is active")
            .timeline_seq;
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
        assert_eq!(snapshot.watermark, last_seq_before_seal + 1);
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

    #[test]
    fn checkpoint_overwrites_text_and_later_deltas_keep_appending() {
        let mut appender = TimelineAppender::new();
        appender.open_turn("s", "t", "question").unwrap();
        appender
            .open_block("s", "t", 0, "answer", TimelineBlockKind::Text, None)
            .unwrap();
        appender
            .append_text("s", "t", 0, "answer", 0, "hel")
            .unwrap();
        // 追加式整流：只发余量（`arg`），不发全量文本。
        let checkpoint = appender
            .checkpoint_block("s", "t", 0, "answer", "hello wor")
            .unwrap();
        assert!(matches!(
            checkpoint.event,
            TimelineEvent::BlockCheckpoint {
                ref block_id,
                ref arg,
                ref text,
            } if block_id == "answer"
                && arg.as_deref() == Some("lo wor")
                && text.is_empty()
        ));
        // fragment accounting is untouched: next delta validates against 1.
        appender
            .append_text("s", "t", 0, "answer", 1, "ld")
            .unwrap();
        let snapshot = appender.snapshot("s").unwrap();
        assert_eq!(snapshot.turns[0].rounds[0].blocks[0].text, "hello world");
    }

    #[test]
    fn checkpoint_via_intent_roundtrips_as_event() {
        let mut appender = TimelineAppender::new();
        let intents = [
            TimelineIntent::TurnOpened {
                turn_id: "t".into(),
                user_text: "question".into(),
            },
            TimelineIntent::BlockOpened {
                turn_id: "t".into(),
                round_num: 0,
                block_id: "reasoning".into(),
                kind: TimelineBlockKind::Reasoning,
                tool: None,
            },
            TimelineIntent::BlockCheckpoint {
                turn_id: "t".into(),
                round_num: 0,
                block_id: "reasoning".into(),
                text: "full thinking".into(),
            },
        ];
        let entries: Vec<_> = intents
            .into_iter()
            .map(|intent| appender.apply_intent("s", intent).unwrap())
            .collect();
        // 空块上的首次整流 = 从空串追加，`arg` 即全文（自愈语义不变）。
        assert!(matches!(
            &entries[2].event,
            TimelineEvent::BlockCheckpoint { block_id, arg, text }
                if block_id == "reasoning"
                    && arg.as_deref() == Some("full thinking")
                    && text.is_empty()
        ));
        assert_eq!(
            appender.snapshot("s").unwrap().turns[0].rounds[0].blocks[0].text,
            "full thinking"
        );
    }

    #[test]
    fn checkpoint_rejected_on_sealed_block() {
        let mut appender = TimelineAppender::new();
        appender.open_turn("s", "t", "question").unwrap();
        appender
            .open_block("s", "t", 0, "answer", TimelineBlockKind::Text, None)
            .unwrap();
        appender.seal_block("s", "t", 0, "answer").unwrap();
        assert!(matches!(
            appender.checkpoint_block("s", "t", 0, "answer", "late"),
            Err(TimelineError::SealedBlock(_))
        ));
    }

    #[test]
    fn checkpoint_rejected_on_tool_or_missing_block() {
        let mut appender = TimelineAppender::new();
        appender.open_turn("s", "t", "question").unwrap();
        appender
            .open_block("s", "t", 0, "tool:1", TimelineBlockKind::Tool, Some(tool()))
            .unwrap();
        assert!(matches!(
            appender.checkpoint_block("s", "t", 0, "tool:1", "nope"),
            Err(TimelineError::InvalidBlockKind(_))
        ));
        assert!(matches!(
            appender.checkpoint_block("s", "t", 0, "ghost", "nope"),
            Err(TimelineError::MissingBlock(_))
        ));
    }

    #[test]
    fn replay_tail_covers_every_entry_above_the_watermark() {
        // 断线重连回放尾的契约（进程内 journal）：`replay_since(watermark)` 必须
        // 逐条覆盖 watermark 之后的全部条目，且 seq 严格单调、无缺无重。
        // timeline-journal 移除后，replay 只依赖内存 journal，本测试守住该语义。
        let mut appender = TimelineAppender::new();
        appender.open_turn("s", "t1", "q1").unwrap();
        appender
            .open_block(
                "s",
                "t1",
                0,
                "reasoning",
                TimelineBlockKind::Reasoning,
                None,
            )
            .unwrap();
        appender
            .append_text("s", "t1", 0, "reasoning", 0, "think")
            .unwrap();
        appender.seal_block("s", "t1", 0, "reasoning").unwrap();
        appender.seal_round("s", "t1", 0, true).unwrap();

        let all = appender.replay_since("s", 0);
        assert!(!all.is_empty(), "journal must hold the written entries");
        let watermark = appender.snapshot("s").unwrap().watermark;
        assert_eq!(
            watermark,
            all.last().unwrap().timeline_seq,
            "snapshot watermark equals the newest entry"
        );

        // 从任意水位切片：尾部必须恰好是 cut 之后的严格后缀。
        let mid = all.len() / 2;
        let cut = all[mid].timeline_seq;
        let tail = appender.replay_since("s", cut);
        assert_eq!(tail, all[mid + 1..], "tail is exactly the suffix above cut");
        assert!(
            tail.windows(2)
                .all(|w| w[0].timeline_seq < w[1].timeline_seq),
            "replay tail must be strictly monotonic"
        );
        // 水位到底 = 无条目可回放（客户端已对齐，不产生 gap）。
        assert!(appender.replay_since("s", watermark).is_empty());

        // seal 后回放尾清空（seal 即时裁剪契约，详见 sealed_turn 测试）。
        appender.seal_turn("s", "t1").unwrap();
        assert!(appender.replay_since("s", 0).is_empty());
    }

    #[test]
    fn sealed_turn_journal_is_pruned_immediately() {
        // Phase 4 seal 即时裁剪：turn seal 后其全部条目必须立即离开内存
        // journal（快照已物化）；后续 turn 的条目照常进入回放窗口。
        let mut appender = TimelineAppender::new();
        appender.open_turn("s", "t1", "q1").unwrap();
        appender
            .open_block("s", "t1", 0, "r", TimelineBlockKind::Reasoning, None)
            .unwrap();
        appender
            .append_text("s", "t1", 0, "r", 0, "long reasoning text")
            .unwrap();
        appender.seal_block("s", "t1", 0, "r").unwrap();
        appender.seal_round("s", "t1", 0, false).unwrap();
        assert!(!appender.replay_since("s", 0).is_empty());

        appender.seal_turn("s", "t1").unwrap();
        assert!(
            appender.replay_since("s", 0).is_empty(),
            "sealed turn entries must leave the replay tail immediately"
        );
        let snapshot = appender.snapshot("s").unwrap();
        assert_eq!(
            snapshot.turns.len(),
            1,
            "snapshot keeps the materialized turn"
        );
        assert!(snapshot.turns[0].sealed);
        assert_eq!(
            snapshot.turns[0].rounds[0].blocks[0].text,
            "long reasoning text"
        );

        appender.open_turn("s", "t2", "q2").unwrap();
        assert_eq!(appender.replay_since("s", 0).len(), 1);
    }

    #[test]
    fn journal_enforcement_bounds_entries_and_bytes() {
        // 双限驱逐：字节越界后最老 delta 被驱逐，且被驱逐区间是连续前缀。
        // 测试覆写为 512 KB（生产 256 MB 无法在单测内写满）。
        crate::ringing::persistence_policy::set_journal_byte_limit_for_test(512 * 1024);
        let mut appender = TimelineAppender::new();
        appender.open_turn("s", "t1", "long stream").unwrap();
        appender
            .open_block("s", "t1", 0, "r", TimelineBlockKind::Reasoning, None)
            .unwrap();
        // 600 条 x 1 KB = 600 KB payload > 512 KB 测试字节界。
        let chunk = "x".repeat(1024);
        for seq in 0..600u64 {
            appender
                .append_text("s", "t1", 0, "r", seq, chunk.clone())
                .unwrap();
        }
        let tail = appender.replay_since("s", 0);
        assert!(
            tail.len() < 600,
            "byte budget must evict old deltas, got {} entries",
            tail.len()
        );
        let fragment_seqs: Vec<u64> = tail
            .iter()
            .filter_map(|e| match &e.event {
                TimelineEvent::TextDelta { fragment_seq, .. } => Some(*fragment_seq),
                _ => None,
            })
            .collect();
        // 窗口必须覆盖最新 delta，且保留区连续。
        assert!(
            fragment_seqs.contains(&599),
            "newest delta must survive eviction, got tail len {}",
            tail.len()
        );
        let min_kept = *fragment_seqs.iter().min().unwrap();
        let expected: Vec<u64> = (min_kept..600).collect();
        assert_eq!(
            fragment_seqs, expected,
            "retained window must be contiguous"
        );

        // 条数上限分支：微小 delta 推过 8192 条后同样头部驱逐。
        let mut many = TimelineAppender::new();
        many.open_turn("s2", "t1", "many deltas").unwrap();
        many.open_block("s2", "t1", 0, "r", TimelineBlockKind::Reasoning, None)
            .unwrap();
        for seq in 0..8500u64 {
            many.append_text("s2", "t1", 0, "r", seq, "x".to_string())
                .unwrap();
        }
        let bounded = many.replay_since("s2", 0);
        assert!(
            bounded.len() <= 8192,
            "entry budget must bound the replay tail, got {}",
            bounded.len()
        );
        let last_seq = bounded.last().unwrap().timeline_seq;
        // 最新条目仍在窗口内 → watermark 与其 seq 一致；
        // watermark 与窗口头部的差值 = 被驱逐的前缀长度。
        assert_eq!(
            many.snapshot("s2").unwrap().watermark,
            last_seq,
            "newest allocated entry must remain in the bounded tail"
        );
    }

    #[test]
    fn restore_rebuilds_journal_byte_budget() {
        // restore 后字节预算必须与 journal 实际 payload 一致。
        let mut appender = TimelineAppender::new();
        appender.open_turn("s", "t1", "q").unwrap();
        appender
            .open_block("s", "t1", 0, "r", TimelineBlockKind::Reasoning, None)
            .unwrap();
        appender
            .append_text("s", "t1", 0, "r", 0, "payload-123")
            .unwrap();
        let journal = appender.replay_since("s", 0);
        let snapshot = appender.snapshot("s").unwrap();

        let mut restored = TimelineAppender::new();
        restored.restore("s".into(), snapshot, journal);
        let expected: u64 = restored
            .replay_since("s", 0)
            .iter()
            .map(|e| journal_entry_payload_bytes(&e.event))
            .sum();
        let seed = restored.seeds.get("s").unwrap();
        assert_eq!(seed.journal_bytes, expected);
    }

    /// 8192 条上限之上的摊还成本必须与上限之内同阶（O(1)），且驱逐只丢最老前缀。
    ///
    /// BUG-2026-09-12-13：`enforce_journal_budget` 用 `Vec::remove(0)` 头部驱逐，
    /// 每条新事件 memmove 整个 journal；报告实测 2.21 µs → 115.88 µs（16000 条）
    /// /182.83 µs（32768 条），52–83× 阶跃，且全程持全局 timeline 锁。修法是
    /// `VecDeque::pop_front()`（摊还 O(1)）。
    ///
    /// 阈值是**同机比值**而非绝对时长（无绝对耗时阈值，机器无关）：越界窗口的
    /// 平均每事件耗时不得超过上限内窗口的 2.5×——O(1) 驱逐下该比值恒 ≈1，O(n)
    /// memmove 下随 CAP 线性放大。修复前本机实测 ratio **7.1**（debug，38.2 vs
    /// 1.3 µs/event）；修复后 **0.24–0.96**（示例 1.30 vs 1.36 µs/event，越界侧
    /// 甚至更快，因 int 编码后少一条移除）。2.5× 两侧仍各有 ≥2× 余量。
    #[test]
    fn journal_eviction_is_amortized_constant_time_past_the_entry_cap() {
        const CAP: u64 = 8192;
        const WINDOW: u64 = 2048;
        const BUDGET_RATIO: f64 = 2.5;

        // 单条 payload 64 B：~0.5 MB 总量，远离 256 MB 字节上限，
        // 保证越界驱逐**只**由条数上限触发（否则测的不是这条路径）。
        let chunk = "x".repeat(64);
        let mut appender = TimelineAppender::new();
        appender
            .open_turn("perf", "t1", "high frequency deltas")
            .unwrap();
        appender
            .open_block("perf", "t1", 0, "r", TimelineBlockKind::Reasoning, None)
            .unwrap();

        // 窗口 A：上限之内写 CAP 条（无驱逐）。
        let inside_start = std::time::Instant::now();
        for seq in 0..CAP {
            appender
                .append_text("perf", "t1", 0, "r", seq, chunk.clone())
                .unwrap();
        }
        let inside = inside_start.elapsed();

        // 窗口 B：越过上限，每条驱逐一条最老条目。
        let over_start = std::time::Instant::now();
        for seq in CAP..(CAP + WINDOW) {
            appender
                .append_text("perf", "t1", 0, "r", seq, chunk.clone())
                .unwrap();
        }
        let over = over_start.elapsed();

        let tail = appender.replay_since("perf", 0);
        assert_eq!(
            tail.len() as u64,
            CAP,
            "replay tail must stay pinned at the entry cap while evicting"
        );
        assert_eq!(
            tail.last().map(|entry| entry.timeline_seq),
            Some(CAP + WINDOW + 2),
            "newest delta must survive eviction"
        );
        let inside = inside.as_secs_f64().max(1e-9);
        assert!(
            over.as_secs_f64() <= inside * BUDGET_RATIO,
            "eviction past the cap must stay O(1): {:.3} µs/event over the whole \
             {CAP}-entry run vs {:.3} µs/event while evicting (ratio {:.2} > {BUDGET_RATIO})",
            inside * 1e6 / CAP as f64,
            over.as_secs_f64() * 1e6 / WINDOW as f64,
            over.as_secs_f64() / inside,
        );
    }
}
