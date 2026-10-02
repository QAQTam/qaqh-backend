//! daemon 生命周期（plan-webui-tauri D1 策略）。
//!
//! 读 discovery → 兼容校验（protocol_version + 发布 lane）→ 兼容则复用在跑
//! daemon;不兼容 → `conn://incompatible` 事件 + 前端给动作（退出旧实例 /
//! 一键停止后拉起 sidecar）,**不静默杀**（可能正在跑 TUI 的 Turn）;无 discovery
//! 或 pid 已死 → 经 `qaqh-client` 按需拉起 sidecar（dev 下默认
//! `target/debug/qaqh-daemon`,打包后为 exe 同目录）。
//!
//! app 自身多开由 `tauri-plugin-single-instance` 挡;宿主退出走 `Client::close()`
//! （detach,不触 `stop_daemon`）——窗口关闭 ≠ 取消 Turn/审批（§5.3）。

use std::path::PathBuf;
use std::sync::Mutex as StdMutex;

use qaqh_client::{Client, ClientOptions, StopStatus, read_discovery};
use qaqh_types::discovery::CONTROL_PROTOCOL_VERSION;
use serde_json::{Value, json};
use tauri::{AppHandle, Emitter, Manager};

use crate::challenge::ChallengeStore;
use crate::events;

/// 宿主全局状态（`tauri::Builder::manage`）。
pub struct HostState {
    client: StdMutex<Option<Client>>,
    /// 当前持有流的 active seed(单活动标签持流,plan D4)。
    pub active_seed: StdMutex<Option<String>>,
    pub approvals: ChallengeStore,
}

impl HostState {
    pub fn new() -> Self {
        Self {
            client: StdMutex::new(None),
            active_seed: StdMutex::new(None),
            approvals: ChallengeStore::default(),
        }
    }
}

impl Default for HostState {
    fn default() -> Self {
        Self::new()
    }
}

impl HostState {
    pub fn lock_client(&self) -> std::sync::MutexGuard<'_, Option<Client>> {
        self.client
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    pub fn lock_active_seed(&self) -> std::sync::MutexGuard<'_, Option<String>> {
        self.active_seed
            .lock()
            .unwrap_or_else(|error| error.into_inner())
    }

    /// 退出路径:detach（close）而非 stop_daemon。
    pub fn detach(&self) {
        if let Some(client) = self.lock_client().take() {
            client.close();
        }
    }
}

/// dev 便利:显式指认 daemon 二进制（默认走 qaqh-client 的候选顺序,即
/// `$QAQH_BACKEND_ROOT/target/debug` / exe 同目录）。
pub fn daemon_path_override() -> Option<PathBuf> {
    std::env::var_os("QAQH_DAEMON_PATH").map(PathBuf::from)
}

/// 宿主自身发布的 lane（与 daemon `server.rs::daemon_channel` 同规则）。
pub fn app_channel() -> String {
    std::env::var("QAQH_CHANNEL").unwrap_or_else(|_| {
        if cfg!(debug_assertions) {
            "dev".into()
        } else {
            "stable".into()
        }
    })
}

/// D1 兼容校验:在跑的 daemon 与宿主不匹配时返回问题详情（不杀进程）。
/// 无 discovery / pid 已死 → `None`（交给 client 的按需拉起路径）。
pub fn compat_problem() -> Option<Value> {
    let discovery = read_discovery().ok()?;
    if !qaqh_client::discovery::process_is_running(discovery.pid) {
        return None;
    }
    if discovery.protocol_version != CONTROL_PROTOCOL_VERSION {
        return Some(json!({
            "detail": format!(
                "协议版本不匹配(daemon v{}, 宿主 v{})",
                discovery.protocol_version, CONTROL_PROTOCOL_VERSION
            ),
            "endpoint": discovery.endpoint,
            "daemon_channel": discovery.channel,
            "daemon_protocol_version": discovery.protocol_version,
            "app_channel": app_channel(),
            "app_protocol_version": CONTROL_PROTOCOL_VERSION,
        }));
    }
    // 旧 discovery 未写 channel(空)时放行,避免误杀升级前的实例。
    if !discovery.channel.is_empty() && discovery.channel != app_channel() {
        return Some(json!({
            "detail": format!(
                "发布通道(lane)不匹配(daemon {}, 宿主 {})",
                discovery.channel, app_channel()
            ),
            "endpoint": discovery.endpoint,
            "daemon_channel": discovery.channel,
            "daemon_protocol_version": discovery.protocol_version,
            "app_channel": app_channel(),
            "app_protocol_version": CONTROL_PROTOCOL_VERSION,
        }));
    }
    None
}

