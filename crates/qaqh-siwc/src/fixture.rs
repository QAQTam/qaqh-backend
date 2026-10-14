//! 测试夹具：**测试专用** RSA 私钥 + 可编程的本地 OIDC 提供方。
//!
//! 为什么需要固定私钥：`ring` 不生成 RSA 密钥（只有签名/验签），而 ID token 的
//! 校验路径必须真跑一遍 RS256。这把密钥随仓库公开、只用于测试，
//! 不具备也不代表任何生产权限。
//!
//! 为什么需要本地提供方：ISSER/端点可注入（[`crate::SiwcConfig`]），
//! 于是「注册 → 回访 → 刷新 → 撤销」能在离线、确定性的条件下端到端跑通。

use std::io::{Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::thread::JoinHandle;
use std::time::Duration;

use serde_json::{Value, json};

use crate::util::base64url_decode;
use crate::util::base64url_encode;

/// 测试私钥（PKCS#8, RSA-2048）。**仅供测试。**
const TEST_PKCS8_B64: &str = "\
MIIEvAIBADANBgkqhkiG9w0BAQEFAASCBKYwggSiAgEAAoIBAQCvk8RMlNUwt5cN\
8NWR0vdgMvotFqz3WP8eQZfkBovMcWoBuG7d2X15kOMjHp1S8bmb9HYTms14/od9\
Gy/3M0eCAPLB+SR0Yly9snw5Msl5kzW3Bm/800iP0/9IpqTuDL4rn/5SCXum2yc+\
WVT7kmGCP3ObJOkraETfklj/a5+V6UHdIOD6sdHcenrtOEzMN9neNCxTICfICkP0\
VtZWULBgX3q3I3yV5tD+k8NFFReLuycFypknZSVL2vVDRswi5whnH+LaDxIHYvZX\
y1PwpWWhqpEsGPfXjd2oXHCK8hG71H14scVCGgii46llFAOkkw0OR7JIVcLHTWXA\
wYUTz6n1AgMBAAECggEAF7snOxqavhzyY8X7d9xF51C8k7+O3VfAE7zV5NIlrxpd\
M6JUCdRNq1K0AJkwtndJmRSvqPKxdvsgIP69uvHJZn7uUN112e05j8JCHBELPk1x\
Yy2RkWCVXCtRQ+aSQEKkqYzUeQ4jEmhoo3q7i807NLmSzJNumJldRcKoGM3ZIg/N\
fL2WTMDgvH7JsD0U8RhWpzsHE8HNPjhauKpqLzUUQKX6WdregoM42/Scn+R3lsVO\
duHz740MTKcjvYSs/WM2Rc8YLZJqcL9l5gvBnq9AUh93Lec0uiqY0KKzCL5/LMds\
AUrD78vltNkzhqGXS2wlcWNzAUlwBrUdZvDZs7CY+QKBgQDmxNDwKW2lp+UpONqC\
lDVjdUBN6G+blXzBqpQUaE6aDRbTCm89eu32Yp7vk3WRur9lAZW/oHRy4c4hv7Db\
x8n98lO48fLQb3aT3Gm00mwWDreFfoIak9vFUMnKIzmV3E9aUJiHefZdFNQfh/zQ\
HIY3XosG84sPjuipoBmp6L8wnwKBgQDCxiVtT4byiculiz9pDOk1+XbHY6ubDTmc\
2NfFwWg8p7llRCo1H6llWuNL57F0urRHwxxRiTlMk+oEzJN/H0ih7Dt9EBdmjWiF\
suFQNQewSv5WQmpW5UTD6s5Ak31MUu5euoswad71k4aKKFCoUVQxmnDD7uBHl0T/\
wMG7ekz46wKBgA3iU4vPgkhQNKppQERQ9HFacZb+YBClSs9AGw6rQ+di2De0TgMN\
7VlAj71sSsuMx80k7kSMO9VZnpRLOzHN8Go19TGQZQKDYkf9H+MAMf6VvhbrCJre\
wH87oBhoTCOnEKMnbDXicZ8iDK0k+mHvBWg5aPXvGzPBbmAeQM0HMtGTAoGAOqDI\
PUoV9cWyjgcHNEjVnhCUR1piGk71y5eeA4G/LZrS/H9zMQBxzfd4ua1pRhi5EWzS\
hlyG6d0QMRefQkGZbFweX/vK/2yLTMtWdP0xbkh0nGRre/KRK2PvZ5K9wY6o/8yo\
/w6vNT6N3eJavP+WF0REs0WhLwuCknVkG1mp97cCgYBIM8asc68EnZXWYVjqdXHZ\
XYZQ3b5pV+d03ur8D4E/ZfQ7mQ5+3z7Q4w9BXj+OcDWr5vjFnRiP39tYo4t4GGw6\
ud0a6t2F7x6gbKoGe1lnxoahX5YvFyBjJJYzadkWMgNOBgt6T2NrRkEFzat5vUpp\
Egz0hNwij3U5a+LuiJ8D2A==";

/// 公开模数（与测试私钥配对，供 mock JWKS 使用）。
pub const TEST_JWK_N: &str = "r5PETJTVMLeXDfDVkdL3YDL6LRas91j_HkGX5AaLzHFqAbhu3dl9eZDjIx6dUvG5m_R2E5rNeP6HfRsv9zNHggDywfkkdGJcvbJ8OTLJeZM1twZv_NNIj9P_SKak7gy-K5_-Ugl7ptsnPllU-5Jhgj9zmyTpK2hE35JY_2uflelB3SDg-rHR3Hp67ThMzDfZ3jQsUyAnyApD9FbWVlCwYF96tyN8lebQ_pPDRRUXi7snBcqZJ2UlS9r1Q0bMIucIZx_i2g8SB2L2V8tT8KVloaqRLBj3143dqFxwivIRu9R9eLHFQhoIouOpZRQDpJMNDkeySFXCx01lwMGFE8-p9Q";

pub const TEST_KID: &str = "test-kid";

fn key_pair() -> ring::signature::RsaKeyPair {
    let der = base64url_decode(TEST_PKCS8_B64).expect("fixture key base64");
    ring::signature::RsaKeyPair::from_pkcs8(&der).expect("fixture key pkcs8")
}

/// 用测试私钥签一段 JWT（RS256）。
pub fn sign_id_token(claims: &Value) -> String {
    let header = base64url_encode(
        format!(r#"{{"alg":"RS256","kid":"{TEST_KID}","typ":"JWT"}}"#).as_bytes(),
    );
    let payload = base64url_encode(
        serde_json::to_string(claims)
            .expect("claims json")
            .as_bytes(),
    );
    let signing_input = format!("{header}.{payload}");
    let key = key_pair();
    let mut signature = vec![0u8; key.public().modulus_len()];
    key.sign(
        &ring::signature::RSA_PKCS1_SHA256,
        &ring::rand::SystemRandom::new(),
        signing_input.as_bytes(),
        &mut signature,
    )
    .expect("sign");
    format!("{signing_input}.{}", base64url_encode(&signature))
}

/// 一份标准 claims（身份校验单测与 mock 共用）。
pub fn claims_json(issuer: &str, client_id: &str, nonce: &str) -> Value {
    json!({
        "iss": issuer,
        "aud": client_id,
        "sub": "sub-fixture",
        "email": "user@example.com",
        "name": "Fixture User",
        "nonce": nonce,
        "iat": 1_700_000_000u64,
        "exp": 4_100_000_000u64
    })
}

/// mock 提供方的可变状态。
#[derive(Debug)]
pub struct MockState {
    /// 本流程的 nonce（由测试在触发回调前写入）。
    pub nonce: String,
    /// 注册回调会下发的 client_id。
    pub issued_client_id: String,
    pub subject: String,
    /// 是否授予套餐用量 scope（false = 只登录，未授权套餐）。
    pub grant_sharing: bool,
    /// token 响应注入：非 200 时返回错误体。
    pub token_status: u16,
    pub token_error_code: Option<String>,
    pub expires_in_secs: u64,
    pub earliest_refresh_at: Option<String>,
    /// 收到的 token / revoke 请求（表单键值），供断言。
    pub token_requests: Vec<Vec<(String, String)>>,
    pub revoke_requests: Vec<Vec<(String, String)>>,
    /// 计数：每次换码/刷新递增，用于生成不同的 token 串。
    pub token_serial: u64,
}

impl Default for MockState {
    fn default() -> Self {
        Self {
            nonce: String::new(),
            issued_client_id: "oaiapp_mock".to_string(),
            subject: "sub-fixture".to_string(),
            grant_sharing: true,
            token_status: 200,
            token_error_code: None,
            expires_in_secs: 3600,
            earliest_refresh_at: None,
            token_requests: Vec::new(),
            revoke_requests: Vec::new(),
            token_serial: 0,
        }
    }
}

/// 本地 OIDC 提供方（discovery / token / jwks / revoke / models）。
pub struct MockOidc {
    pub issuer: String,
    pub api_base: String,
    pub state: Arc<Mutex<MockState>>,
    shutdown: Arc<AtomicBool>,
    handle: Mutex<Option<JoinHandle<()>>>,
}

impl MockOidc {
    pub fn start() -> Self {
        let listener = TcpListener::bind(("127.0.0.1", 0)).expect("mock bind");
        listener
            .set_nonblocking(true)
            .expect("mock nonblocking");
        let port = listener.local_addr().expect("addr").port();
        let issuer = format!("http://127.0.0.1:{port}");
        let shutdown = Arc::new(AtomicBool::new(false));
        let state = Arc::new(Mutex::new(MockState::default()));

        let mock = Self {
            issuer: issuer.clone(),
            api_base: format!("{issuer}/v1"),
            state: Arc::clone(&state),
            shutdown,
            handle: Mutex::new(None),
        };
        let thread_issuer = issuer.clone();
        let thread_state = Arc::clone(&state);
        let thread_shutdown = Arc::clone(&mock.shutdown);
        let handle = std::thread::Builder::new()
            .name("mock-oidc".to_string())
            .spawn(move || {
                while !thread_shutdown.load(Ordering::Relaxed) {
                    match listener.accept() {
                        Ok((stream, _)) => {
                            let _ = stream.set_nonblocking(false);
                            serve(&thread_issuer, &thread_state, stream);
                        }
                        Err(e) if e.kind() == std::io::ErrorKind::WouldBlock => {
                            std::thread::sleep(Duration::from_millis(5));
                        }
                        Err(_) => break,
                    }
                }
            })
            .expect("mock thread");
        *mock.handle.lock().expect("handle") = Some(handle);
        mock
    }

    pub fn config(&self) -> crate::SiwcConfig {
        crate::SiwcConfig {
            issuer: self.issuer.clone(),
            api_base: self.api_base.clone(),
            callback_port: 0,
            http_timeout: Duration::from_secs(5),
            app_name: "QAQ-Harness 测试".to_string(),
            ..crate::SiwcConfig::default()
        }
    }

    pub fn with_state<T>(&self, f: impl FnOnce(&mut MockState) -> T) -> T {
        let mut guard = self.state.lock().expect("mock state");
        f(&mut guard)
    }

    pub fn token_requests(&self) -> Vec<Vec<(String, String)>> {
        self.with_state(|state| state.token_requests.clone())
    }

    pub fn revoke_requests(&self) -> Vec<Vec<(String, String)>> {
        self.with_state(|state| state.revoke_requests.clone())
    }

    /// 扮演浏览器：把一次性 code 送回回环回调。
    pub fn deliver_callback(
        &self,
        redirect_uri: &str,
        state: &str,
        client_id: Option<&str>,
        auth_error: Option<&str>,
    ) -> String {
        let mut url = format!("{redirect_uri}?state={state}");
        if let Some(error) = auth_error {
            url.push_str(&format!("&error={error}"));
        } else {
            url.push_str("&code=mock-code-1");
            if let Some(client_id) = client_id {
                url.push_str(&format!("&client_id={client_id}"));
            }
        }
        let target = url::Url::parse(&url).expect("callback url");
        let host = target.host_str().expect("host").to_string();
        let port = target.port().expect("port");
        let mut request_target = target.path().to_string();
        if let Some(query) = target.query() {
            request_target.push('?');
            request_target.push_str(query);
        }
        let mut stream = TcpStream::connect((host.as_str(), port)).expect("connect callback");
        let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
        let body = format!(
            "GET {request_target} HTTP/1.1\r\nHost: 127.0.0.1:{port}\r\nConnection: close\r\n\r\n"
        );
        stream.write_all(body.as_bytes()).expect("write callback");
        let mut response = String::new();
        let _ = stream.read_to_string(&mut response);
        response
    }
}

impl Drop for MockOidc {
    fn drop(&mut self) {
        self.shutdown.store(true, Ordering::Relaxed);
        if let Ok(mut guard) = self.handle.lock()
            && let Some(handle) = guard.take()
        {
            let _ = handle.join();
        }
    }
}

fn serve(issuer: &str, state: &Arc<Mutex<MockState>>, mut stream: TcpStream) {
    let Some((method, target, body)) = read_http(&mut stream) else {
        return;
    };
    let path = target.split('?').next().unwrap_or("").to_string();
    match (method.as_str(), path.as_str()) {
        ("GET", "/.well-known/openid-configuration") => json_response(
            &mut stream,
            200,
            &json!({
                "issuer": issuer,
                "authorization_endpoint": format!("{issuer}/api/accounts/authorize"),
                "token_endpoint": format!("{issuer}/api/accounts/oauth/token"),
                "jwks_uri": format!("{issuer}/jwks"),
                "revocation_endpoint": format!("{issuer}/api/accounts/oauth/revoke"),
            }),
        ),
        ("GET", "/jwks") => json_response(
            &mut stream,
            200,
            &json!({"keys": [{
                "kty": "RSA",
                "kid": TEST_KID,
                "alg": "RS256",
                "use": "sig",
                "n": TEST_JWK_N,
                "e": "AQAB",
            }]}),
        ),
        ("POST", "/api/accounts/oauth/token") => {
            let form = parse_form(&body);
            let (status, error_code, nonce, grant_sharing, expires_in, earliest, issued, subject, serial) = {
                let mut guard = state.lock().expect("state");
                guard.token_requests.push(form.clone());
                guard.token_serial += 1;
                (
                    guard.token_status,
                    guard.token_error_code.clone(),
                    guard.nonce.clone(),
                    guard.grant_sharing,
                    guard.expires_in_secs,
                    guard.earliest_refresh_at.clone(),
                    guard.issued_client_id.clone(),
                    guard.subject.clone(),
                    guard.token_serial,
                )
            };
            if status != 200 {
                let mut payload = json!({"error": "invalid_grant"});
                if let Some(code) = error_code {
                    payload = json!({"error": {"code": code, "message": "mock failure"}});
                }
                json_response(&mut stream, status, &payload);
                return;
            }
            let client_id = form_value(&form, "client_id").unwrap_or(issued.clone());
            let scope = if grant_sharing {
                crate::SCOPES
            } else {
                "openid profile email"
            };
            let id_token = sign_id_token(&json!({
                "iss": issuer,
                "aud": client_id,
                "sub": subject,
                "email": "user@example.com",
                "name": "Fixture User",
                "nonce": nonce,
                "iat": 1_700_000_000u64,
                "exp": 4_100_000_000u64
            }));
            let mut payload = json!({
                "id_token": id_token,
                "token_type": "Bearer",
                "expires_in": expires_in,
                "scope": scope,
            });
            if let Some(map) = payload.as_object_mut() {
                if grant_sharing {
                    map.insert("access_token".into(), json!(format!("at-{serial}")));
                    map.insert("refresh_token".into(), json!(format!("rt-{serial}")));
                }
                if let Some(earliest) = earliest {
                    map.insert("earliest_refresh_at".into(), json!(earliest));
                }
            }
            json_response(&mut stream, 200, &payload);
        }
        ("POST", "/api/accounts/oauth/revoke") => {
            state
                .lock()
                .expect("state")
                .revoke_requests
                .push(parse_form(&body));
            // 文档：空 200 即撤销成功。
            let _ = stream.write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        }
        ("GET", "/v1/models") => json_response(
            &mut stream,
            200,
            &json!({"models": [
                {"slug": "gpt-6.1-sol", "display_name": "GPT-6.1 Sol", "visibility": "list"},
                {"slug": "internal", "display_name": "Internal", "visibility": "hidden"},
                {"slug": "gpt-6.1-mini", "display_name": "GPT-6.1 mini", "visibility": "list"}
            ]}),
        ),
        _ => {
            let _ = stream.write_all(b"HTTP/1.1 404 Not Found\r\nContent-Length: 0\r\nConnection: close\r\n\r\n");
        }
    }
}

fn form_value(form: &[(String, String)], key: &str) -> Option<String> {
    form.iter()
        .find(|(k, _)| k == key)
        .map(|(_, v)| v.clone())
}

fn parse_form(body: &str) -> Vec<(String, String)> {
    url::form_urlencoded::parse(body.as_bytes())
        .map(|(k, v)| (k.into_owned(), v.into_owned()))
        .collect()
}

fn read_http(stream: &mut TcpStream) -> Option<(String, String, String)> {
    let _ = stream.set_read_timeout(Some(Duration::from_secs(5)));
    let mut buffer = Vec::new();
    let mut chunk = [0u8; 1024];
    let header_end = loop {
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            return None;
        }
        buffer.extend_from_slice(&chunk[..read]);
        if let Some(position) = buffer.windows(4).position(|w| w == b"\r\n\r\n") {
            break position + 4;
        }
    };
    let head = String::from_utf8_lossy(&buffer[..header_end]).to_string();
    let mut lines = head.split("\r\n");
    let request_line = lines.next()?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next()?.to_string();
    let target = parts.next()?.to_string();
    let content_length: usize = lines
        .filter_map(|line| {
            let (name, value) = line.split_once(':')?;
            name.eq_ignore_ascii_case("content-length")
                .then(|| value.trim().parse::<usize>().ok())?
        })
        .next()
        .unwrap_or(0);
    let mut body = buffer[header_end..].to_vec();
    while body.len() < content_length {
        let read = stream.read(&mut chunk).ok()?;
        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }
    body.truncate(content_length);
    Some((method, target, String::from_utf8_lossy(&body).to_string()))
}

fn json_response(stream: &mut TcpStream, status: u16, payload: &Value) {
    let body = serde_json::to_string(payload).expect("json");
    let head = format!(
        "HTTP/1.1 {status} OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nx-request-id: mock-{status}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    let _ = stream.write_all(head.as_bytes());
    let _ = stream.write_all(body.as_bytes());
}
