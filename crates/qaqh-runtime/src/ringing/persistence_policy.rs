//! ringing::persistence_policy — 持久化决策层（Phase 4，对标 Codex `rollout/policy.rs`）。
//!
//! 单一事实源：回答"一个事件属于持久层还是瞬态"。此前该判断分散在
//! delivery 分类（`qaqh_domain`）、timeline 快照物化、journal 落盘三处，
//! 未来新增事件类型时容易漂移成"默认全落盘"的写放大劣化（旧
//! timeline-journal 829 MB 事故的根因之一）。这里显式冻结分类，配回归
//! 测试锁定。
//!
//! # 分类（QAQH Ringing V1 现状）
//!
//! | 事件 | 持久层 | 说明 |
//! |---|---|---|
//! | TurnOpened / BlockOpened / BlockSealed / RoundSealed | ✅ timeline 快照（turns 全文） | 结构边界；1s 合并窗口异步 checkpoint |
//! | TurnSealed | ✅ timeline 快照 | 回合恢复边界；terminal 优先队列立即落盘（非发布会话线程同步） |
//! | BlockCheckpoint | ❌ 不触发快照重写 | 增量载荷（`arg`），文本可由 `TextDelta`/块投影重建 |
//! | ToolUpdated | ✅ timeline 快照 | 覆盖语义，物化进 turns |
//! | TextDelta / ToolProgress | ❌ 不落盘 | 只进内存投影 + SSE 实时流 + 回放尾（有界）；崩溃后由快照 watermark 重基线 |
//! | 三频道 Reliable（TurnStarted/Finished、ToolPrepared/Finished、Interaction*） | ✅ 三频道 journal | 投影恢复权威 |
//! | 三频道 Replaceable（RoundDelta / BlockCheckpoint / Usage / …） | ⚠️ 折叠落盘 | 同 identity 只保留最新值（64 次一 checkpoint），RoundCompleted 时整轮 compact |
//! | 三频道 Ephemeral | ❌ 不落盘 | 纯实时 |
//!
//! # 内存回放尾预算
//!
//! `TimelineAppender` 的内存 journal 是 SSE 断线重连的补帧窗口，不是持久
//! 层：超过 [`MAX_TIMELINE_JOURNAL_ENTRIES`] 或 [`MAX_TIMELINE_JOURNAL_BYTES`]
//! 就从头部驱逐最老条目。被驱逐区间的重连走 `TimelineGap` → 客户端
//! `recover_gap` 快照重基线——这是 Phase 0 已明确接受的代价（"最后几秒未
//! 落盘可接受，因为丢得起"）。
//!
//! ⚠ **2026-09-23 起不再在 turn seal 时裁剪该 turn 的条目**（#314）。原论据是
//! 「sealed 内容已在快照内物化，回放不再需要」，但它在**回合中途重基线**时不
//! 成立：客户端 gap 恢复取到的快照可能落在该 turn 内，此后要靠 `seq > watermark`
//! 的条目把这个回合补完；若这些条目在 seal 时被裁掉、而重连又晚于它们的 live
//! 投递，客户端就**永远收不到 `TurnSealed`**，回复不渲染。
//!
//! 代价是**稳态占用**：journal 不再每回合收缩，而是常驻在双限附近
//! （8192 条 / 256 MB per seed，见下）。另注意持久化侧
//! [`super::hub::RingingHub::prune_sealed_timeline_journal`] 仍会丢弃已 seal
//! 回合的条目，因此**跨 daemon 重启**的重连依旧走快照重基线。

/// timeline 内存回放尾的硬上限（条目数）。
///
/// 与三频道 journal 的内存窗口（8192）对齐。按实测信封 ~200 B 计，
/// 8192 条 ≈ 1.6 MB/seed，120 个活跃 seed 也在 200 MB 内。
pub const MAX_TIMELINE_JOURNAL_ENTRIES: usize = 8192;

/// timeline 内存回放尾的字节硬上限（payload 估算）。
///
/// 条数上限按 ~200 B/条估算，但单条 payload 仍可能很大（长 reasoning 块
/// 的 TextDelta / ToolProgress 块），条数上限挡不住单条膨胀。字节上限直接
/// 约束内存窗口的真实占用：密集流下 8192 条可达 ~79 MB，256 MB 上限既给
/// 活跃回放留足余量，又把最坏情形钉死。
///
/// BlockCheckpoint 已改为**增量载荷**（只带上次之后的余量），不再是内存窗
/// 口的单条膨胀源；全量覆盖仅出现在丢帧后的降级路径。
pub const MAX_TIMELINE_JOURNAL_BYTES: u64 = 256 * 1024 * 1024;

/// 测试用字节上限覆写（OnceLock 一次性；仅测试模块设置，模式同
/// hub.rs 的 JOURNAL_REWRITE_THRESHOLD_OVERRIDE）。
static JOURNAL_BYTE_LIMIT_OVERRIDE: std::sync::OnceLock<u64> = std::sync::OnceLock::new();

