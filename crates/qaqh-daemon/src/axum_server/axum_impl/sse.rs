//! axum_impl::sse — see parent module docs.

use super::test_hooks::SseTerminate;
use super::*;

pub(crate) fn parse_timeline_cursor(cursor: &str, epoch: &str) -> u64 {
    if cursor.is_empty() {
        // 首次连接：没有 cursor 就是从头开始，这是正常路径，不告警。
        return 0;
    }
    let mut parts = cursor.split(':');
    let e = parts.next().unwrap_or_default();
    let kind = parts.next().unwrap_or_default();
    let seq = parts.next().and_then(|v| v.parse::<u64>().ok());
    let shape_ok = e == epoch && kind == "timeline" && parts.next().is_none();
    match (shape_ok, seq) {
        (true, Some(seq)) => seq,
        _ => {
            // 两类都落到这里，且都属于「静默全量重放」，所以都要告警：
            // 1) 形状不符 —— channel SSE 写的是 `{epoch}:{channel}:{seq}`，epoch 轮换
            //    或游标被截断都会出现；
            // 2) 形状合法但 seq 缺失/非法 —— 如 `{epoch}:timeline:` 或
            //    `{epoch}:timeline:abc`。
            log::warn!(
                "[sse] timeline Last-Event-ID {cursor:?} is not a usable `{{epoch}}:timeline:{{seq}}` cursor for epoch {epoch:?}; replaying from 0"
            );
            0
        }
    }
}

fn injected_termination_event(
    fault: &SseTerminate,
    channel: Option<&str>,
    session_id: Option<&str>,
) -> Event {
    let mut payload = serde_json::json!({
        "code": fault.code,
        "message": "test-injected stream termination; reconnect to continue",
    });
    if let Some(channel) = channel {
        payload["channel"] = serde_json::Value::String(channel.to_string());
    }
    if let Some(session_id) = session_id {
        payload["session_id"] = serde_json::Value::String(session_id.to_string());
    }
    if let Some(skipped) = fault.skipped {
        payload["skipped"] = serde_json::Value::from(skipped);
    }
    Event::default()
        .event("ringing.stream_terminated")
        .data(payload.to_string())
}

fn injected_termination_response(
    fault: SseTerminate,
    channel: Option<&str>,
    session_id: Option<&str>,
) -> Response {
    let event = injected_termination_event(&fault, channel, session_id);
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, Infallible>>(1);
    tokio::spawn(async move {
        let _ = tx.send(Ok(event)).await;
    });
    Sse::new(ReceiverStream::new(rx)).into_response()
}

fn timeline_entry_to_event(
    epoch: &str,
    session_id: &str,
    entry: &qaqh_domain::TimelineEntry,
) -> Event {
    // wire 形状由 `qaqh-ringing::TimelineSseFrame` DTO 锁定。写死 `version: 1`
    // 曾让每一帧都被判 `protocol violation: invalid timeline SSE frame`，
    // timeline 流 1s 重连死循环、transcript 永远无法渲染——收口后字段值
    // 与 `qaqh-client` 校验共用同一常量，这类漂移不再可能。
    let frame = qaqh_ringing::TimelineSseFrame {
        schema: qaqh_ringing::RINGING_SCHEMA.to_owned(),
        version: qaqh_ringing::RINGING_V2_VERSION,
        server_epoch: epoch.to_owned(),
        session_id: session_id.to_owned(),
        entry: entry.clone(),
    };
    Event::default()
        .id(format!("{}:timeline:{}", epoch, entry.timeline_seq))
        .event("timeline.entry")
        .json_data(&frame)
        .unwrap_or_else(|_| Event::default().data("{}"))
}

