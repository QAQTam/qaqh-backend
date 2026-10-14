//! 平台客户端：把 wire 契约变成九次调用（授权 3 + 账户面 5 + 推理目录 1）。
//!
//! origin 是**配置项而不是常量**（与 `qaqh-siwc` 同思路）：这样端到端测试能起一个
//! 本地假平台，把「信封解析、401 分类、重定向不跟随、凭据头的位置」全部钉住，而不
//! 依赖真站点的可重复性。
//!
//! 凭据头只有一条规则：**账号 token 走 `x-dsh-auth-token`，且只发给已知 origin**
//! （[`is_known_inference_origin`]）——没有 Bearer、没有 API key 回退。

use std::time::Duration;

use serde_json::Value;

use crate::account::{AccountProfile, Bonus, Summary, parse_bonuses, project_profile};
use crate::auth::{AuthCancelBody, AuthExchangeBody, AuthInit, AuthInitBody, AuthSession};
use crate::envelope;
use crate::error::{DsCode, DsError, classify_http_status};
use crate::http::{Http, HttpResponse};
use crate::protocol::{self, ClientIdentity, ClientPlatform, Locale};

/// 账号凭据允许发往的推理 origin 白名单。
///
/// slice-3 的配置校验用它拦「把账号额度配到别的 base_url」：`deepseek_account` 凭据
/// 与 `https://api.deepseek.com` 是绑定的，换端点不等于转发凭据。
pub fn is_known_inference_origin(origin: &str) -> bool {
    origin.trim_end_matches('/') == protocol::INFERENCE_ORIGIN
}

/// 客户端配置。
#[derive(Debug, Clone)]
pub struct DsClientConfig {
    /// 平台 origin（授权 + 账户面）。
    pub platform_origin: String,
    /// 推理 origin（模型目录；账号凭据唯一允许去的第二个 origin）。
    pub inference_origin: String,
    /// 单次请求超时。
    pub timeout: Duration,
    /// 客户端身份（五个头 + 语言）。
    pub identity: ClientIdentity,
    /// 登出时远端撤销的重试预算（见 [`LogoutRetry`]）。
    pub logout_retry: LogoutRetry,
}

impl Default for DsClientConfig {
    fn default() -> Self {
        Self {
            platform_origin: protocol::PLATFORM_ORIGIN.to_string(),
            inference_origin: protocol::INFERENCE_ORIGIN.to_string(),
            timeout: Duration::from_secs(30),
            identity: ClientIdentity {
                version: env!("CARGO_PKG_VERSION").to_string(),
                locale: Locale::ZhCn,
                timezone_offset_seconds: ClientIdentity::system_timezone_offset_seconds(),
                platform: ClientPlatform::current(),
            },
            logout_retry: LogoutRetry::default(),
        }
    }
}

/// 远端撤销的重试预算。
///
/// 取值与参照实现的默认策略一致（`logoutMaxRetries` 5、`logoutRetryDelayMs` 1000，
/// 每轮翻倍 ⇒ 1/2/4/8/16 秒，合计约 31 秒）。**必须有上限**：本地凭据在发起撤销
/// 之前就已经删掉了，为了一次还没成功的撤销把 token 写回盘上重试，等于让撤销白做
/// （参照实现同样明文写着 "pending revocations are not persisted"）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct LogoutRetry {
    /// 首次之外的额外尝试次数。
    pub max_retries: u32,
    /// 首次重试前的等待；此后每轮翻倍。
    pub base_delay: Duration,
}

impl Default for LogoutRetry {
    fn default() -> Self {
        Self {
            max_retries: 5,
            base_delay: Duration::from_secs(1),
        }
    }
}

/// 平台客户端。
#[derive(Debug, Clone)]
pub struct DsClient {
    http: Http,
    config: DsClientConfig,
}

impl DsClient {
    /// 由配置构造（超时同时取自配置）。
    pub fn new(config: DsClientConfig) -> Self {
        let http = Http::new(config.timeout);
        Self { http, config }
    }

    /// 当前配置（诊断与 store 的 issuer 绑定用）。
    pub fn config(&self) -> &DsClientConfig {
        &self.config
    }

    fn url(&self, path: &str) -> String {
        format!("{}{path}", self.config.platform_origin)
    }

