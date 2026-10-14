//! 回环回调监听器（RFC 8252 原生应用风格），**只监听 127.0.0.1**。
//!
//! 与 `qaqh-siwc::loopback` 同一套纪律（那边已经踩过这些坑）：
//!
//! - `redirect_uri` 与监听器绑成一个值，调用方**没有**「另拼一个 uri」的机会——
//!   `auth_init`、换码、回调三处必须逐字一致。
//! - 只接受 `GET`，且 `Host` 必须精确等于 `127.0.0.1:<port>`（挡 DNS rebinding 与代理改写）。
//! - `state` 必须**恰好出现一次**且常数时间相等；不匹配或无关请求**不得消费**事务
//!   （否则一个后台探测就能废掉正在进行的授权）。
//! - 响应带 `Cache-Control: no-store`、`Referrer-Policy: no-referrer` 与严格 CSP，
//!   返回页**不含任何脚本**。
//! - 回调只接收**一次性 code**：token 的交换发生在服务端到服务端的那一步。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::error::{DsCode, DsError};
use crate::protocol::CALLBACK_PATH;
use crate::util::constant_time_eq;

/// 一台回环监听器 = 一次授权尝试的 `redirect_uri`。
#[derive(Debug)]
pub struct CallbackListener {
    listener: TcpListener,
    port: u16,
    redirect_uri: String,
}

const ACCEPT_POLL: Duration = Duration::from_millis(20);
const MAX_REQUEST_BYTES: usize = 8 * 1024;

impl CallbackListener {
    /// 绑定回环端口。首选端口被占用时**回退到临时端口**并记日志：端口可变
    /// （scheme/host/path 不可变），而失败会让「另一个实例正在登录」变成用户
    /// 不可理解的报错。
    pub fn bind(preferred_port: u16) -> Result<Self, DsError> {
        let listener = match TcpListener::bind(("127.0.0.1", preferred_port)) {
            Ok(listener) => listener,
            Err(first_error) if preferred_port != 0 => {
                log::info!(
                    "[dsaccount] 回环端口 {preferred_port} 不可用（{first_error}），改用临时端口"
                );
                TcpListener::bind(("127.0.0.1", 0)).map_err(|error| {
                    DsError::new(DsCode::Callback, format!("回环监听失败: {error}"))
                })?
            }
            Err(error) => {
                return Err(DsError::new(
                    DsCode::Callback,
                    format!("回环监听失败: {error}"),
                ));
            }
        };
        listener
            .set_nonblocking(true)
            .map_err(|error| DsError::new(DsCode::Callback, format!("设置非阻塞失败: {error}")))?;
        let port = listener
            .local_addr()
            .map_err(|error| DsError::new(DsCode::Callback, format!("读取本地端口失败: {error}")))?
            .port();
        Ok(Self {
            listener,
            port,
            redirect_uri: crate::protocol::callback_uri(port),
        })
    }

    /// 必须与 `auth_init`、换码两处逐字一致。
    pub fn redirect_uri(&self) -> &str {
        &self.redirect_uri
    }

    /// 实际端口（首选端口被占时会变）。
    pub fn port(&self) -> u16 {
        self.port
    }

    /// 等待一次性回调，成功返回 `code`。
    ///
    /// `cancel` 由调用方持有（用户取消 / 进程收尾）。
    pub fn wait(
        &self,
        state: &str,
        timeout: Duration,
        cancel: &AtomicBool,
    ) -> Result<String, DsError> {
        let deadline = Instant::now() + timeout;
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(DsError::new(DsCode::Cancelled, "登录已取消"));
            }
            if Instant::now() >= deadline {
                return Err(DsError::new(
                    DsCode::Expired,
                    "登录超时（未在期限内完成浏览器授权）",
                ));
            }
            match self.listener.accept() {
                Ok((stream, _peer)) => {
                    // 只处理「本次尝试」的回调；其余请求一律拒绝且继续等待。
                    if let Some(result) = self.handle(stream, state) {
                        return result;
                    }
                }
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(ACCEPT_POLL);
                }
                Err(error) => {
                    return Err(DsError::new(
                        DsCode::Callback,
                        format!("回环监听中断: {error}"),
                    ));
                }
            }
        }
    }

    /// `Some(_)` = 本次事务有结论；`None` = 与此无关（继续等）。
    fn handle(&self, mut stream: TcpStream, state: &str) -> Option<Result<String, DsError>> {
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let _ = stream.set_write_timeout(Some(Duration::from_secs(5)));
        let request = read_request(&mut stream)?;

        // Host 必须精确等于回环地址：代理 / DNS rebinding / 端口混淆在此挡下。
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
        if path != CALLBACK_PATH {
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
            // 不消费事务：可能只是无关的后台请求或过期标签页。
            respond(
                &mut stream,
                400,
                "登录状态校验失败：请回到发起登录的那个标签页重试",
            );
            return None;
        }

        // 事务已归属本次连接：先给浏览器一个确定的落点，再判定结果。
        respond_html(&mut stream);

        if let Some(error) = params
            .iter()
            .find(|(key, _)| key == "error")
            .map(|(_, value)| value.as_str())
        {
            return Some(Err(callback_error(error)));
        }

        let codes: Vec<&str> = params
            .iter()
            .filter(|(key, _)| key == "code")
            .map(|(_, value)| value.as_str())
            .collect();
        if codes.len() != 1 || codes[0].is_empty() {
            return Some(Err(DsError::new(DsCode::Callback, "回调缺少唯一的授权码")));
        }
        Some(Ok(codes[0].to_string()))
    }
}

