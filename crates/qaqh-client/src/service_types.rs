//! `siwc.*` / `dsaccount.*` 服务面响应的 **typed DTO**。
//!
//! ## 为什么在这里
//!
//! 这两个账号面的服务方法返回的曾经都是 `serde_json::Value`（`qaqh-siwc` /
//! `qaqh-dsaccount` 的 `status()` / `profiles()` 直接拼 JSON）。结果每个壳层
//! 只能**手抄一份结构**：桌面端抄了 `SiwcSessionStatus` / `SiwcModelEntry` /
//! `SiwcFlowState`，TUI 一旦接上登录流程就会再抄第二份，第三份由第三个客户端
//! 负责——三份手抄里只要有一份把 `usageUrl` 拼成 `usage_url`，症状是界面上
//! 一个空链接，归因成本极高。
//!
//! 形状的真源是 daemon 的投影代码（`qaqh-runtime/src/service.rs` 的
//! `siwc.*` / `dsaccount.*` 分支 + 两个 crate 里的 `to_public_json` / `to_json`）。
//! 逐字段说明见 `docs/spec-service-surface-siwc-dsaccount.md`。
//!
//! ## 命名口径（**已钉死，别猜**）
//!
//! - `siwc.*`：**全 camelCase**（`usageUrl` / `expiresAtMs` / `clientId` /
//!   `savedAtMs` / `displayName` / `profileId`），枚举值是 snake_case
//!   （`reauth_required`）。
//! - `dsaccount.*` 的**外层**：camelCase（daemon 转发层做了一次归一）。
//! - `dsaccount.status` 的**嵌套 `account`**：snake_case——那是
//!   `StoredAccount::to_public_json` 的原样投影，转发层没有翻它。
//! - `dsaccount.sign_in.status`：**snake_case**（`attempt_id` / `expires_at_ms`），
//!   而 `dsaccount.status.flow` 里同一个状态机是 **camelCase**。这是历史事实，
//!   不是笔误：两处 DTO 分开定义，各自 `rename`，别试图合并。
//!
//! ## 容错策略
//!
//! 除标了 `#[serde(default)]` 的字段（服务端可能省略/置空）外，缺字段即解析失败。
//! 这与 `qaqh-config-api` 的读模型同一条纪律：**失败比错值好**。特别是金额类字段
//! 一律保留服务端原串（`"0E-16"` 就是 0），不折算成 `f64`——权威永远是原串。

use serde::Deserialize;
use serde_json::Value;

// ─────────────────────────── 公共：错误对象 ───────────────────────────

/// 服务面错误对象（`qaqh_siwc::SiwcError::to_json`）。
///
/// 只出现在**成功响应内部的 error 槽**里（`siwc.status.error`、
/// `siwc.sign_in.status.error`）。方法级失败走的是 HTTP 非 2xx + `{ code, message }`
/// （`service_methods::error_response`，与这里是两个不同形状，别混）。
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct ServiceError {
    /// 稳定错误码（`sign_in_required` / `profile_not_found` / …）。
    pub code: String,
    pub message: String,
    /// 是否值得重试（端点不可用类）。
    #[serde(default)]
    pub retryable: bool,
    /// 是否属「凭据无效」类：为真时凭据已被清，必须重新登录。
    #[serde(default)]
    pub requires_reauth: bool,
    /// 上游请求 id（诊断用，可能没有）。
    #[serde(default)]
    pub request_id: Option<String>,
}

// ─────────────────────────────── siwc ───────────────────────────────

/// `siwc.status` 的连接态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SiwcConnectionState {
    /// 有登录流程正在等待浏览器回调（**只在流程未终结且未过期时**出现）。
    Connecting,
    /// 有可用凭据（注意 `sharing` 另判：可能「已登录但套餐未授权」）。
    Connected,
    /// 凭据被判失效，需要重登。
    ReauthRequired,
    /// 未登录 / 已登出。
    Disconnected,
}

/// `siwc.status` 的响应。
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SiwcStatus {
    pub status: SiwcConnectionState,
    /// 套餐用量是否真的可用（已连接 + 有凭据 + 授予了直连 scope）。
    pub sharing: bool,
    /// **已登记的 profile 条数**（数字，不是数组；列表走 [`SiwcProfiles`]）。
    pub profiles: u64,
    /// 「管理用量」深链（FE 不得自己拼）。
    pub usage_url: String,
    /// 官方 API 端点：`chatgpt_plan` 只允许它（设置页直接填这个值）。
    pub api_base: String,
    /// 活动 profile（未登录时缺省）。
    #[serde(default)]
    pub profile_id: Option<String>,
    #[serde(default)]
    pub label: Option<String>,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    /// 仅 `reauth_required` 时出现。
    #[serde(default)]
    pub error: Option<ServiceError>,
}

