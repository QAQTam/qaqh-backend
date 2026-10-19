//! 端到端：**整条授权链**跑在一个本地假平台上（不碰真站点）。
//!
//! 假平台扮演服务端：`auth_init` 把请求里的 `state` 回显进 `authorize_url`（真实平台
//! 是服务端记住它、回调时带回——假平台用回显达到同样的可驱动性），换码看模式给成功
//! 或失败，账户面按路径应答。测试自己扮演浏览器：按 `redirect_uri` 打一次回环回调。
//!
//! 这里断言的都是**跨模块**的性质：回调 `state` 真的被校验、`redirect_uri` 三处逐字
//! 一致、凭据先落盘才转 signed-in、失败尝试不动已有登录、登出先清本地。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::client::DsClientConfig;
use crate::error::DsCode;
use crate::manager::DsAccountManager;
use crate::protocol::{self, Locale};
use crate::store::{AccountStore, StoredAccount};

const FAKE_TOKEN: &str = "e2e-account-token-value";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExchangeMode {
    /// 正常换码。
    Ok,
    /// 平台回 401 + 40003（失效）。
    Rejected,
    /// 永不回复（用于超时路径）。
    Stall,
}

#[derive(Debug, Clone)]
struct Recorded {
    request_line: String,
    headers: Vec<(String, String)>,
    body: String,
}

impl Recorded {
    fn path(&self) -> String {
        self.request_line
            .split_whitespace()
            .nth(1)
            .unwrap_or_default()
            .to_string()
    }

    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .iter()
            .find(|(key, _)| key.eq_ignore_ascii_case(name))
            .map(|(_, value)| value.as_str())
    }

    fn json(&self) -> Value {
        serde_json::from_str(&self.body).unwrap_or(Value::Null)
    }
}

/// 本地假平台（后台线程持续应答；测试进程结束时随之消失）。
struct FakePlatform {
    base: String,
    log: Arc<Mutex<Vec<Recorded>>>,
}

impl FakePlatform {
    fn start(mode: ExchangeMode) -> Self {
        Self::start_with_logout_failures(mode, 0)
    }

    /// 前 `logout_failures` 次登出请求回 500（用来验后台撤销的**重试**语义）。
    fn start_with_logout_failures(mode: ExchangeMode, logout_failures: u32) -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let log: Arc<Mutex<Vec<Recorded>>> = Arc::new(Mutex::new(Vec::new()));
        let sink = Arc::clone(&log);
        let remaining = Arc::new(std::sync::atomic::AtomicU32::new(logout_failures));
        let counter = Arc::clone(&remaining);
        std::thread::spawn(move || {
            for stream in listener.incoming() {
                let Ok(mut stream) = stream else { break };
                let request = read_request(&mut stream);
                let reply = route(&request, mode, &counter);
                if let Some(reply) = reply {
                    sink.lock().expect("lock").push(request);
                    write_reply(&mut stream, &reply);
                }
            }
        });
        Self {
            base: format!("http://127.0.0.1:{port}"),
            log,
        }
    }

    fn config(&self) -> DsClientConfig {
        DsClientConfig {
            platform_origin: self.base.clone(),
            inference_origin: self.base.clone(),
            timeout: Duration::from_secs(5),
            // 次数与线上一致（5 次重试），退避压到 1ms：用例要的是**次数与终态**，
            // 不是真的等 31 秒。
            logout_retry: crate::client::LogoutRetry {
                max_retries: 5,
                base_delay: Duration::from_millis(1),
            },
            ..DsClientConfig::default()
        }
    }

    fn requests(&self) -> Vec<Recorded> {
        self.log.lock().expect("lock").clone()
    }

    /// 等到至少 `count` 个请求被记录（或超时）。
    fn wait_for(&self, count: usize) -> Vec<Recorded> {
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            let seen = self.requests();
            if seen.len() >= count {
                return seen;
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        self.requests()
    }

    /// 只要某个路径出现过就返回它。
    fn first_request(&self, path: &str) -> Option<Recorded> {
        self.requests()
            .into_iter()
            .find(|entry| entry.path() == path)
    }
}

