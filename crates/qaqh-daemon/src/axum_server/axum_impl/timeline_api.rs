//! axum_impl::timeline_api — see parent module docs.

use super::*;

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
    let total_turns = snapshot.turns.len();
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
    });
    (
        StatusCode::OK,
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::to_vec(&body).unwrap_or_default(),
    )
        .into_response()
}