/// 一个 profile 的状态（`ProfileStatus` 的线上形态）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SiwcProfileState {
    Connected,
    ReauthRequired,
    Disconnected,
}

/// `siwc.profiles` 列表里的一条（也用于 `pending`：注册尚未完成）。
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SiwcProfile {
    pub id: String,
    pub label: String,
    pub status: SiwcProfileState,
    /// 套餐用量可用位（见 [`SiwcStatus::sharing`]）。
    pub sharing: bool,
    /// 注册回调下发的 `client_id`（`pending` 条目没有）。
    #[serde(default)]
    pub client_id: Option<String>,
    #[serde(default)]
    pub scopes: Vec<String>,
    #[serde(default)]
    pub saved_at_ms: Option<u64>,
    #[serde(default)]
    pub email: Option<String>,
    #[serde(default)]
    pub name: Option<String>,
    /// 注册还没完成：**能显示，还不能选**。
    #[serde(default)]
    pub pending: bool,
}

/// `siwc.profiles` 的响应。
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SiwcProfiles {
    /// 活动 profile id（未登录时 `null`）。
    #[serde(default)]
    pub active: Option<String>,
    #[serde(default)]
    pub profiles: Vec<SiwcProfile>,
    /// 注册中途的账户（状态恒为 `disconnected`，选它会报「注册尚未完成」）。
    #[serde(default)]
    pub pending: Vec<SiwcProfile>,
}

/// `siwc.models` 数组里的一条。
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SiwcModel {
    pub slug: String,
    pub display_name: String,
}

/// `siwc.sign_in.start` 的响应：把 `authorize_url` 交给**宿主**用系统浏览器打开。
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SiwcSignInStart {
    pub flow_id: String,
    pub authorize_url: String,
    /// 回环回调地址（诊断用；正常流程不需要它）。
    pub redirect_uri: String,
    /// 流程过期时刻（epoch ms）。
    pub expires_at_ms: u64,
    /// 「管理用量」深链（登录成功后引导用）。
    pub usage_url: String,
}

/// `siwc.sign_in.status` 的流程态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum SiwcFlowState {
    Pending,
    Completed,
    Failed,
}

/// `siwc.sign_in.status` 的响应（终态后不再轮询）。
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SiwcSignInStatus {
    pub flow_id: String,
    pub state: SiwcFlowState,
    /// 仅 `pending` 出现。
    #[serde(default)]
    pub expires_at_ms: Option<u64>,
    /// 仅 `completed` 出现。
    #[serde(default)]
    pub profile_id: Option<String>,
    /// 仅 `failed` 出现。
    #[serde(default)]
    pub error: Option<ServiceError>,
}

/// `siwc.sign_out` 的响应。
///
/// `revoked == false` 不代表登出失败：本地凭据已清，只是远端撤销没成功
/// （网络类失败是可重试档，不算登出失败）——UI 应提示用户去设置里手动断开。
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct SiwcSignOutResult {
    pub profile_id: String,
    pub revoked: bool,
    /// 登出前是否还有 refresh token（诊断「为什么没有可撤销的」）。
    pub had_refresh_token: bool,
    pub usage_url: String,
}

// ───────────────────────────── dsaccount ─────────────────────────────

/// `dsaccount.*` 登录尝试的状态机（线上值见 [`DsFlowState`] 的 `rename`）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Deserialize)]
pub enum DsFlowState {
    #[serde(rename = "idle")]
    Idle,
    #[serde(rename = "authorizing")]
    Authorizing,
    #[serde(rename = "exchanging")]
    Exchanging,
    #[serde(rename = "signed-in")]
    SignedIn,
    #[serde(rename = "failed")]
    Failed,
    #[serde(rename = "cancelled")]
    Cancelled,
}

impl DsFlowState {
    /// 是否仍在进行（UI 据此显示等待态、决定是否继续轮询）。
    pub fn is_running(self) -> bool {
        matches!(self, Self::Authorizing | Self::Exchanging)
    }
}