    /// 账号请求头：五个客户端身份头 + （可选）账号凭据头。
    fn headers(&self, token: Option<&str>) -> Vec<(&'static str, String)> {
        let mut headers: Vec<(&'static str, String)> = self.config.identity.headers().to_vec();
        if let Some(token) = token {
            headers.push((protocol::AUTH_TOKEN_HEADER, token.to_string()));
        }
        headers
    }

    fn pairs<'a>(headers: &'a [(&'static str, String)]) -> Vec<(&'static str, &'a str)> {
        headers
            .iter()
            .map(|(name, value)| (*name, value.as_str()))
            .collect()
    }

    fn ensure_success(response: &HttpResponse) -> Result<(), DsError> {
        if response.is_success() {
            return Ok(());
        }
        // 非 2xx 先看信封：`40003` 是「凭据无效」，与 HTTP 401 同义。只看状态码
        // 会把「凭据已死」判成 `Protocol`（请求形状被拒）——那是两种完全不同的处置。
        if response.business_code() == Some(crate::error::BIZ_INVALID_AUTHORIZATION) {
            return Err(DsError::new(DsCode::Unauthorized, response.diagnostic()));
        }
        Err(DsError::new(
            classify_http_status(response.status),
            response.diagnostic(),
        ))
    }

    fn get_body(&self, path: &str, token: Option<&str>) -> Result<String, DsError> {
        let headers = self.headers(token);
        let response = self.http.get(&self.url(path), &Self::pairs(&headers))?;
        Self::ensure_success(&response)?;
        Ok(response.body)
    }

    fn post_body(&self, path: &str, token: Option<&str>, body: &Value) -> Result<String, DsError> {
        let headers = self.headers(token);
        let response = self
            .http
            .post_json(&self.url(path), &Self::pairs(&headers), body)?;
        Self::ensure_success(&response)?;
        Ok(response.body)
    }

    fn to_value(body: &impl serde::Serialize) -> Result<Value, DsError> {
        serde_json::to_value(body)
            .map_err(|error| DsError::new(DsCode::Protocol, format!("请求体序列化失败: {error}")))
    }

    /// 开始一次授权：拿 `authorize_url` 与 `authorize_id`。
    pub fn auth_init(&self, body: &AuthInitBody) -> Result<AuthInit, DsError> {
        let raw = self.post_body(protocol::AUTH_INIT_PATH, None, &Self::to_value(body)?)?;
        envelope::parse(&raw)
    }

    /// 用回调 code 换账号凭据。
    pub fn auth_exchange(&self, body: &AuthExchangeBody) -> Result<AuthSession, DsError> {
        let raw = self.post_body(protocol::AUTH_EXCHANGE_PATH, None, &Self::to_value(body)?)?;
        envelope::parse(&raw)
    }

    /// 作废一次**远端待授权**（本地取消后顺手清干净；失败只记日志）。
    pub fn auth_cancel(&self, body: &AuthCancelBody) -> Result<(), DsError> {
        self.post_body(protocol::AUTH_CANCEL_PATH, None, &Self::to_value(body)?)
            .map(|_| ())
    }

    /// 资料（用户对象本体）。
    pub fn current_user(&self, token: &str) -> Result<AccountProfile, DsError> {
        let raw = self.get_body(protocol::USERS_CURRENT_PATH, Some(token))?;
        project_profile(&envelope::parse_value(&raw)?)
    }

    /// 余额（充值 + 赠金 + 累计消费）。
    pub fn summary(&self, token: &str) -> Result<Summary, DsError> {
        let raw = self.get_body(protocol::USER_SUMMARY_PATH, Some(token))?;
        envelope::parse(&raw)
    }

    /// 未读赠额。
    pub fn bonuses(&self, token: &str) -> Result<Vec<Bonus>, DsError> {
        let raw = self.get_body(protocol::UNNOTIFIED_BONUSES_PATH, Some(token))?;
        parse_bonuses(&envelope::parse_value(&raw)?)
    }

    /// 确认一笔赠额已展示。
    pub fn ack_bonus(&self, token: &str, order_id: &str) -> Result<(), DsError> {
        let body = serde_json::json!({ "order_id": order_id });
        self.post_body(protocol::ACK_BONUS_PATH, Some(token), &body)
            .map(|_| ())
    }

    /// 登出 = **撤销账号授权**（本地凭据由调用方在同一动作里清掉）。
    ///
    /// 形状照参照实现：**裸 POST**（身份头 + 凭据头），没有正文、没有
    /// `content-type`、没有 `accept`。理由见 [`Http::post_empty`]——带 JSON 正文的
    /// 写法实测被平台以 400「Authorization Failed (invalid token)」拒掉。
    pub fn logout(&self, token: &str) -> Result<(), DsError> {
        let headers = self.headers(Some(token));
        let response = self.http.post_empty(
            &self.url(protocol::USERS_LOGOUT_PATH),
            &Self::pairs(&headers),
        )?;
        Self::ensure_success(&response)
    }

    /// 推理 origin 的模型目录（**不是**平台信封，是 OpenAI 风格 `{data:[…]}`）。
    pub fn models(&self, token: &str) -> Result<Value, DsError> {
        let url = format!(
            "{}/models",
            self.config.inference_origin.trim_end_matches('/')
        );
        let headers = [(protocol::AUTH_TOKEN_HEADER, token)];
        let response = self.http.get(&url, &headers)?;
        Self::ensure_success(&response)?;
        serde_json::from_str(&response.body)
            .map_err(|error| DsError::new(DsCode::Protocol, format!("模型目录形状不符: {error}")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{Read, Write};
    use std::net::{TcpListener, TcpStream};
    use std::sync::{Arc, Mutex};

    /// 假平台的一条回复。
    struct Reply {
        status: u16,
        headers: Vec<(&'static str, &'static str)>,
        body: String,
    }

    impl Reply {
        fn json(status: u16, body: &str) -> Self {
            Self {
                status,
                headers: Vec::new(),
                body: body.to_string(),
            }
        }

        fn redirect(to: &'static str) -> Self {
            Self {
                status: 302,
                headers: vec![("Location", to)],
                body: String::new(),
            }
        }
    }

    #[derive(Debug, Clone, Default)]
    struct Recorded {
        request_line: String,
        headers: Vec<(String, String)>,
        body: String,
    }

    impl Recorded {
        fn header(&self, name: &str) -> Option<&str> {
            self.headers
                .iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(name))
                .map(|(_, value)| value.as_str())
        }

        fn path(&self) -> String {
            self.request_line
                .split_whitespace()
                .nth(1)
                .unwrap_or_default()
                .to_string()
        }
    }

    /// 只服务固定条数回复的本地假平台（顺序应答，收满即退出）。
    struct FakePlatform {
        base: String,
        recorded: Arc<Mutex<Vec<Recorded>>>,
        handle: Option<std::thread::JoinHandle<()>>,
    }

    impl FakePlatform {
        fn start(replies: Vec<Reply>) -> Self {
            let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind");
            let port = listener.local_addr().expect("addr").port();
            let recorded: Arc<Mutex<Vec<Recorded>>> = Arc::new(Mutex::new(Vec::new()));
            let sink = Arc::clone(&recorded);
            let handle = std::thread::spawn(move || {
                for reply in replies {
                    let Ok((mut stream, _)) = listener.accept() else {
                        return;
                    };
                    sink.lock().expect("lock").push(read_request(&mut stream));
                    write_reply(&mut stream, &reply);
                }
            });
            Self {
                base: format!("http://127.0.0.1:{port}"),
                recorded,
                handle: Some(handle),
            }
        }

        fn config(&self) -> DsClientConfig {
            DsClientConfig {
                platform_origin: self.base.clone(),
                inference_origin: self.base.clone(),
                timeout: Duration::from_secs(5),
                identity: ClientIdentity {
                    version: "0.0.0-test".to_string(),
                    locale: Locale::ZhCn,
                    timezone_offset_seconds: 28_800,
                    platform: ClientPlatform::DesktopWin,
                },
                logout_retry: LogoutRetry::default(),
            }
        }

        /// 收满回复后取走记录（join 掉线程，避免测试相互干扰）。
        fn finish(mut self) -> Vec<Recorded> {
            if let Some(handle) = self.handle.take() {
                let _ = handle.join();
            }
            let mut guard = self.recorded.lock().expect("lock");
            std::mem::take(&mut *guard)
        }
    }

    fn read_request(stream: &mut TcpStream) -> Recorded {
        let mut buffer = Vec::new();
        let mut chunk = [0u8; 1024];
        loop {
            let read = stream.read(&mut chunk).unwrap_or(0);
            if read == 0 {
                break;
            }
            buffer.extend_from_slice(&chunk[..read]);
            if let Some(end) = buffer.windows(4).position(|window| window == b"\r\n\r\n") {
                let head = String::from_utf8_lossy(&buffer[..end]).to_string();
                if buffer.len() >= end + 4 + content_length(&head) {
                    break;
                }
            }
            if buffer.len() > 64 * 1024 {
                break;
            }
        }
        let text = String::from_utf8_lossy(&buffer).to_string();
        let (head, body) = text.split_once("\r\n\r\n").unwrap_or((text.as_str(), ""));
        let mut lines = head.split("\r\n");
        let request_line = lines.next().unwrap_or_default().to_string();
        let headers = lines
            .filter_map(|line| {
                let (name, value) = line.split_once(':')?;
                Some((name.trim().to_string(), value.trim().to_string()))
            })
            .collect();
        Recorded {
            request_line,
            headers,
            body: body.to_string(),
        }
    }

    fn content_length(head: &str) -> usize {
        for line in head.split("\r\n") {
            if let Some((name, value)) = line.split_once(':')
                && name.eq_ignore_ascii_case("content-length")
                && let Ok(parsed) = value.trim().parse::<usize>()
            {
                return parsed;
            }
        }
        0
    }

    fn write_reply(stream: &mut TcpStream, reply: &Reply) {
        let extra: String = reply
            .headers
            .iter()
            .map(|(name, value)| format!("{name}: {value}\r\n"))
            .collect();
        let text = format!(
            "HTTP/1.1 {} Done\r\nContent-Type: application/json\r\n{extra}Content-Length: {}\r\nConnection: close\r\n\r\n{}",
            reply.status,
            reply.body.len(),
            reply.body
        );
        let _ = stream.write_all(text.as_bytes());
        let _ = stream.flush();
    }

    const TOKEN: &str = "account-token-for-tests";

    #[test]
    fn summary_call_carries_the_account_header_and_parses_the_envelope() {
        let fake = FakePlatform::start(vec![Reply::json(
            200,
            r#"{"code":0,"msg":"","data":{"biz_code":0,"biz_msg":"","biz_data":{"normal_wallets":[{"currency":"CNY","balance":"0E-16","token_estimation":"0"}],"bonus_wallets":[],"total_costs":[{"currency":"CNY","amount":"12.3400000000000000"}]}}}"#,
        )]);
        let client = DsClient::new(fake.config());
        let summary = client.summary(TOKEN).expect("余额必须能读到");
        assert_eq!(summary.recharge("CNY").expect("充值").balance, "0E-16");
        assert_eq!(
            summary.total_cost("CNY").expect("累计").amount,
            "12.3400000000000000"
        );

        let requests = fake.finish();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].path(), protocol::USER_SUMMARY_PATH);
        assert_eq!(requests[0].header(protocol::AUTH_TOKEN_HEADER), Some(TOKEN));
        assert_eq!(
            requests[0].header("authorization"),
            None,
            "账号凭据不带 Bearer"
        );
        // 五个客户端身份头一并发出（实测非准入，但服务端用它识别调用方）。
        for name in [
            protocol::CLIENT_BUNDLE_ID,
            protocol::CLIENT_PLATFORM,
            protocol::CLIENT_VERSION,
            protocol::CLIENT_LOCALE,
            protocol::CLIENT_TIMEZONE_OFFSET,
        ] {
            assert!(requests[0].header(name).is_some(), "缺少 {name}");
        }
    }

    #[test]
    fn a_401_on_a_revoked_token_is_a_reauth_failure() {
        // 实测原文：注销后立刻打 users/current 就是这个响应。
        let fake = FakePlatform::start(vec![Reply::json(
            401,
            r#"{"code":40003,"msg":"Authorization Failed (invalid token)","data":null}"#,
        )]);
        let client = DsClient::new(fake.config());
        let error = client.current_user(TOKEN).expect_err("失效必须报错");
        assert_eq!(error.code(), DsCode::Unauthorized);
        assert!(error.requires_reauth());
        assert_eq!(error.message(), "Authorization Failed (invalid token)");
        let _ = fake.finish();
    }

    #[test]
    fn redirects_are_surfaced_instead_of_followed() {
        // 带凭据的请求绝不跟随跨 origin 重定向：否则等于把账号 token 转发给第三方。
        let fake = FakePlatform::start(vec![Reply::redirect("http://evil.example/steal")]);
        let client = DsClient::new(fake.config());
        let error = client.summary(TOKEN).expect_err("3xx 必须报错");
        assert_eq!(error.code(), DsCode::Protocol);
        assert!(!error.requires_reauth());
        let requests = fake.finish();
        assert_eq!(requests.len(), 1, "只应看到一个请求（没有跟随）");
    }

    #[test]
    fn auth_init_sends_the_documented_body_without_the_verifier() {
        let fake = FakePlatform::start(vec![Reply::json(
            200,
            r#"{"code":0,"msg":"","data":{"biz_code":0,"biz_msg":"","biz_data":{"authorize_url":"https://platform.deepseek.com/dsh/authorize?authorize_id=abc","authorize_id":"abc","expires_in":600}}}"#,
        )]);
        let client = DsClient::new(fake.config());
        let pkce = crate::pkce::Pkce::new();
        let init = client
            .auth_init(&AuthInitBody::new(
                &pkce,
                51337,
                Locale::ZhCn,
                crate::auth::LOGIN_SOURCE_DESKTOP,
            ))
            .expect("auth_init");
        assert_eq!(init.expires_in, 600);
        assert_eq!(init.authorize_id, "abc");

        let requests = fake.finish();
        assert_eq!(requests[0].path(), protocol::AUTH_INIT_PATH);
        let body: Value = serde_json::from_str(&requests[0].body).expect("请求体是 JSON");
        assert_eq!(body["code_challenge_method"], "S256");
        assert_eq!(
            body["redirect_uri"],
            "http://127.0.0.1:51337/oauth/callback"
        );
        assert_eq!(body["login_source"], "desktop");
        assert!(
            !requests[0].body.contains(pkce.verifier()),
            "verifier 不得出现在 auth_init"
        );
        assert_eq!(
            requests[0].header(protocol::AUTH_TOKEN_HEADER),
            None,
            "还没登录，不该带凭据"
        );
    }

    #[test]
    fn models_reads_the_inference_origin_with_the_same_credential_header() {
        let inference = FakePlatform::start(vec![Reply::json(
            200,
            r#"{"object":"list","data":[{"id":"deepseek-flash"},{"id":"deepseek-v4-pro"}]}"#,
        )]);
        let mut config = inference.config();
        config.platform_origin = "http://127.0.0.1:9".to_string();
        let client = DsClient::new(config);
        let models = client.models(TOKEN).expect("模型目录");
        let ids: Vec<&str> = models["data"]
            .as_array()
            .expect("数组")
            .iter()
            .filter_map(|entry| entry["id"].as_str())
            .collect();
        assert_eq!(ids, vec!["deepseek-flash", "deepseek-v4-pro"]);

        let requests = inference.finish();
        assert_eq!(requests[0].path(), "/models");
        assert_eq!(requests[0].header(protocol::AUTH_TOKEN_HEADER), Some(TOKEN));
        assert_eq!(requests[0].header("authorization"), None);
    }

    #[test]
    fn logout_and_ack_tolerate_a_null_payload() {
        let fake = FakePlatform::start(vec![
            Reply::json(
                200,
                r#"{"code":0,"msg":"","data":{"biz_code":0,"biz_msg":"","biz_data":null}}"#,
            ),
            Reply::json(
                200,
                r#"{"code":0,"msg":"","data":{"biz_code":0,"biz_msg":"","biz_data":null}}"#,
            ),
        ]);
        let client = DsClient::new(fake.config());
        client.ack_bonus(TOKEN, "order-1").expect("确认赠额");
        client.logout(TOKEN).expect("登出");
        let requests = fake.finish();
        assert_eq!(requests[0].path(), protocol::ACK_BONUS_PATH);
        assert_eq!(requests[1].path(), protocol::USERS_LOGOUT_PATH);
        assert_eq!(
            serde_json::from_str::<Value>(&requests[0].body).expect("JSON")["order_id"],
            "order-1"
        );
    }

    #[test]
    fn only_the_measured_inference_origin_counts_as_known() {
        assert!(is_known_inference_origin("https://api.deepseek.com"));
        assert!(is_known_inference_origin("https://api.deepseek.com/"));
        assert!(!is_known_inference_origin("https://api.deepseek.com/v1"));
        assert!(
            !is_known_inference_origin("http://api.deepseek.com"),
            "scheme 也必须一致"
        );
        assert!(!is_known_inference_origin("https://evil.example"));
        assert!(!is_known_inference_origin(
            "https://api.deepseek.com.evil.example"
        ));
    }
}
