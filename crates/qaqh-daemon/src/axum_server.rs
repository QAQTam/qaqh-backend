//! axum 迁移 P1.5：主干直切，已去 feature-gated。
//! P0 已完成 health；P1 补齐无状态 REST + 中间件/限流骨架；P1.5 起为默认 HTTP 栈。

#[cfg(test)]
mod authz_matrix_tests;
#[cfg(test)]
mod route_table_tests;
#[cfg(test)]
mod v2_storm_tests;
mod axum_impl;

pub(crate) use axum_impl::challenge::ChallengeStore;
pub(crate) use axum_impl::driver::DriverSeats;
pub(crate) use axum_impl::pairing::PairingTable;
pub(crate) use axum_impl::test_hooks::TestHooks;
pub use axum_impl::{AppState, build_router};

#[cfg(test)]
/// Isolate daemon test resources from the user's data root.
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
    });
}

/// 鉴权矩阵测试共用 AppState:无 v2 投影(业务面已切 owner)。
#[cfg(test)]
pub(crate) fn test_state() -> crate::axum_server::axum_impl::AppState {
    let leases = std::sync::Arc::new(std::sync::Mutex::new(
        qaqh_ringing_runtime::RingingLeaseStore::new(),
    ));
    let pending = std::sync::Arc::new(std::sync::Mutex::new(
        qaqh_ringing_runtime::PendingCommandStore::new(),
    ));
    static TEST_SERVICE: std::sync::OnceLock<qaqh_runtime::QaqhService> =
        std::sync::OnceLock::new();
    let service = TEST_SERVICE
        .get_or_init(|| {
            init_session_manager();
            qaqh_runtime::QaqhService::init(std::sync::Arc::new(qaqh_session::SessionManager::new(
                qaqh_types::platform::data_dir(),
            )))
        })
        .clone();
    let hub = std::sync::Arc::new(qaqh_ringing_runtime::RingingHub::with_database(
        String::from("test-epoch"),
        std::sync::Arc::clone(service.owners().db()),
    ));
    let (shutdown, _) = tokio::sync::watch::channel(false);
    crate::axum_server::axum_impl::AppState {
        hub,
        leases,
        pending,
        service,
        admin_token: String::from("test-token"),
        devices: std::sync::Arc::new(std::sync::Mutex::new(
            qaqh_ringing_runtime::DeviceRegistry::new(),
        )),
        pairings: std::sync::Arc::new(std::sync::Mutex::new(
            crate::axum_server::PairingTable::new(),
        )),
        tls_fingerprint: None,
        challenges: std::sync::Arc::new(crate::axum_server::ChallengeStore::default()),
        drivers: std::sync::Arc::new(DriverSeats::new()),
        epoch: String::from("test-epoch"),
        shutdown,
        test_hooks: std::sync::Arc::new(crate::axum_server::TestHooks::disabled()),
    }
}
