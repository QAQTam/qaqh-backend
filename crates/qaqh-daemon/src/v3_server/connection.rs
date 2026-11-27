//! V3 服务端：接受循环 + 十一域静态分派 + 鉴权推送（`ADR-0042` 铁律 2/4/5/6）。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use qaqh_core_vocab::{ContentHash, SessionId};
use qaqh_ringing_v3::{
    BlobChunkFrame, BlobTransferRole, ClientHello, ControlCall, ControlReply, ControlResponse,
    DeliveryClass, ErrorCode, FrameMeta, HandshakeReject, PROTOCOL_FINGERPRINT, PROTOCOL_REV,
    ResyncReason, ServerHello, SessionEvent, SessionEventFrame, ShellOutputFrame, StreamAttach,
    StreamKind, verify_handshake,
};
use qaqh_v3_transport::{MuxError, MuxTransport, read_frame, write_frame};
use sha2::{Digest, Sha256};
use tokio::sync::{mpsc, watch};

use super::V3Server;
use super::bus::{DeltaCoalescer, SubscribeOutcome};
use super::error::{ServerError, rpc};
use super::facade::{BlobPush, CallContext, V3Backend};
use super::identity::ConnectionIdentity;
use super::receipts::ReceiptStart;
use super::state::ServerState;
use super::util::{lock, now_ms};

/// 客户端上传单条 Blob 上限（64 MiB）。
const MAX_CLIENT_UPLOAD_BLOB_BYTES: u64 = 64 * 1024 * 1024;

async fn wait_true(rx: &mut watch::Receiver<bool>) {
    let _ = rx.wait_for(|&flag| flag).await;
}

/// 一条连接上共享的状态（所有逻辑流看到的是同一个身份）。
pub(crate) struct ConnectionState {
    control_claimed: AtomicBool,
    identity: std::sync::Mutex<Option<Arc<ConnectionIdentity>>>,
    cancel: watch::Sender<bool>,
    blob_pushes: mpsc::UnboundedSender<BlobPush>,
    /// 本连接附着过的会话（收摊时据此撤销附着与驱动权，并约束推送范围）。
    attached_sessions: Arc<std::sync::Mutex<std::collections::BTreeSet<SessionId>>>,
}