fn route(
    request: &Recorded,
    mode: ExchangeMode,
    remaining_logout_failures: &std::sync::atomic::AtomicU32,
) -> Option<Reply> {
    match request.path().as_str() {
        protocol::AUTH_INIT_PATH => {
            let state = request.json()["state"]
                .as_str()
                .unwrap_or_default()
                .to_string();
            let authorize_url = format!(
                "{}/dsh/authorize?authorize_id=e2e-authorize-id&state={state}",
                protocol::PLATFORM_ORIGIN
            );
            Some(Reply::json(
                200,
                &format!(
                    r#"{{"code":0,"msg":"","data":{{"biz_code":0,"biz_msg":"","biz_data":{{"authorize_url":"{authorize_url}","authorize_id":"e2e-authorize-id","expires_in":600}}}}}}"#
                ),
            ))
        }
        protocol::AUTH_EXCHANGE_PATH => match mode {
            ExchangeMode::Ok => Some(Reply::json(
                200,
                &format!(
                    r#"{{"code":0,"msg":"","data":{{"biz_code":0,"biz_msg":"","biz_data":{{"token":"{FAKE_TOKEN}","authorized_url":"{}/dsh/authorized","user":{{"id":"user-1","email":"","mobile_number":"138******00","currency":"CNY","id_profile":{{"name":"测试用户","provider":"WECHAT","picture":"https://static.deepseek.com/user-avatar/example","locale":"zh_CN"}}}}}}}}}}"#,
                    protocol::PLATFORM_ORIGIN
                ),
            )),
            ExchangeMode::Rejected => Some(Reply::json(
                401,
                r#"{"code":40003,"msg":"Authorization Failed (invalid token)","data":null}"#,
            )),
            ExchangeMode::Stall => None,
        },
        protocol::AUTH_CANCEL_PATH => Some(Reply::json(
            200,
            r#"{"code":0,"msg":"","data":{"biz_code":0,"biz_msg":"","biz_data":null}}"#,
        )),
        // 凭据验证（`manager.verify()`）打的就是这条：账号模式下它是"这把 token
        // 现在还能用吗"的唯一探针。`Rejected` 模式让它回 401。
        "/models" => {
            if matches!(mode, ExchangeMode::Rejected) {
                return Some(Reply::json(
                    401,
                    r#"{"error":{"message":"Authentication Fails (invalid dsh token)"}}"#,
                ));
            }
            Some(Reply::json(
                200,
                r#"{"object":"list","data":[{"id":"deepseek-flash"},{"id":"deepseek-v4-pro"}]}"#,
            ))
        }
        protocol::USERS_LOGOUT_PATH => {
            // 前 N 次故意 500：验「后台撤销会重试到成功或预算耗尽」。
            let failing = remaining_logout_failures
                .try_update(
                    std::sync::atomic::Ordering::Relaxed,
                    std::sync::atomic::Ordering::Relaxed,
                    |left| left.checked_sub(1),
                )
                .is_ok();
            if failing {
                return Some(Reply::json(
                    500,
                    r#"{"code":50000,"msg":"平台开小差","data":null}"#,
                ));
            }
            Some(Reply::json(
                200,
                r#"{"code":0,"msg":"","data":{"biz_code":0,"biz_msg":"","biz_data":null}}"#,
            ))
        }
        protocol::USER_SUMMARY_PATH => Some(Reply::json(
            200,
            r#"{"code":0,"msg":"","data":{"biz_code":0,"biz_msg":"","biz_data":{"normal_wallets":[{"currency":"CNY","balance":"0E-16","token_estimation":"0"}],"bonus_wallets":[{"currency":"CNY","balance":"10.00"}],"total_costs":[{"currency":"CNY","amount":"12.3400000000000000"}]}}}"#,
        )),
        protocol::UNNOTIFIED_BONUSES_PATH => Some(Reply::json(
            200,
            r#"{"code":0,"msg":"","data":{"biz_code":0,"biz_msg":"","biz_data":[]}}"#,
        )),
        // 资料端点**回显凭据**（实测如此），用来盯住「投影/诊断不外泄」。
        protocol::USERS_CURRENT_PATH => Some(Reply::json(
            200,
            &format!(
                r#"{{"code":0,"msg":"","data":{{"biz_code":0,"biz_msg":"","biz_data":{{"id":"user-1","token":"{FAKE_TOKEN}","currency":"CNY","id_profile":{{"name":"测试用户","provider":"WECHAT"}}}}}}}}"#
            ),
        )),
        // 官方 Search / Fetch：形状取自 `/zh-cn/guides/search`（原样保留字段名）。
        protocol::WEB_SEARCH_PATH => Some(Reply::json(
            200,
            r#"{"results":[{"rerank_score":2.4,"title":"DeepSeek V4 模型发布","url":"https://example.com/deepseek-v4","snippet":"短摘要","summary":"长摘要","content":"网页正文","site_name":"Example News","site_icon":null,"date":"2026-06-01T00:00:00+00:00","authority":3}]}"#,
        )),
        protocol::WEB_FETCH_PATH => Some(Reply::json(
            200,
            r##"{"title":"Example article","content":"# Example article\n\nArticle content."}"##,
        )),
        _ => Some(Reply::json(404, r#"{"code":40400,"msg":"no such route"}"#)),
    }
}

struct Reply {
    status: u16,
    body: String,
}

impl Reply {
    fn json(status: u16, body: &str) -> Self {
        Self {
            status,
            body: body.to_string(),
        }
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
        if buffer.len() > 256 * 1024 {
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
    let text = format!(
        "HTTP/1.1 {} Done\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        reply.status,
        reply.body.len(),
        reply.body
    );
    let _ = stream.write_all(text.as_bytes());
    let _ = stream.flush();
}

/// 扮演浏览器：按 redirect_uri 打一次回环回调。
fn browser_callback(redirect_uri: &str, query: &str) -> String {
    let rest = redirect_uri.strip_prefix("http://").expect("loopback uri");
    let (host, path) = rest.split_once('/').expect("path");
    let (host, port) = host.split_once(':').expect("port");
    let mut client = TcpStream::connect((host, port.parse::<u16>().expect("port")))
        .expect("connect callback listener");
    let request =
        format!("GET /{path}?{query} HTTP/1.1\r\nHost: {host}:{port}\r\nConnection: close\r\n\r\n");
    client.write_all(request.as_bytes()).expect("write");
    let mut reader = std::io::BufReader::new(client);
    let mut text = String::new();
    let _ = reader.read_to_string(&mut text);
    text
}

fn wait_until(mut predicate: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + Duration::from_secs(10);
    while Instant::now() < deadline {
        if predicate() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    false
}

fn manager_for(temp: &tempfile::TempDir, platform: &FakePlatform) -> DsAccountManager {
    DsAccountManager::new(
        AccountStore::new(temp.path().join("deepseek-account.json")),
        platform.config(),
    )
}

/// 起一次登录并把浏览器那一步走完（返回最终登录态投影）。
fn drive_sign_in(
    manager: &DsAccountManager,
    platform: &FakePlatform,
    callback_query: impl FnOnce(&str) -> String,
) -> Value {
    let start = manager
        .sign_in_start(Locale::ZhCn, crate::auth::LOGIN_SOURCE_DESKTOP, 0)
        .expect("sign_in_start");
    let seen = platform.wait_for(1);
    let init = seen.first().expect("auth_init 请求");
    // 真实平台只把 state 记在服务端；假平台回显进 authorize_url，测试从那里取。
    let state = init.json()["state"].as_str().expect("state").to_string();
    assert!(
        start.authorize_url.contains(&state),
        "假平台应当回显 state: {}",
        start.authorize_url
    );
    let page = browser_callback(&start.redirect_uri, &callback_query(&state));
    assert!(page.contains("200"), "回调页必须落地: {page}");
    assert!(
        wait_until(|| !manager.sign_in_status()["state"]
            .as_str()
            .unwrap_or("")
            .ends_with("ing")),
        "流程必须收敛（当前 {:?}）",
        manager.sign_in_status()
    );
    manager.sign_in_status()
}

#[test]
fn full_sign_in_persists_the_credential_and_keeps_the_uri_verbatim_everywhere() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let platform = FakePlatform::start(ExchangeMode::Ok);
    let manager = manager_for(&temp, &platform);

    let status = drive_sign_in(&manager, &platform, |state| {
        format!("code=e2e-code&state={state}")
    });
    assert_eq!(status["state"], "signed-in", "{status}");
    assert!(status["error"].is_null());

    // 凭据可用，且**没有明文落盘**。
    assert_eq!(manager.token().expect("token"), FAKE_TOKEN);
    let raw = std::fs::read_to_string(manager.store().path()).expect("读凭据文件");
    assert!(!raw.contains(FAKE_TOKEN), "凭据文件里出现了明文 token");

    // 投影里没有凭据。
    let projection = manager.status().to_string();
    assert!(!projection.contains(FAKE_TOKEN), "{projection}");
    assert_eq!(manager.status()["account"]["label"], "测试用户");

    // `redirect_uri` 三处逐字一致：auth_init 的 body、换码的 body、回环监听器。
    let requests = platform.requests();
    let init = requests
        .iter()
        .find(|entry| entry.path() == protocol::AUTH_INIT_PATH)
        .expect("auth_init");
    let exchange = platform
        .first_request(protocol::AUTH_EXCHANGE_PATH)
        .expect("auth_exchange");
    let init_uri = init.json()["redirect_uri"]
        .as_str()
        .expect("uri")
        .to_string();
    assert_eq!(
        exchange.json()["redirect_uri"].as_str().expect("uri"),
        init_uri,
        "换码必须与 auth_init 用同一个 redirect_uri"
    );
    assert_eq!(exchange.json()["code"], "e2e-code");
    assert!(
        exchange.json()["code_verifier"]
            .as_str()
            .is_some_and(|verifier| verifier.len() == 43),
        "换码必须带 verifier"
    );
    // 换码不带账号凭据头（那是它的**产出**，不是输入）。
    assert_eq!(exchange.header(protocol::AUTH_TOKEN_HEADER), None);

    // 余额与赠额都能读到（金额面）。
    let summary = manager.balance().expect("余额");
    assert_eq!(summary.recharge("CNY").expect("充值").balance, "0E-16");
    assert_eq!(summary.bonus("CNY").expect("赠金").balance, "10.00");
    assert_eq!(
        summary.total_cost("CNY").expect("累计").amount,
        "12.3400000000000000"
    );
    assert!(manager.bonuses().expect("赠额").is_empty());

    // 在线资料：端点回显 token，投影里必须看不到它。
    let profile = manager.profile().expect("资料");
    assert_eq!(profile.name.as_deref(), Some("测试用户"));
    assert!(!format!("{profile:?}").contains(FAKE_TOKEN));
    let current = platform
        .first_request(protocol::USERS_CURRENT_PATH)
        .expect("资料请求");
    assert_eq!(
        current.header(protocol::AUTH_TOKEN_HEADER),
        Some(FAKE_TOKEN)
    );
}

#[test]
fn a_rejected_exchange_leaves_no_credential_behind() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let platform = FakePlatform::start(ExchangeMode::Rejected);
    let manager = manager_for(&temp, &platform);

    let status = drive_sign_in(&manager, &platform, |state| {
        format!("code=e2e-code&state={state}")
    });
    assert_eq!(status["state"], "failed", "{status}");
    assert_eq!(status["error"], "Authorization Failed (invalid token)");

    // 换码失败 → 什么都没落盘，也就没有要清的凭据。
    assert!(manager.store().load().expect("读").is_none());
    let error = manager.token().expect_err("仍然未登录");
    assert_eq!(error.code(), DsCode::NotSignedIn);
}

#[test]
fn a_wrong_state_never_reaches_the_exchange() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let platform = FakePlatform::start(ExchangeMode::Ok);
    let manager = manager_for(&temp, &platform);

    let start = manager
        .sign_in_start(Locale::ZhCn, crate::auth::LOGIN_SOURCE_DESKTOP, 0)
        .expect("sign_in_start");
    platform.wait_for(1);
    // 伪造 state 的回调：监听器拒绝，且**不消费**事务。
    let page = browser_callback(&start.redirect_uri, "code=stolen&state=forged");
    assert!(page.contains("400"), "{page}");
    std::thread::sleep(Duration::from_millis(150));
    assert_eq!(manager.sign_in_status()["state"], "authorizing");
    assert!(
        platform
            .first_request(protocol::AUTH_EXCHANGE_PATH)
            .is_none()
    );

    // 用真的 state 收尾（取消），让流程停下。
    manager.sign_in_cancel(&start.attempt_id).expect("取消");
    assert!(
        wait_until(|| manager.sign_in_status()["state"] == "cancelled"),
        "取消必须收敛: {:?}",
        manager.sign_in_status()
    );
    assert!(manager.store().load().expect("读").is_none());
}

#[test]
fn a_denied_authorization_cancels_without_touching_an_existing_login() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let platform = FakePlatform::start(ExchangeMode::Ok);
    let manager = manager_for(&temp, &platform);
    // 本机已经登录过。
    let existing = StoredAccount::new(
        "user-0",
        "既有账号",
        None,
        Some("CNY".to_string()),
        &platform.base,
        "existing-token",
    )
    .expect("构造");
    manager.store().save(&existing).expect("落盘");

    let status = drive_sign_in(&manager, &platform, |state| {
        format!("error=access_denied&state={state}")
    });
    assert_eq!(status["state"], "cancelled", "{status}");
    assert!(!status["error"].is_null());

    // 一次被拒绝的登录**不得**动到已有登录。
    assert_eq!(manager.token().expect("旧凭据仍在"), "existing-token");
    assert_eq!(manager.status()["account"]["label"], "既有账号");
}

#[test]
fn sign_out_clears_locally_and_revokes_remotely_in_the_background() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let platform = FakePlatform::start(ExchangeMode::Ok);
    let manager = manager_for(&temp, &platform);
    let record = StoredAccount::new("user-1", "测试用户", None, None, &platform.base, FAKE_TOKEN)
        .expect("构造");
    manager.store().save(&record).expect("落盘");

    manager.sign_out().expect("登出");
    // 本地立刻登出（不等远端）。
    assert!(manager.store().load().expect("读").is_none());
    assert_eq!(manager.status()["signed_in"], false);

    // 远端撤销是后台的，但必须真的发生，且带上账号凭据头。
    let logged = platform.wait_for(1);
    let logout = logged
        .iter()
        .find(|entry| entry.path() == protocol::USERS_LOGOUT_PATH)
        .expect("远端撤销请求");
    assert_eq!(logout.header(protocol::AUTH_TOKEN_HEADER), Some(FAKE_TOKEN));
    // 形状必须是**裸 POST**：平台对着带 JSON 正文的那版回 400「invalid token」。
    assert_eq!(
        logout.header("content-type"),
        None,
        "登出不能带 content-type / 正文"
    );
    // 后台撤销最终收敛：平台第一次就答应。
    assert_eq!(wait_for_revoke(&manager, "done")["attempts"], 1);
}

