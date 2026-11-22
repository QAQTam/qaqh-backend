//! axum_impl::shell_ws — 用户 shell 终端的 ws 数据面（ADR-0028 §D3 裁定=候选 B，切片 4）。
//!
//! # 这一条流的全部规则
//!
//! - **一条路由**：`GET /ringing/v2/shells/{shell_id}/stream`（ws upgrade）。表里登记的
//!   method 写 `GET`，因为升级请求按 HTTP 事实就是 GET + `Upgrade: websocket`；ws 不是
//!   HTTP 方法，把 `WS` 写进 method 列就等于让 B02 闸的 method 维度失去意义。
//! - **身份只有一套**：`authz::authenticate` 已经解析过 Bearer 并把 [`Identity`] 注入
//!   extensions；这里只调 [`service_api::shell_owner`] —— 与 `shell.*` RPC 臂**同一个函数**。
//!   本文件不解析 token、不自写 scope 判断、不读 params 里的自报 owner。
//! - **4 个控制帧名，不发明第五个**：上行 `attach`/`stdin`/`resize`（text JSON），下行
//!   `closed`（text JSON）+ **裸字节**数据帧。没有 seq、没有 ack、没有
//!   `ringing.resync_required`：shell 不存历史，恢复语义就是 `attach{from_offset}` 那条
//!   有界环（`SHELL_REPLAY_CAP`），与老流的 offset 同形。
//! - **断联不 kill**：观看者走人只减 `receiver_count`，不动会话（与 `ExecStopReason` 无
//!   client 档同律）。revoke 才杀，且 revoke 的路在注册表里（`shutdown_owner`）。
//! - **字节永不进 `SessionEventFrame`**：会话 SSE 的 `EVENT_CHANNEL_CAPACITY` / 回放尾 /
//!   `epoch:seq` 游标一概不动。
//!
//! # 落后与终止为什么不需要新协议
//!
//! `broadcast::Receiver` 报 `Lagged` 时不掐流，而是拿**自己已交付的 offset** 重新
//! `shell_attach` 取回放尾 —— 服务端不记任何客户端游标，恢复动作与客户端重连是同一条路。
//!
//! 终止原因只有一个真源：注册表的 `ShellAttach::stopped`（watch，单向：一旦 `Some` 就
//! 不回退）。所以「先把尾字节搬完、再宣布死了」这个顺序能成立 —— 观看者不会丢退出前的
//! 最后一段输出。

use super::*;
use axum::extract::ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade};
use axum::extract::FromRequestParts;
use base64::{Engine as _, engine::general_purpose::STANDARD};
use futures_util::{SinkExt, StreamExt};
use qaqh_runtime::{ShellAttach, ShellStopReason};
use tokio::sync::broadcast::{self, error::RecvError};

type WsSink = futures_util::stream::SplitSink<WebSocket, Message>;
type FrameReceiver = broadcast::Receiver<qaqh_runtime::ShellFrame>;

/// 控制帧上限。粘贴整块是一次 `stdin` 帧（base64 后约 1.34×），1MiB 足够宽，又不至于
/// 让一条终端连接变成无界内存入口。
const SHELL_WS_MAX_CONTROL_BYTES: usize = 1024 * 1024;

/// 1002 = 协议错（帧形状不合法），1008 = 策略拒（词表外的帧、非法几何）。
///
/// 错误走 ws 的 close 帧，**不**造第五种控制帧：往用户的终端字节流里编「错误文本」既会
/// 花屏，又是第二份错误事实源。
const CLOSE_PROTOCOL: u16 = 1002;
const CLOSE_POLICY: u16 = 1008;

/// RFC 6455 §5.5.1：close reason ≤123 字节。按**字符**截断（切在 UTF-8 中间即 panic，
/// 而注册表的错误文案里带路径）。
fn close_frame(code: u16, message: &str) -> Message {
    let reason: String = message.chars().take(96).collect();
    Message::Close(Some(CloseFrame { code, reason: reason.into() }))
}