fn callback_error(error: &str) -> DsError {
    // `access_denied` 的语义是「用户没授权」：这是**取消**，不是失败。
    if error == "access_denied" {
        return DsError::new(DsCode::Cancelled, "用户在浏览器中拒绝了授权");
    }
    DsError::new(DsCode::Callback, format!("授权回调返回错误: {error}"))
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

fn respond_html(stream: &mut TcpStream) {
    let body = "<!doctype html><html lang=\"zh\"><meta charset=\"utf-8\"><title>返回应用</title><body style=\"font:15px system-ui;max-width:32rem;margin:15vh auto\"><h1 style=\"font-size:20px\">可以返回应用了</h1><p>应用正在核对你的 DeepSeek 账号，这个标签页可以关闭。</p></body></html>";
    let _ = write_response(stream, 200, body);
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
    use std::io::BufRead;

    fn get(port: u16, target: &str, host: Option<&str>) -> String {
        let mut client = TcpStream::connect(("127.0.0.1", port)).expect("connect");
        let host_line = match host {
            Some(host) => format!("Host: {host}\r\n"),
            None => String::new(),
        };
        let request = format!("GET {target} HTTP/1.1\r\n{host_line}Connection: close\r\n\r\n");
        client.write_all(request.as_bytes()).expect("write");
        let mut reader = std::io::BufReader::new(client);
        let mut text = String::new();
        let _ = reader.read_line(&mut text);
        text
    }

    #[test]
    fn redirect_uri_is_loopback_with_the_fixed_path() {
        let listener = CallbackListener::bind(0).expect("bind");
        let uri = listener.redirect_uri();
        assert!(uri.starts_with("http://127.0.0.1:"), "{uri}");
        assert!(uri.ends_with("/oauth/callback"), "{uri}");
        assert!(!uri.contains("localhost"), "回调必须与换码逐字一致: {uri}");
        assert_eq!(uri, &crate::protocol::callback_uri(listener.port()));
    }

    #[test]
    fn busy_preferred_port_falls_back_to_an_ephemeral_one() {
        let first = CallbackListener::bind(0).expect("first");
        let busy = first.port();
        let second = CallbackListener::bind(busy).expect("fallback");
        assert_ne!(second.port(), busy);
    }

    #[test]
    fn foreign_host_wrong_path_and_wrong_state_do_not_consume_the_transaction() {
        let listener = CallbackListener::bind(0).expect("bind");
        let port = listener.port();
        let cancel = AtomicBool::new(false);
        let worker =
            std::thread::spawn(move || listener.wait("s1", Duration::from_secs(5), &cancel));

        // 1) 错误 Host。
        assert!(
            get(
                port,
                "/oauth/callback?code=x&state=s1",
                Some("evil.example")
            )
            .contains("400")
        );
        // 2) 错误路径（`/callback` 不是 `/oauth/callback`）。
        assert!(
            get(
                port,
                "/callback?code=x&state=s1",
                Some(&format!("127.0.0.1:{port}"))
            )
            .contains("404")
        );
        // 3) state 不匹配。
        assert!(
            get(
                port,
                "/oauth/callback?code=x&state=wrong",
                Some(&format!("127.0.0.1:{port}"))
            )
            .contains("400")
        );
        // 4) state 出现两次（重放/拼接）同样拒绝。
        assert!(
            get(
                port,
                "/oauth/callback?code=x&state=s1&state=s1",
                Some(&format!("127.0.0.1:{port}"))
            )
            .contains("400")
        );
        // 5) 正确回调。
        let response = get(
            port,
            "/oauth/callback?code=the-code&state=s1",
            Some(&format!("127.0.0.1:{port}")),
        );
        assert!(response.contains("200"), "{response}");

        assert_eq!(worker.join().expect("join").expect("code"), "the-code");
    }

    #[test]
    fn a_denied_authorization_is_a_cancellation_not_a_failure() {
        let listener = CallbackListener::bind(0).expect("bind");
        let port = listener.port();
        let cancel = AtomicBool::new(false);
        let worker =
            std::thread::spawn(move || listener.wait("s1", Duration::from_secs(5), &cancel));
        let _ = get(
            port,
            "/oauth/callback?error=access_denied&state=s1",
            Some(&format!("127.0.0.1:{port}")),
        );
        let error = worker.join().expect("join").expect_err("拒绝授权");
        assert_eq!(error.code(), DsCode::Cancelled);
        assert!(!error.requires_reauth());
    }

    #[test]
    fn a_timeout_is_expired_and_a_cancel_flag_wins_immediately() {
        let listener = CallbackListener::bind(0).expect("bind");
        let cancel = AtomicBool::new(false);
        let error = listener
            .wait("s1", Duration::from_millis(60), &cancel)
            .expect_err("超时");
        assert_eq!(error.code(), DsCode::Expired);

        let cancel = AtomicBool::new(true);
        let error = listener
            .wait("s1", Duration::from_secs(5), &cancel)
            .expect_err("取消");
        assert_eq!(error.code(), DsCode::Cancelled);
    }

    #[test]
    fn a_callback_without_a_code_is_rejected_after_the_page_is_served() {
        let listener = CallbackListener::bind(0).expect("bind");
        let port = listener.port();
        let cancel = AtomicBool::new(false);
        let worker =
            std::thread::spawn(move || listener.wait("s1", Duration::from_secs(5), &cancel));
        let response = get(
            port,
            "/oauth/callback?state=s1",
            Some(&format!("127.0.0.1:{port}")),
        );
        assert!(response.contains("200"), "先给浏览器落点: {response}");
        let error = worker.join().expect("join").expect_err("缺 code");
        assert_eq!(error.code(), DsCode::Callback);
    }
}
