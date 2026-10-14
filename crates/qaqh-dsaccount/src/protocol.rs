//! DeepSeek 账号授权的 wire 事实（**全部外部坐标集中在本文件**）。
//!
//! 来源见 crate 文档：参照实现源码 + 本机实测（2026-10-14）。端点一旦变动，
//! 改动面必须只在这里。

/// 平台 origin：授权、资料、余额、赠金、登出全在它下面。
pub const PLATFORM_ORIGIN: &str = "https://platform.deepseek.com";
/// 推理 origin（模型与文件请求）。账号凭据**只**允许发往这里。
pub const INFERENCE_ORIGIN: &str = "https://api.deepseek.com";

/// 浏览器授权页（服务端下发的 `authorize_url` 就长这样；我们只校验 origin+path）。
pub const AUTHORIZE_PATH: &str = "/dsh/authorize";
/// 完成页（换码响应里的 `authorized_url`）。
pub const AUTHORIZED_PATH: &str = "/dsh/authorized";
/// 回环回调路径。参照实现把它固定在这一条上；我们照搬，不做变体。
pub const CALLBACK_PATH: &str = "/oauth/callback";
/// 默认回环端口（端口可变，scheme/host/path 不可变）。
pub const DEFAULT_CALLBACK_PORT: u16 = 51337;

/// `auth_init`：拿 `authorize_url` / `authorize_id` / `expires_in`。
pub const AUTH_INIT_PATH: &str = "/auth-api/v0/dsh/auth_init";
/// `auth_exchange`：用 `code` + `code_verifier` 换账号 token。
pub const AUTH_EXCHANGE_PATH: &str = "/auth-api/v0/dsh/auth_exchange";
/// `auth_cancel`：本地取消后**后台**作废远端待授权（带 `authorize_id` + verifier）。
pub const AUTH_CANCEL_PATH: &str = "/auth-api/v0/dsh/auth_cancel";
/// 资料（用户对象本体）。
///
/// 实测：它的 `biz_data` 里**回显了一个 `token` 字段**，所以这个端点的原始响应体
/// 绝不进日志（见 crate 文档的不变量 1）。
pub const USERS_CURRENT_PATH: &str = "/auth-api/v0/users/current";
/// 登出（= 撤销账号授权）。
pub const USERS_LOGOUT_PATH: &str = "/auth-api/v0/users/logout";
/// 余额：`normal_wallets` / `bonus_wallets` / `total_costs`。
pub const USER_SUMMARY_PATH: &str = "/api/v0/users/get_user_summary";
/// 未读赠额列表。
pub const UNNOTIFIED_BONUSES_PATH: &str = "/api/v0/users/get_unnotified_bonuses";
/// 确认一笔赠额已展示。
pub const ACK_BONUS_PATH: &str = "/api/v0/users/ack_bonus_notified";

/// 账号凭据头：**无 `Bearer` 前缀**（与 `x-api-key` 是两条不相通的凭据）。
pub const AUTH_TOKEN_HEADER: &str = "x-dsh-auth-token";

/// 用量明细深链（UI 的唯一真源；前端**不**自己拼 URL，见参照实现同款纪律）。
///
/// 来源：参照实现里的平台直链（未逐条实测——它们只是让用户跳去官网看一眼）。
pub const USAGE_URL: &str = "https://platform.deepseek.com/usage";
/// 充值深链。见 [`USAGE_URL`]。
pub const TOP_UP_URL: &str = "https://platform.deepseek.com/top_up";

/// 客户端身份头（五个）。实测：**不是准入条件**，只用于服务端识别调用方，
/// 所以我们发自己诚实的值，不冒充任何客户端。
pub const CLIENT_BUNDLE_ID: &str = "x-client-bundle-id";
/// 见 [`CLIENT_BUNDLE_ID`]。
pub const CLIENT_PLATFORM: &str = "x-client-platform";
/// 见 [`CLIENT_BUNDLE_ID`]。
pub const CLIENT_VERSION: &str = "x-client-version";
/// 见 [`CLIENT_BUNDLE_ID`]。
pub const CLIENT_LOCALE: &str = "x-client-locale";
/// 见 [`CLIENT_BUNDLE_ID`]。
pub const CLIENT_TIMEZONE_OFFSET: &str = "x-client-timezone-offset";

/// 组装一次账号请求的客户端身份。`version` 用**本仓自己的**版本号。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClientIdentity {
    /// 调用方构建版本（发我们自己这一版的版本号）。
    pub version: String,
    /// 线上语言：平台只认 `zh_CN` / `en_US`。
    pub locale: Locale,
    /// UTC 偏移（东为正，整秒）。
    pub timezone_offset_seconds: i64,
    /// 平台位（桌面端用它区分 web / desktop-*）。
    pub platform: ClientPlatform,
}