/// 取（或建立）共享 daemon 连接。幂等:已连接直接复用。
pub async fn ensure_connected(app: &AppHandle) -> Result<Client, String> {
    let state = app.state::<HostState>();
    if let Some(client) = state.lock_client().clone() {
        return Ok(client);
    }
    if let Some(problem) = compat_problem() {
        let _ = app.emit("conn://incompatible", problem);
        return Err("daemon_incompatible".into());
    }
    let client = Client::connect_async(ClientOptions {
        handlers: events::build_handlers(app.clone()),
        launch_daemon_if_missing: true,
        daemon_path: daemon_path_override(),
        ..Default::default()
    })
    .await
    .map_err(|error| {
        let message = format!("daemon connect failed: {error}");
        let _ = app.emit("conn://error", json!({ "message": message }));
        message
    })?;
    *state.lock_client() = Some(client.clone());
    state.approvals.clear();
    Ok(client)
}

/// D1 动作:一键停止旧 daemon（走 control v1,不经兼容校验——被停对象本就是
/// 不兼容实例）。Busy = daemon 正在跑 Turn,不动它,由前端给动作文案。
pub async fn stop_stale_daemon() -> Result<StopStatus, String> {
    let discovery = match read_discovery() {
        Ok(discovery) => discovery,
        // discovery 已消失视为已停止(下一轮 ensure_connected 会拉起 sidecar)。
        Err(_) => return Ok(StopStatus::Stopping),
    };
    if !qaqh_client::discovery::process_is_running(discovery.pid) {
        return Ok(StopStatus::Stopping);
    }
    // 一次性 client:只为了 stop 调用,不进宿主状态。
    let client = Client::connect_async(ClientOptions {
        launch_daemon_if_missing: false,
        ..Default::default()
    })
    .await
    .map_err(|error| format!("stale daemon connect failed: {error}"))?;
    let status = client
        .stop_daemon(false)
        .await
        .map_err(|error| format!("stale daemon stop failed: {error}"));
    client.close();
    status
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn channel_matches_daemon_rule() {
        // 与 daemon server.rs::daemon_channel 保持同调(dev/stable 二分)。
        // SAFETY(测试专用):进程级 env 变异,edition 2024 下需显式 unsafe;
        // 此处与其他测试并行时无同变量竞争(QAQH_CHANNEL 仅本测试触碰)。
        unsafe { std::env::remove_var("QAQH_CHANNEL") };
        if cfg!(debug_assertions) {
            assert_eq!(app_channel(), "dev");
        } else {
            assert_eq!(app_channel(), "stable");
        }
    }

    #[test]
    fn daemon_path_override_reads_env() {
        // SAFETY(测试专用):见 channel_matches_daemon_rule。
        unsafe { std::env::set_var("QAQH_DAEMON_PATH", "/tmp/qaqh-daemon") };
        assert_eq!(
            daemon_path_override(),
            Some(PathBuf::from("/tmp/qaqh-daemon"))
        );
        unsafe { std::env::remove_var("QAQH_DAEMON_PATH") };
        assert_eq!(daemon_path_override(), None);
    }
}
