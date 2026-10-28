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

    /// 服务面方法不在 [`crate::service::SERVICE_METHODS`] 白名单内。
    ///
    /// 这是**本地拒绝**：一个请求都没发出去。与 `Api`（daemon 报错，带服务端
    /// `code`）在类型上就分得开——调用方不需要去嗅探错误文案或 HTTP 状态码。
    #[error("service method not allowed by the client whitelist: {method}")]
    ServiceMethodNotAllowed { method: String },

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

    /// 服务端已无法从 journal 补齐本客户端 cursor 之后的帧，必须重取快照
    /// 把 cursor 重基线到 watermark（B1.3 遗留契约）。
    ///
    /// `TimelineGap` 是客户端 seq 连续性自检的结果；`StreamTerminated(code
    /// == "journal_gap")` 是 daemon 对「空回放 + watermark 落后」的显式
    /// reset。两者同语义：不做快照重基线的话，客户端会带着旧 cursor 重连，
    /// 再次命中同一个 gap（journal_gap 甚至会无限循环）。
    pub fn needs_gap_recovery(&self) -> bool {
        match self {
            Self::TimelineGap { .. } => true,
            Self::StreamTerminated { code, .. } => code == "journal_gap",
            _ => false,
        }
    }

    /// 是否被客户端白名单拒掉（请求未发出，不是 daemon 的答复）。
    pub fn is_method_not_allowed(&self) -> bool {
        matches!(self, Self::ServiceMethodNotAllowed { .. })
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

    #[test]
    fn only_journal_gap_termination_requires_snapshot_rebaseline() {
        // 客户端自检的 seq 断层 → 重基线。
        assert!(
            ClientError::TimelineGap {
                expected: 4,
                received: 9
            }
            .needs_gap_recovery()
        );
        // daemon 显式 journal_gap reset → 重基线。
        assert!(ClientError::stream_terminated(r#"{"code":"journal_gap"}"#).needs_gap_recovery());
        // lagged / transport 等仍走 journal replay 重连，不重基线。
        assert!(!ClientError::stream_terminated(r#"{"code":"lagged"}"#).needs_gap_recovery());
        assert!(!ClientError::Transport("idle".into()).needs_gap_recovery());
    }
}
