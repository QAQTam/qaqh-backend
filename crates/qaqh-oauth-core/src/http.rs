//! HTTP 机械层：同步外观，异步 `reqwest` 传输。
//!
//! OAuth 流程可能跑在专用 std 线程，也可能由 Tokio `spawn_blocking` 调用；
//! 两者都不能直接使用 `reqwest::blocking`。`run_async` 在 ambient runtime
//! 存在时用 `block_in_place + Handle::block_on`，否则临时建 current-thread
//! runtime，确保调用线程语义稳定。

use std::future::Future;
use std::time::Duration;

use serde_json::Value;
use tokio::runtime::Handle;

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
    client: reqwest::Client,
}

impl HttpAgent {
    /// `deny_redirects = true` 时 3xx 原样返回（调用方自行判断）。
    pub fn new(timeout: Duration, user_agent: &str, deny_redirects: bool) -> Self {
        let mut builder = reqwest::Client::builder()
            .timeout(timeout)
            .user_agent(user_agent);
        if deny_redirects {
            builder = builder.redirect(reqwest::redirect::Policy::none());
        }
        Self {
            client: builder.build().expect("reqwest client must build"),
        }
    }

    /// GET（`headers` 为额外请求头；accept: application/json 恒带）。
    pub fn get(&self, url: &str, headers: &[(&str, &str)]) -> Result<RawResponse, HttpFailure> {
        let mut request = self.client.get(url).header("accept", "application/json");
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        Self::run_async(Self::execute(request))?
    }

    /// form-urlencoded POST（OAuth token / revocation 端点的唯一形态）。
    pub fn post_form(&self, url: &str, form: &[(&str, &str)]) -> Result<RawResponse, HttpFailure> {
        let mut serializer = url::form_urlencoded::Serializer::new(String::new());
        for (name, value) in form {
            serializer.append_pair(name, value);
        }
        let request = self
            .client
            .post(url)
            .header("accept", "application/json")
            .header("content-type", "application/x-www-form-urlencoded")
            .body(serializer.finish());
        Self::run_async(Self::execute(request))?
    }

    /// JSON POST。
    pub fn post_json(
        &self,
        url: &str,
        headers: &[(&str, &str)],
        body: &Value,
    ) -> Result<RawResponse, HttpFailure> {
        let mut request = self
            .client
            .post(url)
            .header("accept", "application/json")
            .json(body);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        Self::run_async(Self::execute(request))?
    }

    /// **无正文、无 `content-type`** 的 POST（某些平台的登出端点只认这个形状）。
    pub fn post_empty(
        &self,
        url: &str,
        headers: &[(&str, &str)],
    ) -> Result<RawResponse, HttpFailure> {
        let mut request = self.client.post(url);
        for (name, value) in headers {
            request = request.header(*name, *value);
        }
        Self::run_async(Self::execute(request))?
    }

