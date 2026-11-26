//! V3 服务端错误词汇（骨架期只要"能说清是什么坏了"，不堆砌业务分支）。

use qaqh_ringing_v3::{HandshakeRejectReason, RpcError, ErrorCode};

/// 服务端致命错误（连接级：一出即整条连接收摊）。
#[derive(Debug, thiserror::Error)]
pub(crate) enum ServerError {
    /// 多路复用传输层失败。
    #[error("传输层失败：{0}")]
    Mux(#[from] qaqh_v3_transport::MuxError),
    /// 帧读写失败。
    #[error("帧 I/O 失败：{0}")]
    Frame(#[from] FrameIoError),
    /// 握手被拒（连接随即关闭，不进入业务帧阶段）。
    #[error("握手被拒：{0:?}")]
    Handshake(HandshakeRejectReason),
}

/// 帧 I/O 错误（与协议层 [`qaqh_ringing_v3::FrameError`] 分开：一个是线路形状错，
/// 一个是"线路本身断了"）。
#[derive(Debug, thiserror::Error)]
pub(crate) enum FrameIoError {
    /// 对端正常关闭（读到 0 字节）。
    #[error("对端已关闭连接")]
    Eof,
    /// 帧形状非法（超限/截断/走私/解码失败）。
    #[error("帧形状非法：{0}")]
    Frame(#[from] qaqh_ringing_v3::FrameError),
    /// 底层 I/O 失败。
    #[error("底层 I/O 失败：{0}")]
    Io(#[from] std::io::Error),
}

impl FrameIoError {
    /// 是否只是"对端走了"（用来把正常断联与真错误区分开）。
    pub(crate) fn is_disconnect(&self) -> bool {
        matches!(
            self,
            Self::Eof | Self::Io(_) | Self::Frame(qaqh_ringing_v3::FrameError::TruncatedPrefix { .. })
        )
    }
}

/// 构造一条 RPC 错误回执。
pub(crate) fn rpc(code: ErrorCode, message: impl Into<String>) -> RpcError {
    RpcError {
        code,
        message: message.into(),
        // 只有"暂时不可用"与"限流"值得重试；其余重试只是重复制造同一个错误。
        retryable: matches!(code, ErrorCode::Unavailable | ErrorCode::RateLimited),
    }
}

/// 构造一条 `PermissionDenied`（身份/驱动权/作用域三类拒绝共用）。
pub(crate) fn denied(message: impl Into<String>) -> RpcError {
    rpc(ErrorCode::PermissionDenied, message)
}

/// 构造一条"该能力在本构建中尚未接线"的拒绝。
///
/// 码按计划选用 [`ErrorCode::Unavailable`]（"骨架期占位"），但 **`retryable=false`**：
/// 能力没接线不是"稍后就好"，重试只会得到同一个答案。`Unavailable` 的契约语义是
/// "暂时不可用（可重试）"，所以这两个字段必须成对出现才不会误导前端。
pub(crate) fn not_wired(message: impl Into<String>) -> RpcError {
    RpcError {
        code: ErrorCode::Unavailable,
        message: message.into(),
        retryable: false,
    }
}

/// 构造一条 `NotFound`。
pub(crate) fn not_found(message: impl Into<String>) -> RpcError {
    rpc(ErrorCode::NotFound, message)
}
