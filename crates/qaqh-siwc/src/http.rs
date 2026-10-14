//! HTTP 薄封装（ureq，同步）。
//!
//! 为什么同步：OAuth 流程与回环监听跑在**专用 std 线程**上，不进 tokio。
//! 这既避开「tokio worker 被阻塞」，也避开「runtime 内建 runtime」的事故面。
//!
//! 关键选择：`http_status_as_error(false)`。文档要求保留真实 HTTP 状态、
//! 响应体形状、`error.code` 与 request id —— 4xx/5xx 必须能读到 body，
//! 否则限流/不适用这些**产品状态**会被压成一句「HTTP 错误」。

use std::time::Duration;

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
            for path in [
                ["error", "message"],
                ["detail", ""],
                ["message", ""],
            ] {
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

/// 带超时的同步 HTTP 客户端。
#[derive(Debug, Clone)]
pub struct Http {
    agent: ureq::Agent,
}

impl Http {
    pub fn new(timeout: Duration) -> Self {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .http_status_as_error(false)
            .user_agent(concat!("qaqh-siwc/", env!("CARGO_PKG_VERSION")))
            .build()
            .into();
        Self { agent }
    }

    pub fn get(&self, url: &str, bearer: Option<&str>) -> Result<HttpResponse, SiwcError> {
        let mut request = self.agent.get(url).header("accept", "application/json");
        if let Some(token) = bearer {
            request = request.header("authorization", format!("Bearer {token}"));
        }
        self.finish(request.call())
    }

    /// form-urlencoded POST（OAuth token / revocation 端点的唯一形态）。
    pub fn post_form(
        &self,
        url: &str,
        form: &[(&str, &str)],
    ) -> Result<HttpResponse, SiwcError> {
        let request = self
            .agent
            .post(url)
            .header("accept", "application/json")
            .header("content-type", "application/x-www-form-urlencoded");
        self.finish(request.send_form(form.iter().copied()))
    }

    fn finish(&self, result: Result<ureq::http::Response<ureq::Body>, ureq::Error>) -> Result<HttpResponse, SiwcError> {
        match result {
            Ok(mut response) => {
                let status = response.status().as_u16();
                let request_id = ["x-request-id", "openai-request-id"]
                    .iter()
                    .find_map(|name| {
                        response
                            .headers()
                            .get(*name)
                            .and_then(|v| v.to_str().ok())
                            .map(str::to_owned)
                    });
                let body = response
                    .body_mut()
                    .read_to_string()
                    .map_err(|e| SiwcError::new(SiwcCode::Transport, format!("读取响应失败: {e}")))?;
                Ok(HttpResponse {
                    status,
                    body,
                    request_id,
                })
            }
            Err(error) => Err(SiwcError::new(
                SiwcCode::Transport,
                format!("请求失败: {error}"),
            )),
        }
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
