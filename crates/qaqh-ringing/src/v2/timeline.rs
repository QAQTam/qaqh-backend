//! Timeline wire contract（`GET .../timeline` 分页响应与 `GET .../timeline/events`
//! 的 SSE 帧）。
//!
//! 契约所有权：这两个形状此前只活在 `qaqh-client`（无 ts 导出，daemon
//! handler 用 `json!` 拼装）。按 B02 收口纪律迁入 wire crate——timeline
//! 词表在 `qaqh-domain`（本 crate 合法向下依赖），`qaqh-client` 经
//! re-export 保留壳层命名，daemon handler 直接序列化这一份 DTO。

use serde::{Deserialize, Serialize};
#[cfg(feature = "ts")]
use ts_rs::TS;

use qaqh_domain::{TimelineEntry, TimelineSnapshot};

use crate::protocol::RINGING_SCHEMA;
use crate::v2::RINGING_V2_VERSION;

/// Versioned response from `GET /ringing/v2/sessions/{session_id}/timeline`.
///
/// `snapshot` is the authoritative materialized transcript. Pagination
/// metadata remains outside it because it describes the current HTTP page,
/// not transcript state.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize, Serialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct TimelinePage {
    pub schema: String,
    pub version: u32,
    pub server_epoch: String,
    #[serde(rename = "session_id")]
    pub session_id: String,
    pub snapshot: TimelineSnapshot,
    /// 本页之前（游标方向）**仍有可交付的回合**，即在已物化的 timeline 里还能
    /// 再往前翻一页。
    ///
    /// 不变式：`has_more == true ⇒ 本页非空`（BUG-2026-09-13-18——违反它会让
    /// 按 has_more 驱动的翻页客户端拿到零行却永不终止）。
    pub has_more: bool,
    /// 会话持久化的**真实回合总数**，与物化窗口无关。
    ///
    /// 与「已交付回合数」的差即未交付的历史；见 [`Self::truncated_before`]。
    pub total_turns: usize,
    /// 物化窗口**未覆盖到历史开头**：更早的回合存在（在 daemon 归档里），但
    /// 本次交付不到，且当前**没有**深翻页接口能取到它们。
    ///
    /// T-08：重建路径只物化最近几十轮。此前这种截断对客户端完全不可见——既无
    /// 提示、也无从知道历史更长。它**不能**用 `has_more` 表达，因为那会让客户端
    /// 反复请求一个永远拿不到内容的页（见上）。
    ///
    /// 无 `#[serde(default)]`：daemon 侧恒发此键。缺键=形状不对，应当
    /// **响亮失败**而不是静默当成未截断。
    pub truncated_before: bool,
}

impl TimelinePage {
    pub fn validate_for(&self, session_id: &str) -> Result<(), String> {
        if self.schema != RINGING_SCHEMA
            || self.version != RINGING_V2_VERSION
            || self.session_id != session_id
            || self.server_epoch.is_empty()
        {
            return Err("invalid timeline page".into());
        }
        Ok(())
    }
}

/// Timeline SSE frame（`GET .../timeline/events` 的 `timeline.entry` 事件 data）。
///
/// daemon 原以 `json!` 拼装同一形状；`version` 曾写死 1 导致 timeline 流
/// 1s 重连死循环（见 `axum_impl::sse::timeline_entry_to_event`）——
/// 收口成 DTO 后字段值由类型一侧的常量保证。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct TimelineSseFrame {
    pub schema: String,
    pub version: u32,
    pub server_epoch: String,
    #[serde(rename = "session_id")]
    pub session_id: String,
    pub entry: TimelineEntry,
}

#[cfg(test)]
mod tests {
    use super::*;
    use qaqh_domain::{TimelineRound, TimelineTurn, TimelineTurnState};

    /// wire 形状锁：schema/version/键名/分页元数据一个都不能漂。
    /// daemon handler 与本测试共享同一份 DTO；前端消费同一 JSON 形状。
    #[test]
    fn timeline_page_round_trips_the_daemon_wire_shape() {
        let page = TimelinePage {
            schema: RINGING_SCHEMA.into(),
            version: RINGING_V2_VERSION,
            server_epoch: "epoch-1".into(),
            session_id: "seed-1".into(),
            snapshot: TimelineSnapshot {
                watermark: 3,
                turns: vec![TimelineTurn {
                    turn_id: "t1".into(),
                    turn_index: Some(0),
                    created_seq: 1,
                    user_text: "synthetic".into(),
                    sealed: true,
                    offloaded: false,
                    state: TimelineTurnState::Completed,
                    failure: None,
                    rounds: Vec::<TimelineRound>::new(),
                }],
            },
            has_more: false,
            total_turns: 1,
            truncated_before: false,
        };
        page.validate_for("seed-1").expect("valid page");
        assert!(page.validate_for("seed-2").is_err());
        let wire = serde_json::to_value(&page).unwrap();
        assert_eq!(
            wire,
            serde_json::json!({
                "schema": "qaqh.Ringing",
                "version": 2,
                "server_epoch": "epoch-1",
                "session_id": "seed-1",
                "snapshot": {"watermark": 3, "turns": [
                    {"turn_id": "t1", "turn_index": 0, "created_seq": 1,
                     "user_text": "synthetic", "sealed": true, "state": "completed",
                     "rounds": []}
                ]},
                "has_more": false,
                "total_turns": 1,
                "truncated_before": false,
            })
        );
        let back: TimelinePage = serde_json::from_value(wire).unwrap();
        assert_eq!(back, page);
    }

    /// `truncated_before` 缺键必须响亮失败（不带 serde default）——静默当成
    /// 「未截断」会让客户端把不可达历史当已完整展示。
    #[test]
    fn timeline_page_rejects_missing_truncation_key() {
        let body = serde_json::json!({
            "schema": "qaqh.Ringing",
            "version": 2,
            "server_epoch": "epoch-1",
            "session_id": "seed-1",
            "snapshot": {"watermark": 0, "turns": []},
            "has_more": false,
            "total_turns": 0,
        });
        assert!(serde_json::from_value::<TimelinePage>(body).is_err());
    }
}
