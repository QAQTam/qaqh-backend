//! 回环回调监听器（RFC 8252 原生应用风格），**只监听 127.0.0.1**。
//!
//! 本模块是 `qaqh-siwc/src/loopback.rs` 与 `qaqh-dsaccount/src/loopback.rs`
//! 两份机械层的唯一归宿（v4.5 P2-1b）。契约逐条来自文档与 DevKit 实证：
//!
//! - `redirect_uri` 必须是 `http://127.0.0.1:<port>{callback_path}`；
//!   **禁 `localhost`**；监听器与 uri 绑成一个值，调用方没有「另拼一个 uri」
//!   的机会——authorize、换码、回调三处必须逐字一致；
//! - 只接受 `GET`，且 `Host` 头必须等于 `127.0.0.1:<port>`（防 DNS rebinding
//!   与代理改写）；
//! - `state` 必须**恰好出现一次**且常数时间相等；不匹配或无关的回环请求
//!   **不得消费**待处理事务（否则一个后台探测就能废掉一次正在进行的授权）；
//! - 响应带 `Cache-Control: no-store`、`Referrer-Policy: no-referrer` 与
//!   严格 CSP；返回页**不含任何脚本**（比内联 hash 白名单更强）；
//! - 本层只交出**解析后的回调查询参数**：token 交换、client_id 归属判定、
//!   错误码映射与成功页文案都留在 provider crate（[`LoopbackOptions`]）。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::util::constant_time_eq;

const ACCEPT_POLL: Duration = Duration::from_millis(20);
const MAX_REQUEST_BYTES: usize = 8 * 1024;

/// 回环监听失败类别。错误码与用户文案映射留在 provider crate
/// （两家对同一机械故障挂了不同产品语义：siwc 超时记 `SignInCancelled`，
/// dsaccount 记 `Expired`——都是各自服务面的既有形状，不许在这里统一）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LoopbackError {
    /// 首选端口与临时端口都绑不上（preferred=0 时即直接绑不上）。载荷为 OS 错误文本。
    Bind(String),
    /// 非阻塞设置失败。
    NonBlocking(String),
    /// 读取本地端口失败。
    Addr(String),
    /// accept 循环中断。
    Accept(String),
    /// 调用方取消（`cancel` 置位）。
    Cancelled,
    /// 期限内没有等到回调。
    Timeout,
}

/// 一次「归属本次事务」的回调：state 校验已通过、浏览器已拿到落点页，
/// 剩下的是 provider 产品判定（code 唯一性、client_id 归属、error 参数）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallbackEvent {
    /// 查询参数原样（保序、可含重复键——唯一性判定是产品层的职责）。
    pub params: Vec<(String, String)>,
}

/// 构造差异点：回调路径、日志前缀、成功落点页 HTML。
#[derive(Debug)]
pub struct LoopbackOptions<'a> {
    /// provider 固定回调路径（siwc `/auth/callback`，dsaccount `/oauth/callback`）。
    pub callback_path: &'a str,
    /// 端口回退日志前缀（`"[siwc]"` / `"[dsaccount]"`）。
    pub log_target: &'a str,
    /// state 命中后回给浏览器的落点页（不含任何脚本）。
    pub success_html: &'a str,
}

/// 一台回环监听器 = 一次授权尝试的 redirect_uri。
#[derive(Debug)]
pub struct CallbackListener {
    listener: TcpListener,
    port: u16,
    redirect_uri: String,
    options: &'static LoopbackOptions<'static>,
}

impl CallbackListener {
    /// 绑定回环端口。首选端口被占用时**回退到临时端口**并记日志：
    /// 文档允许端口可变（scheme/host/path 不可变），而失败会让「另一个实例
    /// 正在登录」变成用户不可理解的报错。
    pub fn bind(
        preferred_port: u16,
        options: &'static LoopbackOptions<'static>,
    ) -> Result<Self, LoopbackError> {
        let listener = match TcpListener::bind(("127.0.0.1", preferred_port)) {
            Ok(listener) => listener,
            Err(first_error) if preferred_port != 0 => {
                log::info!(
                    "{} 回环端口 {preferred_port} 不可用（{first_error}），改用临时端口",
                    options.log_target
                );
                TcpListener::bind(("127.0.0.1", 0))
                    .map_err(|e| LoopbackError::Bind(e.to_string()))?
            }
            Err(e) => return Err(LoopbackError::Bind(e.to_string())),
        };
        listener
            .set_nonblocking(true)
            .map_err(|e| LoopbackError::NonBlocking(e.to_string()))?;
        let port = listener
            .local_addr()
            .map_err(|e| LoopbackError::Addr(e.to_string()))?
            .port();
        Ok(Self {
            listener,
            port,
            redirect_uri: format!("http://127.0.0.1:{port}{}", options.callback_path),
            options,
        })
    }

