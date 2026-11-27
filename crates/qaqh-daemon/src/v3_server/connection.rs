//! V3 服务端：接受循环 + 十一域静态分派 + 鉴权推送（`ADR-0042` 铁律 2/4/5/6）。

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};

use qaqh_core_vocab::SessionId;
use qaqh_ringing_v3::{
    BlobTransferRole, ClientHello, ControlCall, ControlReply, ControlRequest, ControlResponse,
    ErrorCode, HandshakeReject, PROTOCOL_FINGERPRINT, PROTOCOL_REV, ServerHello, StreamKind,
    SystemResponse, verify_handshake,
};
use qaqh_v3_transport::{MuxError, MuxTransport, read_frame, write_frame};
use tokio::sync::{mpsc, watch};
use tokio::task::JoinSet;

use super::V3Server;
use super::error::{ServerError, rpc};
use super::facade::{BlobPush, CallContext, V3Backend};
use super::identity::ConnectionIdentity;
use super::receipts::ReceiptStart;
use super::util::lock;

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
    fn teardown<B: V3Backend>(&self, backend: &B) {
        let Some(identity) = self.identity() else {
            return;
        };
        backend.teardown(&identity, &lock(&self.attached_sessions));
    }
}

impl<T: MuxTransport, B: V3Backend> V3Server<T, B> {
    /// 跑接受循环，直到传输层关闭或本端收摊。
    pub(crate) async fn serve(self: Arc<Self>) -> Result<(), ServerError> {
        self.state.peers.fetch_add(1, Ordering::Relaxed);
        let (connection, cancel_rx, blob_rx) = ConnectionState::new();
        let mut tasks = JoinSet::new();
        tasks.spawn(blob_pump(
            Arc::clone(&self),
            Arc::clone(&connection),
            blob_rx,
            connection.subscribe_cancel(),
        ));
        let result = accept_loop(
            Arc::clone(&self),
            Arc::clone(&connection),
            cancel_rx,
            &mut tasks,
        )
        .await;
        connection.cancel();
        tasks.shutdown().await;
        connection.teardown(&self.backend);
        self.state.peers.fetch_sub(1, Ordering::Relaxed);
        result
    }
}