impl ConnectionState {
    pub(crate) fn new() -> (
        Arc<Self>,
        watch::Receiver<bool>,
        mpsc::UnboundedReceiver<BlobPush>,
    ) {
        let (cancel, cancel_rx) = watch::channel(false);
        let (blob_pushes, blob_rx) = mpsc::unbounded_channel();
        (
            Arc::new(Self {
                control_claimed: AtomicBool::new(false),
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

    /// 收摊：把本连接留下的附着与驱动权全部撤掉。
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

impl<T: MuxTransport, B: V3Backend> V3Server<T, B> {
    /// 跑接受循环，直到传输层关闭或本端收摊。
    pub(crate) async fn serve(self: Arc<Self>) -> Result<(), ServerError> {
        self.state.peers.fetch_add(1, Ordering::Relaxed);
        let (connection, cancel_rx, blob_rx) = ConnectionState::new();
        let pump = tokio::spawn(blob_pump(
            Arc::clone(&self),
            Arc::clone(&connection),
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

async fn accept_loop<T: MuxTransport, B: V3Backend>(
    server: Arc<V3Server<T, B>>,
    connection: Arc<ConnectionState>,
    mut cancel: watch::Receiver<bool>,
) -> Result<(), ServerError> {
    loop {
        let accepted = tokio::select! {
            biased;
            _ = wait_true(&mut cancel) => return Ok(()),
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

/// 服务端推 blob 的任务（每条连接一个；**严格按身份 + 会话授权过滤**）。
async fn blob_pump<T: MuxTransport, B: V3Backend>(
    server: Arc<V3Server<T, B>>,
    connection: Arc<ConnectionState>,
    mut requests: mpsc::UnboundedReceiver<BlobPush>,
    mut cancel: watch::Receiver<bool>,
) {
    let mut broadcast = server.state.push_bus.subscribe();
    loop {
        let (push, from_broadcast) = tokio::select! {
            biased;
            _ = wait_true(&mut cancel) => return,
            maybe = requests.recv() => match maybe {
                Some(push) => (push, false),
                None => return,
            },
            broadcasted = broadcast.recv() => match broadcasted {
                Ok(push) => (push, true),
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => continue,
                Err(_) => return,
            },
        };
        // 鉴权门禁：未认证 / 已吊销 / 无权访问目标会话 / 广播模式下未附着目标会话 → 严禁推送。
        let Some(identity) = connection.identity() else {
            continue;
        };
        if identity.is_revoked() {
            return;
        }
        let Some(session) = server.state.session(&push.session_id) else {
            continue;
        };
        if !session.can_access(&identity.device) {
            continue;
        }
        if from_broadcast && !lock(&connection.attached_sessions).contains(&push.session_id) {
            continue;
        }

        let mut stream = match server.transport.open_bi().await {
            Ok(stream) => stream,
            Err(error) => {
                log::warn!("v3_blob: 开 BlobTransfer 流失败：{error}");
                return;
            }
        };
        if let Err(error) = write_frame(
            &mut stream,
            &StreamKind::BlobTransfer {
                role: BlobTransferRole::ServerPush,
            },
        )
        .await
        {
            log::warn!("v3_blob: 写流头失败：{error}");
            return;
        }
        match server.backend.push_blob(&mut stream, push.hash, push.len).await {
            Ok(bytes) => log::debug!("v3_blob: 推送 {} 字节", bytes),
            Err(error) => log::warn!("v3_blob: 推送失败：{error:?}"),
        }
    }
}

async fn serve_stream<T: MuxTransport, B: V3Backend>(
    server: Arc<V3Server<T, B>>,
    mut stream: T::BiStream,
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
            // 原子抢占单控制流席位，防并发开两条 Control 流竞态绕过。
            if connection.control_claimed.swap(true, Ordering::SeqCst) {
                log::warn!("v3_conn: 第二条 Control 流被拒（一问一答面必须单流）");
                return;
            }
            let Some(identity) = handshake(&server, &mut stream).await else {
                return;
            };
            connection.set_identity(Arc::clone(&identity));
            // 吊销哨兵 → 整条连接收摊；正常断联时随 conn_cancel 一起退出，绝不泄漏任务。
            let mut revoked = identity.revoked.clone();
            let mut conn_cancel = connection.subscribe_cancel();
            let cancel_handle = Arc::clone(&connection);
            tokio::spawn(async move {
                tokio::select! {
                    _ = wait_true(&mut revoked) => cancel_handle.cancel(),
                    _ = wait_true(&mut conn_cancel) => {}
                }
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
            if identity.is_revoked() {
                return;
            }
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
            if identity.is_revoked() {
                return;
            }
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
        StreamKind::BlobTransfer { role } => {
            let Some(identity) = connection.identity() else {
                log::warn!("v3_conn: 未认证连接试图开 BlobTransfer 流");
                return;
            };
            if identity.is_revoked() {
                return;
            }
            serve_client_blob(&server, stream, &identity, role).await;
        }
    }
}

async fn serve_client_blob<T: MuxTransport, B: V3Backend>(
    server: &V3Server<T, B>,
    mut stream: T::BiStream,
    identity: &Arc<ConnectionIdentity>,
    role: BlobTransferRole,
) {
    match role {
        BlobTransferRole::ServerPush => {
            log::warn!("v3_conn: 客户端不得以 ServerPush 打开 BlobTransfer 流");
        }
        BlobTransferRole::ClientFetch { hash } => {
            let Some(blob) = server.state.blobs.get(&hash) else {
                log::warn!("v3_blob: 客户端拉取不存在的 blob `{}`", hash.to_hex());
                return;
            };
            if let Err(error) = server
                .backend
                .push_blob(&mut stream, hash, blob.len() as u64)
                .await
            {
                log::warn!("v3_blob: ClientFetch 推送失败：{error:?}");
            }
        }
        BlobTransferRole::ClientUpload {
            session_id,
            hash,
            len,
        } => {
            let Some(session) = server.state.session(&session_id) else {
                log::warn!("v3_blob: 上传目标会话 `{session_id}` 不存在");
                let _ = write_frame(&mut stream, &false).await;
                return;
            };
            if !session.can_access(&identity.device) {
                log::warn!("v3_blob: 设备越权向会话 `{session_id}` 上传 blob");
                let _ = write_frame(&mut stream, &false).await;
                return;
            }
            if len > MAX_CLIENT_UPLOAD_BLOB_BYTES {
                log::warn!("v3_blob: 上传字节 {len} 超过上限 {MAX_CLIENT_UPLOAD_BLOB_BYTES}");
                let _ = write_frame(&mut stream, &false).await;
                return;
            }
            let ok = receive_upload(&server.state, &mut stream, hash, len).await;
            let _ = write_frame(&mut stream, &ok).await;
        }
    }
}

async fn receive_upload<S: tokio::io::AsyncRead + Unpin>(
    state: &ServerState,
    stream: &mut S,
    expected_hash: ContentHash,
    expected_len: u64,
) -> bool {
    let mut bytes = Vec::with_capacity(expected_len as usize);
    loop {
        let Ok(frame) = read_frame::<_, BlobChunkFrame>(stream).await else {
            return false;
        };
        if frame.hash != expected_hash || frame.offset != bytes.len() as u64 {
            return false;
        }
        let next_len = (bytes.len() as u64).saturating_add(frame.bytes.len() as u64);
        if next_len > expected_len {
            return false;
        }
        bytes.extend_from_slice(&frame.bytes);
        if frame.last {
            break;
        }
    }
    if bytes.len() as u64 != expected_len {
        return false;
    }
    let actual_digest: [u8; 32] = Sha256::digest(&bytes).into();
    if actual_digest != *expected_hash.as_bytes() {
        return false;
    }
    state.blobs.put(bytes);
    true
}

async fn handshake<T: MuxTransport, B: V3Backend>(
    server: &V3Server<T, B>,
    stream: &mut T::BiStream,
) -> Option<Arc<ConnectionIdentity>> {
    let hello: ClientHello = match read_frame(stream).await {
        Ok(hello) => hello,
        Err(error) => {
            log::debug!("v3_conn: 客户端首帧不可读（{error}）");
            return None;
        }
    };
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

async fn serve_control<T: MuxTransport, B: V3Backend>(
    server: &V3Server<T, B>,
    mut stream: T::BiStream,
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
            _ = wait_true(&mut cancel) => {
                log::info!("v3_conn: 连接已取消（吊销/停机），控制面收流");
                return;
            }
            frame = read_frame(&mut stream) => match frame {
                Ok(call) => call,
                Err(error) => {
                    log::debug!("v3_conn: 控制面收流（{error}），整条连接收摊");
                    connection.cancel();
                    return;
                }
            },
        };
        let reply = execute(server, &context, call).await;
        if let Err(error) = write_frame(&mut stream, &reply).await {
            log::debug!("v3_conn: 回执写失败（{error}），整条连接收摊");
            connection.cancel();
            return;
        }
    }
}

/// 执行一条控制面调用（**幂等键绑定 `DeviceKey`**，严禁跨设备串话）。
async fn execute<T: MuxTransport, B: V3Backend>(
    server: &V3Server<T, B>,
    context: &CallContext,
    call: ControlCall,
) -> ControlReply {
    let device = &context.identity.device;
    let command_id = call.command_id.clone();
    let mut request = Some(call.request);
    let mut retries = 0u32;
    loop {
        match server.state.receipts.begin(device, &command_id) {
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
                return (*server
                    .state
                    .receipts
                    .finish(device, &command_id, reply, &publish))
                .clone();
            }
            ReceiptStart::Wait(mut receiver) => match receiver.changed().await {
                Ok(()) => {
                    if let Some(reply) = receiver.borrow().clone() {
                        return (*reply).clone();
                    }
                }
                Err(_) => {
                    server.state.receipts.abandon(device, &command_id);
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

async fn serve_session_events<T: MuxTransport, B: V3Backend>(
    server: &V3Server<T, B>,
    mut stream: T::BiStream,
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
                    timeline_epoch: session.epoch(),
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
            _ = wait_true(&mut cancel) => return,
            _ = wait_true(&mut lagged) => {
                let frame = SessionEventFrame {
                    meta: FrameMeta {
                        seq: session.bus.last_seq(),
                        server_ts_ms: now_ms(),
                        delivery: DeliveryClass::Reliable,
                    },
                    event: SessionEvent::ResyncRequired {
                        session_id,
                        reason: ResyncReason::CursorGap,
                        timeline_epoch: session.epoch(),
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

async fn serve_shell_pty<T: MuxTransport, B: V3Backend>(
    server: &V3Server<T, B>,
    mut stream: T::BiStream,
    identity: Arc<ConnectionIdentity>,
    shell_id: String,
    from_offset: u64,
    mut cancel: watch::Receiver<bool>,
) {
    let Some(handle) = server.state.shells.get(&shell_id) else {
        log::warn!("v3_shell_pty: 没有 shell `{shell_id}`");
        return;
    };
    if let Some(session_id) = &handle.session_id {
        let Some(session) = server.state.session(session_id) else {
            log::warn!("v3_shell_pty: shell `{shell_id}` 的会话已不存在");
            return;
        };
        if !session.can_access(&identity.device) {
            log::warn!("v3_shell_pty: 设备越权访问 shell `{shell_id}`");
            return;
        }
    } else if handle.owner != identity.device {
        log::warn!("v3_shell_pty: 设备越权访问全局 shell `{shell_id}`");
        return;
    }
    let attach_parts = handle.attach(from_offset, &server.state.config);
    let replay = attach_parts.replay;
    let mut live = attach_parts.live;
    let mut lagged = attach_parts.lagged;
    let attach_offset = attach_parts.attach_offset;
    let gap = attach_parts.gap;
    let generation = attach_parts.generation;
    if gap {
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
            _ = wait_true(&mut cancel) => break,
            _ = wait_true(&mut lagged) => break,
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
