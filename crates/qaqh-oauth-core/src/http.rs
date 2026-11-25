//! 同步 HTTP：ureq 客户端的超时封装（provider 无关的机械层）。
//!
//! 为什么同步：OAuth 流程与回环监听跑在**专用 std 线程**上，不进 tokio
//! （既避开「tokio worker 被阻塞」，也避开「runtime 内建 runtime」的事故面）。
//!
//! 关键选择（两个 provider crate 的旧拷贝在此合一）：
//! - `timeout_global`：整次请求（连接 + 读写）的预算；
//! - `http_status_as_error(false)`：4xx/5xx 必须能读到 body——产品状态
//!   （限流/凭据失效/区域拒绝）都写在响应体里，压成一句「HTTP 错误」就
//!   丢掉了「到底是凭据失效还是余额不足」；
//! - 是否 `max_redirects(0)` 由调用方决定：带凭据的请求，跨 origin 跟随
//!   重定向等于把凭据转发给第三方（dsaccount 关，siwc 维持其既有形态）。

use std::time::Duration;

use serde_json::Value;

/// 单次 HTTP 结果（状态、原文、request id）。
#[derive(Debug, Clone)]
pub struct RawResponse {
    pub status: u16,
    /// 原始响应体。**是否可进日志由产品层判断**（某些端点会回显凭据）。
    pub body: String,
    /// 诊断用 request id（`x-request-id` / `openai-request-id`，取到为止）。
    pub request_id: Option<String>,
}

/// 传输层失败。两类都属**可用性**问题而非凭据结论——映射到什么错误码
/// 由 provider crate 决定（旧语义：siwc `Transport`、dsaccount `Network`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HttpFailure {
    pub kind: HttpFailureKind,
    pub message: String,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HttpFailureKind {
    /// 连不上、超时、TLS。
    Transport,
    /// 状态与头都在，读正文失败。
    BodyRead,
}

/// 带超时的同步 HTTP 客户端。
#[derive(Debug, Clone)]
pub struct HttpAgent {
    agent: ureq::Agent,
}

impl HttpAgent {
    /// `deny_redirects = true` 时 3xx 原样返回（调用方自行判断）。
    pub fn new(timeout: Duration, user_agent: &str, deny_redirects: bool) -> Self {
        let mut builder = ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .http_status_as_error(false)
            .user_agent(user_agent);
        if deny_redirects {
            builder = builder.max_redirects(0);
        }
        Self {
            agent: builder.build().into(),
        }
    }

    /// GET（`headers` 为额外请求头；accept: application/json 恒带）。
    pub fn get(
        &self,
        url: &str,
        headers: &[(&str, &str)],
    ) -> Result<RawResponse, HttpFailure> {
        let mut request = self.agent.get(url).header("accept", "application/json");
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        self.finish(request.call())
    }

    /// form-urlencoded POST（OAuth token / revocation 端点的唯一形态）。
    pub fn post_form(&self, url: &str, form: &[(&str, &str)]) -> Result<RawResponse, HttpFailure> {
        let request = self
            .agent
            .post(url)
            .header("accept", "application/json")
            .header("content-type", "application/x-www-form-urlencoded");
        self.finish(request.send_form(form.iter().copied()))
    }

    /// JSON POST。
    pub fn post_json(
        &self,
        url: &str,
        headers: &[(&str, &str)],
        body: &Value,
    ) -> Result<RawResponse, HttpFailure> {
        let mut request = self
            .agent
            .post(url)
            .header("accept", "application/json")
            .header("content-type", "application/json");
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        self.finish(request.send_json(body))
    }

    /// **无正文、无 `content-type`** 的 POST（某些平台的登出端点只认这个形状）。
    pub fn post_empty(&self, url: &str, headers: &[(&str, &str)]) -> Result<RawResponse, HttpFailure> {
        let mut request = self.agent.post(url);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        self.finish(request.send_empty())
    }

    fn finish(
        &self,
        result: Result<ureq::http::Response<ureq::Body>, ureq::Error>,
    ) -> Result<RawResponse, HttpFailure> {
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
                let body = response.body_mut().read_to_string().map_err(|e| HttpFailure {
                    kind: HttpFailureKind::BodyRead,
                    message: format!("读取响应失败: {e}"),
                })?;
                Ok(RawResponse {
                    status,
                    body,
                    request_id,
                })
            }
            // 传输层失败（连不上、超时、TLS）：**可用性**问题，不是凭据问题。
            Err(error) => Err(HttpFailure {
                kind: HttpFailureKind::Transport,
                message: format!("请求失败: {error}"),
            }),
        }
    }
}