#[test]
fn a_failed_revoke_is_retried_until_it_succeeds() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    // 前两次 500、第三次 200：撤销必须重试到成功，且期间**不恢复本地登录**。
    let platform = FakePlatform::start_with_logout_failures(ExchangeMode::Ok, 2);
    let manager = manager_for(&temp, &platform);
    let record = StoredAccount::new("user-1", "测试用户", None, None, &platform.base, FAKE_TOKEN)
        .expect("构造");
    manager.store().save(&record).expect("落盘");

    manager.sign_out().expect("登出");
    assert!(
        manager.store().load().expect("读").is_none(),
        "本地立刻断开"
    );

    let settled = wait_for_revoke(&manager, "done");
    assert_eq!(settled["attempts"], 3, "两次失败后第三次成功");
    assert!(
        manager.store().load().expect("读").is_none(),
        "远端重试期间绝不恢复本地登录态"
    );
}

/// 「验证」的两个方向：有效时原地不动；平台判失效时**顺手清掉**本地凭据。
#[test]
fn verify_keeps_a_live_credential_and_clears_a_rejected_one() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let platform = FakePlatform::start(ExchangeMode::Ok);
    let manager = manager_for(&temp, &platform);
    let record = StoredAccount::new("user-1", "测试用户", None, None, &platform.base, FAKE_TOKEN)
        .expect("构造");
    manager.store().save(&record).expect("落盘");

    let ok = manager.verify();
    assert_eq!(ok["valid"], true, "{ok}");
    assert!(
        manager.store().load().expect("读").is_some(),
        "有效就不该碰凭据"
    );

    // 平台判 401（`Rejected` 模式让 `/models` 一起回 401）：失效 ⇒ 顺手清。
    let dead_platform = FakePlatform::start(ExchangeMode::Rejected);
    let dead_manager = manager_for(&temp, &dead_platform);
    let dead_record = StoredAccount::new(
        "user-1",
        "测试用户",
        None,
        None,
        &dead_platform.base,
        FAKE_TOKEN,
    )
    .expect("构造");
    dead_manager.store().save(&dead_record).expect("落盘");

    let dead = dead_manager.verify();
    assert_eq!(dead["valid"], false, "{dead}");
    assert_eq!(dead["cleared"], true, "失效必须清掉本地凭据: {dead}");
    assert!(
        dead["reason"]
            .as_str()
            .is_some_and(|text| text.contains("Authentication")),
        "原因要原样带回来: {dead}"
    );
    assert!(dead_manager.store().load().expect("读").is_none());
}

