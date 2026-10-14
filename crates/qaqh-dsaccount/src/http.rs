//! HTTP 薄封装（ureq，同步）。
//!
//! 为什么同步：授权流程与回环监听跑在**专用 std 线程**上，不进 tokio（既避开
//! 「阻塞 tokio worker」，也避开「runtime 内建 runtime」）。与 `qaqh-siwc::http`
//! 同构。
//!
//! 两个关键选择：
//!
//! - `http_status_as_error(false)`：4xx/5xx 要能读到 body——平台把业务结论放在
//!   信封里，压成一句「HTTP 错误」就丢掉了「到底是凭据失效还是余额不足」。
//! - **`max_redirects(0)`**：请求带账号凭据，重定向必须由**我们**判断（跨 origin
//!   跟随就等于把凭据转发给第三方）。3xx 会原样回来，由
//!   [`crate::error::classify_http_status`] 判成协议异常。

use std::time::Duration;

use serde_json::Value;

use crate::error::{DsCode, DsError};

/// 单次 HTTP 结果。
#[derive(Debug, Clone)]
pub struct HttpResponse {
    /// HTTP 状态码。
    pub status: u16,
    /// 原始响应体。**不进日志**（`users/current` 会回显 token，见 crate 文档不变量 1）。
    pub body: String,
}

/// 诊断文本的取值路径，按优先级排列：平台信封 → OpenAI 风格错误 → 裸 `message`。
const DIAGNOSTIC_PATHS: [&[&str]; 4] = [
    &["data", "biz_msg"],
    &["msg"],
    &["error", "message"],
    &["message"],
];

fn dig<'a>(value: &'a Value, path: &[&str]) -> Option<&'a Value> {
    let mut cursor = value;
    for key in path {
        cursor = cursor.get(*key)?;
    }
    Some(cursor)
}

impl HttpResponse {
    /// 2xx。
    pub fn is_success(&self) -> bool {
        (200..300).contains(&self.status)
    }

    /// 可展示的诊断文本。
    ///
    /// **只从结构化的文案字段取**，取不到就返回一句固定说明——绝不回正文片段：
    /// 平台会在某些响应的 `biz_data` 里回显凭据，任何「打印 body 前 200 字」的
    /// 偷懒做法都会把 token 送进日志。
    pub fn diagnostic(&self) -> String {
        if let Ok(value) = serde_json::from_str::<Value>(&self.body) {
            for path in DIAGNOSTIC_PATHS {
                if let Some(text) = dig(&value, path)
                    .and_then(Value::as_str)
                    .filter(|text| !text.is_empty())
                {
                    return text.to_string();
                }
            }
        }
        format!("HTTP {}（响应体已省略：可能含凭据回显）", self.status)
    }

    /// 信封里的业务码（`data.biz_code`，退化读顶层 `code`）。
    ///
    /// **非 2xx 的响应体同样要读**：平台把「凭据无效」放在 body 里（实测形状是
    /// `400` + `code: 40003`），只看 HTTP 状态码会把它误判成「请求形状被拒」——
    /// 于是「凭据已死」表现成「要改代码」。参照实现同样先解析 body 再判。
    pub fn business_code(&self) -> Option<i64> {
        let value: Value = serde_json::from_str(&self.body).ok()?;
        dig(&value, &["data", "biz_code"])
            .and_then(Value::as_i64)
            .or_else(|| value.get("code").and_then(Value::as_i64))
    }
}

/// 带超时的同步 HTTP 客户端。
#[derive(Debug, Clone)]
pub struct Http {
    agent: ureq::Agent,
}

impl Http {
    /// `timeout` 是**整次请求**的预算（连接 + 读写）。
    pub fn new(timeout: Duration) -> Self {
        let agent: ureq::Agent = ureq::Agent::config_builder()
            .timeout_global(Some(timeout))
            .http_status_as_error(false)
            .max_redirects(0)
            .user_agent(concat!("qaqh-dsaccount/", env!("CARGO_PKG_VERSION")))
            .build()
            .into();
        Self { agent }
    }

