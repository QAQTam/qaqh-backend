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
mod framing;
mod session_events;
mod shell_output;

pub use blob::{AssembledBlob, BlobInbox};
pub use endpoint::{EndpointError, resolve_daemon_endpoint};
pub use framing::FrameIoError;
pub use session_events::{SessionEventsStream, SessionStreamStats};
pub use shell_output::{ShellOutputStream, ShellStreamStats};

use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use qaqh_core_vocab::SessionId;
use qaqh_ringing_v3::{
    ClientHello, ControlCall, ControlReply, ControlRequest, ControlResponse, ErrorCode, RpcError,
    ServerHello, StreamAttach, StreamKind, verify_handshake,
};
use qaqh_v3_transport::{MuxError, MuxTransport};
use tokio::sync::{Mutex, watch};
use tokio::task::JoinHandle;

use framing::{read_frame, write_frame};

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

/// 进程内客户端序号：`command_id` 里必须带上它。
///
/// 只靠 `client_instance_id` 生成 `command_id` 会让"两个客户端用了同一个实例名"
/// 直接撞进服务端的幂等表——后者的回执会拿到前者的结果（串话）。实例名是自报
/// 标签，不能承担唯一性；进程内唯一序号才能。
static NEXT_CLIENT_NONCE: AtomicU64 = AtomicU64::new(0);

/// V3 客户端句柄。
pub struct V3Client<B: MuxTransport> {
    transport: B,
    control: Arc<Mutex<ControlChannel<B::BiStream>>>,
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

    /// 发一条请求，`command_id` 自动生成（**除非你确实要重放**——重放请用
    /// [`Self::call_idempotent`] 并复用同一个 id）。
    pub async fn call(&self, request: ControlRequest) -> Result<ControlResponse, ClientError> {
        let id = self.next_command_id.fetch_add(1, Ordering::SeqCst) + 1;
        let nonce = NEXT_CLIENT_NONCE.fetch_add(1, Ordering::SeqCst);
        let command_id = format!("{}-{nonce:04}-cmd-{id}", self.hello.client_instance_id);
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
        Ok(ShellOutputStream::new(stream, attach))
    }

    /// 取走 blob 收件箱（服务端推来的 `BlobTransfer` 流在这里被组装）。
    ///
    /// 取走而不是借用：收件箱通常要交给一个独立任务持续消费，而控制面调用
    /// 又需要 `&self`——两者同时持有 `&mut` 是不可能的，所以设计成一次性取出。
    pub fn take_blob_inbox(&mut self) -> Option<BlobInbox> {
        self.blobs.take()
    }
}

impl<B: MuxTransport> Drop for V3Client<B> {
    fn drop(&mut self) {
        self.cancel.send_replace(true);
        self.accept_task.abort();
    }
}