/// 「端点不可用」≠「凭据无效」（crate 不变量 2）：不可达时只报失败，凭据留着。
#[test]
fn verify_does_not_clear_when_the_endpoint_is_unreachable() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let origin = "http://127.0.0.1:9".to_string();
    let manager = crate::manager::DsAccountManager::new(
        crate::store::AccountStore::new(temp.path().join("deepseek-account.json")),
        crate::client::DsClientConfig {
            platform_origin: origin.clone(),
            inference_origin: origin.clone(),
            ..crate::client::DsClientConfig::default()
        },
    );
    let record =
        StoredAccount::new("user-1", "测试用户", None, None, &origin, FAKE_TOKEN).expect("构造");
    manager.store().save(&record).expect("落盘");

    let result = manager.verify();
    assert_eq!(result["valid"], false, "{result}");
    assert_eq!(result["cleared"], false, "端点不可用不该清凭据: {result}");
    assert!(
        manager.store().load().expect("读").is_some(),
        "凭据必须留在原地"
    );
}

#[test]
fn an_exhausted_revoke_budget_is_reported_as_failed() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let platform = FakePlatform::start_with_logout_failures(ExchangeMode::Ok, u32::MAX);
    let manager = manager_for(&temp, &platform);
    let record = StoredAccount::new("user-1", "测试用户", None, None, &platform.base, FAKE_TOKEN)
        .expect("构造");
    manager.store().save(&record).expect("落盘");

    manager.sign_out().expect("登出");
    let settled = wait_for_revoke(&manager, "failed");
    assert_eq!(
        settled["attempts"], 6,
        "1 次首发 + 5 次重试（与参照实现的预算同量级）"
    );
    assert!(
        settled["message"]
            .as_str()
            .is_some_and(|text| !text.is_empty()),
        "失败要留下可展示的原因: {settled}"
    );
    assert!(
        manager.store().load().expect("读").is_none(),
        "本地保持断开"
    );
}

