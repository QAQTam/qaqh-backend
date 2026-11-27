//! 六大跨代不变量守卫（`ADR-0042` r6：`yamux` + `local_ipc` + `iroh` 三后端矩阵）。

mod disconnect;
mod identity;
mod isolation;
mod multi_device;
mod render_budget;
mod subscribe_replay;

/// 为每条守卫同时实例化内存双工、OS 本地 IPC 与本机 QUIC 回环三套后端。
macro_rules! guard_for {
    ($yamux_test:ident, $local_test:ident, $iroh_test:ident => $body:path) => {
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $yamux_test() {
            $body(crate::v3_server::harness::YamuxFactory).await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $local_test() {
            $body(crate::v3_server::harness::LocalIpcFactory).await;
        }

        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $iroh_test() {
            $body(crate::v3_server::harness::IrohFactory).await;
        }
    };
}

guard_for!(
    guard_01_atomic_subscribe_and_bounded_replay_tail_yamux,
    guard_01_atomic_subscribe_and_bounded_replay_tail_local_ipc,
    guard_01_atomic_subscribe_and_bounded_replay_tail_iroh
    => subscribe_replay::guard
);
guard_for!(
    guard_02_backpressure_isolation_with_quantified_latency_yamux,
    guard_02_backpressure_isolation_with_quantified_latency_local_ipc,
    guard_02_backpressure_isolation_with_quantified_latency_iroh
    => isolation::guard
);
guard_for!(
    guard_03_disconnect_keeps_process_and_replays_receipt_yamux,
    guard_03_disconnect_keeps_process_and_replays_receipt_local_ipc,
    guard_03_disconnect_keeps_process_and_replays_receipt_iroh
    => disconnect::guard
);
guard_for!(
    guard_04_identity_is_single_authority_yamux,
    guard_04_identity_is_single_authority_local_ipc,
    guard_04_identity_is_single_authority_iroh
    => identity::guard
);
guard_for!(
    guard_05_multi_device_driver_ordering_and_revocation_yamux,
    guard_05_multi_device_driver_ordering_and_revocation_local_ipc,
    guard_05_multi_device_driver_ordering_and_revocation_iroh
    => multi_device::guard
);
guard_for!(
    guard_06_frontend_render_budget_and_ordering_yamux,
    guard_06_frontend_render_budget_and_ordering_local_ipc,
    guard_06_frontend_render_budget_and_ordering_iroh
    => render_budget::guard
);
