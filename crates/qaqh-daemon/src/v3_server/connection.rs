//! V3 服务端：接受循环 + **十一域静态分派**（`ADR-0042` 铁律 2/4/6）。
//!
//! ## 一条连接 = 一个 `MuxTransport`，四条逻辑流 = 四条 bi 流
//!
//! 接受循环只做三件事：接流、认首帧、把流交给对应的服务循环。分派是编译期
//! `match`（无 `dyn`），所以"哪条路走哪个门面"在类型层面就是确定的。
//!
//! ## 握手在 Control 流上，且必须最先完成
//!
//! 多路复用没有"连接建立"事件，只有一条条流。所以身份必须落在**第一条 Control
//! 流**上：`StreamKind::Control` → `ClientHello` → `ServerHello`。在此之前到达的
//! 任何业务流一律关掉——**未认证的连接没有资格开流**，这条规则比"先收下再补认证"
//! 少一整类竞态。
//!
//! ## 第二条 Control 流会被拒
//!
//! 控制面是"一问一答"：两条 Control 流会让同一个 `command_id` 的回执出现两个
//! 乱序的写者。宁可拒绝，也不让定序失去物理载体。
//!
//! ## blob 推送是**服务端开流**
//!
//! 门面把"要推这个 blob"投进本连接的推送队列，推送任务据此 `open_bi` 开一条
//! `BlobTransfer` 流。方向与服务端推 `SessionEvents` 一致：服务端知道有什么要推，
//! 客户端只需要收。

use std::sync::Arc;
use std::sync::atomic::Ordering;

use qaqh_core_vocab::SessionId;
use qaqh_ringing_v3::{
    ClientHello, ControlCall, ControlReply, ControlResponse, DeliveryClass,
    ErrorCode, FrameMeta, HandshakeReject, PROTOCOL_FINGERPRINT, PROTOCOL_REV, ResyncReason,
    ServerHello, SessionEvent, SessionEventFrame, ShellOutputFrame, StreamAttach, StreamKind,
    verify_handshake,
};
use qaqh_v3_transport::{MuxError, MuxTransport};
use tokio::sync::{mpsc, watch};

use super::V3Server;
use super::bus::{DeltaCoalescer, SubscribeOutcome};
use super::error::{ServerError, rpc};
use super::facade::{BlobPush, CallContext};
use super::framing::{read_frame, write_frame};
use super::identity::ConnectionIdentity;
use super::state::{ReceiptStart, ServerState};
use super::util::{lock, now_ms};

/// 一条连接上共享的状态（所有逻辑流看到的是**同一个身份**）。
pub(crate) struct ConnectionState {
    identity: std::sync::Mutex<Option<Arc<ConnectionIdentity>>>,
    cancel: watch::Sender<bool>,
    blob_pushes: mpsc::UnboundedSender<BlobPush>,
    /// 本连接附着过的会话（收摊时据此撤销附着与驱动权）。
    attached_sessions: Arc<std::sync::Mutex<std::collections::BTreeSet<SessionId>>>,
}

impl ConnectionState {
    /// 建一份连接状态，返回它自己与两个接收端（取消哨兵、blob 推送队列）。
    pub(crate) fn new() -> (
        Arc<Self>,
        watch::Receiver<bool>,
        mpsc::UnboundedReceiver<BlobPush>,
    ) {
        let (cancel, cancel_rx) = watch::channel(false);
        let (blob_pushes, blob_rx) = mpsc::unbounded_channel();
        (
            Arc::new(Self {
                identity: std::sync::Mutex::new(None),
                cancel,
                blob_pushes,
                attached_sessions: Arc::new(std::sync::Mutex::new(
                    std::collections::BTreeSet::new(),
                )),
            }),
            cancel_rx,
            blob_rx,
        )
    }

    fn identity(&self) -> Option<Arc<ConnectionIdentity>> {
        lock(&self.identity).clone()
    }

    fn set_identity(&self, identity: Arc<ConnectionIdentity>) {
        *lock(&self.identity) = Some(identity);
    }

    fn subscribe_cancel(&self) -> watch::Receiver<bool> {
        self.cancel.subscribe()
    }

    /// 取消整条连接（吊销/停机/收摊）。
    pub(crate) fn cancel(&self) {
        self.cancel.send_replace(true);
    }

    fn blob_sender(&self) -> mpsc::UnboundedSender<BlobPush> {
        self.blob_pushes.clone()
    }

