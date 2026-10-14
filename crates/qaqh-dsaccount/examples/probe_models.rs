//! 一次性探针：把**远端模型目录**拉一次并原样打出来。
//!
//! 为什么要它：我们在三处依赖平台自己声明的模型事实——
//! 1. 模型清单（编排器的选择器直接用它，`dsaccount.models`）；
//! 2. `effort.supported_levels`（我们据此决定 DeepSeek 这一家能选哪些档）；
//! 3. 上下文窗口（`DSACCOUNT_CONTEXT_LENGTH`，本地压缩的分母）。
//! 前两处此前只凭一次记录，值域一类的事必须能重放。
//!
//! ```text
//! cargo run -p qaqh-dsaccount --example probe_models
//! ```
//!
//! 纪律与 `probe_logout` 相同：**只读**本地凭据、不打印 token（含响应体脱敏）、
//! 不发任何写请求。

use qaqh_dsaccount::error::DsError;

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

    match manager.models() {
        Ok(catalog) => {
            // 响应体脱敏后再打印：万一平台在某一层回显凭据，也不进终端历史。
            let text = serde_json::to_string_pretty(&catalog)
                .unwrap_or_else(|_| catalog.to_string())
                .replace(&token, "<redacted>");
            println!("\n远端模型目录：\n{text}");
            println!("\n模型数：{}", model_count(&catalog));
        }
        Err(error) => println!("拉取失败：{}", describe(&error)),
    }
}

/// 类别 + 可展示文案（`Display` 对业务码有专门的措辞）。
fn describe(error: &DsError) -> String {
    format!("{:?} / {}", error.code(), error)
}

fn model_count(catalog: &serde_json::Value) -> usize {
    catalog
        .get("data")
        .and_then(|data| data.as_array())
        .map(|entries| entries.len())
        .unwrap_or(0)
}

/// 只露出长度与首 4 字符：足够确认"就是那把"，不足以泄密。
fn mask(token: &str) -> String {
    let head: String = token.chars().take(4).collect();
    format!("{head}…（{} 字符）", token.chars().count())
}
