//! axum 迁移 P1.5：主干直切，已去 feature-gated。
//! P0 已完成 health；P1 补齐无状态 REST + 中间件/限流骨架；P1.5 起为默认 HTTP 栈。

#[cfg(test)]
mod authz_matrix_tests;
mod axum_impl;

pub(crate) use axum_impl::challenge::ChallengeStore;
pub(crate) use axum_impl::pairing::PairingTable;
pub(crate) use axum_impl::test_hooks::TestHooks;
pub use axum_impl::{AppState, build_router};

#[cfg(test)]
/// 进程级 SessionManager 初始化守卫。
///
/// `SessionManager::init` 内部是 `OnceLock::set().expect(...)`——同一测试
/// 二进制的多个用例共享进程，谁先谁后不确定，裸调 `init` 必然 `already
/// initialized` panic。所有测试模块统一走这里，只初始化一次。
///
/// 同时做**测试数据根隔离**：默认指向进程唯一的临时目录，避免测试在真实
/// `<USERPROFILE>\.qaqh` 里建会话（泄漏的会话会进 `session.list` 干扰前端）。
/// 调用方显式设了 `QAQH_DATA_DIR` 时不覆盖。
#[cfg(test)]
fn init_session_manager() {
    static INIT: std::sync::OnceLock<()> = std::sync::OnceLock::new();
    INIT.get_or_init(|| {
        // SAFETY: 处于 `OnceLock` 初始化窗口，此后不再修改进程环境。
        unsafe {
            if std::env::var("QAQH_DATA_DIR")
                .map(|value| value.trim().is_empty())
                .unwrap_or(true)
            {
                let root =
                    std::env::temp_dir().join(format!("qaqh-daemon-test-{}", std::process::id()));
                let _ = std::fs::create_dir_all(&root);
                std::env::set_var("QAQH_DATA_DIR", &root);
            }
            std::env::set_var("QAQH_ALLOW_TEST_DATA_ROOT", "1");
        }
        qaqh_session::SessionManager::init(qaqh_types::platform::data_dir())
    });
}

/// 鉴权矩阵测试共用 AppState:无 v2 投影(业务面已切 owner)。
#[cfg(test)]
pub(crate) fn test_state() -> crate::axum_server::axum_impl::AppState {
    let hub = std::sync::Arc::new(qaqh_runtime::RingingHub::with_persistence(
        String::from("test-epoch"),
        std::env::temp_dir().join("qaqh-axum-test"),
    ));
    let leases = std::sync::Arc::new(std::sync::Mutex::new(
        qaqh_runtime::ringing::RingingLeaseStore::new(),
    ));
    let pending = std::sync::Arc::new(std::sync::Mutex::new(
        qaqh_runtime::ringing::PendingCommandStore::new(),
    ));
    static TEST_SERVICE: std::sync::OnceLock<qaqh_runtime::QaqhService> =
        std::sync::OnceLock::new();
    let service = TEST_SERVICE
        .get_or_init(|| {
            init_session_manager();
            qaqh_runtime::QaqhService::init(qaqh_session::SessionManager::global())
        })
        .clone();
    let (shutdown, _) = tokio::sync::watch::channel(false);
    crate::axum_server::axum_impl::AppState {
        hub,
        leases,
        pending,
        service,
        admin_token: String::from("test-token"),
        devices: std::sync::Arc::new(std::sync::Mutex::new(
            qaqh_runtime::ringing::DeviceRegistry::new(),
        )),
        pairings: std::sync::Arc::new(std::sync::Mutex::new(
            crate::axum_server::PairingTable::new(),
        )),
        tls_fingerprint: None,
        challenges: std::sync::Arc::new(crate::axum_server::ChallengeStore::default()),
        epoch: String::from("test-epoch"),
        shutdown,
        test_hooks: std::sync::Arc::new(crate::axum_server::TestHooks::disabled()),
    }
}
