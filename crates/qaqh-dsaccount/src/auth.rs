//! 授权三个调用的请求/响应契约。
//!
//! 请求体的字段**逐字**来自参照实现（`deepseek-account-platform/src/index.ts`）：
//!
//! ```text
//! auth_init     { code_challenge, code_challenge_method:'S256', state, redirect_uri, locale, login_source }
//! auth_exchange { code, code_verifier, redirect_uri, device_id, device_model, os_version }
//! auth_cancel   { authorize_id, code_verifier }
//! ```
//!
//! `auth_init` 与 `auth_exchange` 已实测（2026-10-14，见 crate 文档）；`auth_cancel`
//! **未实测**（我们只用了 logout 撤销），字段形状照参照实现，取消路径上的任何失败
//! 都不得影响本地凭据（它只是"顺手把远端的待授权作废"）。

use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::account::{AccountProfile, project_profile};
use crate::error::DsError;
use crate::pkce::Pkce;
use crate::protocol::{Locale, callback_uri};

/// PKCE 的挑战方法：**固定** `S256`（平台不接受 `plain`，也不该被配置项打开）。
pub const CHALLENGE_METHOD: &str = "S256";

/// 登录来源：桌面端走 `desktop`（另一取值是 `web`）。
pub const LOGIN_SOURCE_DESKTOP: &str = "desktop";

/// 一次授权尝试里设备信息的来源。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DeviceInfo {
    /// 安装级设备 ID。
    pub device_id: String,
    /// 形如 `windows-x86_64`。
    pub device_model: String,
    /// 系统版本。
    pub os_version: String,
}

impl DeviceInfo {
    /// 取当前进程所在机器（`device_id` 每次登录新生成：平台侧只把它当一次性指纹）。
    pub fn current() -> Self {
        Self {
            device_id: crate::util::random_uuid_v4(),
            device_model: format!("{}-{}", std::env::consts::OS, std::env::consts::ARCH),
            os_version: std::env::consts::OS.to_string(),
        }
    }
}

/// `POST /auth-api/v0/dsh/auth_init` 请求体。
#[derive(Debug, Serialize)]
pub struct AuthInitBody {
    /// `BASE64URL(SHA256(verifier))`。
    pub code_challenge: String,
    /// 固定 `S256`。
    pub code_challenge_method: String,
    /// 一次性事务量；服务端记住它并在回调时带回。
    pub state: String,
    /// 必须与回环监听、换码三处**逐字一致**。
    pub redirect_uri: String,
    /// 线上语言（`zh_CN` / `en_US`）。
    pub locale: String,
    /// `desktop` / `web`。
    pub login_source: String,
}

impl AuthInitBody {
    /// 由一次尝试的 PKCE 与回环端口组装。
    pub fn new(pkce: &Pkce, port: u16, locale: Locale, login_source: &str) -> Self {
        Self {
            code_challenge: pkce.challenge().to_string(),
            code_challenge_method: CHALLENGE_METHOD.to_string(),
            state: pkce.state().to_string(),
            redirect_uri: callback_uri(port),
            locale: locale.wire().to_string(),
            login_source: login_source.to_string(),
        }
    }
}

/// `auth_init` 的成功载荷。
#[derive(Debug, Clone, Deserialize, PartialEq, Eq)]
pub struct AuthInit {
    /// 浏览器要去的地址。
    pub authorize_url: String,
    /// 本次待授权的远端 ID（`auth_cancel` 用它）。
    pub authorize_id: String,
    /// 有效期（秒）。
    pub expires_in: u64,
}

/// `POST /auth-api/v0/dsh/auth_exchange` 请求体。
#[derive(Debug, Serialize)]
pub struct AuthExchangeBody {
    /// 回调带回的一次性 code。
    pub code: String,
    /// 与 `code_challenge` 配对的原文。
    pub code_verifier: String,
    /// 与 `auth_init` 逐字一致。
    pub redirect_uri: String,
    /// 设备信息（实测带上即通过；是否必填未单独验证）。
    pub device_id: String,
    /// 见 [`DeviceInfo`]。
    pub device_model: String,
    /// 见 [`DeviceInfo`]。
    pub os_version: String,
}

impl AuthExchangeBody {
    /// 组装。
    pub fn new(code: &str, pkce: &Pkce, port: u16, device: &DeviceInfo) -> Self {
        Self {
            code: code.to_string(),
            code_verifier: pkce.verifier().to_string(),
            redirect_uri: callback_uri(port),
            device_id: device.device_id.clone(),
            device_model: device.device_model.clone(),
            os_version: device.os_version.clone(),
        }
    }
}

/// `auth_exchange` 的成功载荷。
#[derive(Debug, Clone, Deserialize)]
pub struct AuthSession {
    /// **账号凭据本体**（`x-dsh-auth-token`）。全进程只有 store 与 gate 拿得到它。
    pub token: String,
    /// 完成页地址（浏览器已停在那里；仅用于诊断/展示）。
    #[serde(default)]
    pub authorized_url: Option<String>,
    /// 用户对象（形状同 `users/current`）。
    #[serde(default)]
    pub user: Option<Value>,
}

