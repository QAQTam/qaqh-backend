//! 可靠事件 journal（有界，内存实现）。
//!
//! PLAN 硬规则：
//! - journal 只保存 **reliable** 事件与稀疏 progress checkpoint，
//!   禁止保存每个 provider token；
//! - 相同 `event_id` 至少一次投递但只允许应用一次（幂等）；
//! - cursor 超出保留窗口时发送 `ringing.reset_required`，客户端经 HTTP
//!   读取权威 snapshot（本层返回 `CursorExpired` 信号）。

use std::collections::{HashMap, VecDeque};

use deepx_ringing::RingingEventEnvelope;

/// cursor 超出保留窗口。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CursorExpired {
    pub earliest_available_seq: u64,
}

const DEFAULT_JOURNAL_CAPACITY: usize = 8192;

/// 有界可靠 journal（每 seed+channel 一个实例）。
#[derive(Debug)]
pub struct ReliableJournal {
    entries: VecDeque<RingingEventEnvelope>,
    /// event_id 去重（有界：只保留窗口内）。
    seen_event_ids: HashMap<String, u64>,
    /// 稀疏 replaceable checkpoint：identity → 最新 stream_seq。
    checkpoints: HashMap<String, u64>,
    capacity: usize,
}

impl ReliableJournal {
    pub fn new() -> Self {
        Self::with_capacity(DEFAULT_JOURNAL_CAPACITY)
    }

    pub fn with_capacity(capacity: usize) -> Self {
        Self {
            entries: VecDeque::new(),
            seen_event_ids: HashMap::new(),
            checkpoints: HashMap::new(),
            capacity: capacity.max(1),
        }
    }

    /// 追加可靠事件。返回是否首次出现（幂等语义：重复 event_id 拒绝）。
    pub fn append(&mut self, envelope: &RingingEventEnvelope) -> AppendOutcome {
        if self.seen_event_ids.contains_key(&envelope.event_id) {
            return AppendOutcome::Duplicate;
        }
        while self.entries.len() >= self.capacity {
            let evicted = self
                .entries
                .pop_front()
                .expect("non-empty while len >= capacity");
            self.seen_event_ids.remove(&evicted.event_id);
        }
        self.seen_event_ids
            .insert(envelope.event_id.clone(), envelope.stream_seq);
        self.entries.push_back(envelope.clone());
        AppendOutcome::Appended
    }

    /// 记录 replaceable checkpoint（稀疏：terminal 前或周期性调用）。
    pub fn checkpoint_replaceable(&mut self, identity: &str, stream_seq: u64) {
        self.checkpoints.insert(identity.to_string(), stream_seq);
    }

    /// 从 cursor 回放。cursor 早于保留窗口 → `CursorExpired`。
    pub fn replay_since(&self, after_stream_seq: u64) -> Result<Vec<RingingEventEnvelope>, CursorExpired> {
        let earliest = self
            .entries
            .front()
            .map(|e| e.stream_seq)
            .unwrap_or(0);
        if !self.entries.is_empty() && after_stream_seq < earliest.saturating_sub(1) {
            return Err(CursorExpired { earliest_available_seq: earliest });
        }
        Ok(self
            .entries
            .iter()
            .filter(|e| e.stream_seq > after_stream_seq)
            .cloned()
            .collect())
    }

    pub fn checkpoints(&self) -> &HashMap<String, u64> {
        &self.checkpoints
    }

    pub fn len(&self) -> usize {
        self.entries.len()
    }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AppendOutcome {
    Appended,
    Duplicate,
}

#[cfg(test)]
mod tests {
    use super::*;
    use deepx_domain::{ConversationEvent, DomainEvent};

    fn env(seq: u64, event_id: &str) -> RingingEventEnvelope {
        RingingEventEnvelope::new(
            "epoch",
            "s",
            seq,
            seq,
            seq,
            event_id,
            DomainEvent::Conversation(ConversationEvent::ConversationCancelled { turn_id: None })
                .into(),
        )
    }

    #[test]
    fn duplicate_event_id_rejected() {
        let mut journal = ReliableJournal::new();
        assert_eq!(journal.append(&env(1, "e1")), AppendOutcome::Appended);
        assert_eq!(journal.append(&env(2, "e1")), AppendOutcome::Duplicate);
        assert_eq!(journal.len(), 1);
    }

    #[test]
    fn replay_within_window_works() {
        let mut journal = ReliableJournal::new();
        for seq in 1..=5 {
            journal.append(&env(seq, &format!("e{seq}")));
        }
        let tail = journal.replay_since(2).expect("within window");
        assert_eq!(tail.len(), 3);
        assert_eq!(tail[0].stream_seq, 3);
    }

    #[test]
    fn cursor_before_window_triggers_expired() {
        let mut journal = ReliableJournal::with_capacity(4);
        for seq in 1..=4 {
            journal.append(&env(seq, &format!("e{seq}")));
        }
        // 再追加 2 个，窗口变成 3..=6
        journal.append(&env(5, "e5"));
        journal.append(&env(6, "e6"));
        let err = journal.replay_since(1).expect_err("expired");
        assert_eq!(err.earliest_available_seq, 3);
    }

    #[test]
    fn checkpoint_tracks_replaceable_sparse() {
        let mut journal = ReliableJournal::new();
        journal.checkpoint_replaceable("tool:c1", 10);
        journal.checkpoint_replaceable("tool:c1", 20);
        assert_eq!(journal.checkpoints().get("tool:c1"), Some(&20));
    }
}
