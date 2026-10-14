//! 进程级装配（与 `qaqh_siwc::global` 同构）：**不改 `RingContext`**。
//!
//! 账号管理器是进程级单例：daemon 启动时装配一次，服务面方法通过
//! [`manager`] 取用。用 `OnceLock` 而不是可变全局，是为了让「谁装配、装了几次」
//! 变成一个可观测的布尔值。

use std::sync::OnceLock;

use crate::client::DsClientConfig;
use crate::manager::DsAccountManager;
use crate::store::AccountStore;

static MANAGER: OnceLock<DsAccountManager> = OnceLock::new();

/// 装配管理器。首次调用生效；重复调用返回 `false`（**不覆盖**既有实例——
/// 覆盖会让正在进行的登录凭空消失）。
pub fn install_manager(manager: DsAccountManager) -> bool {
    MANAGER.set(manager).is_ok()
}

/// 用默认位置（`<config 目录>/deepseek-account.json`）与默认配置装配。
pub fn install_default() -> bool {
    install_manager(DsAccountManager::new(
        AccountStore::default_location(),
        DsClientConfig::default(),
    ))
}

/// 取用管理器（未装配 = `None`）。
pub fn manager() -> Option<&'static DsAccountManager> {
    MANAGER.get()
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::TempDir;

    #[test]
    fn install_is_first_one_wins() {
        let dir = TempDir::new().expect("tempdir");
        let config = DsClientConfig::default();
        let first =
            DsAccountManager::new(AccountStore::new(dir.path().join("a.json")), config.clone());
        // 同一个静态只装一次：这里只断言语义（第二次必失败），不依赖测试顺序。
        let installed = install_manager(first);
        if installed {
            assert!(!install_manager(DsAccountManager::new(
                AccountStore::new(dir.path().join("b.json")),
                config,
            )));
            assert!(manager().is_some());
            assert_eq!(
                manager().expect("已装配").store().path(),
                dir.path().join("a.json")
            );
        } else {
            // 别的用例已经装过：语义相同（不覆盖）。
            assert!(manager().is_some());
        }
    }
}
