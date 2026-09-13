//! axum_impl::sse — see parent module docs.

use super::*;

pub(crate) fn parse_sse_cursor(cursor: &str, epoch: &str, channel: RingingChannel) -> u64 {
    let mut parts = cursor.split(':');
    let e = parts.next().unwrap_or("");
    let c = parts.next().unwrap_or("");
    let seq = parts
        .next()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);
    if e == epoch && c == channel.as_str() {
        seq
    } else {
        0
    }
}

pub(crate) fn parse_timeline_cursor(cursor: &str, epoch: &str) -> u64 {
    let mut parts = cursor.split(':');
    let e = parts.next().unwrap_or_default();
    let kind = parts.next().unwrap_or_default();
    let seq = parts.next().and_then(|v| v.parse::<u64>().ok());
    if e == epoch && kind == "timeline" && parts.next().is_none() {
        seq.unwrap_or(0)
    } else {
        0
    }
}

fn envelope_to_event(
    epoch: &str,
    channel: RingingChannel,
    env: &qaqh_ringing::RingingEventEnvelope,
) -> Event {
    let event_type = serde_json::to_value(&env.event)
        .ok()
        .and_then(|v| v["type"].as_str().map(|s| s.to_string()))
        .unwrap_or_else(|| "message".into());
    // data is full envelope JSON (renderer expects seed/stream_seq/event_id)
    let data = serde_json::to_string(env).unwrap_or_else(|_| "{}".into());
    Event::default()
        .id(format!("{}:{}:{}", epoch, channel.as_str(), env.stream_seq))
        .event(event_type)
        .data(data)
}

fn reset_to_event(reset: &RingingResetRequired) -> Event {
    let data = serde_json::to_string(reset).unwrap_or_else(|_| "{}".into());
    Event::default().event("ringing.reset_required").data(data)
}

fn timeline_entry_to_event(epoch: &str, seed: &str, entry: &qaqh_domain::TimelineEntry) -> Event {
    let data = serde_json::json!({
        "schema": "qaqh.Ringing",
        "version": 1,
        "server_epoch": epoch,
        "seed": seed,
        "entry": entry,
    });
    Event::default()
        .id(format!("{}:timeline:{}", epoch, entry.timeline_seq))
        .event("timeline.entry")
        .json_data(data)
        .unwrap_or_else(|_| Event::default().data("{}"))
}

fn filter_replay_for_session(
    mut replay: qaqh_runtime::ringing::hub::ChannelReplay,
    session_id: &str,
    leases: &Arc<Mutex<RingingLeaseStore>>,
) -> qaqh_runtime::ringing::hub::ChannelReplay {
    let mut g = leases.lock().unwrap_or_else(|e| e.into_inner());
    replay.events.retain(|e| g.owns_seed(session_id, &e.seed));
    replay.resets.retain(|r| g.owns_seed(session_id, &r.seed));
    replay
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
    session_id: &str,
    seed: &str,
    after: u64,
    replayed: &HashSet<u64>,
    leases: &Arc<Mutex<RingingLeaseStore>>,
) -> bool {
    // 1) 逐事件归属复查（吊销后立即截断）
    if live.seed != seed
        || !leases
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .owns_seed(session_id, &live.seed)
    {
        return false;
    }
    // 2) replay 窗口与去重
    !(live.entry.timeline_seq <= after || replayed.contains(&live.entry.timeline_seq))
}

