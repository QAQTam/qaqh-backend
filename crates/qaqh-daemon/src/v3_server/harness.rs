//! 六大不变量守卫的共享脚手架（仅测试可见，按后端参数化）。

use std::future::Future;
use std::sync::Arc;
use std::time::Duration;

use qaqh_client::v3::V3Client;
use qaqh_core_vocab::{BlockId, SessionId};
use qaqh_ringing_v3::{
    ClientCredential, ClientHello, ClientKind, ControlRequest, ControlResponse, SessionEvent,
    ShellRequest, ShellResponse,
};
use qaqh_v3_transport::duplex::duplex_pair_with_buffer;
use qaqh_v3_transport::{MuxConfig, MuxTransport, YamuxTransport};
use tokio::task::JoinHandle;

use super::V3Server;
use super::error::ServerError;
use super::facade::SkeletonBackend;
use super::identity::{DeviceKey, Scope};
use super::iroh_loopback::{IrohTransport, iroh_loopback_pair};
use super::session_runtime::SessionRuntime;
use super::state::{ServerConfig, ServerState};

/// 传输后端工厂：`yamux`（内存双工）与 `iroh`（本机 QUIC 回环）共用同一套守卫。
pub(crate) trait BackendFactory: Copy + Send + Sync + 'static {
    type Transport: MuxTransport;
    fn name() -> &'static str;
    fn pair(self) -> impl Future<Output = (Self::Transport, Self::Transport)> + Send;
}

#[derive(Clone, Copy)]
pub(crate) struct YamuxFactory;

impl BackendFactory for YamuxFactory {
    type Transport = YamuxTransport;
    fn name() -> &'static str {
        "yamux"
    }
    async fn pair(self) -> (Self::Transport, Self::Transport) {
        duplex_pair_with_buffer(MuxConfig::default(), 256 * 1024)
    }
}

#[derive(Clone, Copy)]
pub(crate) struct IrohFactory;

impl BackendFactory for IrohFactory {
    type Transport = IrohTransport;
    fn name() -> &'static str {
        "iroh"
    }
    async fn pair(self) -> (Self::Transport, Self::Transport) {
        iroh_loopback_pair().await
    }
}

/// 守卫里的"本机 OS 用户"（本地 IPC 的授权依据）。
pub(crate) const OS_USER: &str = "qaqh-v3-guard";

/// 守卫里的已配对设备令牌。
pub(crate) const DEVICE_TOKEN: &str = "guard-phone-token";

/// 本机用户设备键。
pub(crate) fn owner_device() -> DeviceKey {
    format!("os:{OS_USER}")
}

/// 已配对设备键。
pub(crate) fn paired_device() -> DeviceKey {
    format!("device:{DEVICE_TOKEN}")
}

/// 守卫用的服务端配置：环与订阅队列刻意调小，让"有界"这件事真的压得上。
pub(crate) fn config() -> ServerConfig {
    ServerConfig {
        build_id: "guard-build".to_string(),
        server_instance_id: "guard-instance".to_string(),
        channel: "dev".to_string(),
        ring_capacity: 2048,
        subscriber_queue: 512,
        inline_budget_bytes: 64 * 1024,
        blob_chunk_bytes: 64 * 1024,
        exec_delta_chunk_bytes: 4 * 1024,
        max_timeline_blocks: 4096,
        shell_history_bytes: 64 * 1024,
        shell_queue_chunks: 256,
    }
}

/// 建一份带一台已配对设备的状态。
pub(crate) fn state() -> Arc<ServerState> {
    let state = ServerState::new(config(), OS_USER);
    state
        .identity
        .register_device(DEVICE_TOKEN, "守卫手机", Scope::Device);
    state
}

/// 本机用户（主设备）的 hello。
pub(crate) fn owner_hello(instance: &str) -> ClientHello {
    ClientHello::new(
        instance,
        ClientKind::Desktop,
        ClientCredential::LocalOsUser {
            user: OS_USER.to_string(),
        },
    )
}

