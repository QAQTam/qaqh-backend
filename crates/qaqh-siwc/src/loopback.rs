//! 回环回调监听器的 **siwc 产品层**（v4.5 P2-1b）。
//!
//! 机械层（bind/accept 循环、Host/GET/path/state 守卫、拒绝页与落点页写出、
//! CSP/no-store 头）已抽进 [`qaqh_oauth_core::loopback`]（与
//! `qaqh-dsaccount` 的同一份拷贝合一）；本文件只保留 siwc 的产品语义：
//!
//! - `redirect_uri` = `http://127.0.0.1:<port>/auth/callback`，**禁
//!   `localhost`**；同一次尝试内 authorize 与 token 交换必须**逐字一致**；
//! - 回调**不落任何 token**：只接收一次性 code 与**本次注册下发**的 client_id；
//! - `saved_client_id` 回访保护：回调带回**不同**的 client_id 一律拒绝，
//!   不得覆盖选中账户的注册；`dynamic_agent_client` 是入口不是注册结果；
//! - `access_denied` 的正确语义是「未启用套餐用量」，不是凭据失效；
//! - 取消与超时都记 `SignInCancelled`（dsaccount 同机制映射 `Expired`——
//!   两家服务面的既有形状，不因抽层而统一）。

use std::sync::atomic::AtomicBool;
use std::time::Duration;

use qaqh_oauth_core::loopback::{
    CallbackEvent, CallbackListener as CoreListener, LoopbackError, LoopbackOptions,
};

use crate::error::{SiwcCode, SiwcError};
use crate::protocol::CALLBACK_PATH;

/// 单次回调结果：一次性 code + **本次注册下发**的 client_id。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct CallbackResult {
    pub code: String,
    pub client_id: String,
}

/// siwc 机械层参数（成功落点页文案是本 provider 的产品文本）。
static LOOPBACK: LoopbackOptions<'static> = LoopbackOptions {
    callback_path: CALLBACK_PATH,
    log_target: "[siwc]",
    success_html: "<!doctype html><html lang=\"zh\"><meta charset=\"utf-8\"><title>返回应用</title><body style=\"font:15px system-ui;max-width:32rem;margin:15vh auto\"><h1 style=\"font-size:20px\">可以返回应用了</h1><p>应用正在核对你的 ChatGPT 连接，这个标签页可以关闭。</p></body></html>",
};

/// 一台回环监听器 = 一次授权尝试的 redirect_uri。
pub struct CallbackListener {
    inner: CoreListener,
}

impl CallbackListener {
    /// 绑定回环端口（首选端口占用时回退临时端口，见核心层文档）。
    pub fn bind(preferred_port: u16) -> Result<Self, SiwcError> {
        CoreListener::bind(preferred_port, &LOOPBACK)
            .map(|inner| Self { inner })
            .map_err(map_error)
    }

    /// 必须与 authorize 请求、token 交换三处逐字一致。
    pub fn redirect_uri(&self) -> &str {
        self.inner.redirect_uri()
    }

    pub fn port(&self) -> u16 {
        self.inner.port()
    }

    /// 等待一次性回调并做 siwc 产品判定。
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
        let event = self.inner.wait(state, timeout, cancel).map_err(map_error)?;
        adjudicate(event, saved_client_id)
    }
}