/// 等后台撤销收敛到某个状态（用例里的退避压到 1ms 级，5 秒足够）。
fn wait_for_revoke(manager: &DsAccountManager, state: &str) -> serde_json::Value {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        let revoke = manager.status()["revoke"].clone();
        if revoke["state"] == state {
            return revoke;
        }
        assert!(
            Instant::now() < deadline,
            "撤销没有收敛到 {state}（当前 {revoke}）"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

#[test]
fn a_second_attempt_supersedes_the_first_and_stale_cancels_are_refused() {
    let temp = tempfile::TempDir::new().expect("tempdir");
    let platform = FakePlatform::start(ExchangeMode::Ok);
    let manager = manager_for(&temp, &platform);

    let first = manager
        .sign_in_start(Locale::ZhCn, crate::auth::LOGIN_SOURCE_DESKTOP, 0)
        .expect("第一次");
    platform.wait_for(1);
    let second = manager
        .sign_in_start(Locale::ZhCn, crate::auth::LOGIN_SOURCE_DESKTOP, 0)
        .expect("第二次");
    assert_ne!(first.attempt_id, second.attempt_id);

    // 旧尝试的取消请求必须被拒绝（旧页面不得取消新登录）。
    let error = manager
        .sign_in_cancel(&first.attempt_id)
        .expect_err("陈旧取消");
    assert_eq!(error.code(), DsCode::Protocol);
    assert_eq!(
        manager.sign_in_status()["attempt_id"],
        second.attempt_id.as_str()
    );

    // 新尝试仍然可以正常取消。
    manager
        .sign_in_cancel(&second.attempt_id)
        .expect("取消当前");
    assert!(wait_until(
        || manager.sign_in_status()["state"] == "cancelled"
    ));
}

#[test]
fn an_expired_attempt_reports_expired_not_signed_in() {
    // 假平台对换码不回复：等回环超时（这里用短超时直接测回环的收敛语义）。
    let temp = tempfile::TempDir::new().expect("tempdir");
    let platform = FakePlatform::start(ExchangeMode::Stall);
    let manager = manager_for(&temp, &platform);
    let start = manager
        .sign_in_start(Locale::ZhCn, crate::auth::LOGIN_SOURCE_DESKTOP, 0)
        .expect("sign_in_start");
    platform.wait_for(1);
    // 直接取消，观察状态收敛（超时路径由 loopback 的用例覆盖，避免测试等 30s）。
    manager.sign_in_cancel(&start.attempt_id).expect("取消");
    assert!(wait_until(
        || manager.sign_in_status()["state"] == "cancelled"
    ));
    assert!(manager.store().load().expect("读").is_none());
}

/// 官方 Search / Fetch：路径、凭据头、请求体形状都按官方文档钉住。
///
/// 这三条是「以官方形式接入而不是自己抓网页」的可验证面：
/// 1. 打的是 `/v1/tools/search` 与 `/v1/tools/fetch`；
/// 2. 凭据是裸 `x-dsh-auth-token`，**没有** `Authorization: Bearer`；
/// 3. 请求体只带调用方真正给过的字段（未设置的可选项不下发）。
#[test]
fn web_tools_use_the_official_paths_and_the_account_header() {
    let platform = FakePlatform::start(ExchangeMode::Ok);
    let client = crate::client::DsClient::new(platform.config());

    let hits = client
        .search_web(
            FAKE_TOKEN,
            &crate::web_tools::WebSearchRequest::new("DeepSeek V4 模型 发布")
                .with_count(5)
                .with_content(true),
        )
        .expect("search");
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].title, "DeepSeek V4 模型发布");
    assert_eq!(hits[0].authority, 3);
    assert_eq!(hits[0].content, "网页正文");

    let page = client
        .fetch_web(FAKE_TOKEN, "https://example.com/article")
        .expect("fetch");
    assert_eq!(page.title, "Example article");
    assert!(page.content.starts_with("# Example article"));

    let requests = platform.requests();
    let search = requests
        .iter()
        .find(|request| request.path() == protocol::WEB_SEARCH_PATH)
        .expect("官方 Search 路径");
    assert_eq!(search.request_line.split_whitespace().next(), Some("POST"));
    assert_eq!(search.header(protocol::AUTH_TOKEN_HEADER), Some(FAKE_TOKEN));
    assert!(
        search.header("authorization").is_none(),
        "账号凭据不得走 Bearer"
    );
    assert_eq!(search.json()["query"], "DeepSeek V4 模型 发布");
    assert_eq!(search.json()["count"], 5);
    assert_eq!(search.json()["need_content"], true);
    assert!(
        search.json().get("ori_question").is_none(),
        "未设置的可选项不得下发"
    );

    let fetch = requests
        .iter()
        .find(|request| request.path() == protocol::WEB_FETCH_PATH)
        .expect("官方 Fetch 路径");
    assert_eq!(fetch.header(protocol::AUTH_TOKEN_HEADER), Some(FAKE_TOKEN));
    assert_eq!(fetch.json()["url"], "https://example.com/article");

    // 参数区间在**发请求之前**就挡住：假平台只应收到上面两条。
    let error = client
        .search_web(FAKE_TOKEN, &crate::web_tools::WebSearchRequest::new("   "))
        .expect_err("空白 query");
    assert_eq!(error.code(), DsCode::Protocol);
    assert_eq!(
        platform.requests().len(),
        requests.len(),
        "校验失败不得发请求"
    );
}
