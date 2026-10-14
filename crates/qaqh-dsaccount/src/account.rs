//! 账户面只读模型与解析：资料、**余额**、**赠额**。
//!
//! 取值纪律与参照实现一致、但**多留两样东西**：
//!
//! - 金额一律**保留服务端原串**。平台用的是 big.js 语法（可省略整数/小数部分、
//!   允许十进制指数：`0E-16`、`5.0000000000000000`、`1e+3`），`0E-16` 就是 0；
//!   折算成 f64 只用于展示与门限比较（[`decimal_to_f64`]），**权威永远是原串**。
//! - 参照实现的 zod schema 只解析 `normal_wallets` / `bonus_wallets`，于是
//!   `total_costs`（累计消费）与每个钱包的 `token_estimation`（额度折算）被静默
//!   丢弃。这两个字段我们**收下**（[`Summary`] / [`Wallet`]）。
//!
//! 实测来源：`scripts/probe-dsh-account-flow.mjs`（2026-10-14）。回归夹具的形状照
//! 那次原始返回，**值已脱敏/替换**（真实账号 PII 与消费额不进仓库）。

use serde::Deserialize;
use serde_json::Value;

use crate::error::{DsCode, DsError};

// ── 金额语法 ──

/// 平台金额/余额的十进制语法（与 Platform Web 客户端交给 big.js 的同一套）。
///
/// 手写状态机而不是引正则：本 crate 不需要正则依赖，而这条语法很短。
pub fn is_decimal(value: &str) -> bool {
    let bytes = value.as_bytes();
    let mut i = 0usize;
    if i < bytes.len() && bytes[i] == b'-' {
        i += 1;
    }
    let int_start = i;
    while i < bytes.len() && bytes[i].is_ascii_digit() {
        i += 1;
    }
    let int_digits = i - int_start;
    let mut frac_digits = 0usize;
    if i < bytes.len() && bytes[i] == b'.' {
        i += 1;
        let frac_start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        frac_digits = i - frac_start;
    }
    if int_digits == 0 && frac_digits == 0 {
        return false;
    }
    if i < bytes.len() && matches!(bytes[i], b'e' | b'E') {
        i += 1;
        if i < bytes.len() && matches!(bytes[i], b'+' | b'-') {
            i += 1;
        }
        let exp_start = i;
        while i < bytes.len() && bytes[i].is_ascii_digit() {
            i += 1;
        }
        if i == exp_start {
            return false;
        }
    }
    i == bytes.len()
}

/// 展示/门限用的数值投影。非有限值（`NaN` / `Infinity` / 超范围）一律 `None`——
/// 「拿不到数」绝不能表现成「余额是 0」。
pub fn decimal_to_f64(value: &str) -> Option<f64> {
    if !is_decimal(value) {
        return None;
    }
    value
        .parse::<f64>()
        .ok()
        .filter(|number| number.is_finite())
}

// ── 余额 ──

/// 一个钱包条目（充值或赠金）。
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Wallet {
    /// 服务端给的币种（参照实现只认 `CNY` / `USD`；我们收字符串，未知币种照样展示）。
    pub currency: String,
    /// 余额**原串**（big.js 语法，见 [`is_decimal`]）。
    pub balance: String,
    /// 额度折算（参照实现没解析这个字段；`token_estimation` 是它的线上名字）。
    #[serde(default)]
    pub token_estimation: Option<String>,
}

/// 累计消费条目。
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct TotalCost {
    /// 币种。
    pub currency: String,
    /// 金额**原串**。
    pub amount: String,
}

/// `GET /api/v0/users/get_user_summary` 的载荷。
#[derive(Debug, Clone, Default, Deserialize, PartialEq)]
pub struct Summary {
    /// 充值余额。
    #[serde(default)]
    pub normal_wallets: Vec<Wallet>,
    /// 赠金余额（与充值**分开**展示，促销额度不得冒充充值）。
    #[serde(default)]
    pub bonus_wallets: Vec<Wallet>,
    /// 累计消费（参照实现的 schema 丢掉了它）。
    #[serde(default)]
    pub total_costs: Vec<TotalCost>,
}