// ---- SSE handlers (P2) ----
pub(crate) async fn handle_events(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(channel_str): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    if !is_authorized(&headers, &state.token) {
        return unauthorized();
    }
    let Some(session_id) = get_session_id(&headers) else {
        return lease_required_json();
    };
    if !state
        .leases
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_active_session(&session_id)
    {
        return lease_required_json();
    }
    let Some(channel) = parse_channel(&channel_str) else {
        return (StatusCode::NOT_FOUND, "unknown channel").into_response();
    };
    // Last-Event-ID from header or ?last_event_id= query (ringing_http compat)
    let last_event_id = headers
        .get("last-event-id")
        .and_then(|v| v.to_str().ok())
        .or_else(|| query.get("last_event_id").map(|s| s.as_str()))
        .or_else(|| query.get("last-event-id").map(|s| s.as_str()))
        .unwrap_or("");
    let after_seq = parse_sse_cursor(last_event_id, &state.epoch, channel);

    // Subscribe before replay to avoid gap
    let rx = state.hub.subscribe(channel);
    let replay = filter_replay_for_session(
        state
            .hub
            .replay_channel_since(channel, after_seq, after_seq == 0),
        &session_id,
        &state.leases,
    );
    let replayed_ids: HashSet<String> = replay.events.iter().map(|e| e.event_id.clone()).collect();
    let epoch = state.epoch.clone();
    let leases = state.leases.clone();
    let session_id_clone = session_id.clone();

    // Use mpsc channel to bridge broadcast to Sse stream (keeps axum 0.8 Send + 'static)
    let (tx, rx_stream) = tokio::sync::mpsc::channel::<Result<Event, Infallible>>(128);
    tokio::spawn(async move {
        // Replay
        for env in replay.events {
            let ev = envelope_to_event(&epoch, channel, &env);
            if tx.send(Ok(ev)).await.is_err() {
                return;
            }
        }
        for reset in replay.resets {
            let ev = reset_to_event(&reset);
            if tx.send(Ok(ev)).await.is_err() {
                return;
            }
        }
        let mut rx = rx;
        loop {
            match rx.recv().await {
                Ok(envelope) => {
                    if !leases
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .owns_seed(&session_id_clone, &envelope.seed)
                    {
                        continue;
                    }
                    if envelope.stream_seq <= after_seq || replayed_ids.contains(&envelope.event_id)
                    {
                        continue;
                    }
                    if !leases
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .is_active_session(&session_id_clone)
                    {
                        break;
                    }
                    let ev = envelope_to_event(&epoch, channel, &envelope);
                    if tx.send(Ok(ev)).await.is_err() {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    // BUG-2026-09-12-11：此前直接 break——不记日志、不发终止帧，
                    // 客户端只看到「干净结束」无从定案。现在两条都补上。
                    log::warn!(
                        "[sse] {channel} stream lagged: {skipped} events skipped; terminating for client re-baseline"
                    );
                    let ev = Event::default()
                        .event("ringing.stream_terminated")
                        .data(
                            serde_json::json!({
                                "code": "lagged",
                                "channel": channel.as_str(),
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

pub(crate) async fn handle_timeline_events(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(seed): Path<String>,
    Query(query): Query<HashMap<String, String>>,
) -> Response {
    if !is_authorized(&headers, &state.token) {
        return unauthorized();
    }
    let Some(session_id) = get_session_id(&headers) else {
        return lease_required_json();
    };
    if seed.is_empty()
        || !state
            .leases
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .owns_seed(&session_id, &seed)
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
    let replay = state.hub.timeline_replay_since(&seed, after);
    let replayed: HashSet<u64> = replay.iter().map(|e| e.timeline_seq).collect();
    let epoch = state.epoch.clone();
    let leases = state.leases.clone();
    let seed_clone = seed.clone();
    let session_id_clone = session_id.clone();

    let (tx, rx_stream) = tokio::sync::mpsc::channel::<Result<Event, Infallible>>(128);
    tokio::spawn(async move {
        for entry in replay {
            let ev = timeline_entry_to_event(&epoch, &seed_clone, &entry);
            if tx.send(Ok(ev)).await.is_err() {
                return;
            }
        }
        let mut rx = rx;
        loop {
            match rx.recv().await {
                Ok(live) => {
                    if !should_deliver_timeline_live(
                        &live,
                        &session_id_clone,
                        &seed_clone,
                        after,
                        &replayed,
                        &leases,
                    ) {
                        continue;
                    }
                    if !leases
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .is_active_session(&session_id_clone)
                    {
                        break;
                    }
                    let ev = timeline_entry_to_event(&epoch, &seed_clone, &live.entry);
                    if tx.send(Ok(ev)).await.is_err() {
                        break;
                    }
                }
                Err(tokio::sync::broadcast::error::RecvError::Lagged(skipped)) => {
                    // BUG-2026-09-12-11：同 handle_events，补日志与终止帧。
                    log::warn!(
                        "[sse] timeline {seed_clone} stream lagged: {skipped} entries skipped; terminating for client re-baseline"
                    );
                    let ev = Event::default()
                        .event("ringing.stream_terminated")
                        .data(
                            serde_json::json!({
                                "code": "lagged",
                                "seed": seed_clone.as_str(),
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

    fn live(seed: &str, seq: u64) -> qaqh_runtime::TimelineLiveEntry {
        qaqh_runtime::TimelineLiveEntry {
            seed: seed.into(),
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

    fn store_with_seed() -> Arc<Mutex<RingingLeaseStore>> {
        let leases = Arc::new(Mutex::new(RingingLeaseStore::new()));
        {
            let mut g = leases.lock().unwrap();
            g.open("cs-1".into(), "ci-1".into());
            assert!(g.attach_seed("cs-1", "seed-a"));
        }
        leases
    }

    /// BUG-2026-09-13-10 回归：长连接中途 seed 级吊销（detach_seed）后，
    /// 同连接后续事件必须立即截断，不得继续投递。
    #[test]
    fn timeline_live_is_truncated_after_seed_revoked() {
        let leases = store_with_seed();
        let after = 0;
        let replayed = HashSet::new();

        assert!(
            should_deliver_timeline_live(&live("seed-a", 1), "cs-1", "seed-a", after, &replayed, &leases),
            "吊销前同 seed 事件应投递"
        );

        // 中途吊销（session close / delete → detach_seed）；lease 本身仍活跃。
        leases.lock().unwrap().detach_seed("cs-1", "seed-a");
        assert!(
            leases.lock().unwrap().is_active_session("cs-1"),
            "precondition: lease 仍活跃——旧逻辑正是因此放行"
        );
        assert!(
            !should_deliver_timeline_live(&live("seed-a", 2), "cs-1", "seed-a", after, &replayed, &leases),
            "吊销后同连接后续事件必须立即截断"
        );
        // 窗口外/已回放的事件同样不得因早退而绕过复查
        assert!(
            !should_deliver_timeline_live(&live("seed-a", 0), "cs-1", "seed-a", after, &replayed, &leases),
            "吊销后窗口外事件也不得投递"
        );

        // 重新 attach 后恢复投递
        leases.lock().unwrap().attach_seed("cs-1", "seed-a");
        assert!(
            should_deliver_timeline_live(&live("seed-a", 3), "cs-1", "seed-a", after, &replayed, &leases),
            "重新 attach 后恢复投递"
        );
    }

    /// BUG-2026-09-13-10 回归：重新协商后旧 cs 失去归属（僵尸身份），
    /// 旧连接必须被截断；新 cs 未 attach 前同样不投递。
    #[test]
    fn timeline_live_is_truncated_after_renegotiation() {
        let leases = store_with_seed();
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

        // 重新协商：同 instance 换新 cs → 旧 cs 归属被清除。
        leases.lock().unwrap().open("cs-2".into(), "ci-1".into());
        assert!(
            !should_deliver_timeline_live(&live("seed-a", 2), "cs-1", "seed-a", after, &replayed, &leases),
            "重新协商后旧 cs 必须被截断（僵尸身份）"
        );
        assert!(
            !should_deliver_timeline_live(&live("seed-a", 3), "cs-2", "seed-a", after, &replayed, &leases),
            "新 cs 未 attach 不投递"
        );
        leases.lock().unwrap().attach_seed("cs-2", "seed-a");
        assert!(should_deliver_timeline_live(
            &live("seed-a", 4),
            "cs-2",
            "seed-a",
            after,
            &replayed,
            &leases
        ));
    }
}