    fn attached_sessions(&self) -> Arc<std::sync::Mutex<std::collections::BTreeSet<SessionId>>> {
        Arc::clone(&self.attached_sessions)
    }

    /// 收摊：把本连接留下的附着与驱动权**全部撤掉**。
    ///
    /// 不做这一步，"断联不杀进程"的另一面就变成"断联留下幽灵驱动端"：
    /// 会话的驱动权永远指向一个已经不存在的实例，任何重连都无法投递输入。
    fn teardown(&self, state: &ServerState) {
        let Some(identity) = self.identity() else {
            return;
        };
        for session_id in lock(&self.attached_sessions).iter() {
            if let Some(session) = state.session(session_id) {
                session.detach(&identity.device, &identity.client_instance_id);
                session.release_driver(&identity.device, &identity.client_instance_id);
            }
        }
    }
}

impl<B: MuxTransport> V3Server<B> {
    /// 跑接受循环，直到传输层关闭或本端收摊。
    pub(crate) async fn serve(self: Arc<Self>) -> Result<(), ServerError> {
        self.state.peers.fetch_add(1, Ordering::Relaxed);
        let (connection, cancel_rx, blob_rx) = ConnectionState::new();
        let pump = tokio::spawn(blob_pump(
            Arc::clone(&self),
            blob_rx,
            connection.subscribe_cancel(),
        ));
        let result = accept_loop(Arc::clone(&self), Arc::clone(&connection), cancel_rx).await;
        connection.cancel();
        connection.teardown(&self.state);
        let _ = pump.await;
        self.state.peers.fetch_sub(1, Ordering::Relaxed);
        result
    }
}

async fn accept_loop<B: MuxTransport>(
    server: Arc<V3Server<B>>,
    connection: Arc<ConnectionState>,
    mut cancel: watch::Receiver<bool>,
) -> Result<(), ServerError> {
    loop {
        let accepted = tokio::select! {
            biased;
            _ = cancel.changed() => return Ok(()),
            accepted = server.transport.accept_bi() => accepted,
        };
        match accepted {
            Ok(stream) => {
                let server = Arc::clone(&server);
                let connection = Arc::clone(&connection);
                tokio::spawn(async move { serve_stream(server, stream, connection).await });
            }
            Err(MuxError::Closed) => return Ok(()),
            Err(error) => return Err(ServerError::Mux(error)),
        }
    }
}

/// 服务端推 blob 的任务（每条连接一个）。
async fn blob_pump<B: MuxTransport>(
    server: Arc<V3Server<B>>,
    mut requests: mpsc::UnboundedReceiver<BlobPush>,
    mut cancel: watch::Receiver<bool>,
) {
    // 两条触发路径：本连接门面在回执里带出来的外置请求，以及进程内的推送广播
    // （AgentLoop 的位置）。后者与 RPC 完全独立——守卫②量的正是这条路径的隔离性。
    let mut broadcast = server.state.push_bus.subscribe();
    loop {
        let push = tokio::select! {
            biased;
            _ = cancel.changed() => return,
            maybe = requests.recv() => match maybe {
                Some(push) => push,
                None => return,
            },
            broadcasted = broadcast.recv() => match broadcasted {
                Ok(push) => push,
                // 落后于广播：跳过（blob 内容寻址，客户端会按 ContentRef 再要）。
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => return,
            },
        };
        let mut stream = match server.transport.open_bi().await {
            Ok(stream) => stream,
            Err(error) => {
                log::warn!("v3_blob: 开 BlobTransfer 流失败：{error}");
                return;
            }
        };
        // 首帧必须是 StreamKind：`yamux` 的 SYN 挂在第一帧上。
        if let Err(error) = write_frame(&mut stream, &StreamKind::BlobTransfer).await {
            log::warn!("v3_blob: 写流头失败：{error}");
            return;
        }
        match server
            .facades
            .blob
            .push(&mut stream, push.hash, push.len)
            .await
        {
            Ok(bytes) => log::debug!("v3_blob: 推送 {} 字节", bytes),
            Err(error) => log::warn!("v3_blob: 推送失败：{error:?}"),
        }
    }
}

