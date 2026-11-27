//! 账号与凭据管理域强类型 RPC（覆盖通用账号查询与 `siwc` / `dsaccount` 完整流程）。

use qaqh_core_vocab::JsonText;
use serde::{Deserialize, Serialize};

/// 通用账号视图。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountView {
    /// 账号 id。
    pub account_id: String,
    /// 供应商 id。
    pub provider: String,
    /// 展示名。
    pub label: String,
    /// 是否当前活动账号。
    pub current: bool,
}

/// 服务面内嵌错误槽（如凭据失效需重登）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct AccountServiceError {
    /// 稳定错误码。
    pub code: String,
    /// 错误文案。
    pub message: String,
    /// 是否可重试。
    pub retryable: bool,
    /// 是否需重新登录。
    pub requires_reauth: bool,
    /// 上游请求 id。
    pub request_id: Option<String>,
}

/// `siwc` 连接态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SiwcConnectionState {
    /// 等待浏览器回调。
    Connecting,
    /// 已连接。
    Connected,
    /// 需重新授权。
    ReauthRequired,
    /// 未登录。
    Disconnected,
}

/// `siwc` 套餐状态。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SiwcStatusView {
    /// 连接态。
    pub status: SiwcConnectionState,
    /// 套餐用量是否可用。
    pub sharing: bool,
    /// 已登记 profile 数量。
    pub profiles: u64,
    /// 管理用量深链。
    pub usage_url: String,
    /// 官方 API 基址。
    pub api_base: String,
    /// 当前活动 profile id。
    pub profile_id: Option<String>,
    /// 展示标签。
    pub label: Option<String>,
    /// 邮箱。
    pub email: Option<String>,
    /// 名称。
    pub name: Option<String>,
    /// 内嵌错误（如有）。
    pub error: Option<AccountServiceError>,
}

/// `siwc` 单个 profile 态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SiwcProfileState {
    /// 已连接。
    Connected,
    /// 需重新授权。
    ReauthRequired,
    /// 已断开。
    Disconnected,
}

/// `siwc` 账号列表条目。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SiwcProfileView {
    /// profile id。
    pub id: String,
    /// 标签。
    pub label: String,
    /// 状态。
    pub status: SiwcProfileState,
    /// 是否启用共享。
    pub sharing: bool,
    /// 客户端 id。
    pub client_id: Option<String>,
    /// 授权 scope 列表。
    pub scopes: Vec<String>,
    /// 保存时刻（毫秒）。
    pub saved_at_ms: Option<u64>,
    /// 邮箱。
    pub email: Option<String>,
    /// 用户名。
    pub name: Option<String>,
    /// 是否仍在完成注册中。
    pub pending: bool,
}

/// `siwc` 账号表。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SiwcProfilesView {
    /// 当前活动 profile id。
    pub active: Option<String>,
    /// 已完成 profile 列表。
    pub profiles: Vec<SiwcProfileView>,
    /// 注册中途条目。
    pub pending: Vec<SiwcProfileView>,
}

/// `siwc` 模型条目。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SiwcModelView {
    /// 模型 slug。
    pub slug: String,
    /// 展示名。
    pub display_name: String,
}

/// `siwc` 登录启动回执。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SiwcSignInStartView {
    /// 登录流程 id。
    pub flow_id: String,
    /// 系统浏览器授权地址。
    pub authorize_url: String,
    /// 回调地址。
    pub redirect_uri: String,
    /// 过期时刻（毫秒）。
    pub expires_at_ms: u64,
    /// 用量管理链接。
    pub usage_url: String,
}

/// `siwc` 登录轮询态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum SiwcFlowState {
    /// 等待回调。
    Pending,
    /// 登录完成。
    Completed,
    /// 登录失败。
    Failed,
}

/// `siwc` 登录轮询回执。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SiwcSignInStatusView {
    /// 登录流程 id。
    pub flow_id: String,
    /// 当前状态。
    pub state: SiwcFlowState,
    /// 过期时刻（仅等待态）。
    pub expires_at_ms: Option<u64>,
    /// 生成的 profile id（仅完成态）。
    pub profile_id: Option<String>,
    /// 失败原因（仅失败态）。
    pub error: Option<AccountServiceError>,
}

