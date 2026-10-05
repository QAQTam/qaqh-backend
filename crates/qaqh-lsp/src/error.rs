//! LSP 错误模型（mcp error.rs 同款风格：timeis/status/code/message/hint 的
//! `ToolResult::error` JSON 由 tool.rs 统一渲染；本模块只定 kind + 构造）。

/// LSP 错误码（ToolResult JSON 的 `code` 字段）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum LspErrorKind {
    Disabled,
    NotFound,
    ConnectFailed,
    ConnectTimeout,
    ServerCrashed,
    Timeout,
    Protocol,
    Cancelled,
    Shutdown,
}

impl LspErrorKind {
    pub fn code(self) -> &'static str {
        match self {
            LspErrorKind::Disabled => "lsp_disabled",
            LspErrorKind::NotFound => "lsp_not_found",
            LspErrorKind::ConnectFailed => "lsp_connect_failed",
            LspErrorKind::ConnectTimeout => "lsp_connect_timeout",
            LspErrorKind::ServerCrashed => "lsp_server_crashed",
            LspErrorKind::Timeout => "lsp_timeout",
            LspErrorKind::Protocol => "lsp_protocol_error",
            LspErrorKind::Cancelled => "lsp_cancelled",
            LspErrorKind::Shutdown => "lsp_shutdown",
        }
    }
}

/// LSP 错误（kind + 人类可读 message；值永不进 message——secret 只在子进程 env）。
#[derive(Debug, Clone)]
pub struct LspError {
    pub kind: LspErrorKind,
    pub message: String,
}

impl LspError {
    pub fn new(kind: LspErrorKind, message: String) -> Self {
        Self { kind, message }
    }
}

impl std::fmt::Display for LspError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "[{}] {}", self.kind.code(), self.message)
    }
}

impl std::error::Error for LspError {}

impl From<async_lsp::Error> for LspError {
    /// async-lsp 通道/协议错误 → LSP 错误码映射：
    /// - ServiceStopped/Eof/Io → ServerCrashed（连接已死，下次调用重连）；
    /// - Response（server 回的 JSON-RPC 错误）→ Protocol（透传 message）；
    /// - 其余 → Protocol。
    fn from(e: async_lsp::Error) -> Self {
        match &e {
            async_lsp::Error::ServiceStopped | async_lsp::Error::Eof | async_lsp::Error::Io(_) => {
                LspError::new(
                    LspErrorKind::ServerCrashed,
                    format!("lsp connection lost: {e}"),
                )
            }
            async_lsp::Error::Response(resp) => {
                LspError::new(LspErrorKind::Protocol, format!("lsp server error: {resp}"))
            }
            _ => LspError::new(LspErrorKind::Protocol, format!("lsp protocol error: {e}")),
        }
    }
}