async fn serve_stream<B: MuxTransport>(
    server: Arc<V3Server<B>>,
    mut stream: B::BiStream,
    connection: Arc<ConnectionState>,
) {
    let kind: StreamKind = match read_frame(&mut stream).await {
        Ok(kind) => kind,
        Err(error) => {
            log::debug!("v3_conn: 首帧不可读（{error}），关流");
            return;
        }
    };
    match kind {
        StreamKind::Control => {
            if connection.identity().is_some() {
                log::warn!("v3_conn: 第二条 Control 流被拒（一问一答面必须单流）");
                return;
            }
            let Some(identity) = handshake(&server, &mut stream).await else {
                return;
            };
            connection.set_identity(Arc::clone(&identity));
            // 吊销哨兵 → 整条连接收摊（即时踢流）。
            let mut revoked = identity.revoked.clone();
            let cancel = connection.clone();
            tokio::spawn(async move {
                let _ = revoked.changed().await;
                cancel.cancel();
            });
            serve_control(&server, stream, identity, connection).await;
        }
        StreamKind::SessionEvents {
            session_id,
            since_seq,
        } => {
            let Some(identity) = connection.identity() else {
                log::warn!("v3_conn: 未认证连接试图开 SessionEvents 流");
                return;
            };
            serve_session_events(
                &server,
                stream,
                identity,
                session_id,
                since_seq,
                connection.subscribe_cancel(),
            )
            .await;
        }
        StreamKind::ShellPty {
            shell_id,
            from_offset,
        } => {
            let Some(identity) = connection.identity() else {
                log::warn!("v3_conn: 未认证连接试图开 ShellPty 流");
                return;
            };
            serve_shell_pty(
                &server,
                stream,
                identity,
                shell_id,
                from_offset,
                connection.subscribe_cancel(),
            )
            .await;
        }
        StreamKind::BlobTransfer => {
            log::warn!("v3_conn: 客户端不得开 BlobTransfer 流（该流是服务端推通道）");
        }
    }
}

async fn handshake<B: MuxTransport>(
    server: &V3Server<B>,
    stream: &mut B::BiStream,
) -> Option<Arc<ConnectionIdentity>> {
    let hello: ClientHello = match read_frame(stream).await {
        Ok(hello) => hello,
        Err(error) => {
            log::debug!("v3_conn: 客户端首帧不可读（{error}）");
            return None;
        }
    };
    // 先版本后凭据：版本失配时连凭据都不想看（不泄漏"这个令牌存在过"）。
    let accepted = match verify_handshake(
        &hello,
        &server.state.config.server_instance_id,
        &server.state.config.build_id,
    ) {
        ServerHello::Rejected(reject) => {
            let _ = write_frame(stream, &ServerHello::Rejected(reject)).await;
            return None;
        }
        accepted => accepted,
    };
    match server.state.identity.authenticate(&hello) {
        Ok(identity) => {
            if write_frame(stream, &accepted).await.is_err() {
                return None;
            }
            Some(Arc::new(identity))
        }
        Err(reason) => {
            let reject = HandshakeReject {
                reason,
                server_rev: PROTOCOL_REV,
                server_fingerprint: PROTOCOL_FINGERPRINT,
            };
            let _ = write_frame(stream, &ServerHello::Rejected(reject)).await;
            None
        }
    }
}

async fn serve_control<B: MuxTransport>(
    server: &V3Server<B>,
    mut stream: B::BiStream,
    identity: Arc<ConnectionIdentity>,
    connection: Arc<ConnectionState>,
) {
    let context = CallContext {
        identity: Arc::clone(&identity),
        blob_pushes: connection.blob_sender(),
        attached_sessions: connection.attached_sessions(),
    };
    let mut cancel = connection.subscribe_cancel();
    loop {
        let call: ControlCall = tokio::select! {
            biased;
            _ = cancel.changed() => {
                log::info!("v3_conn: 连接已取消（吊销/停机），控制面收流");
                return;
            }
            frame = read_frame(&mut stream) => match frame {
                Ok(call) => call,
                Err(error) => {
                    log::debug!("v3_conn: 控制面收流（{error}）");
                    return;
                }
            },
        };
        let reply = execute(server, &context, call).await;
        if let Err(error) = write_frame(&mut stream, &reply).await {
            log::debug!("v3_conn: 回执写失败（{error}）");
            return;
        }
    }
}