/// 已配对设备的 hello。
pub(crate) fn device_hello(instance: &str) -> ClientHello {
    ClientHello::new(
        instance,
        ClientKind::Mobile,
        ClientCredential::PairedDevice {
            token: DEVICE_TOKEN.to_string(),
        },
    )
}

/// 起一个服务端接受循环。
pub(crate) async fn spawn_server<B: MuxTransport>(
    transport: B,
    state: Arc<ServerState>,
) -> (Arc<V3Server<B, SkeletonBackend>>, JoinHandle<Result<(), ServerError>>) {
    let server = V3Server::with_state(transport, state);
    let handle = tokio::spawn(Arc::clone(&server).serve());
    (server, handle)
}

/// 连上一个客户端（握手必须最先在 Control 流上完成）。
pub(crate) async fn connect<B: MuxTransport>(
    transport: B,
    hello: ClientHello,
) -> V3Client<B> {
    V3Client::connect(transport, hello)
        .await
        .expect("握手必须成功")
}

/// 建一个会话并让某个设备可访问。
pub(crate) fn session_for(state: &Arc<ServerState>, accessor: &DeviceKey) -> Arc<SessionRuntime> {
    let session = state.create_session(&owner_device());
    session.allow_device(accessor);
    session
}

/// 开一个绑定会话的 shell，返回它的 id。
pub(crate) async fn open_shell<B: MuxTransport>(
    client: &V3Client<B>,
    session_id: &SessionId,
) -> String {
    match client
        .call(ControlRequest::Shell(ShellRequest::Open {
            session_id: session_id.clone(),
            cols: 120,
            rows: 40,
            cwd: None,
        }))
        .await
        .expect("开 shell")
    {
        ControlResponse::Shell(ShellResponse::Opened(view)) => view.shell_id,
        other => panic!("开 shell 回执形状不符：{other:?}"),
    }
}

/// 造一条 `AnswerDelta`。
pub(crate) fn answer(
    session_id: &SessionId,
    block_id: &BlockId,
    text: impl Into<String>,
) -> SessionEvent {
    SessionEvent::AnswerDelta {
        session_id: session_id.clone(),
        block_id: block_id.clone(),
        text: text.into(),
    }
}

/// 造一个块 id。
pub(crate) fn block(ordinal: u64) -> BlockId {
    BlockId::new(format!("guard-block-{ordinal:04}")).expect("block id")
}

/// 百分位（`samples` 会被就地排序；`0.99` = P99）。
pub(crate) fn percentile_ms(samples: &mut [Duration], fraction: f64) -> f64 {
    if samples.is_empty() {
        return f64::NAN;
    }
    samples.sort_unstable();
    let index = ((samples.len() as f64 - 1.0) * fraction).round() as usize;
    samples[index.min(samples.len() - 1)].as_secs_f64() * 1000.0
}

/// 均值（毫秒）。
pub(crate) fn mean_ms(samples: &[Duration]) -> f64 {
    if samples.is_empty() {
        return f64::NAN;
    }
    samples.iter().map(|sample| sample.as_secs_f64()).sum::<f64>() * 1000.0
        / samples.len() as f64
}

/// 最大值（毫秒）。
pub(crate) fn max_ms(samples: &[Duration]) -> f64 {
    samples
        .iter()
        .map(|sample| sample.as_secs_f64() * 1000.0)
        .fold(0.0f64, f64::max)
}

/// 机器指纹（写进守卫输出，便于复现与对比）。
pub(crate) fn machine_fingerprint() -> String {
    format!(
        "{} cores / {} MiB RAM / profile=debug",
        std::thread::available_parallelism()
            .map(|value| value.get())
            .unwrap_or(0),
        probe_total_memory_mib()
    )
}

fn probe_total_memory_mib() -> u64 {
    let Ok(info) = std::fs::read_to_string("/proc/meminfo") else {
        return 0;
    };
    for line in info.lines() {
        if let Some(rest) = line.strip_prefix("MemTotal:")
            && let Some(value) = rest.split_whitespace().next()
            && let Ok(kib) = value.parse::<u64>()
        {
            return kib / 1024;
        }
    }
    0
}
