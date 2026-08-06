//! `window.deepx` bridge: WebView2 WebMessage <-> deepx-client.
//!
//! - The renderer's preload API shape (see `apps/desktop/electron/preload.ts`)
//!   is recreated by an injected script that forwards every call over
//!   `window.chrome.webview.postMessage` and dispatches host events.
//! - The Rust side routes `invoke` messages to `deepx-client` (commands,
//!   queries, bootstrap, actions) and pumps client events back to the
//!   renderer as `event` messages.
//!
//! Threading: `BridgeCore` is `Send + Sync` and lives on the tokio side;
//! `Bridge` (WebView + outbox receiver) stays on the STA UI thread.
//!
//! Wire protocol (JSON):
//! ```text
//! renderer -> host : { "type":"invoke", "id":n, "method":"ringing.command", "params":{...} }
//! host -> renderer : { "type":"response", "id":n, "ok":true, "value":... }
//!                     { "type":"response", "id":n, "ok":false, "error":"..." }
//!                     { "type":"event", "kind":"ringing.batch", "payload":{...} }
//! ```

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use std::time::{Duration, Instant};

use deepx_client::{
    Channel, ChannelStatus, Client, ClientHandlers, ClientOptions, EventBatch, TimelineStatus,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use windows_webview::WebView;

use crate::shell_store::{
    parse_activities, parse_activity_event, project_session_meta, ActivityState, SessionItem,
};

/// Outbound messages queued on the UI thread (STA) and pumped to the WebView.
#[derive(Debug, Clone)]
pub enum OutMsg {
    Response { id: u64, ok: bool, value: Value, error: Option<String> },
    Event { kind: &'static str, payload: Value },
}

impl OutMsg {
    fn to_json(&self) -> Value {
        match self {
            OutMsg::Response { id, ok, value, error } => json!({
                "type": "response",
                "id": id,
                "ok": ok,
                "value": value,
                "error": error,
            }),
            OutMsg::Event { kind, payload } => json!({
                "type": "event",
                "kind": kind,
                "payload": payload,
            }),
        }
    }
}

/// XAML 标题栏状态投影（Web `shell.setHeader` 载荷）。
///
/// 字段名对齐 Web 侧 `HeaderState`（camelCase）。`#[serde(default)]` 保证
/// 未来字段扩展向后兼容（P-2 typed struct 预埋，见 WORKFLOW §6.1）。
#[derive(Debug, Clone, Default, PartialEq, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct HeaderState {
    pub view: String,
    pub title: String,
    pub workspace: String,
    pub info_open: bool,
    pub stats_open: bool,
    pub compacting: bool,
    pub compact_disabled: bool,
    pub undo_disabled: bool,
    pub pet_enabled: bool,
}

/// 标题栏动作（壳 → Web `shell.headerAction` 载荷）。
///
/// `action` tag + snake_case：`{"action":"workspace","path":...}`。
/// `ns` 命名空间预埋（P-1）：未来面板/对话框动作复用同一通道时加 ns 字段。
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum HeaderAction {
    /// ①workspace：壳弹目录对话框后把所选路径带回 Web（D2，见 WORKFLOW §3）。
    Workspace { path: Option<String> },
    /// ②location：壳直接处理（open_external），无需 Web 响应。
    Location,
    /// ③console：壳直接处理（DevTools），无需 Web 响应。
    Console,
    /// ④info：回传 Web 翻转 InfoPopover。
    Info,
    /// ⑤stats：回传 Web 翻转 ContextPanel。
    Stats,
    /// ⑥undo：回传 Web 执行 undoLastTurn。
    Undo,
    /// ⑦compact：回传 Web 执行 session.compact。
    Compact,
}

/// `Send + Sync` half of the bridge: client, lease bookkeeping, outbox sender.
/// Lives on the tokio side.
pub struct BridgeCore {
    client: Mutex<Option<Client>>,
    attached: Mutex<HashSet<String>>,
    /// Latest per-channel status payloads (mirrors Electron `ringing.status`).
    channel_status: Mutex<HashMap<String, Value>>,
    /// XAML 侧栏数据源：会话列表投影（`session.list` + `session.activity`）。
    sessions: Mutex<Vec<SessionItem>>,
    /// 实时活动状态（control `session_activity_changed` 事件增量更新）。
    activities: Mutex<HashMap<String, ActivityState>>,
    /// 侧栏数据版本：refresh / activity 事件后递增，UI 侧 timer 比对后刷新。
    session_rev: AtomicU64,
    /// XAML 侧栏当前选中的会话 seed。
    active_seed: Mutex<String>,
    /// XAML 标题栏数据源：Web `shell.setHeader` 状态投影（typed struct）。
    header_state: Mutex<HeaderState>,
    /// 标题栏状态版本：Web 推送后递增，UI 侧 timer 比对后刷新（同 session_rev）。
    header_rev: AtomicU64,
    /// daemon 失联检测（A 方案，WORKFLOW §7）：timeline 流非 Open 的起始时刻。
    timeline_stall_since: Mutex<Option<Instant>>,
    /// 三 ringing 通道无一 Open 的起始时刻。
    channels_stall_since: Mutex<Option<Instant>>,
    /// 重建进行中（防 ensure_client 重入）。
    rebuilding: AtomicBool,
    /// 最近一次重建时刻（冷却防抖，避免网络抖动时反复重建）。
    last_rebuild_at: Mutex<Instant>,
    /// 最近一次 timeline.activate 的 seed（重建后恢复前端 transcript 流）。
    last_timeline_seed: Mutex<String>,
    /// timeline 连接状态缓存（检测用；ringing 状态走 channel_status）。
    timeline_status: Mutex<Option<TimelineStatus>>,
    outbox_tx: std::sync::mpsc::Sender<OutMsg>,
}

/// 失联阈值：backoff 1+2+4+8=15s 内 4 次重试仍失败视为失联（daemon 重启/关闭）。
const STALL_THRESHOLD: Duration = Duration::from_secs(15);
/// 重建冷却：网络抖动时避免每 15s 重建一次。
const REBUILD_COOLDOWN: Duration = Duration::from_secs(60);

impl BridgeCore {
    fn respond(&self, id: u64, ok: bool, value: Value, error: Option<String>) {
        if !ok {
            log_diag(&format!("invoke {id} failed: {}", error.clone().unwrap_or_default()));
        }
        log_diag(&format!("respond {id} ok={ok}"));
        let _ = self.outbox_tx.send(OutMsg::Response { id, ok, value, error });
    }

    fn emit(&self, kind: &'static str, payload: Value) {
        let _ = self.outbox_tx.send(OutMsg::Event { kind, payload });
    }

    /// Spawn an invoke on the shared client runtime.
    pub fn spawn_invoke(&self, id: u64, method: &str, params: Value) {
        log_diag(&format!("invoke {id} {method}"));
        let core = self.self_arc();
        let method = method.to_string();
        let _ = deepx_client::runtime_handle().spawn(async move {
            let result = core.invoke(&method, params).await;
            match result {
                Ok(value) => core.respond(id, true, value, None),
                Err(err) => core.respond(id, false, json!(null), Some(err)),
            }
        });
    }

    /// Arc to self: `BridgeCore` is stored in an `Arc` by the UI-side Bridge.
    fn self_arc(&self) -> Arc<BridgeCore> {
        SHARED_CORE.get().expect("bridge core not initialized").clone()
    }

    // ── XAML 侧栏（shell_store 投影）──────────────────────────────

    /// (items, rev) 快照：UI 侧 timer 比对 rev 决定是否刷新列表。
    pub fn session_snapshot(&self) -> (Vec<SessionItem>, u64) {
        let items = self.sessions.lock().unwrap_or_else(|e| e.into_inner()).clone();
        let rev = self.session_rev.load(Ordering::Relaxed);
        (items, rev)
    }

    pub fn active_seed(&self) -> String {
        self.active_seed.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub fn set_active_seed(&self, seed: &str) {
        *self.active_seed.lock().unwrap_or_else(|e| e.into_inner()) = seed.to_string();
    }

    // ── XAML 标题栏（header 投影，同 sessions 模式）────────────────

    /// (state, rev) 快照：UI 侧 timer 比对 rev 决定是否刷新 TitleBar。
    pub fn header_snapshot(&self) -> (HeaderState, u64) {
        let state = self
            .header_state
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone();
        let rev = self.header_rev.load(Ordering::Relaxed);
        (state, rev)
    }

    /// Web `shell.setHeader` 载荷落缓存并递增 rev。
    /// 反序列化失败时保留旧状态（静默丢弃坏载荷，不中断链路）。
    pub fn apply_header(&self, payload: Value) {
        let Ok(state) = serde_json::from_value(payload) else {
            log_diag("apply_header: invalid payload, keeping previous state");
            return;
        };
        *self.header_state.lock().unwrap_or_else(|e| e.into_inner()) = state;
        self.header_rev.fetch_add(1, Ordering::Relaxed);
    }

    fn seed_set(&self) -> HashSet<String> {
        self.sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .map(|s| s.seed.clone())
            .collect()
    }

    /// XAML 侧生成 command_id（无 uuid 依赖；幂等键只需进程内唯一 + 单调）。
    fn next_command_id(&self) -> String {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let n = SEQ.fetch_add(1, Ordering::Relaxed);
        let ms = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis())
            .unwrap_or(0);
        format!("xaml-{ms}-{n}")
    }

    /// 后台刷新 `session.list` + `session.activity` → 投影进缓存 → rev++。
    /// UI 侧（sidebar timer）读取快照即可，无需跨线程回调。
    pub fn spawn_refresh_sessions(&self) {
        let core = self.self_arc();
        let _ = deepx_client::runtime_handle().spawn(async move {
            core.refresh_sessions_inner().await;
        });
    }

    async fn refresh_sessions_inner(&self) {
        let client = match self.ensure_client().await {
            Ok(client) => client,
            Err(err) => {
                log_diag(&format!("refresh_sessions: connect failed: {err}"));
                return;
            }
        };
        let list = match client.query("session.list", json!({})).await {
            Ok(v) => v,
            Err(err) => {
                log_diag(&format!("refresh_sessions: session.list failed: {err}"));
                return;
            }
        };
        let acts = match client.query("session.activity", json!({})).await {
            Ok(v) => v,
            Err(err) => {
                log_diag(&format!("refresh_sessions: session.activity failed: {err}"));
                return;
            }
        };
        let activities: HashMap<String, ActivityState> =
            parse_activities(&acts).into_iter().collect();
        let mut items = Vec::new();
        if let Some(arr) = list.as_array() {
            items.reserve(arr.len());
            for v in arr {
                let seed = v.get("seed").and_then(|s| s.as_str()).unwrap_or("");
                let running = v.get("running").and_then(|r| r.as_bool()).unwrap_or(false);
                if let Some(item) =
                    project_session_meta(v, activities.get(seed).copied(), running)
                {
                    items.push(item);
                }
            }
        }
        *self.sessions.lock().unwrap_or_else(|e| e.into_inner()) = items;
        *self.activities.lock().unwrap_or_else(|e| e.into_inner()) = activities;
        self.session_rev.fetch_add(1, Ordering::Relaxed);
        log_diag(&format!(
            "refresh_sessions: {} sessions",
            self.sessions.lock().unwrap_or_else(|e| e.into_inner()).len()
        ));
    }

    /// 新建会话：`session_create`（control）+ 轮询发现新 seed（对齐前端
    /// `waitForSessionCreated` 的 15s 超时）→ navigate chat。
    pub fn spawn_new_session(&self) {
        let core = self.self_arc();
        let _ = deepx_client::runtime_handle().spawn(async move {
            let client = match core.ensure_client().await {
                Ok(client) => client,
                Err(err) => {
                    log_diag(&format!("new_session: connect failed: {err}"));
                    return;
                }
            };
            // 先刷新拿基线，避免"空列表时把旧会话当新会话"。
            core.refresh_sessions_inner().await;
            let before = core.seed_set();
            let command_id = core.next_command_id();
            match client
                .command(
                    Channel::Control,
                    None,
                    command_id,
                    json!({ "type": "session_create", "close_current": false }),
                    None,
                )
                .await
            {
                Ok(_) => {
                    for _ in 0..30 {
                        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
                        core.refresh_sessions_inner().await;
                        let now = core.seed_set();
                        if let Some(new_seed) = now.iter().find(|s| !before.contains(*s)) {
                            let new_seed = new_seed.clone();
                            core.set_active_seed(&new_seed);
                            log_diag(&format!("new_session: created {new_seed}"));
                            core.navigate("chat", Some(&new_seed));
                            return;
                        }
                    }
                    log_diag("new_session: no new seed within 15s");
                }
                Err(err) => log_diag(&format!("new_session: command failed: {err}")),
            }
        });
    }

    /// 恢复会话：仅 `attach(seed)`（session_resume 语义）+ navigate chat。
    /// bootstrap/timeline 由 renderer 的 `resumeSession` 完成（幂等，避免双拉）。
    ///
    /// 幂等：seed 已是 active 时跳过 attach（挡重复 attach 的网络往返），
    /// **但仍 emit `shell.navigate`**——壳的 active_seed 只代表壳侧状态，
    /// renderer 视图可能已离开 chat（用户点过技能/设置，或 resume 失败回
    /// home），必须通知 renderer 切回，否则"点同一会话无反应"。
    pub fn spawn_resume(&self, seed: &str) {
        if self.active_seed() == seed {
            log_diag(&format!("resume {seed}: already active, re-navigate only"));
            self.navigate("chat", Some(seed));
            return;
        }
        let core = self.self_arc();
        let seed = seed.to_string();
        let _ = deepx_client::runtime_handle().spawn(async move {
            let client = match core.ensure_client().await {
                Ok(client) => client,
                Err(err) => {
                    log_diag(&format!("resume {seed}: connect failed: {err}"));
                    return;
                }
            };
            if let Err(err) = client.attach(&seed).await {
                log_diag(&format!("resume {seed}: attach failed: {err}"));
                return;
            }
            core.set_active_seed(&seed);
            // rev++ 让侧栏 timer 同步 active 高亮（selected_tag 受控刷新）。
            core.session_rev.fetch_add(1, Ordering::Relaxed);
            log_diag(&format!("resume: attached {seed}"));
            core.navigate("chat", Some(&seed));
        });
    }

    /// 删除会话：Ringing `session_close`（与前端 `request("session.delete")`
    /// 的映射一致）。注意 Ringing 面无专门删除命令——session_close 只关闭
    /// registry 实例、不删持久化文件（与 web 前端现状对齐；缺口待后端统一）。
    pub fn spawn_delete(&self, seed: &str) {
        let core = self.self_arc();
        let seed = seed.to_string();
        let _ = deepx_client::runtime_handle().spawn(async move {
            let client = match core.ensure_client().await {
                Ok(client) => client,
                Err(err) => {
                    log_diag(&format!("delete {seed}: connect failed: {err}"));
                    return;
                }
            };
            let command_id = core.next_command_id();
            match client
                .command(
                    Channel::Control,
                    Some(seed.clone()),
                    command_id,
                    json!({ "type": "session_close", "seed": seed }),
                    None,
                )
                .await
            {
                Ok(_) => {
                    if core.active_seed() == seed {
                        core.set_active_seed("");
                        core.navigate("home", None);
                    }
                    core.refresh_sessions_inner().await;
                }
                Err(err) => log_diag(&format!("delete {seed}: command failed: {err}")),
            }
        });
    }

    /// 通知 renderer 切换视图（XAML 侧栏的导航出口）。
    pub fn navigate(&self, view: &str, seed: Option<&str>) {
        let mut payload = json!({ "view": view });
        if let Some(seed) = seed {
            payload["seed"] = json!(seed);
        }
        self.emit("shell.navigate", payload);
    }

    async fn invoke(&self, method: &str, params: Value) -> Result<Value, String> {
        match method {
            // ── backend ────────────────────────────────────────────────
            "backend.connect" => {
                self.ensure_client().await?;
                Ok(json!({ "ok": true, "transport": "ringing" }))
            }
            "backend.status" => {
                let connected = self.client.lock().unwrap_or_else(|e| e.into_inner()).is_some();
                Ok(json!({ "connected": connected, "transport": if connected { "ringing" } else { "legacy" } }))
            }
            "backend.attach" | "backend.detach" => {
                let seed = params.get("seed").and_then(|v| v.as_str()).unwrap_or("").to_string();
                if seed.is_empty() {
                    return Err("session seed is required".into());
                }
                self.ensure_client().await?;
                let mut attached = self.attached.lock().unwrap_or_else(|e| e.into_inner());
                if method.ends_with("attach") {
                    // Ringing v1: attaching = session_resume (daemon records
                    // seed ownership for this client session).
                    let client = self.client.lock().unwrap_or_else(|e| e.into_inner()).clone();
                    let client = client.ok_or("client not connected")?;
                    drop(attached);
                    client.attach(&seed).await.map_err(|e| e.to_string())?;
                    self.attached
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(seed);
                } else {
                    attached.remove(&seed);
                }
                Ok(json!({ "ok": true, "transport": "ringing" }))
            }
            "backend.restart" => Err("backend.restart not implemented in winui shell".into()),
            "backend.request" => {
                let name = params.get("method").and_then(|v| v.as_str()).unwrap_or("");
                let inner = params.get("params").cloned().unwrap_or(json!({}));
                if name.is_empty() {
                    return Err("method is required".into());
                }
                let client = self.ensure_client().await?;
                client.action(name, inner).await.map_err(|e| e.to_string())
            }

            // ── ringing ────────────────────────────────────────────────
            "ringing.status" => {
                let statuses = self
                    .channel_status
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                Ok(json!(statuses))
            }
            "ringing.bootstrap" => {
                let seed = params.get("seed").and_then(|v| v.as_str()).unwrap_or("").to_string();
                if seed.is_empty() {
                    return Err("seed is required".into());
                }
                let client = self.ensure_client().await?;
                client.bootstrap(&seed).await.map_err(|e| e.to_string())
            }
            "ringing.snapshot" => {
                let seed = params.get("seed").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let channel = params.get("channel").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let client = self.ensure_client().await?;
                let snapshot = client.bootstrap(&seed).await.map_err(|e| e.to_string())?;
                Ok(snapshot.get(&channel).cloned().unwrap_or(json!(null)))
            }
            "ringing.command" => {
                let seed = params.get("seed").and_then(|v| v.as_str()).unwrap_or("").to_string();
                let channel = params.get("channel").and_then(|v| v.as_str()).unwrap_or("");
                let envelope = params.get("envelope").cloned().unwrap_or(json!({}));
                let channel = match channel {
                    "control" => Channel::Control,
                    "conversation" => Channel::Conversation,
                    "tool" => Channel::Tool,
                    _ => return Err("invalid channel".into()),
                };
                let command_id = envelope
                    .get("command_id")
                    .and_then(|v| v.as_str())
                    .unwrap_or("")
                    .to_string();
                let command = envelope.get("command").cloned().unwrap_or(json!({}));
                let expected_revision = envelope.get("expected_revision").and_then(|v| v.as_u64());
                let client = self.ensure_client().await?;
                let seed = if seed.is_empty() { None } else { Some(seed) };
                match client
                    .command(channel, seed, command_id.clone(), command, expected_revision)
                    .await
                {
                    Ok(ack) => Ok(ack),
                    Err(err) => {
                        // The POST response may be lost after daemon acceptance:
                        // resolve the uncertainty with the same command id.
                        match client.command_status(&command_id).await {
                            Ok(receipt) => {
                                if receipt.state == "failed" || receipt.state == "rejected" {
                                    Ok(json!({
                                        "command_id": command_id,
                                        "status": "rejected",
                                        "code": receipt.error_code.unwrap_or(receipt.state),
                                    }))
                                } else {
                                    Ok(json!({
                                        "command_id": command_id,
                                        "status": "accepted",
                                        "receipt_state": receipt.state,
                                    }))
                                }
                            }
                            Err(_) => Err(err.to_string()),
                        }
                    }
                }
            }
            "ringing.query" => {
                let path = params.get("path").and_then(|v| v.as_str()).unwrap_or("").to_string();
                if path.is_empty() {
                    return Err("query path is required".into());
                }
                let inner = params.get("params").cloned().unwrap_or(json!({}));
                let client = self.ensure_client().await?;
                client.query(&path, inner).await.map_err(|e| e.to_string())
            }

            // ── timeline ────────────────────────────────────────────────
            "timeline.activate" => {
                let seed = params.get("seed").and_then(|v| v.as_str()).unwrap_or("").to_string();
                if seed.is_empty() {
                    return Err("seed is required".into());
                }
                // A 方案：记录最近激活的 seed——daemon 失联重建后据此恢复
                // 前端 transcript 流（新 client 新 epoch，快照 watermark 续传）。
                *self
                    .last_timeline_seed
                    .lock()
                    .unwrap_or_else(|e| e.into_inner()) = seed.clone();
                let client = self.ensure_client().await?;
                client.activate_timeline(&seed).await.map_err(|e| e.to_string())
            }
            "timeline.status" => {
                let client = self.ensure_client().await?;
                match client.timeline_status().await {
                    Some(status) => Ok(timeline_status_to_json(&status)),
                    None => Ok(json!(null)),
                }
            }

            // ── desktop ─────────────────────────────────────────────────
            // openDialog/openImageDialog are intercepted on the UI thread by
            // Bridge::handle_message (COM dialogs need the STA apartment);
            // this arm only guards against future callers routing them here.
            "desktop.openDialog" | "desktop.openImageDialog" => {
                Err("desktop.openDialog must be handled on the UI thread".into())
            }
            "desktop.readFileBase64" => {
                let path = params.get("path").and_then(|v| v.as_str()).unwrap_or("");
                if path.is_empty() {
                    return Err("path is required".into());
                }
                let bytes = std::fs::read(path).map_err(|e| e.to_string())?;
                let ext = std::path::Path::new(path)
                    .extension()
                    .and_then(|e| e.to_str())
                    .unwrap_or("")
                    .to_lowercase();
                let mime = match ext.as_str() {
                    "png" => "image/png",
                    "jpg" | "jpeg" => "image/jpeg",
                    "gif" => "image/gif",
                    "webp" => "image/webp",
                    "bmp" => "image/bmp",
                    _ => "image/png",
                };
                Ok(json!({
                    "mimeType": mime,
                    "data": base64_encode(&bytes),
                    "size": bytes.len(),
                }))
            }
            "desktop.readTextFile" => {
                let path = params.get("path").and_then(|v| v.as_str()).unwrap_or("");
                if path.is_empty() {
                    return Err("path is required".into());
                }
                let content = std::fs::read_to_string(path).map_err(|e| e.to_string())?;
                Ok(json!({ "content": content, "size": content.len() }))
            }
            "desktop.confirm" => Ok(json!(true)),
            "desktop.openPath" => {
                let target = params.get("target").and_then(|v| v.as_str()).unwrap_or("");
                if target.is_empty() {
                    return Err("target is required".into());
                }
                open_external(target)?;
                Ok(json!(null))
            }
            "desktop.togglePet" | "desktop.getPetStatus" => Ok(json!(false)),
            "desktop.checkUpdate" => Ok(json!(null)),
            "desktop.stageUpdate" | "desktop.applyUpdate" => {
                Err("update flow is not implemented in the winui shell yet".into())
            }
            "desktop.openDevTools" => {
                // DevTools window is opened by the UI-side Bridge (has WebView).
                Err("openDevTools must be handled on the UI thread".into())
            }
            "desktop.setBackgroundMaterial" => Ok(json!(true)),

            _ => Err(format!("unknown bridge method: {method}")),
        }
    }

    /// Lazily connect the deepx-client and register event forwarding.
    async fn ensure_client(&self) -> Result<Client, String> {
        // A 方案：重建进行中时拒绝新连接（rebuild_client 内部持锁协调），
        // 避免双 client 竞态（两个 connect 各建一套 SSE 流）。
        if self.rebuilding.load(Ordering::Relaxed) {
            return Err("client is rebuilding after daemon stall".into());
        }
        if let Some(client) = self.client.lock().unwrap_or_else(|e| e.into_inner()).clone() {
            return Ok(client);
        }
        log_diag("ensure_client: connecting...");
        let client = Client::connect_async(ClientOptions {
            handlers: ClientHandlers {
                on_batch: Arc::new({
                    let core = self.self_arc();
                    move |batch: EventBatch| core.emit_batch(batch)
                }),
                on_status: Arc::new({
                    let core = self.self_arc();
                    move |channel: Channel, status: ChannelStatus| core.emit_status(channel, status)
                }),
                on_reset: Some(Arc::new({
                    let core = self.self_arc();
                    move |reset: deepx_client::ResetRequired| core.handle_reset(reset)
                })),
                on_timeline_entry: Arc::new({
                    let core = self.self_arc();
                    move |seed: String, entry: deepx_client::TimelineEntry| {
                        core.emit(
                            "timeline.entry",
                            json!({
                                "seed": seed,
                                "entry": serde_json::to_value(entry).unwrap_or(json!({})),
                            }),
                        );
                    }
                }),
                on_timeline_status: Arc::new({
                    let core = self.self_arc();
                    move |status: TimelineStatus| {
                        // A 方案：缓存状态供失联检测（timeline 流死循环判据）。
                        *core
                            .timeline_status
                            .lock()
                            .unwrap_or_else(|e| e.into_inner()) = Some(status.clone());
                        core.emit("timeline.status", timeline_status_to_json(&status));
                    }
                }),
                on_timeline_snapshot: Arc::new({
                    let core = self.self_arc();
                    move |snapshot: Value| {
                        core.emit("timeline.snapshot", snapshot);
                    }
                }),
            },
            launch_daemon_if_missing: true,
            ..Default::default()
        })
        .await
        .map_err(|e| {
            log_diag(&format!("ensure_client connect failed: {e}"));
            e.to_string()
        })?;
        *self.client.lock().unwrap_or_else(|e| e.into_inner()) = Some(client.clone());
        self.emit("backend.status", json!({ "connected": true, "transport": "ringing" }));
        Ok(client)
    }

    /// `ringing.reset_required`: re-bootstrap the affected session and push a
    /// fresh snapshot to the renderer (mirrors browserBridge `handleReset`).
    pub fn handle_reset(&self, reset: deepx_client::ResetRequired) {
        let core = self.self_arc();
        let _ = deepx_client::runtime_handle().spawn(async move {
            let client = match core.ensure_client().await {
                Ok(client) => client,
                Err(err) => {
                    log_diag(&format!("reset: reconnect failed: {err}"));
                    return;
                }
            };
            match client.bootstrap(&reset.seed).await {
                Ok(snapshot) => core.emit(
                    "ringing.snapshot",
                    json!({
                        "seed": reset.seed,
                        "channel": reset.channel,
                        "snapshot": snapshot,
                    }),
                ),
                Err(err) => log_diag(&format!(
                    "reset: bootstrap {} failed: {err}",
                    reset.seed
                )),
            }
        });
    }

    fn emit_batch(&self, batch: EventBatch) {
        // XAML 侧栏实时活动状态：control 频道 `session_activity_changed`
        // 增量更新缓存（不触发全量 refresh）。
        if batch.channel == Channel::Control {
            let mut changed = false;
            for env in &batch.envelopes {
                if let Some((seed, state)) = parse_activity_event(&env.event) {
                    self.activities
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .insert(seed.clone(), state);
                    let mut sessions = self.sessions.lock().unwrap_or_else(|e| e.into_inner());
                    if let Some(item) = sessions.iter_mut().find(|i| i.seed == seed) {
                        item.state = state;
                    }
                    changed = true;
                }
            }
            if changed {
                self.session_rev.fetch_add(1, Ordering::Relaxed);
            }
        }
        self.emit(
            "ringing.batch",
            json!({
                "schema": "deepx.Ringing",
                "version": 1,
                "channel": batch.channel.as_str(),
                "seed": batch.seed,
                "server_epoch": batch.server_epoch,
                "from_stream_seq": batch.from_stream_seq,
                "to_stream_seq": batch.to_stream_seq,
                "envelopes": batch
                    .envelopes
                    .iter()
                    .map(|e| serde_json::to_value(e).unwrap_or(json!({})))
                    .collect::<Vec<_>>(),
            }),
        );
    }

    fn emit_status(&self, channel: Channel, status: ChannelStatus) {
        // Field names mirror the TS `ChannelStatus` (camelCase): renderer code
        // inspects `state` and `serverEpoch` (e.g. ringingMonitor.activate).
        let payload = match status {
            ChannelStatus::Connecting => json!({ "state": "connecting" }),
            ChannelStatus::Open { server_epoch, cursor } => json!({
                "state": "open",
                "serverEpoch": server_epoch,
                "cursor": cursor,
            }),
            ChannelStatus::Reconnecting { retry_ms, last_cursor } => json!({
                "state": "reconnecting",
                "retryMs": retry_ms,
                "lastCursor": last_cursor,
            }),
            ChannelStatus::Closed { reason } => json!({ "state": "closed", "reason": reason }),
        };
        self.channel_status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .insert(channel.as_str().to_string(), payload.clone());
        self.emit(
            "ringing.status",
            json!({ "channel": channel.as_str(), "status": payload }),
        );
    }

    // ── A 方案：daemon 失联检测与 client 重建（WORKFLOW §7）────────────────
    //
    // 背景：daemon 重启后旧 lease（server_epoch/client_session_id）失效。
    // SSE 重连带旧 epoch 的 Last-Event-ID 被 daemon 静默按 0 处理（从头
    // 回放，ringing_http.rs parse_sse_cursor）；ringing 通道回放可恢复，但
    // timeline 客户端对回放的旧 seq 报 Protocol error（deepx-client
    // timeline.rs L257），重连死循环——事件流永久断，表现为"后端在处理
    // 但前端 UI 不更新"。修复：检测失联后重建 client（重新 open 拿新
    // epoch），并恢复已激活 seed 的流（快照驱动，前端零改动自愈）。

    /// 失联检测（pump 每 50ms 调用；纯内存轻量检查，无锁嵌套）。
    pub fn check_daemon_health(&self) {
        if self.rebuilding.load(Ordering::Relaxed) {
            return;
        }
        let now = Instant::now();
        let cooldown_ok = {
            let last = self.last_rebuild_at.lock().unwrap_or_else(|e| e.into_inner());
            now.duration_since(*last) >= REBUILD_COOLDOWN
        };
        if !cooldown_ok {
            return;
        }
        if self.compute_stall(now) {
            self.rebuild_client();
        }
    }

    /// 任一活跃流失联持续超阈值即视为 daemon 失联。
    fn compute_stall(&self, now: Instant) -> bool {
        // 1) timeline 流非 Open/Closed 持续超阈值——daemon 重启后
        //    timeline 回放 Protocol error 死循环的专属判据。
        if let Some(status) = self
            .timeline_status
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            let healthy =
                matches!(status, TimelineStatus::Open { .. } | TimelineStatus::Closed { .. });
            let mut since = self.timeline_stall_since.lock().unwrap_or_else(|e| e.into_inner());
            if healthy {
                *since = None;
            } else if since.is_none() {
                *since = Some(now);
            } else if now.duration_since(since.unwrap()) >= STALL_THRESHOLD {
                log_diag("health: timeline stream stalled, rebuilding client");
                return true;
            }
        }

        // 2) ringing 三通道无一 Open 持续超阈值——daemon 完全不可达场景。
        let statuses = self.channel_status.lock().unwrap_or_else(|e| e.into_inner());
        let any_open = statuses
            .values()
            .any(|v| v.get("state").and_then(|s| s.as_str()) == Some("open"));
        let any_tracked = statuses.values().any(|v| !v.is_null());
        let mut since = self.channels_stall_since.lock().unwrap_or_else(|e| e.into_inner());
        if any_open || !any_tracked {
            *since = None;
        } else if since.is_none() {
            *since = Some(now);
        } else if now.duration_since(since.unwrap()) >= STALL_THRESHOLD {
            log_diag("health: all ringing channels stalled, rebuilding client");
            return true;
        }

        false
    }

    /// 重建 client：停旧（close）→ 重新 open（新 epoch）→ 恢复已激活的流。
    fn rebuild_client(&self) {
        self.rebuilding.store(true, Ordering::Relaxed);
        *self.last_rebuild_at.lock().unwrap_or_else(|e| e.into_inner()) = Instant::now();
        log_diag("health: rebuilding client (daemon stall detected)");
        let core = self.self_arc();
        let _ = deepx_client::runtime_handle().spawn(async move {
            // 1) 停旧 client 及其全部任务（renewal + 3 通道 + timeline 流）。
            let old = core.client.lock().unwrap_or_else(|e| e.into_inner()).take();
            if let Some(client) = old {
                client.close();
                log_diag("health: closed stale client");
            }
            // 2) 重新协商（新 server_epoch + client_session_id；
            //    launch_daemon_if_missing 兜底拉起 daemon）。
            match core.ensure_client().await {
                Ok(_) => log_diag("health: reconnected with fresh session"),
                Err(err) => {
                    log_diag(&format!("health: reconnect failed: {err}"));
                    core.rebuilding.store(false, Ordering::Relaxed);
                    core.reset_stall_timers();
                    return;
                }
            }
            // 3) 恢复已 attach 的 seed（XAML 侧栏）+ Web 最近激活的 seed。
            let seeds: Vec<String> = {
                let mut set = core.attached.lock().unwrap_or_else(|e| e.into_inner()).clone();
                let tseed = core
                    .last_timeline_seed
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .clone();
                if !tseed.is_empty() {
                    set.insert(tseed);
                }
                set.into_iter().collect()
            };
            for seed in &seeds {
                core.restore_seed(seed).await;
            }
            // 4) 状态复位 + 前端通知。
            core.rebuilding.store(false, Ordering::Relaxed);
            core.reset_stall_timers();
            core.emit(
                "backend.status",
                json!({ "connected": true, "transport": "ringing" }),
            );
            core.spawn_refresh_sessions();
            log_diag("health: rebuild complete");
        });
    }

    fn reset_stall_timers(&self) {
        *self
            .timeline_stall_since
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
        *self
            .channels_stall_since
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
        *self
            .timeline_status
            .lock()
            .unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// 恢复单个 seed：attach（session_resume 语义）→ 每通道 bootstrap 快照
    /// → timeline 流（快照 watermark 续传）。前端 ringingMonitor /
    /// timelineMonitor 收到快照后全量重建；SSE 回放由 applied event_id
    /// 去重（renderer ringingStores L868），无重复应用。
    async fn restore_seed(&self, seed: &str) {
        let client = match self
            .client
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .clone()
        {
            Some(c) => c,
            None => {
                log_diag(&format!("health: restore {seed}: no client"));
                return;
            }
        };
        if let Err(err) = client.attach(seed).await {
            log_diag(&format!("health: attach {seed} failed: {err}"));
            return;
        }
        match client.bootstrap(seed).await {
            Ok(snapshot) => {
                // 前端 applySnapshotPayload 期望单通道快照（{state,
                // baseline_stream_seq, turns, ...}），按通道逐个推送。
                for channel in Channel::ALL {
                    if let Some(snap) = snapshot.get(channel.as_str()) {
                        self.emit(
                            "ringing.snapshot",
                            json!({
                                "seed": seed,
                                "channel": channel.as_str(),
                                "snapshot": snap,
                            }),
                        );
                    }
                }
            }
            Err(err) => log_diag(&format!("health: bootstrap {seed} failed: {err}")),
        }
        match client.activate_timeline(seed).await {
            Ok(snapshot) => self.emit("timeline.snapshot", snapshot),
            Err(err) => log_diag(&format!("health: timeline activate {seed} failed: {err}")),
        }
    }
}

/// Timeline connection status -> renderer shape (mirrors TS `TimelineStatus`).
fn timeline_status_to_json(status: &TimelineStatus) -> Value {
    match status {
        TimelineStatus::Connecting { seed } => json!({ "state": "connecting", "seed": seed }),
        TimelineStatus::Open {
            seed,
            server_epoch,
            cursor,
        } => json!({
            "state": "open",
            "seed": seed,
            "serverEpoch": server_epoch,
            "cursor": cursor,
        }),
        TimelineStatus::Reconnecting { seed, retry_ms, cursor } => json!({
            "state": "reconnecting",
            "seed": seed,
            "retryMs": retry_ms,
            "cursor": cursor,
        }),
        TimelineStatus::Closed { seed, reason } => json!({
            "state": "closed",
            "seed": seed,
            "reason": reason,
        }),
    }
}

static SHARED_CORE: OnceLock<Arc<BridgeCore>> = OnceLock::new();

/// UI-thread half of the bridge: WebView + outbox receiver.
pub struct Bridge {
    core: Arc<BridgeCore>,
    ui: UiOnly<UiState>,
}

/// STA-bound state: only ever touched from the WinUI UI thread
/// (`attach_webview` from on_ready, `pump` from the UI timer, devtools from
/// the message handler). The unsafe impls are sound under that discipline.
struct UiOnly<T>(T);
// Safety: confined to the UI thread (see UiState doc).
unsafe impl<T> Send for UiOnly<T> {}
// Safety: confined to the UI thread (see UiState doc).
unsafe impl<T> Sync for UiOnly<T> {}

struct UiState {
    webview: Mutex<Option<WebView>>,
    registration: Mutex<Option<windows_webview::EventRegistration>>,
    outbox_rx: std::sync::mpsc::Receiver<OutMsg>,
}

static SHARED: OnceLock<Arc<Bridge>> = OnceLock::new();

impl Bridge {
    pub fn shared() -> Arc<Bridge> {
        SHARED
            .get_or_init(|| {
                let (tx, rx) = std::sync::mpsc::channel();
                let core = Arc::new(BridgeCore {
                    client: Mutex::new(None),
                    attached: Mutex::new(HashSet::new()),
                    channel_status: Mutex::new(HashMap::from([
                        ("control".to_string(), json!(null)),
                        ("conversation".to_string(), json!(null)),
                        ("tool".to_string(), json!(null)),
                    ])),
                    sessions: Mutex::new(Vec::new()),
                    activities: Mutex::new(HashMap::new()),
                    session_rev: AtomicU64::new(0),
                    active_seed: Mutex::new(String::new()),
                    header_state: Mutex::new(HeaderState::default()),
                    header_rev: AtomicU64::new(0),
                    timeline_stall_since: Mutex::new(None),
                    channels_stall_since: Mutex::new(None),
                    rebuilding: AtomicBool::new(false),
                    last_rebuild_at: Mutex::new(Instant::now()),
                    last_timeline_seed: Mutex::new(String::new()),
                    timeline_status: Mutex::new(None),
                    outbox_tx: tx,
                });
                let _ = SHARED_CORE.set(core.clone());
                Arc::new(Bridge {
                    core,
                    ui: UiOnly(UiState {
                        webview: Mutex::new(None),
                        registration: Mutex::new(None),
                        outbox_rx: rx,
                    }),
                })
            })
            .clone()
    }

    /// Called from the webview `on_ready` callback (UI thread).
    pub fn attach_webview(&self, webview: WebView) {
        *self.ui.0.webview.lock().unwrap_or_else(|e| e.into_inner()) = Some(webview);
    }

    /// XAML 侧栏访问 tokio 侧状态（会话列表 / 命令出口）。
    pub fn core(&self) -> Arc<BridgeCore> {
        self.core.clone()
    }

    // ── XAML 侧栏命令透传（sidebar.rs 只依赖 Bridge）─────────────────

    pub fn spawn_refresh_sessions(&self) {
        self.core.spawn_refresh_sessions();
    }

    pub fn spawn_new_session(&self) {
        self.core.spawn_new_session();
    }

    pub fn spawn_resume(&self, seed: &str) {
        self.core.spawn_resume(seed);
    }

    pub fn spawn_delete(&self, seed: &str) {
        self.core.spawn_delete(seed);
    }

    pub fn navigate(&self, view: &str, seed: Option<&str>) {
        self.core.navigate(view, seed);
    }

    // ── XAML 标题栏 STA 能力（header.rs 只依赖 Bridge；①②③ 壳直接处理）──

    /// ①workspace：目录选择对话框（STA COM；用户取消返回 Ok(null)）。
    pub fn pick_workspace_directory(&self) -> Result<Value, String> {
        show_open_dialog(true, false, false, None)
    }

    /// ②location：系统 shell 打开会话目录（bridge.rs `open_external`）。
    pub fn open_path(&self, target: &str) -> Result<(), String> {
        open_external(target)
    }

    /// ③console：DevTools 窗口（WebView 在 STA 线程，与 handle_message 同约束）。
    pub fn open_devtools(&self) -> bool {
        if let Some(webview) = self.ui.0.webview.lock().unwrap_or_else(|e| e.into_inner()).clone() {
            return webview.open_dev_tools_window().is_ok();
        }
        false
    }

    /// ④-⑦ 动作回传 Web（`shell.headerAction` 事件；D1 事件语义，
    /// 与 shell.navigate 同机制——壳 → Web 无 invoke 通道）。
    pub fn emit_header_action(&self, action: HeaderAction) {
        if let Ok(payload) = serde_json::to_value(action) {
            self.core.emit("shell.headerAction", payload);
        }
    }

    /// 壳系统主题变化（P-5）→ Web 校正（`shell.themeChanged` 事件）。
    pub fn emit_theme_changed(&self, scheme: windows_reactor::ColorScheme) {
        let mode = match scheme {
            windows_reactor::ColorScheme::Light => "light",
            windows_reactor::ColorScheme::Dark => "dark",
        };
        self.core.emit("shell.themeChanged", json!({ "mode": mode }));
    }

    /// Keep the web-message event registration alive for the process lifetime.
    pub fn attach_registration(&self, registration: windows_webview::EventRegistration) {
        *self.ui.0.registration.lock().unwrap_or_else(|e| e.into_inner()) = Some(registration);
    }

    /// Called from `on_web_message_received` (UI thread).
    pub fn handle_message(&self, raw: String) {
        let Ok(msg) = serde_json::from_str::<Value>(&raw) else {
            return;
        };
        // WebView2 may deliver the payload double-encoded (a JSON string
        // literal); unwrap one layer when that happens.
        let msg = match &msg {
            Value::String(s) => serde_json::from_str::<Value>(s).unwrap_or(msg.clone()),
            _ => msg,
        };
        // Renderer 上报（bridge.js reportError，无 id/method）——必须在取
        // id 之前处理，否则被 `msg.get("id")` 的 else-return 静默丢弃。
        if msg.get("type").and_then(|v| v.as_str()) == Some("log") {
            let level = msg.get("level").and_then(|v| v.as_str()).unwrap_or("info");
            let text = msg.get("msg").and_then(|v| v.as_str()).unwrap_or("");
            log_diag(&format!("[renderer:{level}] {text}"));
            return;
        }
        let Some(id) = msg.get("id").and_then(|v| v.as_u64()) else {
            return;
        };
        let Some(method) = msg.get("method").and_then(|v| v.as_str()) else {
            return;
        };
        let params = msg.get("params").cloned().unwrap_or(json!({}));
        if method == "desktop.openDevTools" {
            if let Some(webview) = self.ui.0.webview.lock().unwrap_or_else(|e| e.into_inner()).clone() {
                let _ = webview.open_dev_tools_window();
            }
            self.core.respond(id, true, json!(true), None);
            return;
        }
        // File/folder dialogs must run on the STA UI thread (COM apartment):
        // intercept here, mirroring the openDevTools pattern, instead of
        // dispatching to the tokio-side BridgeCore::invoke.
        if method == "desktop.openDialog" || method == "desktop.openImageDialog" {
            let result = if method == "desktop.openImageDialog" {
                show_open_dialog(false, false, true, None)
            } else {
                let directory = params
                    .get("directory")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let multiple = params
                    .get("multiple")
                    .and_then(|v| v.as_bool())
                    .unwrap_or(false);
                let title = params
                    .get("title")
                    .and_then(|v| v.as_str())
                    .map(|s| s.to_string());
                show_open_dialog(directory, multiple, false, title.as_deref())
            };
            match result {
                Ok(value) => self.core.respond(id, true, value, None),
                Err(err) => self.core.respond(id, false, json!(null), Some(err)),
            }
            return;
        }
        // ── shell.* 壳本地方法（不进 client；P-1 分发表，见 WORKFLOW §6.1）──
        // shell.setHeader：Web 状态投影 → TitleBar 数据源。
        if method == "shell.setHeader" {
            self.core.apply_header(params);
            self.core.respond(id, true, json!(null), None);
            return;
        }
        // shell.setTheme：三态进协议（P-5），壳映射渲染。
        if method == "shell.setTheme" {
            let mode = params.get("mode").and_then(|v| v.as_str()).unwrap_or("");
            let theme = match mode {
                "light" => windows_reactor::RequestedTheme::Light,
                "dark" | "dark-gray" => windows_reactor::RequestedTheme::Dark,
                _ => windows_reactor::RequestedTheme::Default,
            };
            windows_reactor::set_requested_theme(theme);
            self.core.respond(id, true, json!(null), None);
            return;
        }
        self.core.spawn_invoke(id, method, params);
    }

    /// Drain the outbox to the WebView (UI thread, called by a timer).
    pub fn pump(&self) {
        // A 方案：daemon 失联检测（轻量内存检查；重建在 tokio 侧执行）。
        self.core.check_daemon_health();
        let Some(webview) = self.ui.0.webview.lock().unwrap_or_else(|e| e.into_inner()).clone() else {
            return;
        };
        while let Ok(msg) = self.ui.0.outbox_rx.try_recv() {
            let json = msg.to_json().to_string();
            let posted = webview.post_web_message_as_json(&json).is_ok();
            if let OutMsg::Event { kind, .. } = &msg {
                log_diag(&format!("pump: event {kind} posted={posted}"));
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::mpsc;

    fn test_core() -> BridgeCore {
        let (tx, _rx) = mpsc::channel();
        BridgeCore {
            client: Mutex::new(None),
            attached: Mutex::new(HashSet::new()),
            channel_status: Mutex::new(HashMap::from([
                ("control".to_string(), json!(null)),
                ("conversation".to_string(), json!(null)),
                ("tool".to_string(), json!(null)),
            ])),
            sessions: Mutex::new(Vec::new()),
            activities: Mutex::new(HashMap::new()),
            session_rev: AtomicU64::new(0),
            active_seed: Mutex::new(String::new()),
            header_state: Mutex::new(HeaderState::default()),
            header_rev: AtomicU64::new(0),
            timeline_stall_since: Mutex::new(None),
            channels_stall_since: Mutex::new(None),
            rebuilding: AtomicBool::new(false),
            last_rebuild_at: Mutex::new(Instant::now()),
            last_timeline_seed: Mutex::new(String::new()),
            timeline_status: Mutex::new(None),
            outbox_tx: tx,
        }
    }

    fn reconnecting() -> TimelineStatus {
        TimelineStatus::Reconnecting {
            seed: "s1".into(),
            retry_ms: 1000,
            cursor: 3,
        }
    }

    #[test]
    fn timeline_stall_triggers_only_after_threshold() {
        let core = test_core();
        let now = Instant::now();
        // 首次出现非 Open 状态：开始计时，不触发。
        *core.timeline_status.lock().unwrap() = Some(reconnecting());
        assert!(!core.compute_stall(now));
        assert!(core.timeline_stall_since.lock().unwrap().is_some());
        // 未到阈值：仍不触发。
        *core.timeline_stall_since.lock().unwrap() =
            Some(now - STALL_THRESHOLD + Duration::from_secs(1));
        assert!(!core.compute_stall(now));
        // 超过阈值：触发。
        *core.timeline_stall_since.lock().unwrap() =
            Some(now - STALL_THRESHOLD - Duration::from_secs(1));
        assert!(core.compute_stall(now));
    }

    #[test]
    fn open_timeline_status_resets_stall_timer() {
        let core = test_core();
        *core.timeline_stall_since.lock().unwrap() = Some(Instant::now() - Duration::from_secs(60));
        *core.timeline_status.lock().unwrap() = Some(TimelineStatus::Open {
            seed: "s1".into(),
            server_epoch: "e1".into(),
            cursor: 9,
        });
        assert!(!core.compute_stall(Instant::now()));
        assert!(core.timeline_stall_since.lock().unwrap().is_none());
    }

    #[test]
    fn all_channels_stalled_triggers_but_single_open_resets() {
        let core = test_core();
        let now = Instant::now();
        let mut reconnecting_map: HashMap<String, Value> = HashMap::new();
        for ch in ["control", "conversation", "tool"] {
            reconnecting_map.insert(ch.to_string(), json!({ "state": "reconnecting" }));
        }
        *core.channel_status.lock().unwrap() = reconnecting_map;
        // 开始计时，不触发。
        assert!(!core.compute_stall(now));
        assert!(core.channels_stall_since.lock().unwrap().is_some());
        // 超过阈值：触发。
        *core.channels_stall_since.lock().unwrap() =
            Some(now - STALL_THRESHOLD - Duration::from_secs(1));
        assert!(core.compute_stall(now));

        // 任一通道 open → 重置计时。
        let core2 = test_core();
        *core2.channels_stall_since.lock().unwrap() =
            Some(now - STALL_THRESHOLD - Duration::from_secs(1));
        core2.channel_status.lock().unwrap().insert(
            "conversation".into(),
            json!({ "state": "open", "serverEpoch": "e1", "cursor": 0 }),
        );
        assert!(!core2.compute_stall(now));
        assert!(core2.channels_stall_since.lock().unwrap().is_none());
    }

    #[test]
    fn untracked_or_null_status_never_stalls() {
        // 无 client（状态为 null / 空）：不触发、不残留计时。
        let core = test_core();
        assert!(!core.compute_stall(Instant::now()));
        assert!(core.channels_stall_since.lock().unwrap().is_none());
        assert!(core.timeline_stall_since.lock().unwrap().is_none());
    }

    #[test]
    fn rebuild_cooldown_blocks_repeated_rebuilds() {
        let core = test_core();
        *core.last_rebuild_at.lock().unwrap() = Instant::now();
        // 冷却期内即使 stall 也不触发 rebuild（check 的 cooldown 分支）。
        *core.timeline_status.lock().unwrap() = Some(reconnecting());
        *core.timeline_stall_since.lock().unwrap() =
            Some(Instant::now() - STALL_THRESHOLD - Duration::from_secs(1));
        core.check_daemon_health();
        // rebuild_client 未执行（冷却）：rebuilding 保持 false。
        assert!(!core.rebuilding.load(Ordering::Relaxed));
    }
}

/// Minimal file logger (GUI subsystem has no console).
fn log_diag(msg: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(std::env::var("DEEPX_WINUI_LOG").unwrap_or_else(|_| ".deepx-winui.log".into()))
    {
        let _ = writeln!(f, "{}", msg);
    }
}

/// Minimal base64 encoder (avoid a dependency for a single-use helper).
fn base64_encode(bytes: &[u8]) -> String {
    const ALPHABET: &[u8; 64] =
        b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let b0 = chunk[0] as u32;
        let b1 = chunk.get(1).copied().unwrap_or(0) as u32;
        let b2 = chunk.get(2).copied().unwrap_or(0) as u32;
        let n = (b0 << 16) | (b1 << 8) | b2;
        out.push(ALPHABET[(n >> 18) as usize & 63] as char);
        out.push(ALPHABET[(n >> 12) as usize & 63] as char);
        if chunk.len() > 1 {
            out.push(ALPHABET[(n >> 6) as usize & 63] as char);
        } else {
            out.push('=');
        }
        if chunk.len() > 2 {
            out.push(ALPHABET[n as usize & 63] as char);
        } else {
            out.push('=');
        }
    }
    out
}

/// Open a path/URL with the system shell (best effort).
fn open_external(target: &str) -> Result<(), String> {
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        std::process::Command::new("cmd")
            .args(["/c", "start", "", target])
            .creation_flags(CREATE_NO_WINDOW)
            .spawn()
            .map(|_| ())
            .map_err(|e| e.to_string())
    }
    #[cfg(not(windows))]
    {
        let _ = std::process::Command::new("xdg-open").arg(target).spawn();
        Ok(())
    }
}

/// Show the native file/folder picker (Win32 `IFileOpenDialog`).
///
/// **Must be called on the STA UI thread** — this is enforced by the only
/// call site (`Bridge::handle_message`). Mirrors Electron
/// `dialog.showOpenDialog` semantics consumed by the renderer:
///   - `directory` -> `FOS_PICKFOLDERS` (folder picker)
///   - `multiple`  -> `FOS_ALLOWMULTISELECT` (result becomes a JSON array)
///   - `image_filter` -> picture file types filter
///   - cancel      -> `null`; single -> string; multiple -> array of strings
fn show_open_dialog(
    directory: bool,
    multiple: bool,
    image_filter: bool,
    title: Option<&str>,
) -> Result<Value, String> {
    use windows::Win32::{
        CLSCTX_ALL, COMDLG_FILTERSPEC, CoCreateInstance, ERROR_CANCELLED,
        FOS_ALLOWMULTISELECT, FOS_FILEMUSTEXIST, FOS_FORCEFILESYSTEM, FOS_PICKFOLDERS,
        FileOpenDialog, IFileOpenDialog,
    };
    use windows::core::{w, HSTRING};

    unsafe {
        let dialog: IFileOpenDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_ALL as u32)
            .map_err(|e| format!("CoCreateInstance(FileOpenDialog): {e}"))?;

        let mut options = FOS_FORCEFILESYSTEM | FOS_FILEMUSTEXIST;
        if directory {
            options |= FOS_PICKFOLDERS;
        }
        if multiple {
            options |= FOS_ALLOWMULTISELECT;
        }
        dialog
            .SetOptions(options)
            .ok()
            .map_err(|e| format!("IFileDialog::SetOptions: {e}"))?;

        if let Some(title) = title.filter(|t| !t.is_empty()) {
            let title = HSTRING::from(title);
            dialog
                .SetTitle(&title)
                .ok()
                .map_err(|e| format!("IFileDialog::SetTitle: {e}"))?;
        }

        if image_filter {
            let filters = [
                COMDLG_FILTERSPEC {
                    pszName: w!("Images"),
                    pszSpec: w!("*.png;*.jpg;*.jpeg;*.gif;*.webp;*.bmp"),
                },
                COMDLG_FILTERSPEC {
                    pszName: w!("All files"),
                    pszSpec: w!("*.*"),
                },
            ];
            dialog
                .SetFileTypes(filters.len() as u32, filters.as_ptr())
                .ok()
                .map_err(|e| format!("IFileDialog::SetFileTypes: {e}"))?;
        }

        // Show() is modal; ERROR_CANCELLED (user pressed Cancel / Esc) is
        // mapped to `null`, matching the preload API's cancel semantics.
        // (0.100 HRESULT has no from_win32 helper; build the code inline.)
        let hr = dialog.Show(None);
        if hr.is_err() && hr.0 == ((ERROR_CANCELLED as u32 | 0x8007_0000) as i32) {
            return Ok(json!(null));
        }
        hr.ok().map_err(|e| format!("IFileDialog::Show: {e}"))?;

        if multiple {
            let items = dialog
                .GetResults()
                .map_err(|e| format!("IFileOpenDialog::GetResults: {e}"))?;
            let count = items
                .GetCount()
                .map_err(|e| format!("IShellItemArray::GetCount: {e}"))?;
            let mut paths = Vec::with_capacity(count as usize);
            for i in 0..count {
                let item = items
                    .GetItemAt(i)
                    .map_err(|e| format!("IShellItemArray::GetItemAt({i}): {e}"))?;
                paths.push(shell_item_path(&item)?);
            }
            Ok(json!(paths))
        } else {
            let item = dialog
                .GetResult()
                .map_err(|e| format!("IFileDialog::GetResult: {e}"))?;
            Ok(json!(shell_item_path(&item)?))
        }
    }
}

/// Resolve an `IShellItem` to its filesystem path (`SIGDN_FILESYSPATH`).
/// The returned `PWSTR` is CoTaskMem-allocated and freed here.
fn shell_item_path(item: &windows::Win32::IShellItem) -> Result<String, String> {
    use windows::Win32::{CoTaskMemFree, SIGDN_FILESYSPATH};
    let pw = unsafe { item.GetDisplayName(SIGDN_FILESYSPATH) }
        .map_err(|e| format!("IShellItem::GetDisplayName(SIGDN_FILESYSPATH): {e}"))?;
    let path = unsafe { pw.to_string() }
        .map_err(|e| format!("selected path is not valid UTF-16: {e}"))?;
    unsafe { CoTaskMemFree(pw.0 as _) };
    Ok(path)
}