/// 事务归属后的 siwc 判定（核心层已回浏览器落点页，此处只下结论）。
fn adjudicate(event: CallbackEvent, saved_client_id: Option<&str>) -> Result<CallbackResult, SiwcError> {
    let params = &event.params;
    if let Some(error) = params.iter().find(|(k, _)| k == "error").map(|(_, v)| v) {
        return Err(callback_error(error));
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
        return Err(SiwcError::new(
            SiwcCode::RegistrationIncomplete,
            "回调缺少唯一的授权码",
        ));
    }
    let returned = returned_client_ids.first().copied();
    if let (Some(saved), Some(returned)) = (saved_client_id, returned)
        && saved != returned
    {
        return Err(SiwcError::new(
            SiwcCode::AccountMismatch,
            "回调返回了与所选账户不同的注册，已拒绝（不覆盖既有注册）",
        ));
    }
    let Some(client_id) = returned.or(saved_client_id) else {
        return Err(SiwcError::new(
            SiwcCode::RegistrationIncomplete,
            "新注册回调未下发 client_id，注册未完成",
        ));
    };
    if !valid_client_id(client_id) {
        return Err(SiwcError::new(
            SiwcCode::RegistrationIncomplete,
            format!("回调下发的 client_id 形态非法: {client_id}"),
        ));
    }
    Ok(CallbackResult {
        code: codes[0].to_owned(),
        client_id: client_id.to_owned(),
    })
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

/// 机械失败 → siwc 服务面错误（文案与旧拷贝逐字一致）。
fn map_error(error: LoopbackError) -> SiwcError {
    match error {
        LoopbackError::Bind(reason) => SiwcError::new(
            SiwcCode::CallbackPortUnavailable,
            format!("回环监听失败: {reason}"),
        ),
        LoopbackError::NonBlocking(reason) => SiwcError::new(
            SiwcCode::CallbackFailed,
            format!("回环监听设置非阻塞失败: {reason}"),
        ),
        LoopbackError::Addr(reason) => SiwcError::new(
            SiwcCode::CallbackFailed,
            format!("读取本地端口失败: {reason}"),
        ),
        LoopbackError::Accept(reason) => SiwcError::new(
            SiwcCode::CallbackFailed,
            format!("回环监听中断: {reason}"),
        ),
        LoopbackError::Cancelled => {
            SiwcError::new(SiwcCode::SignInCancelled, "登录已取消")
        }
        LoopbackError::Timeout => SiwcError::new(
            SiwcCode::SignInCancelled,
            "登录超时（未在期限内完成浏览器授权）",
        ),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::BufRead;
    use std::io::Write;
    use std::net::TcpStream;
    use std::sync::atomic::AtomicBool;
    use std::time::Duration;

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

        let worker =
            std::thread::spawn(move || listener.wait(state, None, Duration::from_secs(5), &cancel));

        // 1) 错误 Host：拒绝且不消费。
        assert!(
            get(
                port,
                &format!("/auth/callback?code=x&state={state}"),
                Some("evil.example")
            )
            .contains("400")
        );
        // 2) 错误路径（文档：`/callback` 不等于 `/auth/callback`）：404，不消费。
        assert!(
            get(
                port,
                &format!("/callback?code=x&state={state}"),
                Some(&format!("127.0.0.1:{port}"))
            )
            .contains("404")
        );
        // 3) state 不匹配：400，不消费。
        assert!(
            get(
                port,
                "/auth/callback?code=x&state=wrong",
                Some(&format!("127.0.0.1:{port}"))
            )
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
            let _ = get(
                port,
                &format!("/auth/callback?code=c&state=s1{suffix}"),
                Some(&format!("127.0.0.1:{port}")),
            );
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
        let _ = get(
            port,
            "/auth/callback?code=c&state=s2",
            Some(&format!("127.0.0.1:{port}")),
        );
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
            worker
                .join()
                .expect("join")
                .expect_err("must reject")
                .code(),
            SiwcCode::AccountMismatch
        );
    }

    #[test]
    fn access_denied_is_not_an_error_object() {
        let listener = CallbackListener::bind(0).expect("bind");
        let port = listener.port();
        let cancel = AtomicBool::new(false);
        let worker =
            std::thread::spawn(move || listener.wait("s4", None, Duration::from_secs(5), &cancel));
        let _ = get(
            port,
            "/auth/callback?error=access_denied&state=s4",
            Some(&format!("127.0.0.1:{port}")),
        );
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