/// `dsaccount.status.account`：**snake_case**（原样转发的本地凭据投影，无凭据字段）。
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct DsAccountProfile {
    /// 平台 user id（稳定标识）。
    pub user_id: String,
    #[serde(default)]
    pub label: String,
    /// 登录方式（`WECHAT`…）。
    #[serde(default)]
    pub provider: Option<String>,
    /// 结算币种。
    #[serde(default)]
    pub currency: Option<String>,
    /// 这条凭据**允许**用的推理 origin。
    pub inference_origin: String,
    pub saved_at_ms: u64,
    /// 本地是否有可用凭据（永远为真——没有凭据就没有 `account` 对象）。
    pub has_credential: bool,
}

/// `dsaccount.status.flow`：登录尝试的对外状态（**camelCase**）。
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DsAccountFlow {
    #[serde(default)]
    pub attempt_id: Option<String>,
    pub state: DsFlowState,
    #[serde(default)]
    pub authorize_url: Option<String>,
    #[serde(default)]
    pub expires_at_ms: u64,
    #[serde(default)]
    pub error: Option<String>,
}

/// `dsaccount.status.revoke`：上一次远端撤销的结局（**只在内存**，进程退出即丢）。
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct DsRevokeOutcome {
    /// `idle` | `running` | `succeeded` | `failed`。
    pub state: String,
    pub attempts: u32,
    #[serde(default)]
    pub message: Option<String>,
}

/// `dsaccount.status` 的响应。
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DsAccountStatus {
    pub signed_in: bool,
    /// 嵌套层是 **snake_case**（见 [`DsAccountProfile`]）。
    #[serde(default)]
    pub account: Option<DsAccountProfile>,
    /// 本地凭据读取失败的原因（不是「没登录」：那是 `signed_in: false` + `account: null`）。
    #[serde(default)]
    pub storage_error: Option<String>,
    pub inference_origin: String,
    pub usage_url: String,
    pub top_up_url: String,
    pub flow: DsAccountFlow,
    pub revoke: DsRevokeOutcome,
}

/// 一个钱包条目（充值或赠金）。金额是**原串**（big.js 语法，`0E-16` 就是 0）。
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DsWallet {
    pub currency: String,
    pub balance: String,
    /// 额度折算（平台给的字符串，可能缺省）。
    #[serde(default)]
    pub token_estimation: Option<String>,
}

/// 累计消费条目。
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct DsTotalCost {
    pub currency: String,
    pub amount: String,
}

/// `dsaccount.balance` 的响应。
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DsAccountBalance {
    /// 充值钱包。
    #[serde(default)]
    pub recharge: Vec<DsWallet>,
    /// 赠金钱包（与充值**分开**展示，促销额度不得冒充充值）。
    #[serde(default)]
    pub bonus: Vec<DsWallet>,
    #[serde(default)]
    pub total_cost: Vec<DsTotalCost>,
    /// 「有没有可用的余额」的门限判定（服务端算的，别在客户端重算）。
    pub has_usable_balance: bool,
}

/// `dsaccount.bonuses` 数组里的一条（未读赠额）。
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DsAccountBonus {
    /// 确认展示时用它（`dsaccount.bonus.ack` 的 `order_id` 参数）。
    pub order_id: String,
    #[serde(default)]
    pub campaign: String,
    /// 金额原串。
    pub amount: String,
    pub currency: String,
    #[serde(default)]
    pub granted_at: String,
    #[serde(default)]
    pub expires_at: String,
    /// 服务端文案（本地化由服务端负责）。
    #[serde(default)]
    pub message: String,
}

/// `dsaccount.sign_in.start` 的响应。
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
pub struct DsAccountSignInStart {
    /// 本次尝试 id：取回状态与取消都必须带上它（旧页面不得取消新登录）。
    pub attempt_id: String,
    pub authorize_url: String,
    pub redirect_uri: String,
    /// 服务端给的有效期（**秒**，不是毫秒——与 `siwc` 的 `expiresAtMs` 不同口径）。
    pub expires_in: u64,
}

/// `dsaccount.sign_in.status` 的响应：**snake_case**（与 `status.flow` 的 camelCase 不同）。
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct DsAccountSignInStatus {
    #[serde(default)]
    pub attempt_id: Option<String>,
    pub state: DsFlowState,
    #[serde(default)]
    pub authorize_url: Option<String>,
    #[serde(default)]
    pub expires_at_ms: u64,
    #[serde(default)]
    pub error: Option<String>,
}

/// `dsaccount.verify` 的响应：这把凭据现在还有效吗。
///
/// `cleared == true` 表示验证判定为「失效」并**已清本地凭据**——此时 UI 必须
/// 立刻切到未登录态，而不是继续显示余额。
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct DsAccountVerify {
    pub valid: bool,
    #[serde(default)]
    pub reason: Option<String>,
    #[serde(default)]
    pub cleared: bool,
}

