//! 回环回调监听器（RFC 8252 原生应用风格），**只监听 127.0.0.1**。
//!
//! 契约逐条来自文档与 DevKit 实证：
//! - `redirect_uri` 必须是 `http://127.0.0.1:<port>/auth/callback`；**禁 `localhost`**；
//!   同一次尝试内 authorize 与 token 交换必须**逐字一致**——本模块把 uri 与监听器
//!   绑成一个值，调用方没有「另拼一个 uri」的机会。
//! - 只接受 `GET`，且 `Host` 头必须等于 `127.0.0.1:<port>`（防 DNS rebinding 与代理改写）。
//! - `state` 常数时间比对且必须**唯一**；不匹配或无关的回环请求**不得消费**
//!   待处理事务（否则一个后台探测就能废掉一次正在进行的授权）。
//! - 响应带 `Cache-Control: no-store`、`Referrer-Policy: no-referrer` 与严格 CSP；
//!   返回页**不含任何脚本**（比内联 hash 白名单更强）。
//! - 回调**不落任何 token**：只接收一次性 code 与下发的 client_id。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crate::error::{SiwcCode, SiwcError};
use crate::protocol::CALLBACK_PATH;
use crate::util::constant_time_eq;

/// 单次回调结果：一次性 code + **本次注册下发**的 client_id。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallbackResult {
    pub code: String,
    pub client_id: String,
}

/// 一台回环监听器 = 一次授权尝试的 redirect_uri。
#[derive(Debug)]
pub struct CallbackListener {
    listener: TcpListener,
    port: u16,
    redirect_uri: String,
}

const ACCEPT_POLL: Duration = Duration::from_millis(20);
const MAX_REQUEST_BYTES: usize = 8 * 1024;

impl CallbackListener {
    /// 绑定回环端口。首选端口被占用时**回退到临时端口**并记日志：
    /// 文档允许端口可变（scheme/host/path 不可变），而失败会让「另一个实例
    /// 正在登录」变成用户不可理解的报错。
    pub fn bind(preferred_port: u16) -> Result<Self, SiwcError> {
        let listener = match TcpListener::bind(("127.0.0.1", preferred_port)) {
            Ok(listener) => listener,
            Err(first_error) if preferred_port != 0 => {
                log::info!(
                    "[siwc] 回环端口 {preferred_port} 不可用（{first_error}），改用临时端口"
                );
                TcpListener::bind(("127.0.0.1", 0)).map_err(|e| {
                    SiwcError::new(
                        SiwcCode::CallbackPortUnavailable,
                        format!("回环监听失败: {e}"),
                    )
                })?
            }
            Err(e) => {
                return Err(SiwcError::new(
                    SiwcCode::CallbackPortUnavailable,
                    format!("回环监听失败: {e}"),
                ));
            }
        };
        listener.set_nonblocking(true).map_err(|e| {
            SiwcError::new(
                SiwcCode::CallbackFailed,
                format!("回环监听设置非阻塞失败: {e}"),
            )
        })?;
        let port = listener
            .local_addr()
            .map_err(|e| SiwcError::new(SiwcCode::CallbackFailed, format!("读取本地端口失败: {e}")))?
            .port();
        Ok(Self {
            listener,
            port,
            redirect_uri: format!("http://127.0.0.1:{port}{CALLBACK_PATH}"),
        })
    }

    /// 必须与 authorize 请求、token 交换三处逐字一致。
    pub fn redirect_uri(&self) -> &str {
        &self.redirect_uri
    }

    pub fn port(&self) -> u16 {
        self.port
    }