/// `?from_offset=`：握手即 attach 的续读点；缺席 = 0 = 整段有界回放（重连的常规情形）。
/// 它**不是**会话游标：服务端不记这个数，也不因它发 resync。
#[derive(Deserialize)]
pub(crate) struct ShellStreamQuery {
    #[serde(default)]
    pub from_offset: Option<u64>,
}

pub(crate) async fn handle_shell_stream_v2(
    State(state): State<AppState>,
    Extension(identity): Extension<Identity>,
    headers: HeaderMap,
    Path(shell_id): Path<String>,
    Query(query): Query<ShellStreamQuery>,
    // 唯一允许消费请求的抽取器，放最后一位：`into_parts()` 之后再把 `Parts` 交给
    // `WebSocketUpgrade`，这样身份判定永远排在升级之前。
    request: Request,
) -> Response {
    // 抽取顺序是刻意的：`WebSocketUpgrade` **不**放在参数表末尾自动跑，而是等
    // scope/lease/owner/attach 全部判完再手动抽取。反过来写的话（抽取器先行），
    // 一个没权限的调用方会先吃到 426「要升级」——那是把「这条流存在、你的身份不够」
    // 的顺序颠倒给探测者看，而且 `shell_owner` 的判定结果再也无法成为握手的第一道门。
    let owner = match service_api::shell_owner(&state, &headers, &identity) {
        Ok(owner) => owner,
        Err(response) => return response,
    };
    // attach 在升级**之前**做：不存在/不属于本身份的 shell 给 HTTP 错误，而不是一条
    // 升级成功后才慢慢死的连接（宪法 5：失败要响，形状也要对）。
    let attach = match state
        .service
        .shell_attach(&owner, &shell_id, query.from_offset.unwrap_or(0))
    {
        Ok(attach) => attach,
        Err(error) => {
            // 注册表对「不存在」与「不是你的」给同一句话，这里也同一个码：跨 device 的
            // shell_id 探测问不出任何东西（D2 的机器面）。
            return v2::api_error_response(StatusCode::NOT_FOUND, "shell_not_found", &error);
        }
    };
    let mut parts = request.into_parts().0;
    let ws = match WebSocketUpgrade::from_request_parts(&mut parts, &state).await {
        Ok(ws) => ws,
        Err(rejection) => return rejection.into_response(),
    };
    ws.max_message_size(SHELL_WS_MAX_CONTROL_BYTES)
        .max_frame_size(SHELL_WS_MAX_CONTROL_BYTES)
        .on_upgrade(move |socket| stream_shell(socket, state.service, owner, shell_id, attach))
}

