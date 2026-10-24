//! # qaqh-siwc — Sign in with ChatGPT（ChatGPT plan usage）
//!
//! 本 crate 实现 **OSS/本机运行应用** 那条流程：用户在浏览器里授权自己的
//! ChatGPT 账户，凭据（OAuth token）留在**本机**，推理请求以用户的套餐额度
//! 打到 `POST https://api.openai.com/v1/responses`。
//!
//! ## 边界（谁负责什么）
//!
//! - **本 crate**：host 标识、OAuth 一次性事务（state/nonce/PKCE）、回环回调、
//!   换码/刷新/撤销、ID token 校验（RS256 + JWKS）、凭据密文落盘、模型目录。
//! - **daemon**（`qaqh-daemon`）：把本 crate 暴露成 `siwc.*` 服务面方法，
//!   全部 `admin_only`（配对出来的设备不得触发登录或切换账户）。
//! - **runtime/gate**：每次 turn 取一次 [`SiwcManager::access_token`]，把结果
//!   当 bearer 用；协议层（`qaqh-gate`）不需要知道 OAuth 的存在。
//! - **Tauri 前端**：只读状态投影，用系统浏览器打开
//!   [`SignInStart::authorize_url`]，**永不接触 token**。
//!
//! ## 三条安全不变量
//!
//! 1. token 明文不出进程：磁盘上是 DPAPI/0600 密文，服务面投影里没有 token 字段，
//!    日志不打印 token 与 `id_token_hint`。
//! 2. 「端点不可用」≠「凭据无效」：只有 `invalid_grant` 家族才清凭据
//!    （[`SiwcCode::requires_reauth`]）。
//! 3. 注册身份 = `sub` + 下发的 `client_id`。邮箱只是标签，同邮箱的两次注册互不覆盖。
//!
//! ## 协议事实的来源
//!
//! `developers.openai.com/siwc/*` 与 DevKit 仓库（`.md` 版本核对于 2026-10-14）。
//! 该流程仍在 preview：端点/参数/scope 全部集中在 [`protocol`]，
//! 且**不复制** DevKit 的代码与设计资产（其许可为 Noncommercial 且不含商标授权）。

/// Credential-free service response DTOs.
pub mod service;

mod account;
mod error;
mod global;
mod host_id;
mod http;
mod identity;
mod loopback;
mod manager;
mod models;
mod oauth;
mod protocol;
mod store;
mod util;

#[cfg(test)]
mod e2e_tests;
#[cfg(test)]
mod fixture;

pub use error::{SiwcCode, SiwcError, classify_provider_error};
pub use global::{install_manager, manager};
pub use host_id::validate_host_id;
pub use identity::Identity;
pub use manager::{FlowState, SignInOptions, SignInStart, SiwcConfig, SiwcManager};
pub use models::ModelInfo;
pub use protocol::{API_BASE, Discovery, ISSUER, SCOPES, SHARING_SCOPE, USAGE_URL};
pub use store::ProfileStatus;

/// 服务面方法名（daemon 方法表与 FE 共用的单一真源）。
pub mod methods {
    pub const STATUS: &str = "siwc.status";
    pub const PROFILES: &str = "siwc.profiles";
    pub const MODELS: &str = "siwc.models";
    pub const SIGN_IN_START: &str = "siwc.sign_in.start";
    pub const SIGN_IN_STATUS: &str = "siwc.sign_in.status";
    pub const SIGN_IN_CANCEL: &str = "siwc.sign_in.cancel";
    pub const PROFILE_SELECT: &str = "siwc.profile.select";
    pub const SIGN_OUT: &str = "siwc.sign_out";

    /// 全部方法名（越权矩阵测试按此遍历，新增方法自动纳入覆盖）。
    pub const ALL: &[&str] = &[
        STATUS,
        PROFILES,
        MODELS,
        SIGN_IN_START,
        SIGN_IN_STATUS,
        SIGN_IN_CANCEL,
        PROFILE_SELECT,
        SIGN_OUT,
    ];
}