/// `dsaccount.models` 数组里的一条。
///
/// 平台条目比 OpenAI 标准形态多几个字段（上下文窗口、输出上限、能力位、
/// `effort.supported_levels`），daemon **原样转发**——在转发层猜键名就是造第二个
/// 真相源。所以这里只钉住两端都稳定、UI 一定要用的 `id`，其余进 [`Self::extra`]。
#[derive(Debug, Clone, Deserialize, PartialEq)]
pub struct DsAccountModel {
    #[serde(default)]
    pub id: Option<String>,
    #[serde(flatten)]
    pub extra: serde_json::Map<String, Value>,
}

#[cfg(test)]
mod tests {
    use super::*;

    /// fixture 来源：`qaqh-runtime/src/service.rs` 的投影分支
    /// （2026-10 逐字段核对）。**刻意**保留字段的全部大小写形状——
    /// 谁把它们「统一」成一种风格，这条就会红。
    #[test]
    fn siwc_status_fixture() {
        let status: SiwcStatus = serde_json::from_value(serde_json::json!({
            "status": "connected",
            "sharing": true,
            "profiles": 2,
            "usageUrl": "https://chatgpt.com/codex/settings/usage",
            "apiBase": "https://api.openai.com/v1",
            "profileId": "p_1",
            "label": "工作",
            "email": "a@b.c",
            "name": "A"
        }))
        .expect("siwc.status");
        assert_eq!(status.status, SiwcConnectionState::Connected);
        assert_eq!(status.profiles, 2);
        assert_eq!(status.profile_id.as_deref(), Some("p_1"));
        assert!(status.error.is_none());

        // reauth_required 时带 error 槽。
        let reauth: SiwcStatus = serde_json::from_value(serde_json::json!({
            "status": "reauth_required",
            "sharing": false,
            "profiles": 1,
            "usageUrl": "u",
            "apiBase": "a",
            "error": {
                "code": "sign_in_required",
                "message": "需要重新登录",
                "retryable": false,
                "requiresReauth": true
            }
        }))
        .expect("reauth");
        let error = reauth.error.expect("error slot");
        assert!(error.requires_reauth);
        assert!(!error.retryable);
    }

    #[test]
    fn siwc_profiles_fixture_carries_pending_entries() {
        let profiles: SiwcProfiles = serde_json::from_value(serde_json::json!({
            "active": "p_1",
            "profiles": [{
                "id": "p_1",
                "label": "工作",
                "status": "connected",
                "sharing": true,
                "clientId": "oaiapp_x",
                "scopes": ["openid", "chatgpt.tokens.use.direct"],
                "savedAtMs": 1728000000000u64
            }],
            "pending": [{
                "id": "p_2",
                "label": "待完成",
                "status": "disconnected",
                "sharing": false,
                "pending": true
            }]
        }))
        .expect("siwc.profiles");
        assert_eq!(profiles.profiles[0].client_id.as_deref(), Some("oaiapp_x"));
        assert!(!profiles.profiles[0].pending);
        assert!(profiles.pending[0].pending);
        assert!(profiles.pending[0].client_id.is_none());
    }

    #[test]
    fn siwc_sign_in_flow_fixtures() {
        let start: SiwcSignInStart = serde_json::from_value(serde_json::json!({
            "flowId": "f_1",
            "authorizeUrl": "https://auth.openai.com/x",
            "redirectUri": "http://127.0.0.1:1455/callback",
            "expiresAtMs": 1728000000000u64,
            "usageUrl": "https://chatgpt.com/codex/settings/usage"
        }))
        .expect("sign_in.start");
        assert_eq!(start.flow_id, "f_1");

        let pending: SiwcSignInStatus = serde_json::from_value(serde_json::json!({
            "flowId": "f_1",
            "state": "pending",
            "expiresAtMs": 1728000000000u64
        }))
        .expect("pending");
        assert_eq!(pending.state, SiwcFlowState::Pending);
        assert!(pending.profile_id.is_none());

        let completed: SiwcSignInStatus = serde_json::from_value(serde_json::json!({
            "flowId": "f_1",
            "state": "completed",
            "profileId": "p_1"
        }))
        .expect("completed");
        assert_eq!(completed.state, SiwcFlowState::Completed);
        assert_eq!(completed.profile_id.as_deref(), Some("p_1"));
    }

