//! 六大跨代不变量守卫（`ADR-0042` 铁律 4/5/6，计划 §三.4.3）。
//!
//! 六条守卫全部跑在**真双端 + 真多路复用**上（`tokio::io::duplex` 内存双工 + `yamux`）：
//! 客户端是客户端 crate 里的 `V3Client`，服务端是 `V3Server`，中间没有捷径、
//! 没有直连函数调用、没有"测试专用旁路"。
//!
//! ## 按后端参数化
//!
//! 每条守卫的函数体只认 `MuxTransport`，后端由 [`guard_for!`] 注入：`yamux`（本地
//! 定案）与 `v4.8` 的 `iroh` 回环走完全相同的断言。**加后端只加一行实例化**，
//! 不给任何后端开小灶。
//!
//! ## 不达标怎么办
//!
//! ② 的量化红线（控制面 P99 < 50ms、PTY 回显 P99 ≤ 16.6ms）不是建议值：未达标即
//! 触发 `ADR-0042` 第四节第 10 条——整体切 `iroh` 回环，**不调参续命**。
//! 所以这里只报实测数字与断言，绝不放宽阈值。

mod disconnect;
mod identity;
mod isolation;
mod multi_device;
mod render_budget;
mod subscribe_replay;

/// 把"某后端的一对端点"喂给一条守卫。
///
/// `$body` 是 `fn(fn() -> (B, B)) -> impl Future`，由 `B` 决定后端；
/// 守卫体内需要多条连接时（断联重连、多端联动）自己再调 `pair()`。
macro_rules! guard_for {
    ($test:ident => $body:path) => {
        guard_for!(@backend $test => $body, qaqh_v3_transport::YamuxTransport);
    };
    (@backend $test:ident => $body:path, $backend:ty) => {
        #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
        async fn $test() {
            // 后端在这里定：`$body` 的 `B` 由 `pair` 的函数指针类型推出来，
            // 守卫体本身对后端一无所知。
            let pair: fn() -> ($backend, $backend) = crate::v3_server::harness::duplex_backend;
            $body(pair).await;
        }
    };
}

guard_for!(guard_01_atomic_subscribe_and_bounded_replay_tail => subscribe_replay::guard);
guard_for!(guard_02_backpressure_isolation_with_quantified_latency => isolation::guard);
guard_for!(guard_03_disconnect_keeps_process_and_replays_receipt => disconnect::guard);
guard_for!(guard_04_identity_is_single_authority => identity::guard);
guard_for!(guard_05_multi_device_driver_ordering_and_revocation => multi_device::guard);
guard_for!(guard_06_frontend_render_budget_and_ordering => render_budget::guard);
