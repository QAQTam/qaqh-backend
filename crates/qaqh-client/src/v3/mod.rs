//! `RingingV3` 客户端（**物理隔离目录**：`ADR-0042` 铁律 3/6）。
//!
//! ## 消费端只看到三样东西
//!
//! 1. [`V3Client`]：连接句柄。`call` 一问一答，`attach_session` / `attach_shell`
//!    各返回一条**原生 async 流**；
//! 2. [`SessionEventsStream`]：会话事件帧流——**前端唯一状态真相源**；
//! 3. [`ShellOutputStream`]：PTY 原始字节流（零转义加工）。
//!
//! 没有"再拼一个接口才能渲染"的中间层，也没有需要前端猜的隐式约定
//! （宪法 #5：凡是换完协议反而让前端更难写的设计，一律判定失败）。
//!
//! ## `SessionEvents` 是唯一真相源
//!
//! 本模块**不**维护第二份会话状态缓存。渲染层要显示什么，就读这条流累积出来的
//! 东西；要历史就 `GetSessionSnapshot` + `TranscriptPage`（带 `timeline_epoch`
//! 校验）。两份状态一旦并存，就一定会在某个重连窗口里分叉，而分叉出来的那一份
//! 永远看起来"也能用"。
//!
//! ## 断联是断联
//!
//! 传输层断开就是 [`ClientError::Transport`]，绝不"悄悄重连继续"。重连与尾追是
//! 上层显式动作（用 `attach_session(session_id, since_seq)` 带游标重订），
//! 因为只有调用方知道自己能接受多大的回放尾。

mod blob;
mod endpoint;
mod session_events;
mod shell_output;

pub use blob::{AssembledBlob, BlobInbox};
pub use endpoint::{EndpointError, resolve_daemon_endpoint};
pub use qaqh_v3_transport::FrameIoError;
pub use session_events::{SessionEventsStream, SessionStreamStats};
pub use shell_output::{ShellOutputStream, ShellStreamStats};

use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::Duration;

use qaqh_core_vocab::{ContentHash, SessionId};
use qaqh_ringing_v3::{
    BlobChunkFrame, BlobTransferRole, ClientCredential, ClientHello, ClientKind, ControlCall,
    ControlReply, ControlRequest, ControlResponse, DeliveryClass, EndpointReject, ErrorCode,
    FrameMeta, RpcError, ServerHello, StreamAttach, StreamKind, verify_handshake,
};
use qaqh_v3_transport::{
    MuxError, MuxTransport, YamuxTransport, connect_local_ipc, read_frame, write_frame,
};
use sha2::{Digest, Sha256};
use tokio::sync::{Mutex, watch};
use tokio::task::JoinHandle;

