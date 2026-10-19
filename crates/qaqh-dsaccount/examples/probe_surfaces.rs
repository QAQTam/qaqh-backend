//! 一次性探针：**账号凭据**（`x-dsh-auth-token`）在各官方面上到底能不能用。
//!
//! 为什么要问：本仓的 DeepSeek 账号额度此前只验证过 `POST /chat/completions`
//! 与 `GET /models`。官方文档另有三类面，产品上全都要用：
//!
//! 1. `POST /responses`（Responses API）——「对话中间插入工具调用」只有它和
//!    Anthropic 面支持；缓存命中走 `input_tokens_details.cached_tokens`；
//! 2. `POST /v1/tools/search` + `/v1/tools/fetch`——官方搜索/抓取；
//! 3. `GET /files`（Files API）——`file_id` 图片复用。
//!
//! 还有一条只有真机才能回答：Anthropic 兼容面（`/anthropic/v1/messages`）用的是
//! `x-api-key`，账号 token 是另一种凭据——它能当 `x-api-key` 用吗？
//!
//! ```text
//! cargo run -p qaqh-dsaccount --example probe_surfaces
//! ```
//!
//! 纪律：只读本地凭据、不打印 token（输出一律脱敏）、不做任何写操作。
//! 花费：两次 `max_output_tokens: 1` 的短请求 + 一次 count=1 的搜索；其余是列表/抓取。
//! 各面之间互不影响——某一条 401 只说明那个面不认账号 token，不代表凭据失效。

use std::time::Duration;

use qaqh_dsaccount::http::Http;
use qaqh_dsaccount::protocol;
use serde_json::{Value, json};

const MODEL: &str = "deepseek-flash";

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
    println!("凭据：{}（本机已登录）\n", mask(&token));

    let config = manager.config().clone();
    let base = config.inference_origin.trim_end_matches('/').to_string();
    let http = Http::new(Duration::from_secs(60));
    let identity = config.identity.headers().to_vec();
    let account = || {
        headers(
            &identity,
            Some((protocol::AUTH_TOKEN_HEADER, token.clone())),
        )
    };

    // ── ① 官方 Search ──
    report(
        &http,
        &format!("{base}{}", protocol::WEB_SEARCH_PATH),
        account(),
        &json!({ "query": "DeepSeek", "count": 1 }),
        &token,
        "results[] 条数 / 首条标题",
        |body| {
            let results = body.get("results").and_then(Value::as_array);
            let first = results
                .and_then(|items| items.first())
                .and_then(|item| item.get("title"))
                .and_then(Value::as_str)
                .unwrap_or("(无)");
            format!("{} 条 / 首条 {first}", results.map_or(0, Vec::len))
        },
    );

    // ── ② 官方 Fetch ──
    report(
        &http,
        &format!("{base}{}", protocol::WEB_FETCH_PATH),
        account(),
        &json!({ "url": "https://example.com" }),
        &token,
        "title / 正文字符数",
        |body| {
            format!(
                "{} / {} 字符",
                body.get("title").and_then(Value::as_str).unwrap_or("(无)"),
                body.get("content")
                    .and_then(Value::as_str)
                    .map_or(0, |text| text.chars().count())
            )
        },
    );

    // ── ③ Responses（账号 token）──
    report(
        &http,
        &format!("{base}/responses"),
        account(),
        &json!({
            "model": MODEL,
            "input": "ping",
            "max_output_tokens": 1,
            "stream": false,
        }),
        &token,
        "usage.cached_tokens 是否回传",
        |body| {
            let usage = body.get("usage").cloned().unwrap_or(Value::Null);
            let cached = usage
                .get("input_tokens_details")
                .and_then(|details| details.get("cached_tokens"));
            format!(
                "usage={usage} / cached_tokens={}",
                cached.map_or("缺失".to_string(), Value::to_string)
            )
        },
    );

    // ── ④ Responses（Bearer 对照组）──
    report(
        &http,
        &format!("{base}/responses"),
        headers(
            &identity,
            Some(("authorization", format!("Bearer {token}"))),
        ),
        &json!({
            "model": MODEL,
            "input": "ping",
            "max_output_tokens": 1,
            "stream": false,
        }),
        &token,
        "Bearer 变体（只用于区分「面不认账号 token」与「头用错」）",
        |body| {
            body.get("error")
                .and_then(|error| error.get("message"))
                .and_then(Value::as_str)
                .unwrap_or("(无 error 字段)")
                .to_string()
        },
    );

    // ── ⑤ Anthropic 兼容面：文档只认 `x-api-key` ──
    report(
        &http,
        &format!("{base}/anthropic/v1/messages"),
        vec![("x-api-key", token.clone())],
        &json!({
            "model": MODEL,
            "max_tokens": 1,
            "messages": [{"role": "user", "content": "ping"}],
        }),
        &token,
        "x-api-key 能否携带账号 token",
        |body| {
            format!(
                "usage={}",
                body.get("usage").cloned().unwrap_or(Value::Null)
            )
        },
    );

    // ── ⑥ Files 列表（只读，不上传）──
    let files_url = format!("{base}/files?limit=1");
    println!("── GET {files_url}（Files API 列表）");
    match http.get(&files_url, &pairs(&account())) {
        Err(error) => println!("   传输失败：{error:?}\n"),
        Ok(response) if !(200..300).contains(&response.status) => {
            println!("   HTTP {} / {}\n", response.status, response.diagnostic())
        }
        Ok(response) => {
            let body = serde_json::from_str::<Value>(&response.body).unwrap_or(Value::Null);
            println!(
                "   HTTP {} / 对象数 {}\n",
                response.status,
                body.get("data")
                    .and_then(Value::as_array)
                    .map_or(0, Vec::len)
            );
        }
    }

    println!(
        "说明：各面的计费口径见官方文档；\n\
         这里只回答「账号凭据在这个面上是否被接受、字段是否回传」。"
    );
}

fn headers(
    identity: &[(&'static str, String)],
    extra: Option<(&'static str, String)>,
) -> Vec<(&'static str, String)> {
    let mut headers = identity.to_vec();
    if let Some(header) = extra {
        headers.push(header);
    }
    headers
}

fn pairs<'a>(headers: &'a [(&'static str, String)]) -> Vec<(&'static str, &'a str)> {
    headers
        .iter()
        .map(|(name, value)| (*name, value.as_str()))
        .collect()
}

fn report(
    http: &Http,
    url: &str,
    headers: Vec<(&'static str, String)>,
    payload: &Value,
    token: &str,
    what: &str,
    project: impl Fn(&Value) -> String,
) {
    println!("── POST {url}（{what}）");
    match http.post_json(url, &pairs(&headers), payload) {
        Err(error) => println!("   传输失败：{error:?} / {error}\n"),
        Ok(response) if !(200..300).contains(&response.status) => {
            println!(
                "   HTTP {} / {}\n",
                response.status,
                response.diagnostic().replace(token, "<redacted>")
            );
        }
        Ok(response) => {
            let body = serde_json::from_str::<Value>(&response.body).unwrap_or(Value::Null);
            println!("   HTTP {} / {}\n", response.status, project(&body));
        }
    }
}

/// 只露出长度与首 4 字符。
fn mask(token: &str) -> String {
    let head: String = token.chars().take(4).collect();
    format!("{head}…（{} 字符）", token.chars().count())
}