/// timeline live 投递判定（纯函数，便于回归测试）。
///
/// BUG-2026-09-13-10：入口（`handle_timeline_events`）只在建流时查一次
/// `owns_seed`；长连接内若发生 seed 级吊销（`detach_seed`：session
/// close/delete，或重新协商后旧 cs 失去归属），lease 本身仍活跃 →
/// `is_active_session` 仍为 true → 已失去该 seed 的会话会继续收到
/// timeline 新条目（数据暴露窗口）。
///
/// 因此这里对**每个** live 事件都复查 `owns_seed`，且刻意置于窗口/去重
/// 过滤之前——否则「窗口外/重复」的事件会先被 `continue` 吞掉，复查形同
/// 不存在（吊销后仍会继续处理后续事件）。
///
/// 注意 `owns_seed` 已内联 `is_active_session`（活跃性 + 归属），故吊销与
/// 整体失活都能在此拦截；lease 彻底失效仍由调用方按原语义 `break`。
#[allow(clippy::too_many_arguments)]
fn should_deliver_timeline_live(
    live: &qaqh_runtime::TimelineLiveEntry,
    client_session_id: &str,
    session_id: &str,
    after: u64,
    replayed: &HashSet<u64>,
    leases: &Arc<Mutex<RingingLeaseStore>>,
) -> bool {
    // 1) 逐事件 presence + 归属复查（吊销/过期/脱属任一即截断）。
    // §2/§4：归属挂在身份名下，不随单条连接过期/重开失效——失效只来自
    // 显式 detach（seed 级吊销）或 revoke_device（设备吊销）。
    if live.session_id != session_id
        || !leases
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .owns_session(client_session_id, &live.session_id)
    {
        return false;
    }
    // 2) replay 窗口与去重
    !(live.entry.timeline_seq <= after || replayed.contains(&live.entry.timeline_seq))
}

/// 「watermark 落后 + 空回放」reset 判据（B1.3 遗留契约）。
///
/// journal 字节预算可以把超大单事件整体驱逐（`enforce_journal_budget`），
/// 此后 `timeline_replay_since` 返回空窗口：带旧 cursor 重连的客户端拿不到
/// 任何补帧，也无法从流上自证 gap——若被驱逐事件是最后一条，客户端将永远
/// 停在旧 cursor 静默空转。此判据为真时入口必须下发
/// `ringing.stream_terminated { code: "journal_gap" }`，客户端按
/// StreamTerminated 语义重取快照把 cursor 重基线到 watermark。
///
/// 回放非空但起点已越过 `after + 1` 的「窗口内缺帧」**不在此判据内**：
/// 那条路径由客户端 `cursor + 1` 连续性检查兜底（gap 注入测试
/// `QAQH_TEST_TIMELINE_GAP` 明确依赖"流不关、客户端自检"的既有契约）。
fn timeline_replay_lost_coverage(after: u64, watermark: u64, replay_len: usize) -> bool {
    replay_len == 0 && after < watermark
}

