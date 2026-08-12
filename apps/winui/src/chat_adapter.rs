//! Timeline → ChatView presentation 转换（Phase 2 收窄后）。
//!
//! transcript 渲染（snapshot + live + prepend）统一由 timeline 驱动
//! （BlockTranscript 单源）；conversation/tool 频道的 transcript 映射
//! （render_event/restored_turns）已随 Phase 2/3 退役（净删约 400 行），
//! 剩余领域事件（usage/dashboard/activity/audit）由 bridge 直接以 typed
//! 形式消费，不再经过本适配器。
//!
//! 本模块只做 serde 形状转换：`deepx-client` 的 timeline 类型与
//! `markdown-winui::timeline_protocol` 形状一致（对齐 `deepx-domain`），
//! 零胶水 roundtrip；协议漂移由编译器/转换失败暴露。

/// deepx-client timeline entry → markdown-winui timeline entry。
///
/// 形状已对齐 `deepx-domain`（serde 零胶水 roundtrip）；转换失败返回
/// `None`（协议漂移时防御性丢弃，绝不 panic）。
pub fn timeline_entry(
    entry: &deepx_client::TimelineEntry,
) -> Option<markdown_winui::TimelineEntry> {
    serde_json::to_value(entry)
        .ok()
        .and_then(|value| serde_json::from_value(value).ok())
}

/// deepx-client timeline snapshot → markdown-winui timeline snapshot
/// （restore / 分页前插共用；失败返回空快照——调用方按空会话处理）。
pub fn timeline_snapshot(
    snapshot: &deepx_client::TimelineSnapshot,
) -> Option<markdown_winui::TimelineSnapshot> {
    serde_json::to_value(snapshot)
        .ok()
        .and_then(|value| serde_json::from_value(value).ok())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// entry 转换 roundtrip：TextDelta 保持字段完整。
    #[test]
    fn timeline_entry_roundtrips() {
        let entry = deepx_client::TimelineEntry {
            timeline_seq: 7,
            turn_id: "t1".into(),
            round_num: Some(0),
            event: deepx_client::TimelineEvent::TextDelta {
                block_id: "text:b1".into(),
                fragment_seq: 3,
                delta: "hel".into(),
            },
        };
        let converted = timeline_entry(&entry).expect("convert");
        assert_eq!(converted.timeline_seq, 7);
        assert_eq!(converted.turn_id, "t1");
        assert!(matches!(
            converted.event,
            markdown_winui::TimelineEvent::TextDelta {
                block_id,
                fragment_seq: 3,
                delta,
            } if block_id == "text:b1" && delta == "hel"
        ));
    }

    /// 快照转换 roundtrip：turns/rounds/blocks 层级完整。
    #[test]
    fn timeline_snapshot_roundtrips() {
        let snapshot = deepx_client::TimelineSnapshot {
            watermark: 3,
            turns: vec![deepx_client::TimelineTurn {
                turn_id: "t1".into(),
                created_seq: 1,
                user_text: "hi".into(),
                sealed: true,
                state: deepx_client::TimelineTurnState::Completed,
                failure: None,
                rounds: vec![deepx_client::TimelineRound {
                    round_num: 0,
                    sealed: true,
                    is_final: true,
                    blocks: vec![deepx_client::TimelineBlock {
                        block_id: "text:b1".into(),
                        block_order: 0,
                        kind: deepx_client::TimelineBlockKind::Text,
                        state: deepx_client::TimelineBlockState::Sealed,
                        text: "answer".into(),
                        tool: None,
                    }],
                }],
            }],
        };
        let converted = timeline_snapshot(&snapshot).expect("convert");
        assert_eq!(converted.watermark, 3);
        assert_eq!(converted.turns.len(), 1);
        assert_eq!(converted.turns[0].rounds[0].blocks[0].text, "answer");
        assert_eq!(
            converted.turns[0].rounds[0].blocks[0].kind,
            markdown_winui::TimelineBlockKind::Text
        );
    }
}