/// 一条连接的搬字节/搬控制循环。
async fn stream_shell(
    socket: WebSocket,
    service: QaqhService,
    owner: String,
    shell_id: String,
    attach: ShellAttach,
) {
    let ShellAttach { info: _, replay, from_offset: _, next_offset, mut frames, stopped } = attach;
    let (mut sink, mut inbound) = socket.split();
    // select 里等的是这条 clone（各自的「已读到第几个值」是独立的），下面补发/close
    // 走 `stopped` 本体 —— 同一条 watch 的两个视图，不是第二份事实。
    // `stopped` 只读（`borrow()`），所以它不需要 mut：唤醒由 `stopping` 负责。
    let mut stopping = stopped.clone();
    // 已交付水位：这条连接唯一记住的数，且只用于回放去重与落后重取，不进协议字段。
    let mut watermark = next_offset;

    if send_replay(&mut sink, replay).await.is_err() {
        return;
    }

    loop {
        tokio::select! {
            // 先搬输出：打字是本地毫秒级的小活，落后才是会看的问题。
            biased;

            received = frames.recv() => match received {
                Ok(frame) => {
                    // `attach` 是先 subscribe 后取环快照，所以订阅与快照之间的那帧可能与
                    // 回放整帧重叠 —— 按 offset 去重（帧边界即字节边界,不会半帧）。
                    if frame.offset > watermark {
                        watermark = frame.offset;
                        if sink.send(Message::binary(frame.bytes.to_vec())).await.is_err() {
                            return;
                        }
                    }
                }
                Err(RecvError::Lagged(_)) => {
                    // 慢观看者不许钉住读泵,也不许让它带着缺口继续看：拿已交付水位重新
                    // attach 取有界回放尾。attach 失败（会话已被收割）即结束这条流。
                    // 水位必须提到返回的 next_offset：订阅先于环快照，订阅与快照之间
                    // 广播出的那批帧已经含在回放里，拿旧水位去重就会整批再发一遍
                    // （同一段打印两遍，还会把客户端的 delivered 计数推过服务端总数）。
                    match resync(&service, &owner, &shell_id, &mut sink, &mut frames, watermark).await {
                        Ok(fresh_watermark) => watermark = fresh_watermark,
                        Err(StreamGone) => break,
                    }
                }
                // 所有 Sender 都掉线 = 注册表与读泵都已放手 = 这条会话已经收摊。
                Err(RecvError::Closed) => break,
            },

            notice = stopping.changed() => {
                // 终止事实到了。**不**在这里 return：尾字节可能还在 broadcast 里排队，
                // 让它们先走完（下一轮 recv 必拿到 Closed），再落 `closed` 帧。
                if notice.is_err() {
                    break;
                }
            },

            message = inbound.next() => {
                match message {
                    // 观看者走人：只结束这条订阅，会话继续活（断联不 kill）。
                    None | Some(Err(_)) => return,
                    Some(Ok(Message::Text(text))) => {
                        let control = match serde_json::from_str::<qaqh_ringing::ShellStreamControl>(&text) {
                            Ok(control) => control,
                            Err(error) => {
                                let _ = sink.send(close_frame(CLOSE_PROTOCOL, &format!("unrecognized control frame: {error}"))).await;
                                return;
                            }
                        };
                        match control {
                            qaqh_ringing::ShellStreamControl::Attach { from_offset } => {
                                // 客户端主动重建屏幕（`from_offset` 缺席 = 整段缓冲尾再来一遍）。
                                // 与 Lagged 同律：水位提到 next_offset，重叠帧不重发。
                                match resync(&service, &owner, &shell_id, &mut sink, &mut frames, from_offset.unwrap_or(0)).await {
                                    Ok(fresh_watermark) => watermark = fresh_watermark,
                                    Err(StreamGone) => break,
                                }
                            }
                            qaqh_ringing::ShellStreamControl::Stdin { bytes } => {
                                match STANDARD.decode(bytes) {
                                    Ok(raw) => {
                                        if let Err(error) =
                                            service.shell_stdin(&owner, &shell_id, &raw)
                                            && report_control_failure(&stopped, &mut sink, "stdin", &error)
                                                .await
                                        {
                                            return;
                                        }
                                    }
                                    Err(error) => {
                                        let _ = sink.send(close_frame(CLOSE_PROTOCOL, &format!("stdin.bytes is not base64: {error}"))).await;
                                        return;
                                    }
                                }
                            }
                            qaqh_ringing::ShellStreamControl::Resize { cols, rows } => {
                                if let Err(error) =
                                    service.shell_resize(&owner, &shell_id, cols, rows)
                                    && report_control_failure(&stopped, &mut sink, "resize", &error)
                                        .await
                                {
                                    return;
                                }
                            }
                        }
                    }
                    // 上行只认 text 控制帧。把裸字节当 stdin 是第二种写法，两套并存正是
                    // 这次裁定要防的臃肿（D3 硬约束第 3 条）。
                    Some(Ok(Message::Binary(_))) => {
                        let _ = sink.send(close_frame(CLOSE_POLICY, "uplink must be a JSON control frame; binary frames are downlink-only")).await;
                        return;
                    }
                    Some(Ok(Message::Close(_))) => return,
                    // axum 自动回 Pong：不在这里造第二条心跳语义。
                    Some(Ok(Message::Ping(_))) | Some(Ok(Message::Pong(_))) => {}
                }
            }
        }
    }

    // 到这一步 = broadcast 已断（会话已被收割）。原因只有一个真源。
    // 先把值拷出来再分支：`watch::Ref` 是 `RwLockReadGuard`，跨 `.await` 存活会让
    // 这个 future 不 `Send`，而 axum 的 upgrade 任务必须能在多线程运行时上跑。
    let recorded = *stopped.borrow();
    match recorded {
        Some(reason) => {
            let event = qaqh_ringing::ShellStreamEvent::Closed {
                reason: reason.as_str().to_owned(),
            };
            // 两个字段的小类型，编码不可能失败；失败就是 serde 自身出事（不许静默）。
            let text = serde_json::to_string(&event).expect("closed frame is two strings");
            let _ = sink.send(Message::text(text)).await;
        }
        // broadcast 断掉必然经过注册表的 mark_stopped；读不到即内核与协议脱钩，
        // 当场报警，不许编一个原因给用户（宪法 5）。
        None => log::warn!("[shell] {shell_id} 的连接结束而注册表没有终止事实（内核侧脱钩）"),
    }
    let _ = sink.close().await;
}