    /// 等待一次性回调。`cancel` 由调用方持有（用户取消 / 进程收尾）。
    ///
    /// `saved_client_id` 为本次尝试已关联的注册（回访场景）：回调若带回
    /// **不同**的 client_id，一律拒绝——不得覆盖选中账户的注册。
    pub fn wait(
        &self,
        state: &str,
        saved_client_id: Option<&str>,
        timeout: Duration,
        cancel: &AtomicBool,
    ) -> Result<CallbackResult, SiwcError> {
        let deadline = Instant::now() + timeout;
        loop {
            if cancel.load(Ordering::Relaxed) {
                return Err(SiwcError::new(SiwcCode::SignInCancelled, "登录已取消"));
            }
            if Instant::now() >= deadline {
                return Err(SiwcError::new(
                    SiwcCode::SignInCancelled,
                    "登录超时（未在期限内完成浏览器授权）",
                ));
            }
            match self.listener.accept() {
                Ok((stream, _peer)) => {
                    // 只处理「本次尝试」的回调；其余请求一律拒绝且继续等待。
                    if let Some(result) = self.handle(stream, state, saved_client_id) {
                        return result;
                    }
                }
                Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                    std::thread::sleep(ACCEPT_POLL);
                }
                Err(e) => {
                    return Err(SiwcError::new(
                        SiwcCode::CallbackFailed,
                        format!("回环监听中断: {e}"),
                    ));
                }
            }
        }
    }

    /// 处理一条连接：`Some(_)` = 本次事务有结论（成功或失败）；`None` = 与此无关，继续等。
    fn handle(
        &self,
        mut stream: TcpStream,
        state: &str,
        saved_client_id: Option<&str>,
    ) -> Option<Result<CallbackResult, SiwcError>> {
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
        if path != CALLBACK_PATH {
            // `/callback` 不等于 `/auth/callback`（文档点名）。
            respond(&mut stream, 404, "未找到");
            return None;
        }

        let params: Vec<(String, String)> = url::form_urlencoded::parse(query.as_bytes())
            .map(|(k, v)| (k.into_owned(), v.into_owned()))
            .collect();

        // state：必须恰好出现一次且常数时间相等。
        let states: Vec<&str> = params
            .iter()
            .filter(|(k, _)| k == "state")
            .map(|(_, v)| v.as_str())
            .collect();
        let state_ok = states.len() == 1
            && constant_time_eq(states[0].as_bytes(), state.as_bytes());
        if !state_ok {
            // 不消费事务：可能只是一个无关的后台请求或过期标签页。
            respond(
                &mut stream,
                400,
                "登录状态校验失败：请回到发起登录的那个标签页重试",
            );
            return None;
        }

        // 此刻事务已归属本次连接：先给浏览器一个确定的落点，再判定结果。
        respond_html(&mut stream);

        if let Some(error) = params.iter().find(|(k, _)| k == "error").map(|(_, v)| v) {
            return Some(Err(callback_error(error)));
        }

        let codes: Vec<&str> = params
            .iter()
            .filter(|(k, _)| k == "code")
            .map(|(_, v)| v.as_str())
            .collect();
        let returned_client_ids: Vec<&str> = params
            .iter()
            .filter(|(k, _)| k == "client_id")
            .map(|(_, v)| v.as_str())
            .collect();
        if codes.len() != 1 || returned_client_ids.len() > 1 {
            return Some(Err(SiwcError::new(
                SiwcCode::RegistrationIncomplete,
                "回调缺少唯一的授权码",
            )));
        }
        let returned = returned_client_ids.first().copied();
        if let (Some(saved), Some(returned)) = (saved_client_id, returned)
            && saved != returned
        {
            return Some(Err(SiwcError::new(
                SiwcCode::AccountMismatch,
                "回调返回了与所选账户不同的注册，已拒绝（不覆盖既有注册）",
            )));
        }
        let Some(client_id) = returned.or(saved_client_id) else {
            return Some(Err(SiwcError::new(
                SiwcCode::RegistrationIncomplete,
                "新注册回调未下发 client_id，注册未完成",
            )));
        };
        if !valid_client_id(client_id) {
            return Some(Err(SiwcError::new(
                SiwcCode::RegistrationIncomplete,
                format!("回调下发的 client_id 形态非法: {client_id}"),
            )));
        }
        Some(Ok(CallbackResult {
            code: codes[0].to_owned(),
            client_id: client_id.to_owned(),
        }))
    }
}

