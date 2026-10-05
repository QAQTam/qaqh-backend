//! axum_impl::timeline_api — see parent module docs.

use super::*;

/// T-08：分页元数据里「总数」与「是否被裁剪」的取值（纯函数，契约见测试）。
///
/// - `total_turns` = 会话**持久化的真实回合数**；取不到 meta 时回退到物化窗口
///   大小（不可判定 → 只报已知的，不放大）；
/// - `truncated_before` = 物化窗口**未覆盖到历史开头**——更早的回合存在（在归档
///   里），本次交付不到（可由归档深翻页接口 `RingingHub::archive_turn_page` 继续
///   向前翻取，读取有界）。
///
/// 注意 `truncated_before` **不能**用 `has_more` 表达：`has_more` 的契约是
/// 「还能再翻一页且本页非空」（BUG-2026-09-13-18），而 truncated_before 是
/// 「窗口之外还有历史」这一事实（其可达性由深翻页是否触顶另行决定），谎报只会
/// 让客户端反复请求一个永远为空的页。
pub(crate) fn window_metadata(materialized: usize, persisted: Option<usize>) -> (usize, bool) {
    let total_turns = persisted.unwrap_or(materialized).max(materialized);
    let truncated_before = persisted.is_some_and(|total| total > materialized);
    (total_turns, truncated_before)
}

/// 一页的来源与切片区间（纯函数，便于把边界情形写成测试）。
///
/// 语义：`before_index` = **排他**游标（返回序号 < 它的回合）；`None` = 取最新一页。
/// 落点在常驻窗口内就零 I/O 切片；跨出窗口下界则整页改由归档提供
/// （归档读取本身是有界的，见 `RingingHub::archive_turn_page`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct PagePlan {
    /// 本页最旧回合的全局序号（含）
    pub start: usize,
    /// 排他上界
    pub end: usize,
    pub from_archive: bool,
}

pub(crate) fn page_plan(
    before_index: Option<usize>,
    limit: usize,
    total: usize,
    window_base: usize,
) -> PagePlan {
    let limit = limit.max(1);
    let end = before_index.unwrap_or(total).min(total);
    let start = end.saturating_sub(limit);
    // 整页落在窗口内 → 走常驻窗口；否则整页交给归档（不跨两个来源拼一页）。
    // 判据用 `start`（页的最旧一端）而不是 `end`：跨界的页只取窗口那半截会产出
    // 短页，而短页会让客户端的「每页 limit 条」预期失效。
    let from_archive = start < window_base;
    PagePlan {
        start,
        end,
        from_archive,
    }
}

// ---- handlers ----

/// `?limit` 的钳位口径（纯函数，契约见测试）：缺省取一页窗口大小，下限 1
/// （`limit=0` 会造出「空页 + `has_more=true`」的死循环翻页，
/// BUG-2026-09-13-18），上限 [`TIMELINE_PAGE_MAX`]。
///
/// 上限是**回合数**而不是字节：一页 200 个已 rehydrate 全文的回合仍然可能有
/// 数 MB，响应侧目前没有字节上界。
pub(crate) fn page_limit(raw: Option<usize>) -> usize {
    raw.unwrap_or(TIMELINE_PAGE_LIMIT)
        .clamp(1, TIMELINE_PAGE_MAX)
}