pub(crate) async fn handle_timeline_events(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(session_id): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    let Some(client_session_id) = get_session_id(&headers) else {
        return lease_required_json();
    };
    if session_id.is_empty() {
        return (StatusCode::BAD_REQUEST, "missing session_id").into_response();
    }
    if state.test_hooks.session_is_404(&session_id) {
        return session_not_found_response(&session_id);
    }
    if let Some(fault) = state.test_hooks.take_timeline_terminate() {
        return injected_termination_response(fault, Some("timeline"), Some(&session_id));
    }
    if !state
        .leases
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .owns_session(&client_session_id, &session_id)
    {
        return (
            StatusCode::UNAUTHORIZED,
            [(header::CONTENT_TYPE, "application/json")],
            br#"{"code":"lease_required"}"#.to_vec(),
        )
            .into_response();
    }
    let last_event_id = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .or_else(|| query.get("last_event_id").map(|s| s.as_str()))
        .or_else(|| query.get("last-event-id").map(|s| s.as_str()))
        .unwrap_or("");
    let after = parse_timeline_cursor(last_event_id, &state.epoch);
    let rx = state.hub.subscribe_timeline();
    let replay = state.hub.timeline_replay_since(&session_id, after);
    let journal_gap_lost = {
        let watermark = state
            .hub
            .timeline_snapshot(&session_id)
            .map(|snapshot| snapshot.watermark)
            .unwrap_or(0);
        timeline_replay_lost_coverage(after, watermark, replay.len())
    };
    let replayed: HashSet<u64> = replay.iter().map(|e| e.timeline_seq).collect();
    let epoch = state.epoch.clone();
    let leases = state.leases.clone();
    let session_clone = session_id.clone();
    let client_session_id_clone = client_session_id.clone();
    let inject_timeline_gap = state.test_hooks.take_timeline_gap();

    let (tx, rx_stream) = tokio::sync::mpsc::channel::<Result<Event, Infallible>>(128);
    tokio::spawn(async move {
        let mut gap_remaining = usize::from(inject_timeline_gap);
        // B1.3 遗留契约：journal 已整体驱逐、补不了帧时，必须显式 reset，
        // 而不是让客户端静默空转（见 `timeline_replay_lost_coverage`）。
        if journal_gap_lost {
            let ev = Event::default().event("ringing.stream_terminated").data(
                serde_json::json!({
                    "code": "journal_gap",
                    "session_id": session_clone.as_str(),
                    "cursor": after,
                    "message": "journal no longer covers the client cursor (oversized event evicted); re-baseline from snapshot",
                })
                .to_string(),
            );
            let _ = tx.send(Ok(ev)).await;
            return;
        }
        for entry in replay {
            // Gap injection drops exactly one deliverable entry and keeps
            // streaming: the client's cursor stays at `after`, so the next
            // real entry it receives is `after + 2` and its own `cursor + 1`
            // check fires. Never synthesise a frame and never close the
            // stream here — closing would make the client reconnect with the
            // same cursor and receive the already-sent entry twice.
            if gap_remaining > 0 {
                gap_remaining -= 1;
                continue;
            }
            let ev = timeline_entry_to_event(&epoch, &session_clone, &entry);
            if tx.send(Ok(ev)).await.is_err() {
                return;
            }
        }
        let mut rx = rx;
        loop {
            match rx.recv().await {
                Ok(live) => {
                    // Seed ownership and cursor/dedup checks must precede gap
                    // injection; otherwise a foreign seed could be relabeled
                    // with this stream's seed.
                    if !should_deliver_timeline_live(
                        &live,
                        &client_session_id_clone,
                        &session_clone,
                        after,
                        &replayed,
                        &leases,
                    ) {
                        continue;
                    }
                    if gap_remaining > 0 {
                        gap_remaining -= 1;
                        continue;
                    }
                    if !leases
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .is_active_session(&client_session_id_clone)
                    {
                        break;
                    }
                    let ev = timeline_entry_to_event(&epoch, &session_clone, &live.entry);
                    if tx.send(Ok(ev)).await.is_err() {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    // BUG-2026-09-12-11：与（已删除的）v1 频道流同款处理——
                    // 补日志并下发终止帧，让客户端 re-baseline。
                    log::warn!(
                        "[sse] timeline {session_clone} stream lagged: {skipped} entries skipped; terminating for client re-baseline"
                    );
                    let ev = Event::default().event("ringing.stream_terminated").data(
                        serde_json::json!({
                            "code": "lagged",
                            "session_id": session_clone.as_str(),
                            "skipped": skipped,
                            "message": "server event buffer overflow; reconnect to re-baseline",
                        })
                        .to_string(),
                    );
                    let _ = tx.send(Ok(ev)).await;
                    break;
                }
                Err(tokio::sync::broadcast::error::RecvError::Closed) => break,
            }
        }
    });

    let stream = ReceiverStream::new(rx_stream);
    Sse::new(stream)
        .keep_alive(
            KeepAlive::new()
                .interval(Duration::from_secs(15))
                .text("keep-alive"),
        )
        .into_response()
}

// ---- debug static (P3) ----

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    fn live(session_id: &str, seq: u64) -> qaqh_runtime::TimelineLiveEntry {
        qaqh_runtime::TimelineLiveEntry {
            session_id: session_id.into(),
            entry: qaqh_domain::TimelineEntry {
                timeline_seq: seq,
                turn_id: "t1".into(),
                round_num: Some(0),
                event: qaqh_domain::TimelineEvent::TurnOpened {
                    user_text: "q".into(),
                },
            },
        }
    }

    fn store_with_session() -> Arc<Mutex<RingingLeaseStore>> {
        let leases = Arc::new(Mutex::new(RingingLeaseStore::new()));
        {
            let mut g = leases.lock().unwrap();
            g.open("cs-1".into(), "ci-1".into());
            assert!(g.attach_session("cs-1", "seed-a"));
        }
        leases
    }

    /// BUG-2026-09-13-10 回归：长连接中途 seed 级吊销（detach_seed）后，
    /// 同连接后续事件必须立即截断，不得继续投递。
    #[test]
    fn timeline_live_is_truncated_after_session_revoked() {
        let leases = store_with_session();
        let after = 0;
        let replayed = HashSet::new();

        assert!(
            should_deliver_timeline_live(
                &live("seed-a", 1),
                "cs-1",
                "seed-a",
                after,
                &replayed,
                &leases
            ),
            "吊销前同 seed 事件应投递"
        );

        // 中途吊销（session close / delete → detach_seed）；lease 本身仍活跃。
        leases.lock().unwrap().detach_session("cs-1", "seed-a");
        assert!(
            leases.lock().unwrap().is_active_session("cs-1"),
            "precondition: lease 仍活跃——旧逻辑正是因此放行"
        );
        assert!(
            !should_deliver_timeline_live(
                &live("seed-a", 2),
                "cs-1",
                "seed-a",
                after,
                &replayed,
                &leases
            ),
            "吊销后同连接后续事件必须立即截断"
        );
        // 窗口外/已回放的事件同样不得因早退而绕过复查
        assert!(
            !should_deliver_timeline_live(
                &live("seed-a", 0),
                "cs-1",
                "seed-a",
                after,
                &replayed,
                &leases
            ),
            "吊销后窗口外事件也不得投递"
        );

        // 重新 attach 后恢复投递
        leases.lock().unwrap().attach_session("cs-1", "seed-a");
        assert!(
            should_deliver_timeline_live(
                &live("seed-a", 3),
                "cs-1",
                "seed-a",
                after,
                &replayed,
                &leases
            ),
            "重新 attach 后恢复投递"
        );
    }

    /// §2/§3/§4 回归：同身份重开连接（新窗口/重连）不截断旧连接，新连接凭
    /// 身份归属立即投递（无需重放 attach）；seed 级吊销（detach，session
    /// close/delete 路径）仍立即截断该身份下全部连接。
    #[test]
    fn timeline_live_follows_identity_ownership() {
        let leases = store_with_session();
        let after = 0;
        let replayed = HashSet::new();

        assert!(should_deliver_timeline_live(
            &live("seed-a", 1),
            "cs-1",
            "seed-a",
            after,
            &replayed,
            &leases
        ));

        // 同身份第二连接：旧连接不被截断，归属共享。
        leases.lock().unwrap().open("cs-2".into(), "ci-1".into());
        assert!(
            should_deliver_timeline_live(
                &live("seed-a", 2),
                "cs-1",
                "seed-a",
                after,
                &replayed,
                &leases
            ),
            "同身份新连接不得截断旧连接（§3）"
        );
        assert!(
            should_deliver_timeline_live(
                &live("seed-a", 3),
                "cs-2",
                "seed-a",
                after,
                &replayed,
                &leases
            ),
            "新连接凭身份归属立即投递，无需重放 attach（§2/§4）"
        );

        // seed 级吊销（session close/delete → detach）：身份级归属摘除，
        // 该身份全部连接立即截断。
        leases.lock().unwrap().detach_session("cs-2", "seed-a");
        assert!(
            !should_deliver_timeline_live(
                &live("seed-a", 4),
                "cs-1",
                "seed-a",
                after,
                &replayed,
                &leases
            ),
            "detach 后旧连接必须被截断"
        );
        assert!(
            !should_deliver_timeline_live(
                &live("seed-a", 5),
                "cs-2",
                "seed-a",
                after,
                &replayed,
                &leases
            ),
            "detach 后新连接同样截断"
        );
    }

    /// B1.3 遗留契约回归：journal 被超大单事件整体驱逐后，带旧 cursor 重连
    /// 命中「空回放 + watermark 落后」→ 必须判定为失覆盖（下发
    /// `journal_gap` reset）；其余形态（cursor 仍被覆盖、空闲、非空窗口）
    /// 一律不触发，避免干扰 gap 注入与正常回放。
    #[test]
    fn journal_eviction_marks_stale_cursor_as_lost_coverage() {
        // 巨型事件驱逐整个 journal：cursor=1 落后于 watermark=2，回放空。
        assert!(timeline_replay_lost_coverage(1, 2, 0));
        // cursor 已被快照 watermark 覆盖（recover_gap 重基线后再连）：空闲正常。
        assert!(!timeline_replay_lost_coverage(2, 2, 0));
        // 全新会话（无任何事件）。
        assert!(!timeline_replay_lost_coverage(0, 0, 0));
        // 回放非空：cursor+1 连续性由客户端自检，服务端不越权 reset。
        assert!(!timeline_replay_lost_coverage(1, 2, 1));
        assert!(!timeline_replay_lost_coverage(0, 8, 3));
        // cursor 反超 watermark（同 epoch 下不应出现）：保守不判 gap。
        assert!(!timeline_replay_lost_coverage(5, 2, 0));
    }
}
