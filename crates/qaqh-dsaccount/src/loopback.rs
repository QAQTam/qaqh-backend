//! 回环回调监听器（RFC 8252 原生应用风格），**只监听 127.0.0.1**。
//!
//! 机械层（端口回退、Host/路径/state 守卫、拒绝页与落点页响应）在
//! `qaqh_oauth_core::loopback`——与 `qaqh-siwc` 同一套纪律（那边已经踩过这些坑）。
//! 本文件只留 DeepSeek 产品投影：
//!
//! - 错误码与文案映射（`DsCode::{Callback, Cancelled, Expired}`）；
//! - `access_denied` 的语义是「用户没授权」：这是**取消**，不是失败；
//! - 回调只接收**一次性 code**：token 的交换发生在服务端到服务端的那一步；
//! - `redirect_uri` 由 `protocol::callback_uri` 的固定形状给出（core 的
//!   `http://127.0.0.1:{port}{CALLBACK_PATH}` 与之逐字一致，测试锁死）。

use std::sync::atomic::AtomicBool;
use std::time::Duration;

use qaqh_oauth_core::loopback::{
    CallbackEvent, CallbackListener as CoreListener, LoopbackError, LoopbackOptions,
};

use crate::error::{DsCode, DsError};
use crate::protocol::CALLBACK_PATH;

/// core 的构造差异点：DeepSeek 固定回调路径、日志前缀与落点页（无脚本、无外链）。
static OPTIONS: LoopbackOptions<'static> = LoopbackOptions {
    callback_path: CALLBACK_PATH,
    log_target: "[dsaccount]",
    success_html: "<!doctype html><html lang=\"zh\"><meta charset=\"utf-8\"><title>返回应用</title><body style=\"font:15px system-ui;max-width:32rem;margin:15vh auto\"><h1 style=\"font-size:20px\">可以返回应用了</h1><p>应用正在核对你的 DeepSeek 账号，这个标签页可以关闭。</p></body></html>",
};

/// 一台回环监听器 = 一次授权尝试的 `redirect_uri`。
#[derive(Debug)]
pub struct CallbackListener {
    inner: CoreListener,
}

impl CallbackListener {
    /// 绑定回环端口。首选端口被占用时**回退到临时端口**并记日志：端口可变
    /// （scheme/host/path 不可变），而失败会让「另一个实例正在登录」变成用户
    /// 不可理解的报错。
    pub fn bind(preferred_port: u16) -> Result<Self, DsError> {
        let inner = CoreListener::bind(preferred_port, &OPTIONS).map_err(map_error)?;
        Ok(Self { inner })
    }

    /// 必须与 `auth_init`、换码两处逐字一致。
    pub fn redirect_uri(&self) -> &str {
        self.inner.redirect_uri()
    }

    /// 实际端口（首选端口被占时会变）。
    pub fn port(&self) -> u16 {
        self.inner.port()
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
        let event = self.inner.wait(state, timeout, cancel).map_err(map_error)?;
        code_from(&event)
    }
}

fn map_error(error: LoopbackError) -> DsError {
    match error {
        LoopbackError::Bind(reason) => callback_failure(&reason),
        LoopbackError::NonBlocking(reason) => {
            DsError::new(DsCode::Callback, format!("设置非阻塞失败: {reason}"))
        }
        LoopbackError::Addr(reason) => {
            DsError::new(DsCode::Callback, format!("读取本地端口失败: {reason}"))
        }
        LoopbackError::Accept(reason) => {
            DsError::new(DsCode::Callback, format!("回环监听中断: {reason}"))
        }
        LoopbackError::Cancelled => DsError::new(DsCode::Cancelled, "登录已取消"),
        LoopbackError::Timeout => DsError::new(
            DsCode::Expired,
            "登录超时（未在期限内完成浏览器授权）",
        ),
    }
}

fn callback_failure(reason: &str) -> DsError {
    DsError::new(DsCode::Callback, format!("回环监听失败: {reason}"))
}

/// state 已校验、落点页已回，剩下的是 DeepSeek 侧的产品判定。
fn code_from(event: &CallbackEvent) -> Result<String, DsError> {
    if let Some(error) = event
        .params
        .iter()
        .find(|(key, _)| key == "error")
        .map(|(_, value)| value.as_str())
    {
        return Err(callback_error(error));
    }
    let codes: Vec<&str> = event
        .params
        .iter()
        .filter(|(key, _)| key == "code")
        .map(|(_, value)| value.as_str())
        .collect();
    if codes.len() != 1 || codes[0].is_empty() {
        return Err(DsError::new(DsCode::Callback, "回调缺少唯一的授权码"));
    }
    Ok(codes[0].to_string())
}

fn callback_error(error: &str) -> DsError {
    // `access_denied` 的语义是「用户没授权」：这是**取消**，不是失败。
    if error == "access_denied" {
        return DsError::new(DsCode::Cancelled, "用户在浏览器中拒绝了授权");
    }
    DsError::new(DsCode::Callback, format!("授权回调返回错误: {error}"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::protocol::callback_uri;
    use std::io::{BufRead, Write};
    use std::net::TcpStream;

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
        assert_eq!(uri, &callback_uri(listener.port()));
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