impl Summary {
    /// 指定币种的充值钱包。
    pub fn recharge(&self, currency: &str) -> Option<&Wallet> {
        self.normal_wallets
            .iter()
            .find(|wallet| wallet.currency == currency)
    }

    /// 指定币种的赠金钱包。
    pub fn bonus(&self, currency: &str) -> Option<&Wallet> {
        self.bonus_wallets
            .iter()
            .find(|wallet| wallet.currency == currency)
    }

    /// 指定币种的累计消费。
    pub fn total_cost(&self, currency: &str) -> Option<&TotalCost> {
        self.total_costs
            .iter()
            .find(|cost| cost.currency == currency)
    }

    /// 是否还有可用额度（充值与赠金都算，任一为正即可）。
    ///
    /// 解析不出的余额**不算负数**：宁可判「未知」也不凭空说「没额度」——
    /// 后者会让 UI 在平台已经扣费成功时提示用户余额不足。
    pub fn has_usable_balance(&self) -> bool {
        self.normal_wallets
            .iter()
            .chain(self.bonus_wallets.iter())
            .filter_map(|wallet| decimal_to_f64(&wallet.balance))
            .any(|balance| balance > 0.0)
    }
}

// ── 资料 ──

#[derive(Debug, Deserialize)]
struct RawUser {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    email: Option<String>,
    #[serde(default)]
    mobile: Option<String>,
    #[serde(default)]
    mobile_number: Option<String>,
    #[serde(default)]
    currency: Option<String>,
    #[serde(default)]
    id_profile: Option<RawIdentity>,
}

#[derive(Debug, Deserialize)]
struct RawIdentity {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    picture: Option<String>,
    #[serde(default)]
    provider: Option<String>,
    #[serde(default)]
    locale: Option<String>,
}

/// 账户资料投影（只带 UI 要的字段；响应里还有 `status`/`feature_gates`/
/// `balance_alert`/`register_client_type` 等，暂不投影，见 crate 报告）。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct AccountProfile {
    /// 稳定账号 ID（`getPlatformSession` 的 userId 来源）。
    pub id: Option<String>,
    /// 昵称。
    pub name: Option<String>,
    /// 头像 URL。
    pub avatar_url: Option<String>,
    /// 联系方式：**手机优先、邮箱兜底**（微信登录的账号 `email` 是空串，
    /// 而两边的值都由服务端自己掩码，我们原样透传）。
    pub contact: Option<String>,
    /// 登录方式（如 `WECHAT`）——用于"这个账号怎么登的"这类文案。
    pub provider: Option<String>,
    /// 账号语言。
    pub locale: Option<String>,
    /// 结算币种（`CNY` / `USD`）。
    pub currency: Option<String>,
}

fn non_empty(value: &Option<String>) -> Option<&str> {
    value.as_deref().filter(|text| !text.is_empty())
}

/// 把 `users/current`（或 `auth_exchange` 里的 `user`）投影成 [`AccountProfile`]。
pub fn project_profile(user: &Value) -> Result<AccountProfile, DsError> {
    let raw: RawUser = serde_json::from_value(user.clone())
        .map_err(|error| DsError::new(DsCode::Protocol, format!("资料形状不符: {error}")))?;
    let identity = raw.id_profile.as_ref();
    Ok(AccountProfile {
        id: non_empty(&raw.id).map(str::to_string),
        name: identity
            .and_then(|value| non_empty(&value.name))
            .map(str::to_string),
        avatar_url: identity
            .and_then(|value| non_empty(&value.picture))
            .map(str::to_string),
        contact: non_empty(&raw.mobile)
            .or_else(|| non_empty(&raw.mobile_number))
            .or_else(|| non_empty(&raw.email))
            .map(str::to_string),
        provider: identity
            .and_then(|value| non_empty(&value.provider))
            .map(str::to_string),
        locale: identity
            .and_then(|value| non_empty(&value.locale))
            .map(str::to_string),
        currency: non_empty(&raw.currency).map(str::to_string),
    })
}