    /// 必须与 authorize 请求、token 交换三处逐字一致。
    pub fn redirect_uri(&self) -> &str {
        &self.redirect_uri
    }

    /// 实际端口（首选端口被占时会变）。
    pub fn port(&self) -> u16 {
        self.port
    }

    /// 等待一次归属 `state` 的回调。`cancel` 由调用方持有（用户取消 / 进程收尾）。
    pub fn wait(
        &self,
        state: &str,
        timeout: Duration,
        cancel: &AtomicBool,
    ) -> Result<CallbackEvent, LoopbackError> {
        let deadline = Instant::now() + timeout;
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(LoopbackError::Cancelled);
            }
            if Instant::now() >= deadline {
                return Err(LoopbackError::Timeout);
            }
            match self.listener.accept() {
                Ok((stream, _peer)) => {
                    // 只处理「本次尝试」的回调；其余请求一律拒绝且继续等待。
                    if let Some(event) = self.handle(stream, state) {
                        return Ok(event);
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(ACCEPT_POLL);
                }
                Err(e) => return Err(LoopbackError::Accept(e.to_string())),
            }
        }
    }

    /// 处理一条连接：`Some(_)` = 本次事务已归属（成功落点页已回）；
    /// `None` = 与此无关，已回拒绝页并继续等。
    fn handle(&self, mut stream: TcpStream, state: &str) -> Option<CallbackEvent> {
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
        let request = read_request(&mut stream)?;

        // Host 必须精确等于回环地址：代理 / DNS rebinding / 端口混淆都在此挡下。
        let expected_host = format!("127.0.0.1:{}", self.port);
        if request.host.as_deref() != Some(expected_host.as_str()) {
            respond(&mut stream, 400, "无效的 Host");
            return None;
        }
        if request.method != "GET" {
            respond(&mut stream, 405, "只接受 GET");
            return None;
        }
        let Some((path, query)) = request.target.split_once('?') else {
            respond(&mut stream, 404, "未找到");
            return None;
        };
        if path != self.options.callback_path {
            // `/callback` 不等于固定路径（文档点名）。
            respond(&mut stream, 404, "未找到");
            return None;
        }

        let params: Vec<(String, String)> = url::form_urlencoded::parse(query.as_bytes())
            .map(|(key, value)| (key.into_owned(), value.into_owned()))
            .collect();

        // state：必须恰好出现一次且常数时间相等。
        let states: Vec<&str> = params
            .iter()
            .filter(|(key, _)| key == "state")
            .map(|(_, value)| value.as_str())
            .collect();
        let state_ok =
            states.len() == 1 && constant_time_eq(states[0].as_bytes(), state.as_bytes());
        if !state_ok {
            // 不消费事务：可能只是一个无关的后台请求或过期标签页。
            respond(
                &mut stream,
                400,
                "登录状态校验失败：请回到发起登录的那个标签页重试",
            );
            return None;
        }

        // 此刻事务已归属本次连接：先给浏览器一个确定的落点，再交产品层判定。
        let _ = write_response(&mut stream, 200, self.options.success_html);
        Some(CallbackEvent { params })
    }
}

struct RawRequest {
    method: String,
    target: String,
    host: Option<String>,
}

fn read_request(stream: &mut TcpStream) -> Option<RawRequest> {
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 1024];
    loop {
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            break;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if buffer.windows(4).any(|window| window == b"\r\n\r\n")
            || buffer.len() >= MAX_REQUEST_BYTES
        {
            break;
        }
    }
    let text = String::from_utf8_lossy(&buffer);
    let mut lines = text.split("\r\n");
    let request_line = lines.next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?.to_string();
    let host = lines.take_while(|line| !line.is_empty()).find_map(|line| {
        let (name, value) = line.split_once(':')?;
        name.eq_ignore_ascii_case("host")
            .then(|| value.trim().to_string())
    });
    Some(RawRequest {
        method,
        target,
        host,
    })
}

fn respond(stream: &mut TcpStream, status: u16, message: &str) {
    let body = format!(
        "<!doctype html><html lang=\"zh\"><meta charset=\"utf-8\"><title>QAQ-Harness</title><body style=\"font:15px system-ui;max-width:32rem;margin:15vh auto\"><p>{message}</p></body></html>"
    );
    let _ = write_response(stream, status, &body);
}