/// 重新订阅并补放：`from_offset` 由调用方给（落后重取 = 已交付水位；显式 attach =
/// 客户端要的起点）。成功时返回新 attach 的 `next_offset`，调用方必须把它提为
/// 自己的交付水位（理由见两处调用点）。返回 Err 表示这条流没有继续的意义了
/// （会话没了 / 客户端走了）。
async fn resync(
    service: &QaqhService,
    owner: &str,
    shell_id: &str,
    sink: &mut WsSink,
    frames: &mut FrameReceiver,
    from_offset: u64,
) -> Result<u64, StreamGone> {
    let fresh = service
        .shell_attach(owner, shell_id, from_offset)
        .map_err(|error| {
            log::debug!("[shell] {shell_id} 重取失败（会话已收摊）: {error}");
            StreamGone
        })?;
    let next_offset = fresh.next_offset;
    send_replay(sink, fresh.replay).await?;
    *frames = fresh.frames;
    Ok(next_offset)
}

async fn send_replay(sink: &mut WsSink, replay: Vec<u8>) -> Result<(), StreamGone> {
    if replay.is_empty() {
        return Ok(());
    }
    sink.send(Message::binary(replay)).await.map_err(|_| StreamGone)
}

/// 控制帧背后的会话操作失败。
///
/// 会话**已经终止**时不许用 close 帧抢在 `closed` 前面把真正的原因藏掉 —— 那种失败是
/// 退出竞态（`exit` 与 resize 撞上），紧接着 broadcast 就会断线并落 `closed`。其余失败
/// （非法几何、非法 base64 之外的内核拒绝）是客户端的协议错，当场终结连接并带文案。
async fn report_control_failure(
    stopped: &tokio::sync::watch::Receiver<Option<ShellStopReason>>,
    sink: &mut WsSink,
    what: &str,
    error: &str,
) -> bool {
    // 同样先拷贝再分支：guard 不许活到下一个 .await（Send 要求）。
    let recorded = *stopped.borrow();
    if recorded.is_some() {
        log::debug!("[shell] {what} 撞上已终止的会话: {error}");
        return false;
    }
    log::warn!("[shell] {what} 被拒: {error}");
    let _ = sink.send(close_frame(CLOSE_POLICY, error)).await;
    true
}

/// 这条连接已经没必要继续了（会话被收割，或客户端断开）。
struct StreamGone;