/// 下发的 client_id 形态：`dynamic_agent_client` 是**入口**而非注册结果，必须排除。
fn valid_client_id(raw: &str) -> bool {
    !raw.is_empty()
        && raw.len() <= 200
        && raw
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-')
        && raw != crate::protocol::DYNAMIC_CLIENT_ID
}

fn callback_error(error: &str) -> SiwcError {
    // `access_denied` 的正确语义是「用户没授权」：不是错误，而是需要别的付款路径。
    if error == "access_denied" {
        return SiwcError::new(
            SiwcCode::SharingNotEnabled,
            "用户在浏览器中拒绝了授权（未启用 ChatGPT 套餐用量）",
        );
    }
    SiwcError::new(
        SiwcCode::ProviderError,
        format!("授权回调返回错误: {error}"),
    )
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
        if buffer.windows(4).any(|w| w == b"\r\n\r\n") || buffer.len() >= MAX_REQUEST_BYTES {
            break;
        }
    }
    let text = String::from_utf8_lossy(&buffer);
    let mut lines = text.split("\r\n");
    let request_line = lines.next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_owned();
    let target = parts.next()?.to_owned();
    let host = lines
        .take_while(|line| !line.is_empty())
        .find_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("host")
                .then(|| value.trim().to_owned())
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
    let body = "<!doctype html><html lang=\"zh\"><meta charset=\"utf-8\"><title>返回应用</title><body style=\"font:15px system-ui;max-width:32rem;margin:15vh auto\"><h1 style=\"font-size:20px\">可以返回应用了</h1><p>应用正在核对你的 ChatGPT 连接，这个标签页可以关闭。</p></body></html>";
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
        use std::net::TcpStream as Client;
        let mut client = Client::connect(("127.0.0.1", port)).expect("connect");
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
    fn redirect_uri_is_loopback_with_fixed_path() {
        let listener = CallbackListener::bind(0).expect("bind");
        let uri = listener.redirect_uri();
        assert!(uri.starts_with("http://127.0.0.1:"), "{uri}");
        assert!(uri.ends_with("/auth/callback"), "{uri}");
        assert!(!uri.contains("localhost"), "文档禁 localhost: {uri}");
    }

    #[test]
    fn busy_preferred_port_falls_back_to_ephemeral() {
        let first = CallbackListener::bind(0).expect("first");
        let busy_port = first.port();
        let second = CallbackListener::bind(busy_port).expect("fallback");
        assert_ne!(second.port(), busy_port);
    }

    #[test]
    fn foreign_host_and_wrong_path_do_not_consume_the_transaction() {
        let listener = CallbackListener::bind(0).expect("bind");
        let port = listener.port();
        let state = "state-abc";
        let cancel = AtomicBool::new(false);
        let uri = listener.redirect_uri().to_owned();

        let worker = std::thread::spawn(move || {
            listener.wait(state, None, Duration::from_secs(5), &cancel)
        });

        // 1) 错误 Host：拒绝且不消费。
        assert!(get(port, &format!("/auth/callback?code=x&state={state}"), Some("evil.example")).contains("400"));
        // 2) 错误路径（文档：`/callback` 不等于 `/auth/callback`）：404，不消费。
        assert!(
            get(port, &format!("/callback?code=x&state={state}"), Some(&format!("127.0.0.1:{port}")))
                .contains("404")
        );
        // 3) state 不匹配：400，不消费。
        assert!(
            get(port, "/auth/callback?code=x&state=wrong", Some(&format!("127.0.0.1:{port}")))
                .contains("400")
        );
        // 4) 正确回调：拿到结果。
        let response = get(
            port,
            &format!("/auth/callback?code=the-code&state={state}&client_id=oaiapp_ok"),
            Some(&format!("127.0.0.1:{port}")),
        );
        assert!(response.contains("200"), "{response}");

        let result = worker.join().expect("join").expect("callback");
        assert_eq!(result.code, "the-code");
        assert_eq!(result.client_id, "oaiapp_ok");
        assert!(uri.contains(&port.to_string()));
    }

    #[test]
    fn registration_incomplete_when_issued_client_id_missing_or_dynamic() {
        // 非法 client_id 用**百分号编码**表达（裸空格会破坏请求行，测不出真实行为）。
        for suffix in ["", "&client_id=dynamic_agent_client", "&client_id=bad%20id"] {
            let listener = CallbackListener::bind(0).expect("bind");
            let port = listener.port();
            let cancel = AtomicBool::new(false);
            let worker = std::thread::spawn(move || {
                listener.wait("s1", None, Duration::from_secs(5), &cancel)
            });
            let _ = get(port, &format!("/auth/callback?code=c&state=s1{suffix}"), Some(&format!("127.0.0.1:{port}")));
            let error = worker.join().expect("join").expect_err("must fail");
            assert_eq!(error.code(), SiwcCode::RegistrationIncomplete, "{suffix}");
        }
    }

    #[test]
    fn returning_sign_in_may_omit_client_id_but_may_not_change_it() {
        // 回访：回调不带 client_id → 用已保存的注册。
        let listener = CallbackListener::bind(0).expect("bind");
        let port = listener.port();
        let cancel = AtomicBool::new(false);
        let worker = std::thread::spawn(move || {
            listener.wait("s2", Some("oaiapp_saved"), Duration::from_secs(5), &cancel)
        });
        let _ = get(port, "/auth/callback?code=c&state=s2", Some(&format!("127.0.0.1:{port}")));
        let result = worker.join().expect("join").expect("callback");
        assert_eq!(result.client_id, "oaiapp_saved");

        // 回调给了不同的 client_id → 拒绝（防覆盖注册）。
        let listener = CallbackListener::bind(0).expect("bind");
        let port = listener.port();
        let cancel = AtomicBool::new(false);
        let worker = std::thread::spawn(move || {
            listener.wait("s3", Some("oaiapp_saved"), Duration::from_secs(5), &cancel)
        });
        let _ = get(
            port,
            "/auth/callback?code=c&state=s3&client_id=oaiapp_other",
            Some(&format!("127.0.0.1:{port}")),
        );
        assert_eq!(
            worker.join().expect("join").expect_err("must reject").code(),
            SiwcCode::AccountMismatch
        );
    }

    #[test]
    fn access_denied_is_not_an_error_object() {
        let listener = CallbackListener::bind(0).expect("bind");
        let port = listener.port();
        let cancel = AtomicBool::new(false);
        let worker = std::thread::spawn(move || {
            listener.wait("s4", None, Duration::from_secs(5), &cancel)
        });
        let _ = get(port, "/auth/callback?error=access_denied&state=s4", Some(&format!("127.0.0.1:{port}")));
        let error = worker.join().expect("join").expect_err("declined");
        assert_eq!(error.code(), SiwcCode::SharingNotEnabled);
        assert!(!error.code().requires_reauth(), "拒绝授权不等于凭据失效");
    }

    #[test]
    fn cancellation_and_timeout_are_both_sign_in_cancelled() {
        let listener = CallbackListener::bind(0).expect("bind");
        let cancel = AtomicBool::new(true);
        let error = listener
            .wait("s5", None, Duration::from_millis(50), &cancel)
            .expect_err("cancelled");
        assert_eq!(error.code(), SiwcCode::SignInCancelled);

        let listener = CallbackListener::bind(0).expect("bind");
        let cancel = AtomicBool::new(false);
        let error = listener
            .wait("s6", None, Duration::from_millis(60), &cancel)
            .expect_err("timeout");
        assert_eq!(error.code(), SiwcCode::SignInCancelled);
    }
}