/// 生效中的回放尾字节上限（生产为 [`MAX_TIMELINE_JOURNAL_BYTES`]）。
pub fn journal_byte_limit() -> u64 {
    *JOURNAL_BYTE_LIMIT_OVERRIDE
        .get()
        .unwrap_or(&MAX_TIMELINE_JOURNAL_BYTES)
}

#[cfg(test)]
pub(crate) fn set_journal_byte_limit_for_test(limit: u64) {
    let _ = JOURNAL_BYTE_LIMIT_OVERRIDE.set(limit);
}

/// 某个 timeline 事件是否允许进入持久层（timeline 快照物化）。
///
/// 快照物化由 `TimelineAppender::snapshot` 统一处理（turns 全文即物化），
/// 此函数用于文档化 + 测试锁定；新增事件变体时必须在此表态。
pub fn is_snapshot_persisted(event: &qaqh_domain::TimelineEvent) -> bool {
    use qaqh_domain::TimelineEvent;
    matches!(
        event,
        TimelineEvent::TurnOpened { .. }
            | TimelineEvent::BlockOpened { .. }
            | TimelineEvent::ToolUpdated { .. }
            | TimelineEvent::BlockSealed { .. }
            | TimelineEvent::RoundSealed { .. }
            | TimelineEvent::TurnSealed { .. }
    )
    // BlockCheckpoint：不触发快照重写。它只是"把已有块文本整流一遍"，而块文本
    // 早已由 TextDelta 累积在内存投影里 / 由 StructureEvent + 快照物化承载；
    // 把它算作持久化事件会让高频 checkpoint 直接变成快照写放大（issue #28）。
    // TextDelta / ToolProgress：瞬态，同上。
}

/// 某个 timeline 事件是否值得占用回放尾预算。
///
/// TextDelta 是回放尾的主要体积来源（逐 token），但也是重连补帧的
/// 主要内容——保留（受 [`MAX_TIMELINE_JOURNAL_ENTRIES`] 约束），
/// 上界由双限（条数 + 字节）头部驱逐控制。seal 裁剪已于 2026-09-23 移除
/// （#314：回合中途重基线要靠 seal 后的条目补完），故这里不再有第二道收缩。
pub fn occupies_replay_tail(event: &qaqh_domain::TimelineEvent) -> bool {
    let _ = event;
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use qaqh_domain::{
        TimelineBlock, TimelineBlockKind, TimelineBlockState, TimelineEvent, TimelineToolState,
    };

    fn block() -> TimelineBlock {
        TimelineBlock {
            block_id: "b".into(),
            block_order: 0,
            kind: TimelineBlockKind::Text,
            state: TimelineBlockState::Open,
            text: String::new(),
            tool: None,
        }
    }

    #[test]
    fn transient_events_are_never_snapshot_persisted() {
        // 回归守卫：瞬态事件绝不允许被未来改动拉进持久层。
        let transient = [
            TimelineEvent::TextDelta {
                block_id: "b".into(),
                fragment_seq: 1,
                delta: "x".into(),
            },
            TimelineEvent::ToolProgress {
                block_id: "b".into(),
                chunk: "x".into(),
                truncated: false,
                stream: None,
                bytes_total: 0,
            },
        ];
        for event in &transient {
            assert!(
                !is_snapshot_persisted(event),
                "{event:?} must stay transient"
            );
            assert!(occupies_replay_tail(event));
        }
    }

    #[test]
    fn checkpoint_is_not_a_snapshot_rewrite_trigger() {
        // 回归守卫（issue #28）：高频 checkpoint 不得再触发快照全量重写。
        let increment = TimelineEvent::BlockCheckpoint {
            block_id: "b".into(),
            arg: Some("tail".into()),
            text: String::new(),
        };
        let overwrite = TimelineEvent::BlockCheckpoint {
            block_id: "b".into(),
            arg: None,
            text: "full".into(),
        };
        for event in [&increment, &overwrite] {
            assert!(
                !is_snapshot_persisted(event),
                "{event:?} must not trigger a snapshot rewrite"
            );
            assert!(occupies_replay_tail(event));
        }
    }

    #[test]
    fn structural_events_are_snapshot_persisted() {
        let persisted = [
            TimelineEvent::TurnOpened {
                user_text: "hi".into(),
            },
            TimelineEvent::BlockOpened { block: block() },
            TimelineEvent::ToolUpdated {
                block_id: "b".into(),
                tool: qaqh_domain::TimelineTool {
                    tool_call_id: "c".into(),
                    name: "exec".into(),
                    state: TimelineToolState::Running,
                    summary: None,
                    args_json: None,
                    output: None,
                    diff: None,
                    progress: String::new(),
                    progress_truncated: false,
                    progress_stream: None,
                    progress_bytes_total: 0,
                    display: None,
                    failure: None,
                    permission: None,
                },
            },
            TimelineEvent::BlockSealed {
                block_id: "b".into(),
            },
            TimelineEvent::RoundSealed { is_final: false },
            TimelineEvent::TurnSealed {
                state: qaqh_domain::TimelineTurnState::Completed,
                failure: None,
            },
        ];
        for event in &persisted {
            assert!(
                is_snapshot_persisted(event),
                "{event:?} must be snapshot persisted"
            );
        }
    }
}