    fn run_async<T: Send + 'static>(
        future: impl Future<Output = T> + Send + 'static,
    ) -> Result<T, HttpFailure> {
        match Handle::try_current() {
            Ok(handle) if handle.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
                Ok(tokio::task::block_in_place(|| handle.block_on(future)))
            }
            _ => {
                let run = move || -> Result<T, HttpFailure> {
                    let runtime = tokio::runtime::Builder::new_current_thread()
                        .enable_all()
                        .build()
                        .map_err(|error| HttpFailure {
                            kind: HttpFailureKind::Transport,
                            message: format!("创建 HTTP runtime 失败: {error}"),
                        })?;
                    Ok(runtime.block_on(future))
                };
                if Handle::try_current().is_ok() {
                    std::thread::spawn(run).join().map_err(|_| HttpFailure {
                        kind: HttpFailureKind::Transport,
                        message: "HTTP runtime 线程异常退出".to_string(),
                    })?
                } else {
                    run()
                }
            }
        }
    }

    async fn execute(request: reqwest::RequestBuilder) -> Result<RawResponse, HttpFailure> {
        let mut response = request.send().await.map_err(|error| HttpFailure {
            kind: HttpFailureKind::Transport,
            message: format!("请求失败: {error}"),
        })?;
        let status = response.status().as_u16();
        let request_id = ["x-request-id", "openai-request-id"]
            .iter()
            .find_map(|name| {
                response
                    .headers()
                    .get(*name)
                    .and_then(|value| value.to_str().ok())
                    .map(str::to_owned)
            });
        let mut bytes = Vec::new();
        while let Some(chunk) = response.chunk().await.map_err(|error| HttpFailure {
            kind: HttpFailureKind::BodyRead,
            message: format!("读取响应失败: {error}"),
        })? {
            bytes.extend_from_slice(&chunk);
        }
        let body = String::from_utf8(bytes).map_err(|error| HttpFailure {
            kind: HttpFailureKind::BodyRead,
            message: format!("读取响应失败: {error}"),
        })?;
        Ok(RawResponse {
            status,
            body,
            request_id,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::net::TcpListener;
    use std::thread::JoinHandle;

    fn spawn_server(response: String) -> (String, JoinHandle<String>) {
        let listener = TcpListener::bind("127.0.0.1:0").expect("bind test server");
        let address = listener.local_addr().expect("server address");
        let server = std::thread::spawn(move || {
            let (stream, _) = listener.accept().expect("accept test request");
            let mut writer = stream.try_clone().expect("clone stream");
            let mut reader = BufReader::new(stream);
            let mut request = String::new();
            loop {
                let mut line = String::new();
                let read = reader.read_line(&mut line).expect("read request line");
                if read == 0 || line == "\r\n" {
                    break;
                }
                request.push_str(&line);
            }
            writer
                .write_all(response.as_bytes())
                .expect("write response");
            request
        });
        (format!("http://{address}/"), server)
    }

    fn response(status: &str, body: &str, extra_headers: &str) -> String {
        format!(
            "HTTP/1.1 {status}\r\ncontent-length: {}\r\nconnection: close\r\n{extra_headers}\r\n{body}",
            body.len()
        )
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn spawn_blocking_call_does_not_panic_and_preserves_non_2xx() {
        let (url, server) = spawn_server(response(
            "401 Unauthorized",
            "{\"error\":\"expired\"}",
            "x-request-id: req-123\r\n",
        ));
        let agent = HttpAgent::new(Duration::from_secs(2), "qaqh-test", true);
        let response = tokio::task::spawn_blocking(move || agent.get(&url, &[]))
            .await
            .expect("spawn_blocking join")
            .expect("HTTP response");
        assert_eq!(response.status, 401);
        assert_eq!(response.body, "{\"error\":\"expired\"}");
        assert_eq!(response.request_id.as_deref(), Some("req-123"));
        let request = server.join().expect("server join");
        assert!(request.starts_with("GET / HTTP/1.1"), "{request}");
        assert!(
            request
                .to_ascii_lowercase()
                .contains("accept: application/json")
        );
    }

    #[test]
    fn post_empty_has_no_content_type_and_runs_without_ambient_runtime() {
        let (url, server) = spawn_server(response("204 No Content", "", ""));
        let agent = HttpAgent::new(Duration::from_secs(2), "qaqh-test", true);
        let response = agent.post_empty(&url, &[]).expect("empty post");
        assert_eq!(response.status, 204);
        let request = server.join().expect("server join");
        assert!(request.starts_with("POST / HTTP/1.1"), "{request}");
        assert!(!request.to_ascii_lowercase().contains("content-type:"));
    }

    #[test]
    fn deny_redirects_returns_3xx_without_following() {
        let (url, server) = spawn_server(response(
            "302 Found",
            "",
            "location: http://127.0.0.1:9/should-not-follow\r\n",
        ));
        let agent = HttpAgent::new(Duration::from_secs(2), "qaqh-test", true);
        let response = agent.get(&url, &[]).expect("redirect response");
        assert_eq!(response.status, 302);
        server.join().expect("server join");
    }

    #[tokio::test]
    async fn current_thread_runtime_call_does_not_panic() {
        let (url, server) = spawn_server(response("200 OK", "{\"ok\":true}", ""));
        let agent = HttpAgent::new(Duration::from_secs(2), "qaqh-test", true);
        let response = agent.get(&url, &[]).expect("current_thread get");
        assert_eq!(response.status, 200);
        server.join().expect("server join");
    }
}