/// `siwc` 登出结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct SiwcSignOutView {
    /// 被登出的 profile id。
    pub profile_id: String,
    /// 远端撤销是否成功。
    pub revoked: bool,
    /// 登出前是否持有 refresh token。
    pub had_refresh_token: bool,
    /// 用量管理链接。
    pub usage_url: String,
}

/// `dsaccount` 登录流程状态。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum DsFlowState {
    /// 空闲。
    Idle,
    /// 等待授权。
    Authorizing,
    /// 交换令牌中。
    Exchanging,
    /// 已登录。
    SignedIn,
    /// 失败。
    Failed,
    /// 已取消。
    Cancelled,
}

/// `dsaccount` 本地账号摘要。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DsAccountProfileView {
    /// 平台用户 id。
    pub user_id: String,
    /// 展示名。
    pub label: String,
    /// 登录通道。
    pub provider: Option<String>,
    /// 结算币种。
    pub currency: Option<String>,
    /// 凭据允许的推理源。
    pub inference_origin: String,
    /// 保存时刻（毫秒）。
    pub saved_at_ms: u64,
    /// 本地是否有可用凭据。
    pub has_credential: bool,
}

/// `dsaccount` 登录尝试状态。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DsAccountFlowView {
    /// 尝试 id。
    pub attempt_id: Option<String>,
    /// 状态机。
    pub state: DsFlowState,
    /// 授权链接。
    pub authorize_url: Option<String>,
    /// 过期时刻（毫秒）。
    pub expires_at_ms: u64,
    /// 错误说明。
    pub error: Option<String>,
}

/// `dsaccount` 远端撤销结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DsRevokeOutcomeView {
    /// 状态词（`idle` | `running` | `succeeded` | `failed`）。
    pub state: String,
    /// 尝试次数。
    pub attempts: u32,
    /// 附加说明。
    pub message: Option<String>,
}

/// `dsaccount` 账号状态。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DsAccountStatusView {
    /// 是否已登录。
    pub signed_in: bool,
    /// 本地账号档案。
    pub account: Option<DsAccountProfileView>,
    /// 凭据读取失败说明。
    pub storage_error: Option<String>,
    /// 推理源地址。
    pub inference_origin: String,
    /// 用量页。
    pub usage_url: String,
    /// 充值页。
    pub top_up_url: String,
    /// 流程状态。
    pub flow: DsAccountFlowView,
    /// 最近撤销结果。
    pub revoke: DsRevokeOutcomeView,
}

/// `dsaccount` 钱包条目（金额保持服务端原始字符串）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DsWalletView {
    /// 币种。
    pub currency: String,
    /// 金额原串。
    pub balance: String,
    /// 额度折算原串。
    pub token_estimation: Option<String>,
}

/// `dsaccount` 累计消费。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DsTotalCostView {
    /// 币种。
    pub currency: String,
    /// 金额原串。
    pub amount: String,
}

/// `dsaccount` 余额查询结果。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DsBalanceView {
    /// 充值钱包。
    pub recharge: Vec<DsWalletView>,
    /// 赠金钱包。
    pub bonus: Vec<DsWalletView>,
    /// 累计消费。
    pub total_cost: Vec<DsTotalCostView>,
    /// 服务端判定余额是否可用。
    pub has_usable_balance: bool,
}

/// `dsaccount` 未读赠金条目。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DsBonusView {
    /// 订单 id。
    pub order_id: String,
    /// 活动名。
    pub campaign: String,
    /// 金额原串。
    pub amount: String,
    /// 币种。
    pub currency: String,
    /// 发放时间。
    pub granted_at: String,
    /// 过期时间。
    pub expires_at: String,
    /// 展示文案。
    pub message: String,
}

/// `dsaccount` 登录启动回执。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DsSignInStartView {
    /// 本次尝试 id。
    pub attempt_id: String,
    /// 系统浏览器授权地址。
    pub authorize_url: String,
    /// 回调地址。
    pub redirect_uri: String,
    /// 有效期（秒）。
    pub expires_in: u64,
}