    #[test]
    fn dsaccount_status_fixture_mixes_case_on_purpose() {
        let status: DsAccountStatus = serde_json::from_value(serde_json::json!({
            "signedIn": true,
            "account": {
                "user_id": "u_1",
                "label": "DeepSeek",
                "provider": "WECHAT",
                "currency": "CNY",
                "inference_origin": "https://api.deepseek.com",
                "saved_at_ms": 1728000000000u64,
                "has_credential": true
            },
            "storageError": null,
            "inferenceOrigin": "https://api.deepseek.com",
            "usageUrl": "https://platform.deepseek.com/usage",
            "topUpUrl": "https://platform.deepseek.com/top_up",
            "flow": {
                "attemptId": null,
                "state": "idle",
                "authorizeUrl": null,
                "expiresAtMs": 0,
                "error": null
            },
            "revoke": { "state": "idle", "attempts": 0, "message": null }
        }))
        .expect("dsaccount.status");
        assert!(status.signed_in);
        assert_eq!(status.account.expect("account").user_id, "u_1");
        assert_eq!(status.flow.state, DsFlowState::Idle);
        assert_eq!(status.revoke.state, "idle");
    }

    #[test]
    fn dsaccount_balance_and_bonuses_keep_raw_amounts() {
        let balance: DsAccountBalance = serde_json::from_value(serde_json::json!({
            "recharge": [{ "currency": "CNY", "balance": "5.0000000000000000", "tokenEstimation": "1e6" }],
            "bonus": [{ "currency": "CNY", "balance": "0E-16", "tokenEstimation": null }],
            "totalCost": [{ "currency": "CNY", "amount": "1.2500000000000000" }],
            "hasUsableBalance": true
        }))
        .expect("balance");
        assert_eq!(balance.recharge[0].balance, "5.0000000000000000");
        assert_eq!(balance.bonus[0].balance, "0E-16");
        assert!(balance.has_usable_balance);

        let bonuses: Vec<DsAccountBonus> = serde_json::from_value(serde_json::json!([{
            "orderId": "o_1",
            "campaign": "welcome",
            "amount": "1.00",
            "currency": "CNY",
            "grantedAt": "2026-10-01",
            "expiresAt": "2026-11-01",
            "message": "新人赠金"
        }]))
        .expect("bonuses");
        assert_eq!(bonuses[0].order_id, "o_1");
    }

    /// 同一个状态机在 `status.flow`（camelCase）与 `sign_in.status`（snake_case）
    /// 里字段名不同——这条测试就是那份「别乱统一」的护栏。
    #[test]
    fn dsaccount_flow_and_sign_in_status_keep_their_own_spelling() {
        let sign_in: DsAccountSignInStatus = serde_json::from_value(serde_json::json!({
            "attempt_id": "a_1",
            "state": "signed-in",
            "authorize_url": "https://x",
            "expires_at_ms": 1728000000000u64,
            "error": null
        }))
        .expect("sign_in.status");
        assert_eq!(sign_in.state, DsFlowState::SignedIn);
        assert!(!sign_in.state.is_running());
        assert_eq!(sign_in.attempt_id.as_deref(), Some("a_1"));

        // camelCase 写进这个 DTO **不得**被当成 `attempt_id`：
        // 未知键被忽略（服务端加字段不该把客户端弄崩），所以这里断言的是
        // 「没认出来」而不是「解析失败」——认出来才是错，说明有人加了 alias
        // 把两处拼写悄悄统一了。
        let camel: DsAccountSignInStatus = serde_json::from_value(serde_json::json!({
            "attemptId": "a_1",
            "state": "signed-in"
        }))
        .expect("未知键应被忽略，而不是让整个响应解析失败");
        assert!(
            camel.attempt_id.is_none(),
            "camelCase 键不该被当成 attempt_id（两处拼写是刻意分开的）"
        );
    }

    #[test]
    fn verify_and_models_fixtures() {
        let verify: DsAccountVerify = serde_json::from_value(serde_json::json!({
            "valid": false,
            "reason": "凭据已失效",
            "cleared": true
        }))
        .expect("verify");
        assert!(!verify.valid && verify.cleared);

        let models: Vec<DsAccountModel> = serde_json::from_value(serde_json::json!([{
            "id": "deepseek-v4",
            "object": "model",
            "context_window": 128000,
            "effort": { "supported_levels": ["low", "high"] }
        }]))
        .expect("models");
        assert_eq!(models[0].id.as_deref(), Some("deepseek-v4"));
        assert!(models[0].extra.contains_key("context_window"));
    }
}
