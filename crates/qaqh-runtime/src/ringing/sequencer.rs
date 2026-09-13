//! 序号生成器。
//!
//! PLAN 序号规则：
//! - `stream_seq` 在 `server_epoch + channel` 内全局递增（一条 SSE 连接恢复用）。
//! - `channel_seq` 在 `seed + channel` 内递增（领域状态乱序检测）。
//! - `session_seq` 保留因果序（每 seed+channel）。
//! - `state_revision` 每 seed+channel 递增（terminal 到达后旧 revision 作废）。
//!
//! 分片（BUG-2026-09-13-33 / BUG-08 收尾）：`per_seed` 由单一 `Mutex` 改为
//! 固定 16 分片（按 `(channel, seed)` 稳定哈希选片）。缺陷是「跨会话内存态
//! 共享单锁」——session A 的取号会串行化 B~Z 的取号，8 会话下这是热路径
//! 串行化的直接来源之一。
//!
//! 分片是纯性能结构：每个 `(channel, seed)` 恒映射到唯一分片，所有序号不变量
//! （per-(channel,seed) 稠密递增、stream_seq 频道内全局唯一）与未分片时完全
//! 相同——只是把互不相关的 key 分开竞争。
//!
//! `stream_seq` **仍按频道单锁**：它是协议不变量（同频道事件必须取到全局有序
//! 的 stream_seq），分片会破坏回放排序，故不做。该锁临界区只有一次
//! `saturating_add`，不是串行化瓶颈。

use std::collections::HashMap;
use std::sync::Mutex;

use qaqh_domain::RingingChannel;

/// per-(channel, seed) 序号表的分片数。2 的幂：取模用位与。
/// 16 分片在「会话数 ≤ 数十」的真实负载下已能摊开争用，且内存开销可忽略。
const SEED_SHARDS: usize = 16;

#[derive(Debug, Default)]
struct PerSeed {
    channel_seq: u64,
    session_seq: u64,
    state_revision: u64,
}

type SeedShard = Mutex<HashMap<(RingingChannel, String), PerSeed>>;

/// FNV-1a：稳定哈希（跨进程/跨版本一致，便于诊断复现；`HashMap` 的
/// `RandomState` 每次进程随机，不可用于固定分片）。
fn fnv1a(bytes: &[u8]) -> u64 {
    let mut hash: u64 = 0xcbf2_9ce4_8422_2325;
    for byte in bytes {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x0000_0100_0000_01b3);
    }
    hash
}

/// 线程安全序号生成器（daemon 内多线程消费）。
#[derive(Debug)]
pub struct Sequencer {
    stream_seq: Mutex<HashMap<RingingChannel, u64>>,
    /// per-(channel, seed) 序号表的分片。**实例私有**（不是进程级 static）：
    /// 每个 `RingingHub` 持有独立序号空间，`Sequencer::new()` 必须从零开始。
    seed_shards: Box<[SeedShard; SEED_SHARDS]>,
}

impl Default for Sequencer {
    fn default() -> Self {
        Self {
            stream_seq: Mutex::new(HashMap::new()),
            seed_shards: Box::new(std::array::from_fn(|_| Mutex::new(HashMap::new()))),
        }
    }
}

impl Sequencer {
    pub fn new() -> Self {
        Self::default()
    }

    /// `(channel, seed)` → 分片下标（FNV-1a 稳定哈希）。
    fn shard_index(channel: RingingChannel, seed: &str) -> usize {
        let mut hash = fnv1a(channel.as_str().as_bytes());
        hash = hash
            .wrapping_mul(0x0000_0100_0000_01b3)
            .wrapping_add(fnv1a(seed.as_bytes()));
        (hash as usize) & (SEED_SHARDS - 1)
    }

    /// 取分片 + 该分片内的查表键。
    fn shard(&self, channel: RingingChannel, seed: &str) -> (&SeedShard, (RingingChannel, String)) {
        (
            &self.seed_shards[Self::shard_index(channel, seed)],
            (channel, seed.to_string()),
        )
    }

    /// 从持久化 journal 装载后恢复序号（取历史最大值，`next` 继续递增）。
    pub fn seed(
        &self,
        channel: RingingChannel,
        seed: &str,
        stream_seq: u64,
        channel_seq: u64,
        session_seq: u64,
    ) {
        let mut streams = self.stream_seq.lock().unwrap_or_else(|e| e.into_inner());
        let entry = streams.entry(channel).or_default();
        *entry = (*entry).max(stream_seq);
        drop(streams);

        let (shard, key) = self.shard(channel, seed);
        let mut per = shard.lock().unwrap_or_else(|e| e.into_inner());
        let entry = per.entry(key).or_default();
        entry.channel_seq = entry.channel_seq.max(channel_seq);
        entry.session_seq = entry.session_seq.max(session_seq);
    }

