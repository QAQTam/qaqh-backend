//! IPC 请求/响应面(plan §3 命令表)。`invoke_handler` 注册即白名单。
//!
//! 命令与前端 `TauriTransport` 方法一一对应;daemon token / lease 只存在于
//! Rust 宿主侧的 `qaqh-client`,任何命令都不向 webview 返回凭据。

use serde_json::{Value, json};
use tauri::{AppHandle, State};

use qaqh_client::{
    ClientV2CommandAck, CommandOptions, ConversationCommand, ConversationInputPurpose,
    RingingCommand, RingingCommandAckStatus, sanitize_session_list,
};

use crate::challenge::{command_for, command_options};
use crate::daemon::{self, HostState};

/// `service_rpc` 方法白名单(与 gateway 代理面一致;未列入的方法拒绝)。
const SERVICE_METHODS: &[&str] = &[
    "daemon.version",
    "session.list",
    "session.meta",
    "session.activity",
    "session.dashboard",
    "session.get_activity",
    "workspace.get",
    "workspace.list",
    "fs.list",
    "fs.read",
    "todo.status",
    "todo.list",
    "plan.read",
    "plan.context_stats",
    "stats.token_usage",
    "git.diff",
    "git.branch",
    "git.branches",
    "git.file_diff",
];

/// 这些方法之外都要注入 active session_id(gateway 语义平移)。
fn service_requires_session(method: &str) -> bool {
    !matches!(
        method,
        "daemon.version" | "session.list" | "session.activity" | "workspace.list"
    )
}

/// `fs.*` 还需要 `scope_session_id`。
fn service_scope_session(method: &str) -> bool {
    method.starts_with("fs.")
}

fn string_of(error: impl std::fmt::Display) -> String {
    error.to_string()
}

/// ack 序列化 + rejected 判定(gateway 语义:rejected 视为错误抛给前端)。
fn ack_to_result(ack: ClientV2CommandAck) -> Result<Value, String> {
    let rejected = ack.status == RingingCommandAckStatus::Rejected;
    let value = serde_json::to_value(&ack).map_err(string_of)?;
    if rejected {
        Err(format!(
            "command rejected ({}): {}",
            ack.code.as_deref().unwrap_or("unknown"),
            ack.message.as_deref().unwrap_or("")
        ))
    } else {
        Ok(value)
    }
}

fn active_seed_of(state: &HostState) -> Option<String> {
    state.lock_active_seed().clone()
}

/// `() → Vec<SessionMeta>`(sanitize 后字段同 gateway)。
#[tauri::command]
pub async fn session_list(app: AppHandle) -> Result<Value, String> {
    let client = daemon::ensure_connected(&app).await?;
    let raw: Value = client
        .service_v2("session.list", json!({}))
        .await
        .map_err(string_of)?;
    Ok(sanitize_session_list(raw))
}

/// `(seed) → ()`:attach(归属)+ 激活 timeline 流(单活动标签持流)。
#[tauri::command]
pub async fn attach(
    app: AppHandle,
    state: State<'_, HostState>,
    seed: String,
) -> Result<(), String> {
    let client = daemon::ensure_connected(&app).await?;
    let previous = {
        let mut active = state.lock_active_seed();
        let previous = active.clone();
        if previous.as_deref() != Some(seed.as_str()) {
            state.approvals.clear();
            *active = Some(seed.clone());
        }
        // 同 seed 重入不视为切换。
        previous.filter(|previous| previous.as_str() != seed.as_str())
    };
    if let Some(previous) = previous {
        client.deactivate_timeline(&previous).await;
    }
    client.attach(&seed).await.map_err(string_of)?;
    client.activate_timeline(&seed).await.map_err(string_of)?;
    Ok(())
}

/// `() → Vec<ApprovalView>`(challenge 由宿主签发;无 active seed 时为空)。
#[tauri::command]
pub async fn pending_approvals(
    app: AppHandle,
    state: State<'_, HostState>,
) -> Result<Vec<Value>, String> {
    let Some(seed) = active_seed_of(&state) else {
        return Ok(Vec::new());
    };
    let client = daemon::ensure_connected(&app).await?;
    let pending = client.pending_approvals(&seed).await.map_err(string_of)?;
    state
        .approvals
        .issue_views(&seed, &pending)
        .map_err(|code| format!("approval projection failed: {code}"))
}

/// `(challenge_id, decision, payload) → ()`。challenge 一次性消费 + scope 校验
/// 先于任何 daemon 调用(失败的命令不能复用同一 challenge)。
#[tauri::command]
pub async fn respond_approval(
    app: AppHandle,
    state: State<'_, HostState>,
    challenge_id: String,
    decision: String,
    payload: Value,
) -> Result<(), String> {
    let Some(seed) = active_seed_of(&state) else {
        return Err("no_active_seed".into());
    };
    let challenge = state
        .approvals
        .consume(&challenge_id, &seed)
        .map_err(str::to_string)?;
    let command = command_for(&challenge, &decision, &payload).map_err(str::to_string)?;
    let client = daemon::ensure_connected(&app).await?;
    let ack = client
        .send_command(Some(&seed), command, command_options())
        .await
        .map_err(string_of)?;
    ack_to_result(ack).map(|_| ())
}