// ── 赠额 ──

/// 一笔赠额（字段名照线上，不做重命名，便于与平台文档对照）。
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct Bonus {
    /// 订单 ID（`ack_bonus_notified` 用它确认已展示）。
    pub order_id: String,
    /// 活动标识。
    #[serde(default)]
    pub campaign: String,
    /// 金额原串。
    pub amount: String,
    /// 币种。
    pub currency: String,
    /// 发放时间。
    #[serde(default)]
    pub granted_at: String,
    /// 到期时间。
    #[serde(default)]
    pub expires_at: String,
    /// 服务端文案（本地化由服务端负责，我们原样展示）。
    #[serde(default)]
    pub msg: String,
}

/// `get_unnotified_bonuses` 的载荷：一个数组（可能为空）。
pub fn parse_bonuses(value: &Value) -> Result<Vec<Bonus>, DsError> {
    serde_json::from_value(value.clone())
        .map_err(|error| DsError::new(DsCode::Protocol, format!("赠额形状不符: {error}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 实测余额返回（值已替换，形状照原样；`total_costs`/`token_estimation`
    /// 是参照实现没有解析的两个字段）。
    const SUMMARY: &str = r#"{"normal_wallets":[{"currency":"CNY","balance":"0E-16","token_estimation":"0"}],"bonus_wallets":[{"currency":"CNY","balance":"0","token_estimation":"0"}],"total_costs":[{"currency":"CNY","amount":"12.3400000000000000"}]}"#;

    /// 实测用户对象（值已脱敏：ID/手机号/昵称/头像都是占位）。
    const USER: &str = r#"{"id":"00000000-0000-4000-8000-000000000000","email":"","mobile_number":"138******00","area_code":"+86","status":0,"id_profile":{"provider":"WECHAT","id":"11111111-1111-4111-8111-111111111111","picture":"https://static.deepseek.com/user-avatar/example","name":"测试用户","locale":"zh_CN","email":null},"feature_gates":{"PAYMENT":true,"APPLE_PAY_ENABLED":true},"currency":"CNY","register_client_type":"PLATFORM","balance_alert":{"CNY":{"enabled":false,"alert_bound":"1"}}}"#;

    #[test]
    fn decimal_grammar_accepts_what_the_platform_sends() {
        for valid in [
            "0",
            "0E-16",
            "1e+3",
            "12.3400000000000000",
            "-1.5",
            "1.",
            ".5",
            "-.5",
            "1E3",
        ] {
            assert!(is_decimal(valid), "{valid} 应当合法");
        }
        for invalid in [
            "", "-", ".", "abc", "1e", "1.2.3", "0x10", "NaN", "Infinity", "+1", " 1",
        ] {
            assert!(!is_decimal(invalid), "{invalid} 应当非法");
        }
    }

    #[test]
    fn decimal_projection_keeps_authority_in_the_raw_string() {
        // `0E-16` 就是 0：不能因为「看起来不是 0」就让 UI 显示有余额。
        assert_eq!(decimal_to_f64("0E-16"), Some(0.0));
        assert_eq!(decimal_to_f64("1e+3"), Some(1000.0));
        assert_eq!(decimal_to_f64("12.3400000000000000"), Some(12.34));
        assert_eq!(decimal_to_f64("oops"), None);
        assert_eq!(decimal_to_f64("1e400"), None, "超范围必须 None，不能是 inf");
    }

    #[test]
    fn summary_keeps_the_raw_amounts_and_the_fields_the_reference_drops() {
        let summary: Summary = serde_json::from_str(SUMMARY).expect("实测载荷必须能解");
        let recharge = summary.recharge("CNY").expect("充值钱包");
        assert_eq!(recharge.balance, "0E-16", "原串必须原样保留");
        assert_eq!(recharge.token_estimation.as_deref(), Some("0"));
        assert_eq!(summary.bonus("CNY").expect("赠金钱包").balance, "0");
        assert_eq!(
            summary.total_cost("CNY").expect("累计消费").amount,
            "12.3400000000000000",
            "参照实现的 schema 丢掉了 total_costs，我们必须留着"
        );
        assert!(!summary.has_usable_balance(), "0E-16 与 0 都不算有额度");
        assert!(summary.recharge("USD").is_none());
    }

    #[test]
    fn a_positive_wallet_counts_as_usable() {
        let summary: Summary = serde_json::from_str(
            r#"{"normal_wallets":[{"currency":"CNY","balance":"0"}],"bonus_wallets":[{"currency":"CNY","balance":"10.00"}]}"#,
        )
        .expect("载荷");
        assert!(summary.has_usable_balance());
        let unparsable: Summary =
            serde_json::from_str(r#"{"normal_wallets":[{"currency":"CNY","balance":"???"}]}"#)
                .expect("形状合法即可");
        assert!(!unparsable.has_usable_balance(), "解不出数不等于有额度");
    }

    #[test]
    fn profile_uses_the_masked_mobile_before_the_empty_email() {
        let user: Value = serde_json::from_str(USER).expect("用户对象");
        let profile = project_profile(&user).expect("投影");
        assert_eq!(
            profile.contact.as_deref(),
            Some("138******00"),
            "微信账号 email 为空，必须落到手机"
        );
        assert_eq!(profile.name.as_deref(), Some("测试用户"));
        assert_eq!(profile.provider.as_deref(), Some("WECHAT"));
        assert_eq!(profile.currency.as_deref(), Some("CNY"));
        assert_eq!(profile.locale.as_deref(), Some("zh_CN"));
        assert!(profile.avatar_url.is_some());
        assert!(profile.id.is_some());
    }

    #[test]
    fn bonuses_map_every_wire_field_and_tolerate_an_empty_list() {
        let empty: Value = serde_json::from_str("[]").expect("空数组");
        assert!(parse_bonuses(&empty).expect("空列表合法").is_empty());

        let list: Value = serde_json::from_str(
            r#"[{"order_id":"22222222-2222-4222-8222-222222222222","campaign":"welcome","amount":"10.00","currency":"CNY","granted_at":"2026-10-01T00:00:00Z","expires_at":"2026-11-01T00:00:00Z","msg":"新用户赠金"}]"#,
        )
        .expect("赠额数组");
        let bonuses = parse_bonuses(&list).expect("解析");
        assert_eq!(bonuses.len(), 1);
        assert_eq!(bonuses[0].amount, "10.00");
        assert_eq!(bonuses[0].currency, "CNY");
        assert_eq!(bonuses[0].msg, "新用户赠金");
        assert!(is_decimal(&bonuses[0].amount));
    }

    #[test]
    fn profile_projection_ignores_the_token_field_the_endpoint_echoes() {
        // 实测：`GET /auth-api/v0/users/current` 的 biz_data 里**回显了一个 token 字段**。
        // 投影必须看不到它，且不得因为多出来就解不出（serde 忽略未知字段是这里的护栏）。
        let user: Value = serde_json::from_str(
            r#"{"id":"00000000-0000-4000-8000-000000000000","token":"SECRET-SHOULD-NOT-SURVIVE","id_profile":{"name":"测试用户"}}"#,
        )
        .expect("用户对象");
        let profile = project_profile(&user).expect("投影");
        assert_eq!(profile.name.as_deref(), Some("测试用户"));
        assert!(
            !format!("{profile:?}").contains("SECRET-SHOULD-NOT-SURVIVE"),
            "投影不得携带 token：{profile:?}"
        );
    }
}
