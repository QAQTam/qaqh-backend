//! 配置层错误类型。
//!
//! 此前 `Config::load` / `update` / `save` 一律返回 `Result<_, String>`：
//! 调用方无法区分「文件 IO 失败」「校验拒绝」「锁中毒」——日志里只能看到
//! 一段拼接好的中文。这里把语义钉在变体上，消息文本保持逐字不变。
//!
//! `Invalid` 承载校验类拒绝（`validate()` 系列的既有文案），`From<String>`
//! 让配置内部那些仍是 `String` 的校验函数可以继续用 `?`——转换点只有一处，
//! 而不是每个调用点各自 `format!`。

/// 配置读写 / 校验错误。
#[derive(Debug, thiserror::Error)]
pub enum ConfigError {
    /// config.toml / secrets.toml 读写失败。
    #[error("config io {path}: {source}")]
    Io {
        path: String,
        #[source]
        source: std::io::Error,
    },

    /// 校验 / 语义拒绝。消息即用户可见说明。
    #[error("{0}")]
    Invalid(String),

    /// 进程内配置锁中毒（另一个线程 panic 时持有锁）。
    #[error("config lock poisoned")]
    LockPoisoned,
}

impl ConfigError {
    /// 业务校验拒绝。
    pub fn invalid(message: impl Into<String>) -> Self {
        Self::Invalid(message.into())
    }

    /// 文件读写失败。
    pub fn io(path: impl Into<String>, source: std::io::Error) -> Self {
        Self::Io {
            path: path.into(),
            source,
        }
    }

    pub fn is_invalid(&self) -> bool {
        matches!(self, Self::Invalid(_))
    }
}

/// 配置层的 `Result` 别名。
pub type ConfigResult<T> = Result<T, ConfigError>;

/// 下游仍是 `String` 的校验函数经 `?` 进入本层时只在这里转换一次。
impl From<String> for ConfigError {
    fn from(message: String) -> Self {
        Self::Invalid(message)
    }
}