/// 客户端错误（每一种都对应一个可执行动作）。
#[derive(Debug, thiserror::Error)]
pub enum ClientError {
    /// 传输层失败（断联即断联，不做隐式重连）。
    #[error("传输层失败：{0}")]
    Transport(#[from] MuxError),
    /// 帧 I/O 失败。
    #[error("帧 I/O 失败：{0}")]
    Frame(#[from] FrameIoError),
    /// 服务端 RPC 错误（`code` 决定重试/提示/回滚）。
    #[error("服务端拒绝：{code:?} {message}")]
    Rpc {
        /// 机器可读错误码。
        code: ErrorCode,
        /// 人类可读说明。
        message: String,
        /// 是否建议重试。
        retryable: bool,
    },
    /// 握手被拒（版本/指纹/凭据）。
    #[error("握手被拒：{0:?}")]
    HandshakeRejected(qaqh_ringing_v3::HandshakeRejectReason),
    /// 流在 attach 之前被服务端关闭（未认证 / 越权 / 会话不存在）。
    #[error("服务端在 attach 前关闭了流（未认证/越权/会话不存在）")]
    StreamRejected,
    /// 回执与请求的 `command_id` 不匹配（防串话）。
    #[error("回执 command_id 不匹配：期望 `{expected}`，收到 `{actual}`")]
    CommandIdMismatch {
        /// 请求的幂等键。
        expected: String,
        /// 回执里的幂等键。
        actual: String,
    },
    /// 发现文件或构建校验失败。
    #[error("daemon 端点校验失败：{0}")]
    Endpoint(#[from] EndpointError),
    /// daemon 自启失败。
    #[error("启动 qaqh-daemon 失败：{0}")]
    DaemonLaunch(String),
    /// daemon 自启后未在期限内发布可连接端点。
    #[error("qaqh-daemon 未在期限内发布可连接端点")]
    DaemonStartTimeout,
    /// 已连接但 daemon 未在期限内完成 V3 握手。
    #[error("qaqh-daemon 未在期限内完成 V3 握手")]
    HandshakeTimeout,
    /// 协议形状错误。
    #[error("协议错误：{0}")]
    Protocol(String),
}

impl ClientError {
    /// 机器可读错误码（非 RPC 错误为 `None`）。
    pub fn code(&self) -> Option<ErrorCode> {
        match self {
            Self::Rpc { code, .. } => Some(*code),
            _ => None,
        }
    }

    fn from_reply(error: RpcError) -> Self {
        Self::Rpc {
            code: error.code,
            message: error.message,
            retryable: error.retryable,
        }
    }
}

struct ControlChannel<S> {
    stream: S,
}

/// V3 客户端句柄。
pub struct V3Client<B: MuxTransport> {
    transport: B,
    control: Arc<Mutex<ControlChannel<B::BiStream>>>,
    client_nonce: String,
    next_command_id: AtomicU64,
    hello: ClientHello,
    accepted: ServerHello,
    blobs: Option<BlobInbox>,
    accept_task: JoinHandle<()>,
    cancel: watch::Sender<bool>,
}

impl<B: MuxTransport> V3Client<B> {
    /// 连上一条已建立的多路复用连接并完成握手。
    ///
    /// **先 Control 后其它**：握手必须在第一条 Control 流上完成，服务端在此之前
    /// 会关掉任何业务流（这也是服务端拒绝未认证流的物理依据）。
    pub async fn connect(transport: B, hello: ClientHello) -> Result<Self, ClientError> {
        // 本地先自检版本/指纹：能省一次往返，也让"本端就不对"的错误立刻暴露。
        match verify_handshake(&hello, "", "") {
            ServerHello::Rejected(reject) => {
                return Err(ClientError::HandshakeRejected(reject.reason));
            }
            ServerHello::Accepted { .. } => {}
        }
        let mut stream = transport.open_bi().await?;
        write_frame(&mut stream, &StreamKind::Control).await?;
        write_frame(&mut stream, &hello).await?;
        let accepted: ServerHello = match read_frame(&mut stream).await {
            Ok(server_hello) => server_hello,
            // 服务端在写首帧前就关了流：握手被直接拒绝（无结构化答复）。
            Err(FrameIoError::Eof) => return Err(ClientError::StreamRejected),
            Err(error) => return Err(ClientError::Frame(error)),
        };
        if let ServerHello::Rejected(reject) = accepted {
            return Err(ClientError::HandshakeRejected(reject.reason));
        }

        let (cancel, cancel_rx) = watch::channel(false);
        let (blob_tx, blob_inbox) = BlobInbox::channel();
        let accept_transport = transport.clone();
        let accept_task = tokio::spawn(async move {
            blob::accept_pushes(accept_transport, blob_tx, cancel_rx).await;
        });

        Ok(Self {
            transport,
            control: Arc::new(Mutex::new(ControlChannel { stream })),
            client_nonce: uuid::Uuid::new_v4().to_string(),
            next_command_id: AtomicU64::new(0),
            hello,
            accepted,
            blobs: Some(blob_inbox),
            accept_task,
            cancel,
        })
    }

    /// 服务端握手回执（含 `server_instance_id` / `build_id`）。
    pub fn server_hello(&self) -> &ServerHello {
        &self.accepted
    }

    /// 本端身份（客户端实例标签 + 凭据）。
    pub fn hello(&self) -> &ClientHello {
        &self.hello
    }

    /// 传输后端句柄（守卫用来量多流隔离）。
    pub fn transport(&self) -> &B {
        &self.transport
    }

    /// 发一条请求，`command_id` 自动生成（跨进程唯一，**除非你确实要重放**——
    /// 重放请用 [`Self::call_idempotent`] 并复用同一个 id）。
    pub async fn call(&self, request: ControlRequest) -> Result<ControlResponse, ClientError> {
        let id = self.next_command_id.fetch_add(1, Ordering::SeqCst) + 1;
        let command_id = format!("{}-cmd-{id}", self.client_nonce);
        self.call_idempotent(&command_id, request).await
    }

    /// 用指定 `command_id` 发请求（断线重放返回**原回执**，绝不重新执行）。
    pub async fn call_idempotent(
        &self,
        command_id: &str,
        request: ControlRequest,
    ) -> Result<ControlResponse, ClientError> {
        let mut control = self.control.lock().await;
        let call = ControlCall {
            command_id: command_id.to_string(),
            request,
        };
        write_frame(&mut control.stream, &call).await?;
        let reply: ControlReply = read_frame(&mut control.stream).await?;
        if reply.command_id != command_id {
            return Err(ClientError::CommandIdMismatch {
                expected: command_id.to_string(),
                actual: reply.command_id,
            });
        }
        match reply.response {
            ControlResponse::Error(error) => Err(ClientError::from_reply(error)),
            response => Ok(response),
        }
    }

    /// 附着会话事件流（`since_seq` 支持重连尾追；`0` = 从回放尾起点开始）。
    pub async fn attach_session(
        &self,
        session_id: &SessionId,
        since_seq: u64,
    ) -> Result<SessionEventsStream<B::BiStream>, ClientError> {
        let mut stream = self.transport.open_bi().await?;
        let kind = StreamKind::SessionEvents {
            session_id: session_id.clone(),
            since_seq,
        };
        write_frame(&mut stream, &kind).await?;
        let attach: StreamAttach = match read_frame(&mut stream).await {
            Ok(attach) => attach,
            Err(FrameIoError::Eof) => return Err(ClientError::StreamRejected),
            Err(error) => return Err(ClientError::Frame(error)),
        };
        if attach.stream != kind {
            return Err(ClientError::Protocol(format!(
                "attach 回执的流身份不符：请求 {kind:?}，回执 {:?}",
                attach.stream
            )));
        }
        Ok(SessionEventsStream::new(stream, attach))
    }

    /// 附着 shell PTY 流（`from_offset` 支持断线续读）。
    pub async fn attach_shell(
        &self,
        shell_id: &str,
        from_offset: u64,
    ) -> Result<ShellOutputStream<B::BiStream>, ClientError> {
        let mut stream = self.transport.open_bi().await?;
        let kind = StreamKind::ShellPty {
            shell_id: shell_id.to_string(),
            from_offset,
        };
        write_frame(&mut stream, &kind).await?;
        let attach: StreamAttach = match read_frame(&mut stream).await {
            Ok(attach) => attach,
            Err(FrameIoError::Eof) => return Err(ClientError::StreamRejected),
            Err(error) => return Err(ClientError::Frame(error)),
        };
        if attach.stream != kind {
            return Err(ClientError::Protocol(format!(
                "shell attach 回执的流身份不符：请求 {kind:?}，回执 {:?}",
                attach.stream
            )));
        }
        Ok(ShellOutputStream::new(stream, attach, from_offset))
    }

    /// 向服务端上传一条属于指定会话的内容寻址 blob（分片直传，零 Base64 膨胀）。
    pub async fn upload_blob(
        &self,
        session_id: &SessionId,
        bytes: &[u8],
    ) -> Result<ContentHash, ClientError> {
        let digest: [u8; 32] = Sha256::digest(bytes).into();
        let hash = ContentHash::from_bytes(digest);
        let len = bytes.len() as u64;
        let mut stream = self.transport.open_bi().await?;
        let kind = StreamKind::BlobTransfer {
            role: BlobTransferRole::ClientUpload {
                session_id: session_id.clone(),
                hash,
                len,
            },
        };
        write_frame(&mut stream, &kind).await?;
        const CHUNK_BYTES: usize = 64 * 1024;
        if bytes.is_empty() {
            let frame = BlobChunkFrame {
                meta: FrameMeta {
                    seq: 1,
                    server_ts_ms: 0,
                    delivery: DeliveryClass::Reliable,
                },
                hash,
                offset: 0,
                bytes: bytes::Bytes::new(),
                last: true,
            };
            write_frame(&mut stream, &frame).await?;
        } else {
            let mut offset = 0usize;
            let mut seq = 0u64;
            while offset < bytes.len() {
                let end = (offset + CHUNK_BYTES).min(bytes.len());
                seq += 1;
                let frame = BlobChunkFrame {
                    meta: FrameMeta {
                        seq,
                        server_ts_ms: 0,
                        delivery: DeliveryClass::Reliable,
                    },
                    hash,
                    offset: offset as u64,
                    bytes: bytes::Bytes::copy_from_slice(&bytes[offset..end]),
                    last: end == bytes.len(),
                };
                write_frame(&mut stream, &frame).await?;
                offset = end;
            }
        }
        let ack: bool = match read_frame(&mut stream).await {
            Ok(ok) => ok,
            Err(FrameIoError::Eof) => return Err(ClientError::StreamRejected),
            Err(error) => return Err(ClientError::Frame(error)),
        };
        if !ack {
            return Err(ClientError::Protocol("服务端拒绝了 blob 上传".to_string()));
        }
        Ok(hash)
    }

    /// 按内容哈希从服务端拉取一条 blob（经 `sha256` Fail-Closed 校验）。
    pub async fn fetch_blob(&self, hash: &ContentHash) -> Result<bytes::Bytes, ClientError> {
        let mut stream = self.transport.open_bi().await?;
        let kind = StreamKind::BlobTransfer {
            role: BlobTransferRole::ClientFetch { hash: *hash },
        };
        write_frame(&mut stream, &kind).await?;
        blob::receive_fetched_blob(&mut stream, *hash).await
    }

    /// 取走 blob 收件箱（服务端推来的 `BlobTransfer` 流在这里被组装）。
    ///
    /// 取走而不是借用：收件箱通常要交给一个独立任务持续消费，而控制面调用
    /// 又需要 `&self`——两者同时持有 `&mut` 是不可能的，所以设计成一次性取出。
    pub fn take_blob_inbox(&mut self) -> Option<BlobInbox> {
        self.blobs.take()
    }
}

impl V3Client<YamuxTransport> {
    const DAEMON_START_TIMEOUT: Duration = Duration::from_secs(15);

    /// 发现本机 daemon、按需自启，并通过本地 IPC 完成 V3 握手。
    ///
    /// `launch_if_missing = false` 时，端点缺失或不可达会直接返回错误；不会派生进程。
    pub async fn connect_local(
        data_root: &Path,
        expected_build_id: &str,
        expected_channel: &str,
        client_instance_id: &str,
        launch_if_missing: bool,
    ) -> Result<Self, ClientError> {
        if client_instance_id.trim().is_empty() {
            return Err(ClientError::Protocol(
                "client_instance_id must be non-empty".to_string(),
            ));
        }
        let hello = ClientHello::new(
            client_instance_id,
            ClientKind::Desktop,
            ClientCredential::LocalOsUser {
                user: local_os_user()?,
            },
        );
        let deadline = tokio::time::Instant::now() + Self::DAEMON_START_TIMEOUT;
        let mut launched = false;

        loop {
            match resolve_daemon_endpoint(data_root, expected_channel, Some(expected_build_id)) {
                Ok(endpoint) => {
                    let path = endpoint.local_ipc.as_deref().ok_or_else(|| {
                        ClientError::Protocol("daemon.endpoint 未声明本地 IPC 端点".to_string())
                    })?;
                    match connect_local_ipc(Path::new(path)).await {
                        Ok(transport) => {
                            return match tokio::time::timeout_at(
                                deadline,
                                Self::connect(transport, hello.clone()),
                            )
                            .await
                            {
                                Ok(result) => result,
                                Err(_) => Err(ClientError::HandshakeTimeout),
                            };
                        }
                        Err(error) => {
                            if !launch_if_missing {
                                return Err(error.into());
                            }
                            if !launched {
                                launch_daemon(data_root)?;
                                launched = true;
                            }
                        }
                    }
                }
                Err(error) => {
                    let missing = matches!(
                        &error,
                        EndpointError::Rejected(EndpointReject::Unreadable(_))
                    );
                    if !launch_if_missing || !missing {
                        return Err(error.into());
                    }
                    if !launched {
                        launch_daemon(data_root)?;
                        launched = true;
                    }
                }
            }

            if tokio::time::Instant::now() >= deadline {
                return Err(ClientError::DaemonStartTimeout);
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }
}

fn local_os_user() -> Result<String, ClientError> {
    let variable = if cfg!(windows) { "USERNAME" } else { "USER" };
    std::env::var(variable)
        .ok()
        .map(|value| value.trim().to_string())
        .filter(|value| !value.is_empty())
        .ok_or_else(|| {
            ClientError::Protocol(format!("本地 OS 用户标识缺失：环境变量 {variable} 为空"))
        })
}

fn launch_daemon(data_root: &Path) -> Result<(), ClientError> {
    let executable = crate::discovery::daemon_executable()
        .map_err(|error| ClientError::DaemonLaunch(error.to_string()))?;
    crate::discovery::spawn_daemon_process(&executable, data_root)
        .map_err(|error| ClientError::DaemonLaunch(error.to_string()))
}

impl From<qaqh_ringing_v3::FrameError> for ClientError {
    fn from(error: qaqh_ringing_v3::FrameError) -> Self {
        ClientError::Frame(FrameIoError::Frame(error))
    }
}

impl From<std::io::Error> for ClientError {
    fn from(error: std::io::Error) -> Self {
        ClientError::Frame(FrameIoError::Io(error))
    }
}

impl<B: MuxTransport> Drop for V3Client<B> {
    fn drop(&mut self) {
        self.cancel.send_replace(true);
        self.accept_task.abort();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use qaqh_ringing_v3::{
        DAEMON_ENDPOINT_FILE, DaemonEndpoint, EndpointChannel, PROTOCOL_FINGERPRINT, PROTOCOL_REV,
        write_daemon_endpoint,
    };
    use qaqh_v3_transport::bind_local_ipc;
    use std::path::PathBuf;

    fn temp_dir(tag: &str) -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "qaqh-v3-connect-local-{tag}-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("temp dir");
        path
    }

    fn local_ipc_path(_dir: &Path, _tag: &str) -> PathBuf {
        #[cfg(windows)]
        {
            PathBuf::from(format!(
                r"\\.\pipe\qaqh-v3-client-{_tag}-{}",
                std::process::id()
            ))
        }
        #[cfg(not(windows))]
        {
            _dir.join("daemon.sock")
        }
    }

    fn endpoint(dir: &Path, build_id: &str, socket: &Path) {
        let endpoint = DaemonEndpoint {
            build_id: build_id.to_string(),
            channel: EndpointChannel("dev".to_string()),
            local_ipc: Some(socket.to_string_lossy().into_owned()),
            iroh: None,
            protocol_rev: PROTOCOL_REV,
            protocol_fingerprint: PROTOCOL_FINGERPRINT,
            written_at_ms: 1,
        };
        write_daemon_endpoint(dir, &endpoint).expect("publish endpoint");
    }

    #[tokio::test]
    async fn connect_local_discovers_endpoint_and_completes_handshake() {
        let dir = temp_dir("handshake");
        let socket = local_ipc_path(&dir, "handshake");
        endpoint(&dir, "build-1", &socket);
        let mut listener = bind_local_ipc(&socket).await.expect("bind");
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();

        let server = tokio::spawn(async move {
            let transport = listener.accept().await.expect("accept");
            let mut stream = transport.accept_bi().await.expect("control stream");
            let kind: StreamKind = read_frame(&mut stream).await.expect("stream kind");
            assert!(matches!(kind, StreamKind::Control));
            let hello: ClientHello = read_frame(&mut stream).await.expect("client hello");
            assert_eq!(hello.client_instance_id, "client-1");
            assert!(matches!(
                hello.credential,
                ClientCredential::LocalOsUser { .. }
            ));
            write_frame(
                &mut stream,
                &ServerHello::Accepted {
                    server_instance_id: "server-1".to_string(),
                    build_id: "build-1".to_string(),
                    protocol_rev: PROTOCOL_REV,
                },
            )
            .await
            .expect("server hello");
            let _ = release_rx.await;
        });

        let client_result =
            V3Client::connect_local(&dir, "build-1", "dev", "client-1", false).await;
        let _ = release_tx.send(());
        server.await.expect("server task");
        let client = client_result.expect("connect_local");
        match client.server_hello() {
            ServerHello::Accepted {
                server_instance_id, ..
            } => assert_eq!(server_instance_id, "server-1"),
            other => panic!("必须接受握手，实际 {other:?}"),
        }
    }

    #[tokio::test(start_paused = true)]
    async fn connect_local_times_out_when_server_never_completes_handshake() {
        let dir = temp_dir("handshake-timeout");
        let socket = local_ipc_path(&dir, "handshake-timeout");
        endpoint(&dir, "build-1", &socket);
        let mut listener = bind_local_ipc(&socket).await.expect("bind");
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let server = tokio::spawn(async move {
            let _transport = listener.accept().await.expect("accept");
            let _ = release_rx.await;
        });

        let result = V3Client::connect_local(&dir, "build-1", "dev", "client-1", false).await;
        let _ = release_tx.send(());
        server.await.expect("server task");
        assert!(matches!(result, Err(ClientError::HandshakeTimeout)));
    }

    #[tokio::test]
    async fn connect_local_without_launch_rejects_missing_endpoint() {
        let dir = temp_dir("missing");
        let error = match V3Client::connect_local(&dir, "build-1", "dev", "client-1", false).await {
            Ok(_) => panic!("missing endpoint must fail"),
            Err(error) => error,
        };
        assert!(matches!(error, ClientError::Endpoint(_)));
    }

    #[tokio::test]
    async fn connect_local_refuses_another_build_before_connecting() {
        let dir = temp_dir("wrong-build");
        endpoint(&dir, "old-build", &dir.join("daemon.sock"));
        let error = match V3Client::connect_local(&dir, "new-build", "dev", "client-1", false).await
        {
            Ok(_) => panic!("wrong build must fail"),
            Err(error) => error,
        };
        assert!(matches!(
            error,
            ClientError::Endpoint(EndpointError::BuildIdMismatch { .. })
        ));
        assert!(dir.join(DAEMON_ENDPOINT_FILE).exists());
    }
}