/// 执行一条控制面调用（**幂等在这里落地**：同 `command_id` 绝不执行第二次）。
async fn execute<B: MuxTransport>(
    server: &V3Server<B>,
    context: &CallContext,
    call: ControlCall,
) -> ControlReply {
    let command_id = call.command_id.clone();
    let mut request = Some(call.request);
    let mut retries = 0u32;
    loop {
        match server.state.receipts.begin(&command_id) {
            ReceiptStart::Duplicate(reply) => return (*reply).clone(),
            ReceiptStart::Fresh { publish } => {
                let Some(request) = request.take() else {
                    return ControlReply {
                        command_id,
                        response: ControlResponse::Error(rpc(
                            ErrorCode::Internal,
                            "同一 command_id 重入但请求已消耗",
                        )),
                    };
                };
                let response = server.dispatch(context, request).await;
                let reply = ControlReply {
                    command_id: command_id.clone(),
                    response,
                };
                return (*server.state.receipts.finish(&command_id, reply, &publish)).clone();
            }
            ReceiptStart::Wait(mut receiver) => match receiver.changed().await {
                Ok(()) => {
                    if let Some(reply) = receiver.borrow().clone() {
                        return (*reply).clone();
                    }
                }
                Err(_) => {
                    // 领导者异常退出（连接断开等）：摘掉在飞槽位，重新排一次队。
                    server.state.receipts.abandon(&command_id);
                    retries += 1;
                    if retries > 3 {
                        return ControlReply {
                            command_id,
                            response: ControlResponse::Error(rpc(
                                ErrorCode::Unavailable,
                                "同一 command_id 的并发执行反复中断，请重试",
                            )),
                        };
                    }
                }
            },
        }
    }
}

async fn serve_session_events<B: MuxTransport>(
    server: &V3Server<B>,
    mut stream: B::BiStream,
    identity: Arc<ConnectionIdentity>,
    session_id: SessionId,
    since_seq: u64,
    mut cancel: watch::Receiver<bool>,
) {
    let Some(session) = server.state.session(&session_id) else {
        log::warn!("v3_session_events: 没有会话 `{session_id}`");
        return;
    };
    if !session.can_access(&identity.device) {
        log::warn!("v3_session_events: 设备越权访问会话 `{session_id}`");
        return;
    }
    let kind = StreamKind::SessionEvents {
        session_id: session_id.clone(),
        since_seq,
    };
    let epoch = session.epoch();

    let outcome = session.bus.subscribe(since_seq);
    let (replay, mut live, mut lagged, attach_seq) = match outcome {
        SubscribeOutcome::CursorGap {
            requested_since,
            oldest_available,
            newest,
        } => {
            log::info!(
                "v3_session_events: 游标 {requested_since} 跌出回放尾（可用 {oldest_available}..{newest}）"
            );
            let attach = StreamAttach {
                stream: kind,
                seq: newest,
                timeline_epoch: epoch,
            };
            if write_frame(&mut stream, &attach).await.is_err() {
                return;
            }
            let frame = SessionEventFrame {
                meta: FrameMeta {
                    seq: newest,
                    server_ts_ms: now_ms(),
                    delivery: DeliveryClass::Reliable,
                },
                event: SessionEvent::ResyncRequired {
                    session_id,
                    reason: ResyncReason::CursorGap,
                    timeline_epoch: epoch,
                },
            };
            let _ = write_frame(&mut stream, &frame).await;
            return;
        }
        SubscribeOutcome::Attached {
            replay,
            live,
            lagged,
            attach_seq,
        } => (replay, live, lagged, attach_seq),
    };

    let attach = StreamAttach {
        stream: kind,
        seq: attach_seq,
        timeline_epoch: epoch,
    };
    if write_frame(&mut stream, &attach).await.is_err() {
        return;
    }

    let mut coalescer = DeltaCoalescer::new();
    // 回放：环里存的是**未合帧的权威帧**，与实时看到的只是分批差别。
    for frame in replay {
        for out in coalescer.push(frame) {
            if write_frame(&mut stream, out.as_ref()).await.is_err() {
                return;
            }
        }
    }
    for out in coalescer.flush() {
        if write_frame(&mut stream, out.as_ref()).await.is_err() {
            return;
        }
    }

    loop {
        let next = tokio::select! {
            biased;
            _ = cancel.changed() => return,
            _ = lagged.changed() => {
                // 队列溢出：**不静默丢帧**，明说"你落后了，重订"。
                let frame = SessionEventFrame {
                    meta: FrameMeta {
                        seq: session.bus.last_seq(),
                        server_ts_ms: now_ms(),
                        delivery: DeliveryClass::Reliable,
                    },
                    event: SessionEvent::ResyncRequired {
                        session_id,
                        reason: ResyncReason::CursorGap,
                        timeline_epoch: epoch,
                    },
                };
                let _ = write_frame(&mut stream, &frame).await;
                return;
            }
            maybe = live.recv() => match maybe {
                Some(frame) => frame,
                None => return,
            },
        };

        // 贪心取一批再写：洪峰下才有可能真的合帧（一帧一写等于不合）。
        let mut batch = vec![next];
        while let Ok(frame) = live.try_recv() {
            batch.push(frame);
            if batch.len() >= 64 {
                break;
            }
        }
        let mut out = Vec::new();
        for frame in batch {
            out.extend(coalescer.push(frame));
        }
        out.extend(coalescer.flush());
        for frame in out {
            if write_frame(&mut stream, frame.as_ref()).await.is_err() {
                return;
            }
        }
    }
}

