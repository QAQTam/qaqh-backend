//! 一次性探针：把**推理 wire** 上"只能靠通信才知道"的事实一次问清。
//!
//! 这几条我们此前都只能从参照实现或文档推断，而它们直接决定 UI 上的东西：
//! 1. CoT 落在哪个字段（`reasoning_content`？）——决定思考链能不能显示；
//! 2. **流式最后一帧有没有 `usage`**——决定"缓存命中率"那枚芯片会不会出现
//!    （我们生产 wire **没有**下发 `stream_options.include_usage`，所以这条是
//!    分叉点：端点默认带 ⇒ 不用改；不带 ⇒ 要么补这个开关，要么认了没有命中率）；
//! 3. `prompt_cache_hit_tokens` / `prompt_cache_miss_tokens` 是否真的会回来，
//!    以及**同前缀的第二次请求**是否命中（前缀缓存的行为本身）；
//! 4. `reasoning_effort` 的值域里到底认不认 `max`（我们刚把它放进了平台那一家的
//!    档位表，放一个它不认的档会以 400 打在这一次请求上）。
//!
//! ```text
//! cargo run -p qaqh-dsaccount --example probe_inference
//! ```
//!
//! 纪律：只读本地凭据、不打印 token（输出一律脱敏）、只发**推理**请求不写任何账
//! 户状态。花费：几条几十到几千 token 的短请求。

use std::time::Duration;

use qaqh_dsaccount::error::DsError;
use qaqh_dsaccount::http::{Http, HttpResponse};
use qaqh_dsaccount::protocol;
use serde_json::{Value, json};

/// 与 `specs.ts` 的 account 桶对齐（平台实测的模型 id）。
const MODEL: &str = "deepseek-flash";

/// 缓存实验的共享前缀：够长才谈得上"前缀缓存"，但也不必浪费余额。
const SHARED_PREFIX: &str = "你是一个只回答事实的助手。下面是一段用于测试前缀缓存的固定材料，\
请在回答时忽略它，只回答用户最后一句。材料开始：\
Q1 什么是前缀缓存？A1 端点把相同前缀的注意力键值缓存起来，下次同前缀直接复用。\
Q2 缓存命中如何计费？A2 命中的提示 token 按更低单价结算，未命中按标准价。\
Q3 命中率怎么算？A3 命中 token 除以（命中 + 未命中）提示 token。\
Q4 缓存何时失效？A4 前缀发生变化、或缓存过期后失效。\
Q5 为什么要让前缀稳定？A5 前缀一动，从变动点之后全部失效，命中率归零。\
材料结束。";

/// 前缀重复次数。第一次实测用一倍材料（≈200 token）**没有命中**，所以这里把
/// 共享前缀拉到几千 token——"命中率"是端点行为，只能靠试。
const PREFIX_REPEAT: usize = 12;

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

    let config = manager.config().clone();
    let url = format!(
        "{}/chat/completions",
        config.inference_origin.trim_end_matches('/')
    );
    let mut headers: Vec<(&'static str, String)> = config.identity.headers().to_vec();
    headers.push((protocol::AUTH_TOKEN_HEADER, token.clone()));
    let headers = pairs(&headers);
    let http = Http::new(Duration::from_secs(120));
    println!("端点：{url}\n模型：{MODEL}\n");

    // ① 最小流式：看形状是否被接受、CoT 在哪、末帧有没有 usage。
    let short = messages("用一句话回答：1 加 1 等于几？");
    report(
        "① 最小流式（reasoning_effort=low）",
        call(&http, &url, &headers, &body(&short, "low", false)),
        &token,
    );

    // ②③ 同前缀多次：第一轮是冷启动，之后几轮应当命中缓存。
    // 连打三轮（同一进程、背靠背），看命中是否**从来**不出现——出现在第几轮很关键：
    // 第 2 轮就该命中的话没命中，说明不是"要预热"，而是这个前缀压根没进缓存。
    let long_prefix = SHARED_PREFIX.repeat(PREFIX_REPEAT);
    for round in 1..=3 {
        let tail = format!("只回答两个字：第{round}轮");
        let messages = messages_in(&[
            ("user", long_prefix.as_str()),
            ("assistant", "好的，请提问。"),
            ("user", tail.as_str()),
        ]);
        report(
            &format!(
                "② 长前缀第 {round} 轮（{} 字前缀）",
                long_prefix.chars().count()
            ),
            call(&http, &url, &headers, &body(&messages, "low", false)),
            &token,
        );
    }

    // ④ 平台自报的最高档：我们刚把它放进档位表，这里验它认不认。
    report(
        "④ reasoning_effort=max（档位值域）",
        call(&http, &url, &headers, &body(&short, "max", false)),
        &token,
    );

    // ⑤ 显式要 usage：如果 ① 里没带，这里能确认"是不是缺这个开关"。
    report(
        "⑤ stream_options.include_usage=true（对照 ①）",
        call(&http, &url, &headers, &body(&short, "low", true)),
        &token,
    );
}

