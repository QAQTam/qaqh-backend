//! FFI 错误面：把 `qaqh_client::ClientError` 收敛成 Kotlin 可判别的窄枚举。
//!
//! 判别需求来自旧 Android 层的连接策略（HANDOFF.md）：`Api.code` 驱动
//! `lease_required` / `session_not_found` / `insufficient_scope` 等 1s 短退避，
//! `Http.status` 驱动 401 撤销 / 404/409 判定——所以 Api 与 Http 保持独立变体。

use qaqh_client::ClientError;

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum CoreError {
    #[error("discovery: {message}")]
    Discovery { message: String },
    #[error("negotiation: {message}")]
    Negotiation { message: String },
    #[error("transport: {message}")]
    Transport { message: String },
    #[error("protocol: {message}")]
    Protocol { message: String },
    #[error("HTTP {status} on {path}")]
    Http { status: u16, path: String },
    #[error("api {code}: {message} (HTTP {status})")]
    Api { status: u16, code: String, message: String },
}

impl From<ClientError> for CoreError {
    fn from(error: ClientError) -> Self {
        match error {
            ClientError::Discovery(message) => Self::Discovery { message },
            ClientError::Negotiation(message) => Self::Negotiation { message },
            ClientError::Transport(message) => Self::Transport { message },
            ClientError::StreamTerminated { code, skipped } => Self::Transport {
                message: format!("stream terminated: {code} (skipped {skipped:?})"),
            },
            ClientError::Protocol(message) => Self::Protocol { message },
            ClientError::Http { status, path } => Self::Http { status, path },
            ClientError::Api {
                status,
                code,
                message,
            } => Self::Api {
                status,
                code,
                message,
            },
            ClientError::ServiceMethodNotAllowed { method } => Self::Protocol {
                message: format!("service method not allowed: {method}"),
            },
            ClientError::Io(error) => Self::Transport {
                message: format!("io: {error}"),
            },
            ClientError::Reqwest(error) => Self::Transport {
                message: format!("http: {error}"),
            },
            ClientError::Json(error) => Self::Protocol {
                message: format!("json: {error}"),
            },
        }
    }
}