async fn serve_shell_pty<B: MuxTransport>(
    server: &V3Server<B>,
    mut stream: B::BiStream,
    identity: Arc<ConnectionIdentity>,
    shell_id: String,
    from_offset: u64,
    mut cancel: watch::Receiver<bool>,
) {
    let Some(handle) = server.state.shells.get(&shell_id) else {
        log::warn!("v3_shell_pty: 没有 shell `{shell_id}`");
        return;
    };
    let Some(session) = server.state.session(&handle.session_id) else {
        log::warn!("v3_shell_pty: shell `{shell_id}` 的会话已不存在");
        return;
    };
    if !session.can_access(&identity.device) {
        log::warn!("v3_shell_pty: 设备越权访问 shell `{shell_id}`");
        return;
    }
    let ShellAttachParts {
        replay,
        mut live,
        mut lagged,
        attach_offset,
        gap,
        generation,
    } = ShellAttachParts::split(handle.attach(from_offset, &server.state.config));
    if gap {
        // 请求偏移已跌出历史：`attach.seq` 会明确给出当前水位，客户端据此丢弃旧缓冲
        // 并从水位重建（终端的"重连"语义，不是什么都能续上）。
        log::info!("v3_shell_pty: shell `{shell_id}` 请求偏移 {from_offset} 已跌出历史");
    }
    let kind = StreamKind::ShellPty {
        shell_id: shell_id.clone(),
        from_offset,
    };
    let attach = StreamAttach {
        stream: kind,
        seq: attach_offset,
        timeline_epoch: 0,
    };
    if write_frame(&mut stream, &attach).await.is_err() {
        handle.detach(generation);
        return;
    }
    let mut seq = 0u64;
    for (start, chunk) in replay {
        seq += 1;
        let frame = ShellOutputFrame {
            meta: FrameMeta {
                seq,
                server_ts_ms: now_ms(),
                delivery: DeliveryClass::Reliable,
            },
            start_offset: start,
            end_offset: start + chunk.len() as u64,
            bytes: chunk,
        };
        if write_frame(&mut stream, &frame).await.is_err() {
            handle.detach(generation);
            return;
        }
    }

    loop {
        let chunk = tokio::select! {
            biased;
            _ = cancel.changed() => break,
            _ = lagged.changed() => break,
            maybe = live.recv() => match maybe {
                Some(chunk) => chunk,
                None => break,
            },
        };
        let (start, bytes) = chunk;
        seq += 1;
        let frame = ShellOutputFrame {
            meta: FrameMeta {
                seq,
                server_ts_ms: now_ms(),
                delivery: DeliveryClass::Reliable,
            },
            start_offset: start,
            end_offset: start + bytes.len() as u64,
            bytes,
        };
        if write_frame(&mut stream, &frame).await.is_err() {
            break;
        }
    }
    handle.detach(generation);
}

/// [`ShellAttach`] 的拆解形式（`live`/`lagged` 必须先取出来再进 `select!` 循环，
/// 否则每轮 clone 一个 receiver 会立刻"看到变化"而空转）。
struct ShellAttachParts {
    replay: Vec<(u64, bytes::Bytes)>,
    live: mpsc::Receiver<(u64, bytes::Bytes)>,
    lagged: watch::Receiver<bool>,
    attach_offset: u64,
    gap: bool,
    generation: u64,
}

impl ShellAttachParts {
    fn split(attach: super::state::ShellAttach) -> Self {
        Self {
            replay: attach.replay,
            live: attach.live,
            lagged: attach.lagged,
            attach_offset: attach.attach_offset,
            gap: attach.gap,
            generation: attach.generation,
        }
    }
}