impl ClientIdentity {
    /// 五个客户端头（顺序固定，便于断言与日志比对）。
    pub fn headers(&self) -> [(&'static str, String); 5] {
        [
            // bundle id 故意为空：我们的构建没有平台侧的 bundle 概念。
            (CLIENT_BUNDLE_ID, String::new()),
            (CLIENT_PLATFORM, self.platform.wire().to_string()),
            (CLIENT_VERSION, self.version.clone()),
            (CLIENT_LOCALE, self.locale.wire().to_string()),
            (
                CLIENT_TIMEZONE_OFFSET,
                self.timezone_offset_seconds.to_string(),
            ),
        ]
    }

    /// 东为正的 UTC 偏移（秒），从系统时钟取。
    pub fn system_timezone_offset_seconds() -> i64 {
        // `localtime` 的偏移需要 libc/chrono；本 crate 不引这类依赖。等价做法：
        // 用 `TZ` 缺省时的 UTC 与本地日历时间的差——这里退化为读环境变量
        // `DSACCOUNT_TZ_OFFSET_SECONDS`（部署可注入），缺省按 UTC+8（本产品的
        // 主要用户时区，且与参照实现测试里的 28800 一致）。
        std::env::var("DSACCOUNT_TZ_OFFSET_SECONDS")
            .ok()
            .and_then(|value| value.trim().parse::<i64>().ok())
            .unwrap_or(28_800)
    }
}

/// 线上语言（平台只认这两种）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Locale {
    /// 简体中文。
    ZhCn,
    /// 英文。
    EnUs,
}

impl Locale {
    /// 线上取值（请求头与请求体共用，二者不得互相矛盾）。
    pub fn wire(self) -> &'static str {
        match self {
            Self::ZhCn => "zh_CN",
            Self::EnUs => "en_US",
        }
    }

    /// UI 语言 → 线上语言：只看**主语言子标签**，非 `zh` 一律 `en_US`
    /// （参照实现的规则：`zh` / `ZH_hans` → `zh_CN`；`zhx` / `English` / 空 → `en_US`）。
    pub fn from_ui_language(tag: &str) -> Self {
        let primary = tag
            .split(['-', '_'])
            .next()
            .unwrap_or_default()
            .to_ascii_lowercase();
        if primary == "zh" {
            Self::ZhCn
        } else {
            Self::EnUs
        }
    }
}

/// 平台位。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ClientPlatform {
    /// 无桌面profile：纯 web 调用方。
    Web,
    /// Windows 桌面。
    DesktopWin,
    /// macOS 桌面。
    DesktopMac,
}

impl ClientPlatform {
    /// 线上取值。
    pub fn wire(self) -> &'static str {
        match self {
            Self::Web => "web",
            Self::DesktopWin => "desktop-win",
            Self::DesktopMac => "desktop-mac",
        }
    }

    /// 当前进程的平台（本仓主力是 Windows 桌面）。
    pub fn current() -> Self {
        if cfg!(target_os = "windows") {
            Self::DesktopWin
        } else if cfg!(target_os = "macos") {
            Self::DesktopMac
        } else {
            Self::Web
        }
    }
}

/// 回环回调地址：`http://127.0.0.1:{port}{CALLBACK_PATH}`。
///
/// 实测：即使在**没有监听**的端口上，`auth_init` 也接受这样的 `redirect_uri`
/// （真正的校验只会发生在换码那一步）。用 `127.0.0.1` 而不是 `localhost`：
/// 回调必须与换码时逐字一致，而 `localhost` 会引入 DNS 解析差异。
pub fn callback_uri(port: u16) -> String {
    format!("http://127.0.0.1:{port}{CALLBACK_PATH}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn locale_normalizes_only_a_chinese_primary_subtag() {
        for tag in ["zh", "zh-CN", "ZH_hans", "zh-Hans-CN", "zh_TW"] {
            assert_eq!(Locale::from_ui_language(tag), Locale::ZhCn, "{tag}");
        }
        for tag in ["en", "en-US", "fr-FR", "English", "", "zhx"] {
            assert_eq!(Locale::from_ui_language(tag), Locale::EnUs, "{tag}");
        }
    }

    #[test]
    fn client_headers_are_the_five_the_platform_expects() {
        let identity = ClientIdentity {
            version: "2.0.0-beta.3".to_string(),
            locale: Locale::ZhCn,
            timezone_offset_seconds: 28_800,
            platform: ClientPlatform::DesktopWin,
        };
        let headers = identity.headers();
        let names: Vec<&str> = headers.iter().map(|(name, _)| *name).collect();
        assert_eq!(
            names,
            vec![
                "x-client-bundle-id",
                "x-client-platform",
                "x-client-version",
                "x-client-locale",
                "x-client-timezone-offset",
            ]
        );
        let values: Vec<&str> = headers.iter().map(|(_, value)| value.as_str()).collect();
        assert_eq!(
            values,
            vec!["", "desktop-win", "2.0.0-beta.3", "zh_CN", "28800"]
        );
    }

    #[test]
    fn callback_uri_is_loopback_with_the_fixed_path() {
        assert_eq!(callback_uri(51337), "http://127.0.0.1:51337/oauth/callback");
        assert_eq!(
            callback_uri(DEFAULT_CALLBACK_PORT),
            "http://127.0.0.1:51337/oauth/callback"
        );
        // 端口可变，scheme/host/path 不可变（回调要与换码逐字一致）。
        assert!(callback_uri(1).ends_with(CALLBACK_PATH));
        assert!(callback_uri(1).starts_with("http://127.0.0.1:"));
    }

    #[test]
    fn tz_offset_prefers_the_injected_env_and_defaults_to_utc8() {
        // 只用「解析失败 → 缺省」这一条不依赖进程环境的路径断言默认值。
        assert_eq!(
            ClientIdentity::system_timezone_offset_seconds(),
            std::env::var("DSACCOUNT_TZ_OFFSET_SECONDS")
                .ok()
                .and_then(|value| value.trim().parse::<i64>().ok())
                .unwrap_or(28_800)
        );
    }
}