pub(crate) async fn handle_timeline_snapshot(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(session_id): Path<String>,
    Query(q): Query<TimelineQuery>,
) -> Response {
    if !is_authorized(&headers, &state.token) {
        return unauthorized();
    }
    let Some(client_session_id) = get_session_id(&headers) else {
        return lease_required_json();
    };
    if session_id.is_empty() {
        return (StatusCode::BAD_REQUEST, "missing session_id").into_response();
    }
    if state.test_hooks.session_is_404(&session_id) {
        return session_not_found_response(&session_id);
    }
    let owns = state
        .leases
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .owns_session(&client_session_id, &session_id);
    if !owns {
        return (
            StatusCode::UNAUTHORIZED,
            [(header::CONTENT_TYPE, "application/json")],
            br#"{"code":"lease_required","message":"attach the session before reading timeline"}"#
                .to_vec(),
        )
            .into_response();
    }
    let snapshot =
        state
            .hub
            .timeline_snapshot(&session_id)
            .unwrap_or(qaqh_domain::TimelineSnapshot {
                watermark: 0,
                turns: vec![],
            });
    let materialized = snapshot.turns.len();
    let persisted = state.hub.persisted_turn_count(&session_id);
    let (total_turns, window_truncated) = window_metadata(materialized, persisted);
    let limit = page_limit(q.limit);
    // 常驻窗口覆盖的全局序号区间 = [window_base, total_turns)
    let window_base = total_turns.saturating_sub(materialized);
    let plan = page_plan(q.before_index, limit, total_turns, window_base);

    // 两个来源：常驻窗口（零 I/O）与归档（有界读取）。**不跨来源拼一页**——
    // 拼页会让边界回合的 id/序号来自两套算法。
    let (page, has_more, truncated_before) = if plan.from_archive {
        match state.hub.archive_turn_page(&session_id, plan.end, limit) {
            Some((mut turns, page_start, capped)) => {
                // 回填全局序号（与窗口分支同口径，0-based）：归档页的 turn_id
                // 虽由全局序号派生（t{N} ↔ index N-1），但客户端的
                // `before_index` 游标读的是 `turn_index` 字段，深翻页必须同样
                // 带上，否则跨过窗口边界后翻页断链。
                for (offset, turn) in turns.iter_mut().enumerate() {
                    turn.turn_index = Some((page_start + offset) as u64);
                }
                // 触顶 = 更旧的回合取不到 → 不能再宣称「还能翻」（否则客户端会
                // 永远请求同一个空页，BUG-2026-09-13-18 那一族）。
                let more = page_start > 0 && !capped;
                let truncated = capped && page_start > 0;
                (turns, more, truncated)
            }
            // 归档不可用（无 meta / 无会话目录）：当作翻到头，如实说不可达。
            None => (Vec::new(), false, true),
        }
    } else {
        let lo = plan.start.saturating_sub(window_base);
        let hi = plan.end.saturating_sub(window_base).min(materialized);
        let mut page: Vec<qaqh_domain::TimelineTurn> = if lo < hi {
            snapshot.turns[lo..hi].to_vec()
        } else {
            Vec::new()
        };
        // 回填全局序号：它是客户端下一次翻页的游标（turn_id 会复用，当不了游标）。
        for (offset, turn) in page.iter_mut().enumerate() {
            turn.turn_index = Some((plan.start + offset) as u64);
        }
        // 窗口内到底了但归档还有更旧的 → 仍然「能翻」（深翻页接手）。
        let more = plan.start > 0;
        (page, more, window_truncated)
    };
    // BUG-2026-09-13-18 的结构性保证：空页绝不宣称 has_more。
    let has_more = has_more && !page.is_empty();
    // Page before rehydration: the resident snapshot keeps only bounded
    // shells, while the response restores full text for this page only.
    // 归档投影出来的回合本身就是全文（`offloaded=false`），此调用对它们是空操作。
    let page = state.hub.rehydrate_timeline_page(&session_id, page);
    let body = serde_json::json!({
        "schema": "qaqh.Ringing",
        "version": 2,
        "server_epoch": state.hub.epoch(),
        "session_id": session_id,
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

    /// `page_plan` 的边界：游标落在常驻窗口内 → 零 I/O 切片；跨出窗口下界 →
    /// 整页交给归档。**不跨来源拼页**是刻意的：两套来源的 id/序号算法不同。
    #[test]
    fn page_plan_serves_from_window_until_the_cursor_leaves_it() {
        // total=200、物化窗口=最后 40（覆盖 [160,200)）
        let (total, window_base) = (200usize, 160usize);
        // 无游标 → 最新一页，仍在窗口内
        let p = page_plan(None, 30, total, window_base);
        assert_eq!((p.start, p.end, p.from_archive), (170, 200, false));
        // 游标在窗口内、且整页确实落在窗口里 → 零 I/O 切片
        let p = page_plan(Some(195), 10, total, window_base);
        assert_eq!((p.start, p.end, p.from_archive), (185, 195, false));
        // 页**跨出**窗口下界（start 落在窗口之前）→ 整页改由归档提供。
        // 注意判据是 start 而非 end：只取窗口那半截会产出 20 条的短页。
        let p = page_plan(Some(180), 30, total, window_base);
        assert_eq!((p.start, p.end, p.from_archive), (150, 180, true));
        // 游标正好等于窗口下界：窗口里已经没有了 → 改走归档
        let p = page_plan(Some(160), 30, total, window_base);
        assert_eq!((p.start, p.end, p.from_archive), (130, 160, true));
        // 深游标 → 归档
        let p = page_plan(Some(40), 30, total, window_base);
        assert_eq!((p.start, p.end, p.from_archive), (10, 40, true));
    }

    /// 游标越界（0 / 超过 total）不得 panic，且不得产出「负区间」。
    #[test]
    fn page_plan_clamps_out_of_range_cursors() {
        let (total, window_base) = (200usize, 160usize);
        // 游标 0：没有更旧的回合
        let p = page_plan(Some(0), 30, total, window_base);
        assert_eq!((p.start, p.end), (0, 0));
        // 游标超过 total：钳到 total（等价于取最新一页）
        let p = page_plan(Some(9999), 30, total, window_base);
        assert_eq!((p.start, p.end, p.from_archive), (170, 200, false));
        // limit=0 必须钳到 1，否则 start==end 产出空页而 has_more 仍可能为真
        //（BUG-2026-09-13-18 那一族）
        let p = page_plan(None, 0, total, window_base);
        assert_eq!(p.end - p.start, 1, "limit=0 必须钳到 1");
    }

    /// `?limit` 的钳位口径：以前只有 `limit=0 → 1` 被端到端测试钉住，
    /// 上限 200 完全无测试——它是响应侧唯一的量级上界，必须是显式契约。
    #[test]
    fn page_limit_clamps_both_ends() {
        assert_eq!(page_limit(None), TIMELINE_PAGE_LIMIT, "缺省 = 一页窗口大小");
        assert_eq!(page_limit(Some(0)), 1, "0 → 1，否则空页 + has_more 死循环");
        assert_eq!(page_limit(Some(2)), 2, "正常值原样生效（分页真的有用）");
        assert_eq!(
            page_limit(Some(100_000)),
            TIMELINE_PAGE_MAX,
            "上限是口径而不是巧合"
        );
    }

    /// 窗口覆盖全史（未重建的常见情形）→ 永远零 I/O。
    #[test]
    fn page_plan_stays_in_window_when_nothing_was_rebuilt() {
        let p = page_plan(Some(5), 30, 40, 0);
        assert!(!p.from_archive);
        assert_eq!((p.start, p.end), (0, 5));
    }

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