async fn accept_loop<T: MuxTransport, B: V3Backend>(
    server: Arc<V3Server<T, B>>,
    connection: Arc<ConnectionState>,
    mut cancel: watch::Receiver<bool>,
    tasks: &mut JoinSet<()>,
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
                tasks.spawn(async move { serve_stream(server, stream, connection).await });
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
        match server
            .backend
            .push_blob(&mut stream, push.hash, push.len)
            .await
        {
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
            server
                .backend
                .serve_session_events(
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
            server
                .backend
                .serve_shell_pty(
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
            server
                .backend
                .serve_client_blob(stream, identity, role)
                .await;
        }
    }
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
        if matches!(
            &reply.response,
            ControlResponse::System(SystemResponse::StopAck { accepted: true, .. })
        ) {
            server.backend.request_shutdown();
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
    let fingerprint = request_fingerprint(&call.request);
    let pairing_secret = matches!(
        &call.request,
        ControlRequest::Pairing(
            qaqh_ringing_v3::PairingRequest::CreateInvite { .. }
                | qaqh_ringing_v3::PairingRequest::Redeem { .. }
        )
    );
    let replayable_pairing_secret = matches!(
        &call.request,
        ControlRequest::Pairing(qaqh_ringing_v3::PairingRequest::CreateInvite { .. })
    );
    let mut request = Some(call.request);
    let mut retries = 0u32;
    loop {
        match server.state.receipts.begin(device, &command_id) {
            ReceiptStart::Duplicate(reply) => return (*reply).clone(),
            ReceiptStart::Fresh { publish } => {
                match server
                    .backend
                    .lookup_receipt(device, &command_id, &fingerprint)
                    .await
                {
                    Ok(Some(reply)) => {
                        return (*server.state.receipts.finish(
                            device,
                            &command_id,
                            reply,
                            &publish,
                        ))
                        .clone();
                    }
                    Ok(None) => {}
                    Err(error) => {
                        server.state.receipts.abandon(device, &command_id);
                        return ControlReply {
                            command_id,
                            response: ControlResponse::Error(error),
                        };
                    }
                }
                let Some(request) = request.take() else {
                    return ControlReply {
                        command_id,
                        response: ControlResponse::Error(rpc(
                            ErrorCode::Internal,
                            "同一 command_id 重入但请求已消耗",
                        )),
                    };
                };
                if let Err(error) = server
                    .backend
                    .reserve_receipt(device, &command_id, &fingerprint)
                    .await
                {
                    server.state.receipts.abandon(device, &command_id);
                    return ControlReply {
                        command_id,
                        response: ControlResponse::Error(error),
                    };
                }
                let response = server.dispatch(context, request).await;
                let reply = ControlReply {
                    command_id: command_id.clone(),
                    response,
                };
                if pairing_secret {
                    // Pairing credentials never enter durable receipts; only invite creation is replayable in memory.
                    if replayable_pairing_secret {
                        return (*server.state.receipts.finish(
                            device,
                            &command_id,
                            reply,
                            &publish,
                        ))
                        .clone();
                    }
                    server.state.receipts.abandon(device, &command_id);
                    return reply;
                }
                if let Err(error) = server
                    .backend
                    .persist_receipt(device, &command_id, &reply)
                    .await
                {
                    server.state.receipts.abandon(device, &command_id);
                    return ControlReply {
                        command_id,
                        response: ControlResponse::Error(error),
                    };
                }
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

fn request_fingerprint(request: &ControlRequest) -> String {
    qaqh_types::sha256_hex(format!("{request:?}").as_bytes())
}

#[cfg(test)]
mod tests {
    use std::time::Duration;

    use qaqh_v3_transport::{MuxConfig, duplex::duplex_pair_with_buffer};

    use super::*;
    use crate::v3_server::harness;

    #[test]
    fn request_fingerprint_is_a_digest_without_plaintext_secrets() {
        let request = ControlRequest::Pairing(qaqh_ringing_v3::PairingRequest::Redeem {
            code: "one-time-secret".into(),
            client_kind: qaqh_ringing_v3::ClientKind::Mobile,
        });
        let fingerprint = request_fingerprint(&request);
        assert_eq!(fingerprint.len(), 64);
        assert!(!fingerprint.contains("one-time-secret"));
        assert_eq!(
            fingerprint,
            qaqh_types::sha256_hex(format!("{request:?}").as_bytes())
        );
    }

    #[tokio::test]
    async fn closing_control_joins_backpressured_stream_tasks() {
        let (client_transport, server_transport) =
            duplex_pair_with_buffer(MuxConfig::default(), 8 * 1024);
        let state = harness::state();
        let session = harness::session_for(&state, &harness::owner_device());
        let (server, server_task) =
            harness::spawn_server(server_transport, Arc::clone(&state)).await;
        let client = harness::connect(client_transport, harness::owner_hello("stream-join")).await;
        let transport = client.transport().clone();
        let events = client
            .attach_session(&session.id, 0)
            .await
            .expect("attach event stream");
        let block = harness::block(99);
        let payload = "x".repeat(2048);
        for _ in 0..4096 {
            session.emit(harness::answer(&session.id, &block, payload.clone()));
        }
        for _ in 0..32 {
            tokio::task::yield_now().await;
        }

        drop(client);
        tokio::time::timeout(Duration::from_secs(2), server_task)
            .await
            .expect("server must stop after control closes")
            .expect("server task")
            .expect("server loop");
        assert_eq!(
            Arc::strong_count(&server),
            1,
            "a backpressured stream survived connection teardown"
        );
        drop(events);
        drop(transport);
    }
}
