//! axum_impl::timeline_api — see parent module docs.

use super::*;

/// T-08：分页元数据里「总数」与「是否被裁剪」的取值（纯函数，契约见测试）。
///
/// - `total_turns` = 会话**持久化的真实回合数**；取不到 meta 时回退到物化窗口
///   大小（不可判定 → 只报已知的，不放大）；
/// - `truncated_before` = 物化窗口**未覆盖到历史开头**——更早的回合存在（在归档
///   里），但本次交付不到，且当前没有深翻页接口能取到它们。
///
/// 注意 `truncated_before` **不能**用 `has_more` 表达：`has_more` 的契约是
/// 「还能再翻一页且本页非空」（BUG-2026-09-13-18），而这里的更早回合拿不到，
/// 谎报只会让客户端反复请求一个永远为空的页。
pub(crate) fn window_metadata(materialized: usize, persisted: Option<usize>) -> (usize, bool) {
    let total_turns = persisted.unwrap_or(materialized).max(materialized);
    let truncated_before = persisted.is_some_and(|total| total > materialized);
    (total_turns, truncated_before)
}

pub(crate) fn paginate_turns(
    turns: Vec<qaqh_domain::TimelineTurn>,
    before_turn: Option<&str>,
    limit: usize,
) -> (Vec<qaqh_domain::TimelineTurn>, bool) {
    if turns.is_empty() {
        return (turns, false);
    }
    // BUG-2026-09-13-18：limit 是客户端可控的查询参数（`?limit=0`）。0 会让
    // `end == start` 产出空页，但 `start > 0` 仍报 `has_more=true`——按
    // has_more 驱动的翻页客户端于是每次都拿到零行却永不终止。此处把下限
    // 钳到 1，保证「has_more=true ⇒ 本页非空」，翻页单调收敛。
    let limit = limit.max(1);
    let (start, end) = match before_turn {
        Some(id) => {
            let idx = turns
                .iter()
                .position(|t| t.turn_id == id)
                .unwrap_or(turns.len());
            (idx.saturating_sub(limit), idx)
        }
        None => (turns.len().saturating_sub(limit), turns.len()),
    };
    let page: Vec<_> = turns[start..end].to_vec();
    let has_more = start > 0;
    (page, has_more)
}

// ---- handlers ----

pub(crate) async fn handle_bootstrap(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(seed): Path<String>,
) -> Response {
    if !is_authorized(&headers, &state.token) {
        return unauthorized();
    }
    let Some(session_id) = get_session_id(&headers) else {
        return lease_required_json();
    };
    if seed.is_empty() {
        return (StatusCode::BAD_REQUEST, "missing seed").into_response();
    }
    let owns = state
        .leases
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .owns_seed(&session_id, &seed);
    if !owns {
        return (
            StatusCode::UNAUTHORIZED,
            [(header::CONTENT_TYPE, "application/json")],
            br#"{"code":"lease_required","message":"attach the session seed before bootstrap"}"#
                .to_vec(),
        )
            .into_response();
    }
    state.hub.seal_orphan_channel_state(&seed, false);
    let bootstrap = qaqh_ringing::RingingSessionBootstrap::new(
        state.hub.epoch(),
        &seed,
        state.hub.snapshot(RingingChannel::Control, &seed),
        state.hub.conversation_snapshot(&seed),
        state.hub.snapshot(RingingChannel::Tool, &seed),
    );
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::to_vec(&bootstrap).unwrap_or_default(),
    )
        .into_response()
}

pub(crate) async fn handle_timeline_snapshot(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(seed): Path<String>,
    Query(q): Query<TimelineQuery>,
) -> Response {
    if !is_authorized(&headers, &state.token) {
        return unauthorized();
    }
    let Some(session_id) = get_session_id(&headers) else {
        return lease_required_json();
    };
    if seed.is_empty() {
        return (StatusCode::BAD_REQUEST, "missing seed").into_response();
    }
    let owns = state
        .leases
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .owns_seed(&session_id, &seed);
    if !owns {
        return (
            StatusCode::UNAUTHORIZED,
            [(header::CONTENT_TYPE, "application/json")],
            br#"{"code":"lease_required","message":"attach the session seed before reading timeline"}"#.to_vec(),
        )
            .into_response();
    }
    let snapshot = state
        .hub
        .timeline_snapshot(&seed)
        .unwrap_or(qaqh_domain::TimelineSnapshot {
            watermark: 0,
            turns: vec![],
        });
    let materialized = snapshot.turns.len();
    let persisted = state.hub.persisted_turn_count(&seed);
    let (total_turns, truncated_before) = window_metadata(materialized, persisted);
    let (page, has_more) = paginate_turns(
        snapshot.turns,
        q.before_turn.as_deref(),
        q.limit.unwrap_or(TIMELINE_PAGE_LIMIT).min(200),
    );
    // Page before rehydration: the resident snapshot keeps only bounded
    // shells, while the response restores full text for this page only.
    let page = state.hub.rehydrate_timeline_page(&seed, page);
    let body = serde_json::json!({
        "schema": "qaqh.Ringing",
        "version": 1,
        "server_epoch": state.hub.epoch(),
        "seed": seed,
        "snapshot": {"watermark": snapshot.watermark, "turns": page},
        "has_more": has_more,
        "total_turns": total_turns,
        "truncated_before": truncated_before,
    });
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::to_vec(&body).unwrap_or_default(),
    )
        .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// T-08 契约：窗口被裁剪时，`total_turns` 报**真实**回合数、且 `truncated_before`
    /// 置真——前端据此提示「更早的回合未在此窗口」，而不是静默翻不动。
    ///
    /// 现场：200 轮的会话，timeline 被重建后只物化 40 轮。改前 `total_turns` 报 40
    /// （谎报历史长度），`has_more` 在窗口开头为 false 且无任何信号说明还有 160 轮，
    /// 于是那 160 轮对用户**完全不可达也不可见**。
    #[test]
    fn truncated_window_reports_true_total_and_flags_it() {
        let (total, truncated) = window_metadata(40, Some(200));
        assert_eq!(total, 200, "总数必须是会话真实回合数，不是物化窗口大小");
        assert!(truncated, "物化窗口覆盖不到历史开头 → 必须置真");
    }

    /// 窗口完整（物化数 = 持久化数）：不报总数放大、不报裁剪。
    #[test]
    fn complete_window_is_not_truncated() {
        let (total, truncated) = window_metadata(200, Some(200));
        assert_eq!(total, 200);
        assert!(!truncated);
    }

    /// 取不到 meta → 不可判定：只报已知的窗口大小，**不谎报**「还有更多」。
    #[test]
    fn unknown_persisted_count_is_conservative() {
        let (total, truncated) = window_metadata(40, None);
        assert_eq!(total, 40);
        assert!(!truncated, "判不了就不说，宁可少提示也不假提示");
    }

    /// meta 落后于物化窗口（运行中回合已开而 meta 未更新）时，总数不得**小于**
    /// 已交付的回合数——否则前端会算出负数缺口。
    #[test]
    fn total_never_below_materialized() {
        let (total, truncated) = window_metadata(40, Some(39));
        assert_eq!(total, 40);
        assert!(!truncated);
    }
}