    /// 分配一组序号（stream/channel/session 各自独立递增）。
    pub fn next(&self, channel: RingingChannel, seed: &str) -> (u64, u64, u64) {
        let mut streams = self.stream_seq.lock().unwrap_or_else(|e| e.into_inner());
        let s = streams.entry(channel).or_default();
        *s = s.saturating_add(1);
        let stream_seq = *s;
        drop(streams);

        let (shard, key) = self.shard(channel, seed);
        let mut per = shard.lock().unwrap_or_else(|e| e.into_inner());
        let entry = per.entry(key).or_default();
        entry.channel_seq = entry.channel_seq.saturating_add(1);
        entry.session_seq = entry.session_seq.saturating_add(1);
        (stream_seq, entry.channel_seq, entry.session_seq)
    }

    /// 领域状态修订号递增（terminal / revision 变更事件时调用）。
    pub fn bump_revision(&self, channel: RingingChannel, seed: &str) -> u64 {
        let (shard, key) = self.shard(channel, seed);
        let mut per = shard.lock().unwrap_or_else(|e| e.into_inner());
        let entry = per.entry(key).or_default();
        entry.state_revision = entry.state_revision.saturating_add(1);
        entry.state_revision
    }

    pub fn current_revision(&self, channel: RingingChannel, seed: &str) -> u64 {
        let (shard, key) = self.shard(channel, seed);
        let per = shard.lock().unwrap_or_else(|e| e.into_inner());
        per.get(&key).map(|e| e.state_revision).unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sequences_are_per_channel_and_per_seed() {
        let seq = Sequencer::new();
        let (s1, c1, ss1) = seq.next(RingingChannel::Tool, "a");
        let (s2, c2, ss2) = seq.next(RingingChannel::Tool, "a");
        let (s3, c3, _) = seq.next(RingingChannel::Tool, "b");
        let (s4, c4, _) = seq.next(RingingChannel::Control, "a");
        assert_eq!((s1, c1, ss1), (1, 1, 1));
        assert_eq!((s2, c2, ss2), (2, 2, 2));
        assert_eq!((s3, c3), (3, 1)); // seed b 独立 channel_seq
        assert_eq!((s4, c4), (1, 1)); // Control 频道独立 stream_seq
    }

    #[test]
    fn revision_is_per_seed_channel() {
        let seq = Sequencer::new();
        assert_eq!(seq.bump_revision(RingingChannel::Conversation, "s"), 1);
        assert_eq!(seq.bump_revision(RingingChannel::Conversation, "s"), 2);
        assert_eq!(seq.bump_revision(RingingChannel::Conversation, "t"), 1);
        assert_eq!(seq.current_revision(RingingChannel::Conversation, "s"), 2);
        assert_eq!(seq.current_revision(RingingChannel::Tool, "s"), 0);
    }

    #[test]
    fn shard_assignment_is_stable_and_spread() {
        // 同 key 恒映射同分片；不同 seed 应摊到多个分片（不是退化成单锁）。
        let mut used = std::collections::HashSet::new();
        for i in 0..64 {
            let seed = format!("seed-{i}");
            let first = Sequencer::shard_index(RingingChannel::Conversation, &seed);
            let second = Sequencer::shard_index(RingingChannel::Conversation, &seed);
            assert_eq!(first, second, "same key must map to the same shard");
            used.insert(first);
        }
        assert!(
            used.len() >= 8,
            "64 seeds must spread across shards, got {}",
            used.len()
        );
    }

    #[test]
    fn sharded_sequences_stay_unique_under_concurrency() {
        let seq = std::sync::Arc::new(Sequencer::new());
        let mut joins = Vec::new();
        for index in 0..8 {
            let seq = std::sync::Arc::clone(&seq);
            joins.push(std::thread::spawn(move || {
                let seed = format!("conc-{index}");
                for _ in 0..500 {
                    let _ = seq.next(RingingChannel::Conversation, &seed);
                }
            }));
        }
        for join in joins {
            join.join().expect("sequencer thread must not panic");
        }
        // stream_seq 频道内全局唯一且连续到 4000。
        for index in 0..8 {
            let (_, channel_seq, _) =
                seq.next(RingingChannel::Conversation, &format!("conc-{index}"));
            assert_eq!(channel_seq, 501, "per-seed channel_seq stays dense");
        }
    }
}