fn messages(user: &str) -> Value {
    messages_in(&[("user", user)])
}

fn messages_in(turns: &[(&str, &str)]) -> Value {
    Value::Array(
        turns
            .iter()
            .map(|(role, content)| json!({ "role": role, "content": content }))
            .collect(),
    )
}

fn body(messages: &Value, effort: &str, include_usage: bool) -> Value {
    let mut body = json!({
        "model": MODEL,
        "messages": messages,
        "stream": true,
        "reasoning_effort": effort,
    });
    if include_usage {
        body["stream_options"] = json!({ "include_usage": true });
    }
    body
}

fn call(
    http: &Http,
    url: &str,
    headers: &[(&str, &str)],
    body: &Value,
) -> Result<HttpResponse, DsError> {
    http.post_json(url, headers, body)
}

fn report(label: &str, result: Result<HttpResponse, DsError>, token: &str) {
    println!("── {label}");
    match result {
        Err(error) => println!("   传输失败：{error:?} / {error}"),
        Ok(response) if !(200..300).contains(&response.status) => {
            println!("   HTTP {} / {}", response.status, response.diagnostic())
        }
        Ok(response) => {
            let stats = parse_stream(&response.body);
            println!(
                "   HTTP {} / 帧 {} / 顶层键 {:?}",
                response.status, stats.frames, stats.keys
            );
            println!(
                "   内容 {} 字 / 思考 {} 字 / finish_reason {:?}",
                stats.content_chars, stats.reasoning_chars, stats.finish_reason
            );
            match &stats.usage {
                Some(usage) => {
                    let text = usage.to_string().replace(token, "<redacted>");
                    println!("   末帧 usage（原样）：{text}");
                    println!(
                        "   → 命中字段 {} / 未命中字段 {}",
                        usage
                            .get("prompt_cache_hit_tokens")
                            .map_or("缺失".into(), |v| v.to_string()),
                        usage
                            .get("prompt_cache_miss_tokens")
                            .map_or("缺失".into(), |v| v.to_string())
                    );
                }
                None => println!("   **没有任何帧带 usage**（⇒ 前端那枚「缓存命中」芯片不会出现）"),
            }
        }
    }
    println!();
}

#[derive(Default)]
struct StreamStats {
    frames: usize,
    keys: Vec<String>,
    content_chars: usize,
    reasoning_chars: usize,
    finish_reason: Option<String>,
    usage: Option<Value>,
}

fn parse_stream(body: &str) -> StreamStats {
    let mut stats = StreamStats::default();
    for line in body.lines() {
        let Some(payload) = line.strip_prefix("data: ") else {
            continue;
        };
        if payload.trim() == "[DONE]" {
            continue;
        }
        let Ok(frame) = serde_json::from_str::<Value>(payload) else {
            continue;
        };
        stats.frames += 1;
        if let Some(object) = frame.as_object() {
            for key in object.keys() {
                if !stats.keys.iter().any(|seen| seen == key) {
                    stats.keys.push(key.clone());
                }
            }
        }
        if let Some(usage) = frame.get("usage").filter(|value| !value.is_null()) {
            stats.usage = Some(usage.clone());
        }
        if let Some(choice) = frame.get("choices").and_then(|c| c.get(0)) {
            if let Some(reason) = choice.get("finish_reason").and_then(Value::as_str) {
                stats.finish_reason = Some(reason.to_string());
            }
            if let Some(delta) = choice.get("delta") {
                // 字段名是本次要问的事实之一：只报存在的那个。
                for key in ["reasoning_content", "reasoning", "content"] {
                    if let Some(text) = delta.get(key).and_then(Value::as_str) {
                        if key == "content" {
                            stats.content_chars += text.chars().count();
                        } else {
                            stats.reasoning_chars += text.chars().count();
                        }
                    }
                }
            }
        }
    }
    stats
}

fn pairs<'a>(headers: &'a [(&'static str, String)]) -> Vec<(&'static str, &'a str)> {
    headers
        .iter()
        .map(|(name, value)| (*name, value.as_str()))
        .collect()
}

/// 只露出长度与首 4 字符。
fn mask(token: &str) -> String {
    let head: String = token.chars().take(4).collect();
    format!("{head}…（{} 字符）", token.chars().count())
}