/// `dsaccount` 登录轮询回执。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DsSignInStatusView {
    /// 尝试 id。
    pub attempt_id: Option<String>,
    /// 状态机。
    pub state: DsFlowState,
    /// 授权链接。
    pub authorize_url: Option<String>,
    /// 过期时刻（毫秒）。
    pub expires_at_ms: u64,
    /// 错误说明。
    pub error: Option<String>,
}

/// `dsaccount` 凭据有效性验证。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DsVerifyView {
    /// 是否仍有效。
    pub valid: bool,
    /// 失败理由。
    pub reason: Option<String>,
    /// 是否因失效而清理了本地凭据。
    pub cleared: bool,
}

/// `dsaccount` 模型条目（原文走 `JsonText` 不丢字段）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DsAccountModelView {
    /// 模型 id（如有）。
    pub id: Option<String>,
    /// 平台完整原始条目的 JSON 文本。
    pub raw: JsonText,
}

/// 账号域请求。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AccountRequest {
    /// 列出全部通用账号。
    List,
    /// 当前通用账号。
    Current,
    /// 切换账号。
    Select {
        /// 账号 id。
        account_id: String,
    },
    /// 登出通用账号。
    Logout {
        /// 账号 id。
        account_id: String,
    },
    /// 查询 `siwc` 状态。
    SiwcStatus,
    /// 列出 `siwc` profiles。
    SiwcProfiles,
    /// 查询 `siwc` 可用模型。
    SiwcModels,
    /// 发起 `siwc` 登录。
    SiwcSignInStart,
    /// 轮询 `siwc` 登录进度。
    SiwcSignInPoll {
        /// 流程 id。
        flow_id: String,
    },
    /// 取消 `siwc` 登录。
    SiwcSignInCancel {
        /// 流程 id。
        flow_id: String,
    },
    /// `siwc` 登出。
    SiwcSignOut {
        /// 可选指定 profile id。
        profile_id: Option<String>,
    },
    /// 查询 `dsaccount` 状态。
    DsStatus,
    /// 发起 `dsaccount` 登录。
    DsSignInStart {
        /// 语言代码（如 `zh`）。
        locale: String,
    },
    /// 轮询 `dsaccount` 登录进度。
    DsSignInPoll {
        /// 尝试 id。
        attempt_id: String,
    },
    /// 取消 `dsaccount` 登录。
    DsSignInCancel {
        /// 尝试 id。
        attempt_id: String,
    },
    /// `dsaccount` 登出。
    DsSignOut,
    /// 校验 `dsaccount` 凭据有效性。
    DsVerify,
    /// 查询 `dsaccount` 余额。
    DsBalance,
    /// 查询 `dsaccount` 未读赠金。
    DsBonuses,
    /// 确认赠金通知。
    DsAckBonus {
        /// 订单 id。
        order_id: String,
    },
    /// 同步 `dsaccount` 模型列表。
    DsModels,
}

/// 账号域回执。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum AccountResponse {
    /// 通用账号列表。
    Accounts(Vec<AccountView>),
    /// 当前通用账号。
    Current(Option<AccountView>),
    /// 已切换。
    Selected,
    /// 已登出。
    LoggedOut,
    /// `siwc` 状态。
    SiwcStatus(SiwcStatusView),
    /// `siwc` profiles。
    SiwcProfiles(SiwcProfilesView),
    /// `siwc` 模型列表。
    SiwcModels(Vec<SiwcModelView>),
    /// `siwc` 登录已发起。
    SiwcSignInStart(SiwcSignInStartView),
    /// `siwc` 登录进度。
    SiwcSignInPoll(SiwcSignInStatusView),
    /// `siwc` 登出结果。
    SiwcSignOut(SiwcSignOutView),
    /// `dsaccount` 状态。
    DsStatus(DsAccountStatusView),
    /// `dsaccount` 登录已发起。
    DsSignInStart(DsSignInStartView),
    /// `dsaccount` 登录进度。
    DsSignInPoll(DsSignInStatusView),
    /// `dsaccount` 校验结果。
    DsVerify(DsVerifyView),
    /// `dsaccount` 余额。
    DsBalance(DsBalanceView),
    /// `dsaccount` 赠金列表。
    DsBonuses(Vec<DsBonusView>),
    /// `dsaccount` 模型列表。
    DsModels(Vec<DsAccountModelView>),
    /// 操作已受理。
    Ack,
}
