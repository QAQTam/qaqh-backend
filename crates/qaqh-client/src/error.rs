//! Error type shared across the client.

use serde::Deserialize;
use thiserror::Error;

use crate::types::ReconnectReason;

#[derive(Debug, Error)]
pub enum ClientError {
    #[error("discovery error: {0}")]
    Discovery(String),

    #[error("negotiation error: {0}")]
    Negotiation(String),

    #[error("transport error: {0}")]
    Transport(String),

    /// 服务端以 `ringing.stream_terminated` 主动终止流。保留 code/skipped，
    /// 避免重连状态只能从 Display 文本里反推终止原因。
    #[error("server terminated stream ({code}); reconnecting")]
    StreamTerminated { code: String, skipped: Option<u64> },

    #[error("protocol violation: {0}")]
    Protocol(String),

    /// Timeline SSE journal no longer covers the client cursor. The stream
    /// recovers by re-fetching the authoritative snapshot and advancing the
    /// cursor to its watermark (mirrors TS `TimelineGapError`).
    #[error("timeline SSE gap: expected seq {expected}, received {received}")]
    TimelineGap { expected: u64, received: u64 },

    #[error("HTTP {status}: {path}")]
    Http { status: u16, path: String },

    /// Structured API error with a stable server-side `code`.
    #[error("server error {status} ({code}): {message}")]
    Api {
        status: u16,
        code: String,
        message: String,
    },

    #[error("io error: {0}")]
    Io(#[from] std::io::Error),

    #[error("reqwest error: {0}")]
    Reqwest(#[from] reqwest::Error),

    #[error("json error: {0}")]
    Json(#[from] serde_json::Error),
}

impl ClientError {
    pub(crate) fn stream_terminated(data: &str) -> Self {
        #[derive(Deserialize)]
        struct TerminatedPayload {
            code: Option<String>,
            skipped: Option<u64>,
        }

        let payload =
            serde_json::from_str::<TerminatedPayload>(data).unwrap_or(TerminatedPayload {
                code: None,
                skipped: None,
            });
        Self::StreamTerminated {
            code: payload.code.unwrap_or_else(|| "unknown".into()),
            skipped: payload.skipped,
        }
    }

    /// Stable server error code, when the response carried one.
    pub fn code(&self) -> Option<&str> {
        match self {
            Self::Api { code, .. } => Some(code),
            _ => None,
        }
    }

    /// 会话尚未物化：canonical 目录 / commit marker 还不存在
    /// （`session_not_found` / `snapshot_missing`）。新建会话在首个 canonical
    /// 事实落盘前就是这个状态——调用方应**短退避**重试而不是指数退避。
    pub fn is_session_not_ready(&self) -> bool {
        match self {
            Self::Http { status, .. } => *status == 404 || *status == 409,
            Self::Api { status, code, .. } => {
                *status == 404
                    || *status == 409
                    || code == "session_not_found"
                    || code == "snapshot_missing"
            }
            _ => false,
        }
    }

    /// 返回服务端终止流的结构化原因；普通传输/协议错误返回 `None`。
    pub fn reconnect_reason(&self) -> Option<ReconnectReason> {
        match self {
            Self::StreamTerminated { code, skipped } if code == "lagged" => Some(match skipped {
                Some(skipped) => ReconnectReason::Lagged { skipped: *skipped },
                None => ReconnectReason::StreamTerminated { code: code.clone() },
            }),
            Self::StreamTerminated { code, .. } => {
                Some(ReconnectReason::StreamTerminated { code: code.clone() })
            }
            _ => None,
        }
    }
}

pub type Result<T> = std::result::Result<T, ClientError>;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lagged_without_skipped_stays_structured() {
        let error = ClientError::stream_terminated(r#"{"code":"lagged"}"#);
        assert_eq!(
            error.reconnect_reason(),
            Some(ReconnectReason::StreamTerminated {
                code: "lagged".into()
            })
        );
    }
}