impl AuthSession {
    /// 投影出资料。`user` 缺失时返回空投影（**不算失败**：拿不到资料不该让
    /// 一次已经成功的授权作废）。
    pub fn profile(&self) -> Result<AccountProfile, DsError> {
        match &self.user {
            Some(user) => project_profile(user),
            None => Ok(AccountProfile::default()),
        }
    }
}

/// `POST /auth-api/v0/dsh/auth_cancel` 请求体。
#[derive(Debug, Serialize)]
pub struct AuthCancelBody {
    /// `auth_init` 返回的待授权 ID。
    pub authorize_id: String,
    /// 同一次尝试的 verifier。
    pub code_verifier: String,
}

impl AuthCancelBody {
    /// 组装。
    pub fn new(authorize_id: &str, pkce: &Pkce) -> Self {
        Self {
            authorize_id: authorize_id.to_string(),
            code_verifier: pkce.verifier().to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn value_of<T: Serialize>(body: &T) -> Value {
        serde_json::to_value(body).expect("序列化")
    }

    #[test]
    fn auth_init_body_has_exactly_the_fields_the_platform_reads() {
        let pkce = Pkce::new();
        let body = AuthInitBody::new(&pkce, 51337, Locale::ZhCn, LOGIN_SOURCE_DESKTOP);
        let value = value_of(&body);

        // 键集合必须**恰好**是这六个：多一个少一个都要显式改这里。
        // （`serde_json` 默认把对象键排序，故比较前也排序，断的是集合而非顺序。）
        let mut keys: Vec<&str> = value
            .as_object()
            .expect("对象")
            .keys()
            .map(String::as_str)
            .collect();
        keys.sort_unstable();
        let mut expected = [
            "code_challenge",
            "code_challenge_method",
            "state",
            "redirect_uri",
            "locale",
            "login_source",
        ];
        expected.sort_unstable();
        assert_eq!(
            keys, expected,
            "字段集合是与平台的契约，多一个少一个都要显式改这里"
        );
        assert_eq!(
            value["code_challenge"],
            value_of(&pkce.challenge().to_string())
        );
        assert_eq!(value["code_challenge_method"], "S256");
        assert_eq!(value["state"], value_of(&pkce.state().to_string()));
        assert_eq!(
            value["redirect_uri"],
            "http://127.0.0.1:51337/oauth/callback"
        );
        assert_eq!(value["locale"], "zh_CN");
        assert_eq!(value["login_source"], "desktop");
        // verifier 绝不进这个请求体（只在换码与取消里出现）。
        assert!(
            !value.to_string().contains(pkce.verifier()),
            "verifier 泄进了 auth_init"
        );
    }

    #[test]
    fn auth_exchange_body_carries_the_code_the_verifier_and_the_device() {
        let pkce = Pkce::new();
        let device = DeviceInfo {
            device_id: "11111111-1111-4111-8111-111111111111".to_string(),
            device_model: "windows-x86_64".to_string(),
            os_version: "10.0.26300".to_string(),
        };
        let body = AuthExchangeBody::new("the-code", &pkce, 51337, &device);
        let value = value_of(&body);
        assert_eq!(value["code"], "the-code");
        assert_eq!(value["code_verifier"], pkce.verifier());
        assert_eq!(
            value["redirect_uri"],
            "http://127.0.0.1:51337/oauth/callback"
        );
        assert_eq!(value["device_id"], "11111111-1111-4111-8111-111111111111");
        assert_eq!(value["device_model"], "windows-x86_64");
        assert_eq!(value["os_version"], "10.0.26300");
    }

    #[test]
    fn cancel_body_pairs_the_authorize_id_with_the_same_verifier() {
        let pkce = Pkce::new();
        let value = value_of(&AuthCancelBody::new("auth-id-1", &pkce));
        assert_eq!(value["authorize_id"], "auth-id-1");
        assert_eq!(value["code_verifier"], pkce.verifier());
    }

    #[test]
    fn a_session_without_a_user_still_projects() {
        let session: AuthSession =
            serde_json::from_str(r#"{"token":"t","authorized_url":"https://x/dsh/authorized"}"#)
                .expect("载荷");
        let profile = session.profile().expect("缺 user 不该失败");
        assert_eq!(profile, AccountProfile::default());
        assert_eq!(
            session.authorized_url.as_deref(),
            Some("https://x/dsh/authorized")
        );
    }

    #[test]
    fn a_session_with_a_user_projects_the_profile() {
        let session: AuthSession = serde_json::from_str(
            r#"{"token":"t","user":{"id":"u-1","id_profile":{"name":"测试用户","provider":"WECHAT"}}}"#,
        )
        .expect("载荷");
        let profile = session.profile().expect("投影");
        assert_eq!(profile.id.as_deref(), Some("u-1"));
        assert_eq!(profile.name.as_deref(), Some("测试用户"));
    }

    #[test]
    fn device_info_is_shaped_like_a_model_string() {
        let device = DeviceInfo::current();
        assert_eq!(device.device_id.len(), 36, "UUIDv4 形态");
        assert!(
            device.device_model.contains('-'),
            "os-arch: {}",
            device.device_model
        );
        assert!(!device.os_version.is_empty());
    }
}
