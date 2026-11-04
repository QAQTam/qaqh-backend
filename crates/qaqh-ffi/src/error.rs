//! FFI 错误面：把 `qaqh_client::ClientError` 收敛成 Kotlin 可判别的窄枚举。
//!
//! 判别需求来自旧 Android 层的连接策略（HANDOFF.md）：`Api.code` 驱动
//! `lease_required` / `session_not_found` / `insufficient_scope` 等 1s 短退避，
//! `Http.status` 驱动 401 撤销 / 404/409 判定——所以 Api 与 Http 保持独立变体。

use qaqh_client::ClientError;

#[derive(Debug, thiserror::Error, uniffi::Error)]
pub enum CoreError {
    #[error("discovery: {detail}")]
    Discovery { detail: String },
    #[error("negotiation: {detail}")]
    Negotiation { detail: String },
    #[error("transport: {detail}")]
    Transport { detail: String },
    #[error("protocol: {detail}")]
    Protocol { detail: String },
    #[error("HTTP {status} on {path}")]
    Http { status: u16, path: String },
    #[error("api {code}: {detail} (HTTP {status})")]
    Api { status: u16, code: String, detail: String },
}

impl From<ClientError> for CoreError {
    fn from(error: ClientError) -> Self {
        match error {
            ClientError::Discovery(detail) => Self::Discovery { detail },
            ClientError::Negotiation(detail) => Self::Negotiation { detail },
            ClientError::Transport(detail) => Self::Transport { detail },
            ClientError::StreamTerminated { code, skipped } => Self::Transport {
                detail: format!("stream terminated: {code} (skipped {skipped:?})"),
            },
            ClientError::Protocol(detail) => Self::Protocol { detail },
            ClientError::Http { status, path } => Self::Http { status, path },
            ClientError::Api {
                status,
                code,
                message,
            } => Self::Api {
                status,
                code,
                detail: message,
            },
            ClientError::ServiceMethodNotAllowed { method } => Self::Protocol {
                detail: format!("service method not allowed: {method}"),
            },
            ClientError::Io(error) => Self::Transport {
                detail: format!("io: {error}"),
            },
            ClientError::Reqwest(error) => Self::Transport {
                detail: format!("http: {error}"),
            },
            ClientError::Json(error) => Self::Protocol {
                detail: format!("json: {error}"),
            },
        }
    }
}