/// `(seed, text) → ack`。
#[tauri::command]
pub async fn send_message(app: AppHandle, seed: String, text: String) -> Result<Value, String> {
    let client = daemon::ensure_connected(&app).await?;
    let ack = client
        .send_command(
            Some(&seed),
            RingingCommand::Conversation(ConversationCommand::ConversationSendMessage {
                text,
                images: Vec::new(),
                attachments: None,
                message_id: None,
                input_purpose: ConversationInputPurpose::TriggerTurn,
                as_system: false,
                inter_agent: None,
                subagent_terminal: None,
            }),
            CommandOptions::default(),
        )
        .await
        .map_err(string_of)?;
    ack_to_result(ack)
}

/// `(seed) → ack`。
#[tauri::command]
pub async fn cancel_turn(app: AppHandle, seed: String) -> Result<Value, String> {
    let client = daemon::ensure_connected(&app).await?;
    let ack = client
        .send_command(
            Some(&seed),
            RingingCommand::Conversation(ConversationCommand::ConversationCancel { turn_id: None }),
            CommandOptions::default(),
        )
        .await
        .map_err(string_of)?;
    ack_to_result(ack)
}

/// `() → ack`。新 seed 由前端轮询 sessions diff 发现(与现状一致)。
#[tauri::command]
pub async fn create_session(app: AppHandle) -> Result<Value, String> {
    let client = daemon::ensure_connected(&app).await?;
    let ack = client
        .send_command(
            None,
            RingingCommand::Control(qaqh_client::ControlCommand::SessionCreate {
                close_current: false,
                cwd: None,
                tool_mode: None,
                custom_tools: Vec::new(),
            }),
            CommandOptions::default(),
        )
        .await
        .map_err(string_of)?;
    ack_to_result(ack)
}

/// `(seed, limit, before_index) → page(含 server_epoch/has_more/truncated_before)`。
#[tauri::command]
pub async fn timeline_page(
    app: AppHandle,
    seed: String,
    limit: Option<u32>,
    before_index: Option<u64>,
) -> Result<Value, String> {
    let client = daemon::ensure_connected(&app).await?;
    let page = client
        .fetch_timeline_page(&seed, before_index, limit)
        .await
        .map_err(string_of)?;
    serde_json::to_value(&page).map_err(string_of)
}

/// `(seed) → bootstrap`(per-session 三频道快照,刷新 activity 用)。
#[tauri::command]
pub async fn session_bootstrap(app: AppHandle, seed: String) -> Result<Value, String> {
    let client = daemon::ensure_connected(&app).await?;
    let bootstrap = client.bootstrap_v2(&seed).await.map_err(string_of)?;
    serde_json::to_value(&bootstrap).map_err(string_of)
}

/// `(method, params) → Value`,方法白名单 + active session 注入。
#[tauri::command]
pub async fn service_rpc(
    app: AppHandle,
    state: State<'_, HostState>,
    method: String,
    params: Value,
) -> Result<Value, String> {
    if !SERVICE_METHODS.contains(&method.as_str()) {
        return Err(format!("service method not allowed: {method}"));
    }
    let client = daemon::ensure_connected(&app).await?;
    let mut params = params;
    if service_requires_session(&method) {
        let Some(seed) = active_seed_of(&state) else {
            return Err("no_active_seed".into());
        };
        let Some(object) = params.as_object_mut() else {
            return Err("params must be an object".into());
        };
        object
            .entry("session_id".to_string())
            .or_insert(json!(seed));
        if service_scope_session(&method) {
            let scope = object.get("session_id").cloned().unwrap_or(Value::Null);
            object
                .entry("scope_session_id".to_string())
                .or_insert(scope);
        }
    }
    client
        .service_v2::<_, Value>(&method, params)
        .await
        .map_err(string_of)
}

/// `(url) → ()`,仅 http/https,走系统浏览器。
#[tauri::command]
pub fn open_external(app: AppHandle, url: String) -> Result<(), String> {
    use tauri_plugin_opener::OpenerExt;
    if !(url.starts_with("http://") || url.starts_with("https://")) {
        return Err("only http/https urls are allowed".into());
    }
    app.opener().open_url(url, None::<&str>).map_err(string_of)
}

/// `(seed?) → ()`:宿主侧重连(用户点「重试」/窗口聚焦)。同时兜底建立连接。
#[tauri::command]
pub async fn streams_retry(
    app: AppHandle,
    state: State<'_, HostState>,
    seed: Option<String>,
) -> Result<(), String> {
    let client = daemon::ensure_connected(&app).await?;
    let target = match seed {
        Some(seed) => seed,
        None => active_seed_of(&state).ok_or_else(|| "no_active_seed".to_string())?,
    };
    client.activate_timeline(&target).await.map_err(string_of)?;
    Ok(())
}

/// `() → "stopping" | "busy" | "unsupported"`(D1 不兼容路径的一键停止)。
#[tauri::command]
pub async fn stop_stale_daemon() -> Result<&'static str, String> {
    let status = daemon::stop_stale_daemon().await?;
    Ok(match status {
        qaqh_client::StopStatus::Stopping => "stopping",
        qaqh_client::StopStatus::Busy => "busy",
        qaqh_client::StopStatus::Unsupported => "unsupported",
    })
}
