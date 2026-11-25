//! 同步 HTTP 的 **siwc 产品层**（v4.5 P2-1b）。
//!
//! ureq 超时封装（`timeout_global` + `http_status_as_error(false)`、body 读取、
//! request id 提取）已抽进 [`qaqh_oauth_core::http`]（与 `qaqh-dsaccount` 的
//! 同构拷贝合一）；本层只剩 siwc 的产品语义：`Bearer` 注入、错误码映射
//! （一切传输失败记 `Transport`）与 provider 错误形状解读（`error.code` /
//! `error.message` / `detail`——直连路由可能返回 `{"detail":...}`）。
//!
//! 为什么同步：OAuth 流程与回环监听跑在**专用 std 线程**上，不进 tokio。

use std::time::Duration;

use qaqh_oauth_core::http::{HttpAgent, HttpFailureKind, RawResponse};
use serde_json::Value;

use crate::error::{SiwcCode, SiwcError};

/// 单次 HTTP 结果（状态、原文、request id）。
#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub body: String,
    pub request_id: Option<String>,
}

impl HttpResponse {
    fn from_raw(raw: RawResponse) -> Self {
        Self {
            status: raw.status,
            body: raw.body,
            request_id: raw.request_id,
        }
    }

    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// 解析 JSON；非 JSON 体转成可读诊断（文档：直连路由可能返回 `{"detail":...}`）。
    pub fn json(&self) -> Result<Value, SiwcError> {
        serde_json::from_str(&self.body).map_err(|e| {
            SiwcError::new(
                SiwcCode::Transport,
                format!("响应非 JSON（HTTP {}）: {e}", self.status),
            )
            .with_request_id(self.request_id.clone())
        })
    }

    /// `error.code`（Responses 层错误对象）。
    pub fn error_code(&self) -> Option<String> {
        let value: Value = serde_json::from_str(&self.body).ok()?;
        value
            .get("error")
            .and_then(|error| error.get("code"))
            .and_then(Value::as_str)
            .or_else(|| value.get("code").and_then(Value::as_str))
            .map(str::to_owned)
    }

    /// 诊断文本：优先 `error.message` / `detail`，其次裸体。
    pub fn error_message(&self) -> String {
        if let Ok(value) = serde_json::from_str::<Value>(&self.body) {
            for path in [["error", "message"], ["detail", ""], ["message", ""]] {
                let target = if path[1].is_empty() {
                    value.get(path[0])
                } else {
                    value.get(path[0]).and_then(|v| v.get(path[1]))
                };
                if let Some(text) = target.and_then(Value::as_str) {
                    return text.to_owned();
                }
            }
        }
        self.body.chars().take(400).collect()
    }
}

/// 带超时的同步 HTTP 客户端（核心层薄产品壳：Bearer 注入 + 错误码映射）。
#[derive(Debug, Clone)]
pub struct Http {
    agent: HttpAgent,
}

impl Http {
    pub fn new(timeout: Duration) -> Self {
        Self {
            agent: HttpAgent::new(
                timeout,
                concat!("qaqh-siwc/", env!("CARGO_PKG_VERSION")),
                false,
            ),
        }
    }

    pub fn get(&self, url: &str, bearer: Option<&str>) -> Result<HttpResponse, SiwcError> {
        let bearer_header = format!("Bearer {}", bearer.unwrap_or_default());
        let mut headers: Vec<(&str, &str)> = Vec::new();
        if bearer.is_some() {
            headers.push(("authorization", bearer_header.as_str()));
        }
        self.map(self.agent.get(url, &headers))
    }

    /// form-urlencoded POST（OAuth token / revocation 端点的唯一形态）。
    pub fn post_form(&self, url: &str, form: &[(&str, &str)]) -> Result<HttpResponse, SiwcError> {
        self.map(self.agent.post_form(url, form))
    }

    fn map(
        &self,
        result: Result<RawResponse, qaqh_oauth_core::http::HttpFailure>,
    ) -> Result<HttpResponse, SiwcError> {
        result
            .map(HttpResponse::from_raw)
            .map_err(|failure| match failure.kind {
                HttpFailureKind::Transport | HttpFailureKind::BodyRead => {
                    // 传输层与读体失败都是**可用性**问题（旧拷贝同为 Transport）。
                    SiwcError::new(SiwcCode::Transport, failure.message)
                }
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn http_response_helpers_read_provider_error_shape() {
        let response = HttpResponse {
            status: 429,
            body: r#"{"error":{"code":"subscription_sharing_usage_limit_exceeded","message":"limit"}}"#
                .to_string(),
            request_id: Some("req_1".into()),
        };
        assert!(!response.is_success());
        assert_eq!(
            response.error_code().as_deref(),
            Some("subscription_sharing_usage_limit_exceeded")
        );
        assert_eq!(response.error_message(), "limit");
    }

    #[test]
    fn http_response_helpers_tolerate_non_json_bodies() {
        let response = HttpResponse {
            status: 503,
            body: "<html>gateway</html>".to_string(),
            request_id: None,
        };
        assert!(response.error_code().is_none());
        assert!(response.json().is_err());
        assert!(response.error_message().contains("gateway"));
    }

    #[test]
    fn detail_bodies_are_surfaced_as_diagnostics() {
        // 文档：直连路由准入失败可能只给 `{"detail": "..."}`，没有 error 对象。
        let response = HttpResponse {
            status: 403,
            body: r#"{"detail":"region not permitted"}"#.into(),
            request_id: None,
        };
        assert_eq!(response.error_message(), "region not permitted");
        assert!(response.error_code().is_none());
    }
}