fn write_response(stream: &mut TcpStream, status: u16, body: &str) -> std::io::Result<()> {
    let response = format!(
        "HTTP/1.1 {status} {}\r\n\
         Content-Type: text/html; charset=utf-8\r\n\
         Content-Length: {}\r\n\
         Cache-Control: no-store\r\n\
         Referrer-Policy: no-referrer\r\n\
         Content-Security-Policy: default-src 'none'; style-src 'unsafe-inline'; frame-ancestors 'none'; base-uri 'none'\r\n\
         Connection: close\r\n\r\n{body}",
        if status == 200 { "OK" } else { "Bad Request" },
        body.len()
    );
    stream.write_all(response.as_bytes())?;
    stream.flush()
}

#[cfg(test)]
mod tests {
    use super::*;

    static OPTIONS: LoopbackOptions<'static> = LoopbackOptions {
        callback_path: "/auth/callback",
        log_target: "[oauth-core-test]",
        success_html: "<!doctype html><title>ok</title>",
    };

    fn get(port: u16, target: &str, host: Option<&str>) -> String {
        let mut client = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        let host_line = match host {
            Some(host) => format!("Host: {host}\r\n"),
            None => String::new(),
        };
        let request = format!("GET {target} HTTP/1.1\r\n{host_line}Connection: close\r\n\r\n");
        client.write_all(request.as_bytes()).expect("write");
        let mut text = String::new();
        let mut reader = std::io::BufReader::new(client);
        use std::io::BufRead;
        let _ = reader.read_line(&mut text);
        text
    }

    #[test]
    fn redirect_uri_is_loopback_with_fixed_path() {
        let listener = CallbackListener::bind(0, &OPTIONS).expect("bind");
        let uri = listener.redirect_uri();
        assert!(uri.starts_with("http://127.0.0.1:"), "{uri}");
        assert!(uri.ends_with("/auth/callback"), "{uri}");
        assert!(!uri.contains("localhost"), "文档禁 localhost: {uri}");
    }

    #[test]
    fn busy_preferred_port_falls_back_to_ephemeral() {
        let first = CallbackListener::bind(0, &OPTIONS).expect("first");
        let busy_port = first.port();
        let second = CallbackListener::bind(busy_port, &OPTIONS).expect("fallback");
        assert_ne!(second.port(), busy_port);
    }

    #[test]
    fn guards_never_consume_the_transaction() {
        let listener = CallbackListener::bind(0, &OPTIONS).expect("bind");
        let port = listener.port();
        let state = "state-abc";
        let cancel = AtomicBool::new(false);
        let worker =
            std::thread::spawn(move || listener.wait(state, Duration::from_secs(5), &cancel));

        // 错误 Host / 错误路径 / state 不匹配 / state 出现两次：全部不消费。
        assert!(
            get(port, &format!("/auth/callback?code=x&state={state}"), Some("evil.example"))
                .contains("400")
        );
        assert!(
            get(
                port,
                &format!("/callback?code=x&state={state}"),
                Some(&format!("127.0.0.1:{port}"))
            )
            .contains("404")
        );
        assert!(
            get(
                port,
                "/auth/callback?code=x&state=wrong",
                Some(&format!("127.0.0.1:{port}"))
            )
            .contains("400")
        );
        assert!(
            get(
                port,
                &format!("/auth/callback?state={state}&state={state}"),
                Some(&format!("127.0.0.1:{port}"))
            )
            .contains("400")
        );
        // 正确回调：事务归属，参数原样交出（保序、含重复键交给产品层判）。
        let response = get(
            port,
            &format!("/auth/callback?code=the-code&state={state}&client_id=x"),
            Some(&format!("127.0.0.1:{port}")),
        );
        assert!(response.contains("200"), "{response}");
        let event = worker.join().expect("join").expect("callback");
        assert_eq!(
            event.params,
            vec![
                ("code".to_string(), "the-code".to_string()),
                ("state".to_string(), state.to_string()),
                ("client_id".to_string(), "x".to_string()),
            ]
        );
    }

    #[test]
    fn cancellation_and_timeout_are_typed() {
        let listener = CallbackListener::bind(0, &OPTIONS).expect("bind");
        let cancel = AtomicBool::new(true);
        let error = listener
            .wait("s", Duration::from_secs(5), &cancel)
            .expect_err("cancelled");
        assert_eq!(error, LoopbackError::Cancelled);

        let cancel = AtomicBool::new(false);
        let error = listener
            .wait("s", Duration::from_millis(60), &cancel)
            .expect_err("timeout");
        assert_eq!(error, LoopbackError::Timeout);
    }
}
