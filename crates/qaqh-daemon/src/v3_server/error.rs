//! V3 服务端错误词汇（复用 `qaqh_v3_transport::FrameIoError` 单源）。

use qaqh_ringing_v3::{ErrorCode, HandshakeRejectReason, RpcError};
pub(crate) use qaqh_v3_transport::FrameIoError;

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
    #[allow(dead_code)]
    #[error("握手被拒：{0:?}")]
    Handshake(HandshakeRejectReason),
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