    /// GET（`headers` 用于账号头与客户端身份头）。
    pub fn get(&self, url: &str, headers: &[(&str, &str)]) -> Result<HttpResponse, DsError> {
        let mut request = self.agent.get(url).header("accept", "application/json");
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        self.finish(request.call())
    }

    /// JSON POST。
    pub fn post_json(
        &self,
        url: &str,
        headers: &[(&str, &str)],
        body: &Value,
    ) -> Result<HttpResponse, DsError> {
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

    /// **无正文、无 `content-type`** 的 POST。
    ///
    /// 登出就是这个形状：参照实现只发身份头 + `x-dsh-auth-token`，没有正文。实测
    /// （2026-10-14）带 `content-type: application/json` + `{}` 的写法被平台以
    /// `400` + `Authorization Failed (invalid token)` 拒绝——而**同一个凭据**同时
    /// 打得通 `users/current` 与余额接口，所以那不是"凭据坏了"，是请求形状。
    pub fn post_empty(&self, url: &str, headers: &[(&str, &str)]) -> Result<HttpResponse, DsError> {
        let mut request = self.agent.post(url);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        self.finish(request.send_empty())
    }

    fn finish(
        &self,
        result: Result<ureq::http::Response<ureq::Body>, ureq::Error>,
    ) -> Result<HttpResponse, DsError> {
        match result {
            Ok(mut response) => {
                let status = response.status().as_u16();
                let body = response.body_mut().read_to_string().map_err(|error| {
                    DsError::new(DsCode::Network, format!("读取响应失败: {error}"))
                })?;
                Ok(HttpResponse { status, body })
            }
            // 传输层失败（连不上、超时、TLS）：**可用性**问题，不是凭据问题。
            Err(error) => Err(DsError::new(DsCode::Network, format!("请求失败: {error}"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn diagnostic_prefers_the_envelope_text() {
        let response = HttpResponse {
            status: 400,
            body: r#"{"code":0,"msg":"","data":{"biz_code":10001,"biz_msg":"订单不存在"}}"#.into(),
        };
        assert_eq!(response.diagnostic(), "订单不存在");
        assert!(!response.is_success());
    }

    #[test]
    fn diagnostic_never_echoes_the_body() {
        // 实测事实：`GET /auth-api/v0/users/current` 的 biz_data 里回显了一个 token。
        // 诊断文本必须取不到它就退化成固定说明，而不是「打印前 200 字」。
        let response = HttpResponse {
            status: 200,
            body: r#"{"code":0,"msg":"","data":{"biz_code":0,"biz_msg":"","biz_data":{"id":"u1","token":"SECRET-TOKEN-VALUE"}}}"#.into(),
        };
        let text = response.diagnostic();
        assert!(
            !text.contains("SECRET-TOKEN-VALUE"),
            "诊断里出现了凭据: {text}"
        );
        assert!(text.contains("已省略"), "{text}");
    }

    #[test]
    fn diagnostic_falls_back_for_non_json_and_plain_messages() {
        let html = HttpResponse {
            status: 502,
            body: "<html>bad gateway</html>".into(),
        };
        assert!(
            !html.diagnostic().contains("bad gateway"),
            "非 JSON 正文同样不回显"
        );
        let openai = HttpResponse {
            status: 401,
            body: json!({"error": {"message": "Authentication Fails"}}).to_string(),
        };
        assert_eq!(openai.diagnostic(), "Authentication Fails");
    }

    #[test]
    fn transport_failures_are_availability_not_credential_failures() {
        // 连不上的端口：必须是 Network 且**不**要求重新登录。
        let http = Http::new(Duration::from_millis(500));
        let error = http
            .get("http://127.0.0.1:9/never", &[])
            .expect_err("连不上必须报错");
        assert_eq!(error.code(), DsCode::Network);
        assert!(!error.requires_reauth());
    }
}
