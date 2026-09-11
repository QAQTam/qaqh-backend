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
//! | TurnOpened / BlockOpened / BlockSealed / RoundSealed / TurnSealed | ✅ timeline 快照（turns 全文） | 恢复边界（terminal intents 同步落盘） |
//! | BlockCheckpoint / ToolUpdated | ✅ timeline 快照 | 全文覆盖语义，物化进 turns |
//! | TextDelta / ToolProgress | ❌ 不落盘 | 只进内存投影 + SSE 实时流 + 回放尾（有界）；崩溃后由快照 watermark 重基线 |
//! | 三频道 Reliable（TurnStarted/Finished、ToolPrepared/Finished、Interaction*） | ✅ 三频道 journal | 投影恢复权威 |
//! | 三频道 Replaceable（RoundDelta / BlockCheckpoint / Usage / …） | ⚠️ 折叠落盘 | 同 identity 只保留最新值（64 次一 checkpoint），RoundCompleted 时整轮 compact |
//! | 三频道 Ephemeral | ❌ 不落盘 | 纯实时 |
//!
//! # 内存回放尾预算
//!
//! `TimelineAppender` 的内存 journal 是 SSE 断线重连的补帧窗口，不是持久
//! 层：超过 [`MAX_TIMELINE_JOURNAL_ENTRIES`] 从头部驱逐最老条目，turn seal
//! 时即时裁剪该 turn 的全部条目（快照已物化，无需保留）。被驱逐区间的
//! 重连走 `TimelineGap` → 客户端 `recover_gap` 快照重基线——这是 Phase 0
//! 已明确接受的代价（"最后几秒未落盘可接受，因为丢得起"）。

/// timeline 内存回放尾的硬上限（条目数）。
///
/// 与三频道 journal 的内存窗口（8192）对齐。按实测信封 ~200 B 计，
/// 8192 条 ≈ 1.6 MB/seed，120 个活跃 seed 也在 200 MB 内。
pub const MAX_TIMELINE_JOURNAL_ENTRIES: usize = 8192;

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
            | TimelineEvent::BlockCheckpoint { .. }
            | TimelineEvent::ToolUpdated { .. }
            | TimelineEvent::BlockSealed { .. }
            | TimelineEvent::RoundSealed { .. }
            | TimelineEvent::TurnSealed { .. }
    )
    // TextDelta / ToolProgress：瞬态。物化由 checkpoint/terminal 事件承担。
}

/// 某个 timeline 事件是否值得占用回放尾预算。
///
/// TextDelta 是回放尾的主要体积来源（逐 token），但也是重连补帧的
/// 主要内容——保留（受 [`MAX_TIMELINE_JOURNAL_ENTRIES`] 约束），
/// 由 seal 裁剪 + 头部驱逐控制上界。
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
    fn structural_events_are_snapshot_persisted() {
        let persisted = [
            TimelineEvent::TurnOpened {
                user_text: "hi".into(),
            },
            TimelineEvent::BlockOpened { block: block() },
            TimelineEvent::BlockCheckpoint {
                block_id: "b".into(),
                text: "full".into(),
            },
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
                    failure: None,
                    permission: None,
                },
            },
            TimelineEvent::BlockSealed { block_id: "b".into() },
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
