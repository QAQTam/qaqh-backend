//! 一次性探针：把「登出」请求的**形状**在真平台上试一遍。
//!
//! 为什么要它：平台对我们发出的登出请求回了
//! `400` + `Authorization Failed (invalid token)`，而**同一个凭据**同时打得通
//! `users/current` 与余额接口——所以要么是请求形状，要么是这个端点对凭据的要求
//! 与其它端点不同。矩阵一次跑完，省得来回猜。
//!
//! ```text
//! cargo run -p qaqh-dsaccount --example probe_logout
//! ```
//!
//! 三条纪律：
//! - **只读**本地凭据（解密取 token，不改写、不删除、不重新登录）；
//! - 不打印 token（只打掩码）；
//! - 任何一种形状成功都会**真的撤销授权**，所以顺序按"最可能正确"排、成功即停。

use std::time::Duration;

use qaqh_dsaccount::client::DsClientConfig;
use qaqh_dsaccount::http::{Http, HttpResponse};
use qaqh_dsaccount::protocol;

/// 一种请求形状（名字 + 带哪些头 + 怎么发）。
#[derive(Clone, Copy, PartialEq, Eq)]
enum Kind {
    BarePost,
    BarePostTokenOnly,
    Get,
    JsonPost,
    BarePostNoCredential,
}

impl Kind {
    fn label(self) -> &'static str {
        match self {
            Kind::BarePost => "A 裸 POST（参照实现的形状：身份头 + 凭据头，无正文/无 content-type",
            Kind::BarePostTokenOnly => "B 裸 POST + 只带凭据头（不发五个客户端身份头）",
            Kind::Get => "C GET 同路径（万一它要的是 GET）",
            Kind::JsonPost => "D JSON POST + 空对象（本次被拒的那种写法，留作对照）",
            Kind::BarePostNoCredential => {
                "E 裸 POST 但**不带凭据**（对照：看平台是否对谁都回同一句话）"
            }
        }
    }

    fn send(
        self,
        http: &Http,
        url: &str,
        identity: &[(&'static str, String)],
        token: &str,
    ) -> Result<HttpResponse, qaqh_dsaccount::error::DsError> {
        let mut headers: Vec<(&'static str, String)> = match self {
            Kind::BarePostTokenOnly => Vec::new(),
            _ => identity.to_vec(),
        };
        if self != Kind::BarePostNoCredential {
            headers.push((protocol::AUTH_TOKEN_HEADER, token.to_string()));
        }
        match self {
            Kind::Get => http.get(url, &pairs(&headers)),
            Kind::JsonPost => http.post_json(url, &pairs(&headers), &serde_json::json!({})),
            _ => http.post_empty(url, &pairs(&headers)),
        }
    }
}

const SHAPES: [Kind; 5] = [
    Kind::BarePost,
    Kind::BarePostTokenOnly,
    Kind::Get,
    Kind::JsonPost,
    Kind::BarePostNoCredential,
];

fn main() {
    qaqh_dsaccount::global::install_default();
    let Some(manager) = qaqh_dsaccount::global::manager() else {
        eprintln!("账号管理器没装配上（取不到凭据位置）");
        return;
    };
    let token = match manager.token() {
        Ok(token) => token,
        Err(error) => {
            eprintln!(
                "本机没有可用凭据：先在应用里登录 DeepSeek 账号再跑这个探针（{}）",
                error.message()
            );
            return;
        }
    };
    println!("凭据：{}（本机已登录）", mask(&token));

    let config: &DsClientConfig = manager.config();
    let url = format!("{}{}", config.platform_origin, protocol::USERS_LOGOUT_PATH);
    let identity: Vec<(&'static str, String)> = config.identity.headers().to_vec();

    let http = Http::new(Duration::from_secs(30));
    let mut winner: Option<&str> = None;
    for shape in SHAPES {
        match shape.send(&http, &url, &identity, &token) {
            Ok(response) => {
                let status = response.status;
                println!(
                    "{:<70} → HTTP {status} / code {:?} / {}",
                    shape.label(),
                    response.business_code(),
                    response.diagnostic()
                );
                if (200..300).contains(&status) && winner.is_none() {
                    winner = Some(shape.label());
                    break; // 撤销已经发生，再试别的没有意义。
                }
            }
            Err(error) => println!("{:<70} → 传输失败: {}", shape.label(), error.message()),
        }
    }

    match winner {
        Some(name) => println!("\n结论：可用形状是「{name}」。远端授权已撤销。"),
        None => println!(
            "\n结论：五种形状全被拒。那问题不在形状——最可能是平台对这个端点要的凭据/会话\n\
             与我们持有的账号 grant 不同（对照 E 那一行：若它也回同一句话，说明这句话是通用拒绝）。"
        ),
    }
}

fn pairs<'a>(headers: &'a [(&'static str, String)]) -> Vec<(&'static str, &'a str)> {
    headers
        .iter()
        .map(|(name, value)| (*name, value.as_str()))
        .collect()
}

/// 只露出长度与首 4 字符：足够确认"就是那把"，不足以泄密。
fn mask(token: &str) -> String {
    let head: String = token.chars().take(4).collect();
    format!("{head}…（{} 字符）", token.chars().count())
}
