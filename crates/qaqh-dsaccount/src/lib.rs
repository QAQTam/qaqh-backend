//! # qaqh-dsaccount — DeepSeek 账号（官网授权）
//!
//! 第二个「用账号额度而不是 API key」的 provider：用户在系统浏览器里授权自己的
//! DeepSeek 账号，凭据（账号 grant）留在**本机**，推理请求以账号余额结算打到
//! `https://api.deepseek.com`（header `x-dsh-auth-token`）。
//!
//! ## 边界（谁负责什么）
//!
//! - **本 crate**：PKCE（S256）+ state、回环回调、`auth_init`/`auth_exchange`/
//!   `auth_cancel`、凭据密文落盘、账户面只读读取（资料 / **余额** / **赠金**）、
//!   失效分类（401 / 业务码 `40003` ⇒ 清凭据）。
//! - **daemon**：把本 crate 暴露成服务面方法（方法名见 [`methods`]）。
//! - **runtime/gate**：新增 provider 路由，取账号 token 当 `x-dsh-auth-token` 用。
//! - **前端**：只读状态投影；用系统浏览器打开 `authorize_url`，**永不接触 token**。
//!
//! ## 协议事实的来源（不是猜的）
//!
//! 1. 参照实现 `github.com/deepseek-ai/deepseek-harness` 的
//!    `packages/credentials/deepseek-account{,-platform}/src/*`（MIT，公开 npm 包
//!    `@deepseek-ai/dsh-deepseek-account-platform`）——端点、五个客户端头、
//!    信封形状、`/oauth/callback` 固定路径都从那里读出来。
//! 2. **本机实测**（2026-10-14，`scripts/probe-dsh-auth-init.mjs` 与
//!    `scripts/probe-dsh-account-flow.mjs`）：三发 `auth_init` 全 200（五个客户端头
//!    与 `login_source` 都**不是准入条件**）；完整链路（回环回调 → 换码 → 读账户面）
//!    通；`GET https://api.deepseek.com/models` 带账号 token 200、无凭据 401。
//!    [`account`] 的回归夹具就是那次实测的**原始返回**。
//!
//! ## 三条安全不变量
//!
//! 1. token 明文不出进程：磁盘上是 DPAPI/0600 密文（复用 `qaqh-config::secrets`），
//!    服务面投影里没有 token 字段，日志与错误文案都不含 token。
//! 2. 「端点不可用」≠「凭据无效」：只有 HTTP 401 或业务码 `40003` 才清凭据
//!    （[`DsError::requires_reauth`]）；其它失败保留登录。
//! 3. 账号额度只用于**绑定的**推理 origin（默认 `https://api.deepseek.com`）：
//!    换端点不等于转发凭据（[`protocol::INFERENCE_ORIGIN`]）。
//!
//! ## 与 `qaqh-siwc` 的关系
//!
//! 同一个问题的第二个实例（借官方客户端跑本地 harness），但更轻：这条流**没有
//! `client_id`**，是纯 PKCE 公开客户端，五个客户端头只用于身份上报。两 crate 不
//! 互相依赖——provider 门禁各自成对（`qaqh-siwc` 的 `chatgpt_plan` /
//! 本 crate 的 `deepseek_account`），共享的是**纪律**而不是代码。

#![warn(missing_docs)]

pub mod account;
pub mod auth;
pub mod client;
pub mod envelope;
pub mod error;
pub mod global;
pub mod http;
pub mod loopback;
pub mod manager;
pub mod pkce;
pub mod protocol;
pub mod store;
pub mod util;
pub mod web_tools;

#[cfg(test)]
mod e2e_tests;

pub use account::{AccountProfile, Bonus, Summary, TotalCost, Wallet};
pub use auth::{AuthExchangeBody, AuthInit, AuthInitBody, AuthSession, DeviceInfo};
pub use client::{DsClient, DsClientConfig, is_known_inference_origin};
pub use error::{DsCode, DsError};
pub use global::{install_default, install_manager, manager};
pub use http::{Http, HttpResponse};
pub use loopback::CallbackListener;
pub use manager::{DsAccountManager, FlowState, SignInStart};
pub use pkce::Pkce;
pub use protocol::{ClientIdentity, ClientPlatform, Locale, PLATFORM_ORIGIN};
pub use store::{AccountStore, StoredAccount};
pub use web_tools::{FetchedPage, WebSearchHit, WebSearchRequest};

/// 服务面方法名（daemon 方法表与 FE 共用的单一真源）。
pub mod methods {
    /// 账号态（是否已登录、资料、资料读取的失败面）。
    pub const STATUS: &str = "dsaccount.status";
    /// 余额：充值钱包 + 赠金钱包 + 累计消费。
    pub const BALANCE: &str = "dsaccount.balance";
    /// 未读赠额（服务端文案 + 到期时间）。
    pub const BONUSES: &str = "dsaccount.bonuses";
    /// 开始登录：返回 `authorize_url`（前端用系统浏览器打开）。
    pub const SIGN_IN_START: &str = "dsaccount.sign_in.start";
    /// 登录尝试状态（`authorizing` / `exchanging` / `signed-in` / `failed`）。
    pub const SIGN_IN_STATUS: &str = "dsaccount.sign_in.status";
    /// 取消一次登录尝试（必须带 attempt id：旧页面不得取消新登录）。
    pub const SIGN_IN_CANCEL: &str = "dsaccount.sign_in.cancel";
    /// 登出：先删本地凭据，再后台撤销。
    pub const SIGN_OUT: &str = "dsaccount.sign_out";
    /// 确认一笔已展示的赠额。
    pub const ACK_BONUS: &str = "dsaccount.bonus.ack";
    /// 账号额度可用的模型目录（推理 origin 的 `/models`）。
    pub const MODELS: &str = "dsaccount.models";
    /// 「这把凭据现在还有效吗」：命中 401 / 业务码 `40003` 时会**清本地凭据**，
    /// 所以是写类（服务面按 `action_failed` 报错）。
    pub const VERIFY: &str = "dsaccount.verify";

    /// 全部方法名（越权矩阵测试按此遍历，新增方法自动纳入覆盖）。
    pub const ALL: &[&str] = &[
        STATUS,
        BALANCE,
        BONUSES,
        SIGN_IN_START,
        SIGN_IN_STATUS,
        SIGN_IN_CANCEL,
        SIGN_OUT,
        ACK_BONUS,
        MODELS,
        VERIFY,
    ];
}
