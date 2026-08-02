//! Ringing HTTP command/query + 三 SSE 事件流（daemon 侧传输层，T5）。
//!
//! 端点（PLAN 固定）：
//! ```text
//! POST /ringing/v2/clients/open
//! POST /ringing/v2/leases/renew
//! POST /ringing/v2/commands/{control|conversation|tool}
//! GET  /ringing/v2/commands/{command_id}
//! GET  /ringing/v2/sessions/{seed}/bootstrap
//! POST /ringing/v2/queries/{name}
//! POST /ringing/v2/actions/{name}
//! POST /ringing/v2/content
//! GET  /ringing/v2/content/{content_id}
//! GET  /ringing/v2/events/{control|conversation|tool}   (SSE)
//! ```
//!
//! 硬规则：
//! - SSE 断开只表示该频道退化，**不撤销** session lease（TTL + renew 维护）。
//! - token 只经 `Authorization` header，不进 query string。
//! - HTTP command ack 只表达 accepted/rejected；业务完成由对应频道可靠终态表达。

use std::collections::{HashMap, HashSet};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use deepx_domain::{ControlCommand, RingingChannel};
use deepx_ringing::{
    CLIENT_SESSION_HEADER, ClientOpenRequest, ClientOpenResponse, RINGING_BASE_PATH,
    RINGING_SCHEMA, RINGING_VERSION, RingingCommandAck, RingingCommandAckStatus,
    RingingCommandEnvelope, RingingCommandState, RingingCommandStatus, RingingEvent,
    RingingEventEnvelope, RingingResetRequired, RingingSessionBootstrap,
};
use deepx_runtime::ringing::query;
use deepx_runtime::{DeepxService, RingingHub};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use crate::server::random_hex;

fn stringify(error: impl std::fmt::Display) -> String {
    error.to_string()
}

const RENEW_TTL_MS: u64 = 30_000;
const RENEW_INTERVAL_MS: u64 = 10_000;
const TIMELINE_V3_BASE_PATH: &str = "/ringing/v3";
const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;
const SSE_KEEPALIVE_MS: u64 = 15_000;

/// Ringing 逻辑 client session lease。
///
/// 键 = 客户端自生成的 `client_instance_id`；值记录服务端签发的
/// `client_session_id`，后续请求必须通过 header 携带该 session id。
/// open 时双 id 关联，renew 按 client_session_id 反查续期。
#[derive(Debug, Default)]
pub struct RingingLeaseStore {
    leases: HashMap<String, LeaseEntry>,
    seed_leases: HashMap<String, HashSet<String>>,
}

#[derive(Debug, Clone)]
struct LeaseEntry {
    client_session_id: String,
    expiry: Instant,
}

/// 已 accepted 命令的幂等表（有界 TTL；accepted 后断线重试不得重复执行）。
#[derive(Debug, Default)]
pub struct PendingCommandStore {
    accepted: HashMap<String, CommandReceipt>,
    max_entries: usize,
    persistence_path: Option<PathBuf>,
}

#[derive(Debug, Clone)]
struct CommandReceipt {
    fingerprint: String,
    client_session_id: Option<String>,
    accepted_at: Instant,
    state: RingingCommandState,
    terminal_event_id: Option<String>,
    error_code: Option<String>,
}

#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
struct PersistedCommandReceipt {
    fingerprint: String,
    #[serde(default)]
    client_session_id: Option<String>,
    accepted_at_ms: u64,
    state: RingingCommandState,
    #[serde(default)]
    terminal_event_id: Option<String>,
    #[serde(default)]
    error_code: Option<String>,
}

impl PendingCommandStore {
    pub fn new() -> Self {
        Self {
            accepted: HashMap::new(),
            max_entries: 4096,
            persistence_path: None,
        }
    }

    /// Receipt 存在 daemon 数据目录的独立 Ringing v2 namespace；只保存哈希，
    /// 不把命令正文、用户文本或附件元数据写入磁盘。
    pub fn new_persistent() -> Self {
        let path = deepx_types::platform::data_dir().join("ringing-v2-command-receipts.json");
        let mut store = Self {
            persistence_path: Some(path.clone()),
            ..Self::new()
        };
        store.load(&path);
        store
    }

    fn load(&mut self, path: &std::path::Path) {
        let Ok(bytes) = std::fs::read(path) else {
            return;
        };
        let Ok(saved) = serde_json::from_slice::<HashMap<String, PersistedCommandReceipt>>(&bytes)
        else {
            log::warn!("[ringing] command receipt store is unreadable; starting empty");
            return;
        };
        let now_ms = unix_millis();
        for (command_id, receipt) in saved {
            let Some(age) = now_ms.checked_sub(receipt.accepted_at_ms) else {
                continue;
            };
            if age >= RECEIPT_TTL.as_millis() as u64 {
                continue;
            }
            self.accepted.insert(
                command_id,
                CommandReceipt {
                    fingerprint: receipt.fingerprint,
                    client_session_id: receipt.client_session_id,
                    accepted_at: Instant::now() - Duration::from_millis(age),
                    state: receipt.state,
                    terminal_event_id: receipt.terminal_event_id,
                    error_code: receipt.error_code,
                },
            );
        }
    }

    fn persist(&self) {
        let Some(path) = &self.persistence_path else {
            return;
        };
        let saved: HashMap<_, _> = self
            .accepted
            .iter()
            .filter_map(|(command_id, receipt)| {
                let age = receipt.accepted_at.elapsed();
                (age < RECEIPT_TTL).then(|| {
                    (
                        command_id.clone(),
                        PersistedCommandReceipt {
                            fingerprint: receipt.fingerprint.clone(),
                            client_session_id: receipt.client_session_id.clone(),
                            accepted_at_ms: unix_millis().saturating_sub(age.as_millis() as u64),
                            state: receipt.state,
                            terminal_event_id: receipt.terminal_event_id.clone(),
                            error_code: receipt.error_code.clone(),
                        },
                    )
                })
            })
            .collect();
        let Ok(bytes) = serde_json::to_vec(&saved) else {
            return;
        };
        if let Some(parent) = path.parent() {
            let _ = std::fs::create_dir_all(parent);
        }
        let tmp = path.with_extension("json.tmp");
        if std::fs::write(&tmp, bytes).is_ok() && std::fs::rename(&tmp, path).is_err() {
            let _ = std::fs::remove_file(&tmp);
        }
    }

    /// 记录 accepted。返回 false 表示重复（已 accepted 且未过期）。
    #[cfg(test)]
    pub fn record(&mut self, command_id: &str) -> bool {
        self.record_fingerprint(command_id, command_id)
            .unwrap_or(false)
    }

    /// 预留 receipt；相同 ID 不同 payload 是协议错误。
    #[cfg(test)]
    pub fn record_fingerprint(&mut self, command_id: &str, fingerprint: &str) -> Result<bool, ()> {
        self.record_fingerprint_owned(command_id, fingerprint, None)
    }

    pub fn record_fingerprint_for_session(
        &mut self,
        command_id: &str,
        fingerprint: &str,
        client_session_id: &str,
    ) -> Result<bool, ()> {
        self.record_fingerprint_owned(command_id, fingerprint, Some(client_session_id))
    }

    fn record_fingerprint_owned(
        &mut self,
        command_id: &str,
        fingerprint: &str,
        client_session_id: Option<&str>,
    ) -> Result<bool, ()> {
        let now = Instant::now();
        if let Some(receipt) = self.accepted.get(command_id) {
            if receipt.accepted_at + RECEIPT_TTL > now {
                if receipt.fingerprint != fingerprint
                    || receipt.client_session_id.as_deref() != client_session_id
                {
                    return Err(());
                }
                return Ok(false); // 重复：已接受且在 TTL 内
            }
        }
        self.accepted.insert(
            command_id.to_string(),
            CommandReceipt {
                fingerprint: fingerprint.to_string(),
                client_session_id: client_session_id.map(str::to_string),
                accepted_at: now,
                state: RingingCommandState::Accepted,
                terminal_event_id: None,
                error_code: None,
            },
        );
        while self.accepted.len() > self.max_entries {
            let victim = self
                .accepted
                .iter()
                .min_by_key(|(_, receipt)| receipt.accepted_at)
                .map(|(id, _)| id.clone())
                .expect("non-empty");
            self.accepted.remove(&victim);
        }
        self.persist();
        Ok(true)
    }

    #[cfg(test)]
    pub fn is_known(&self, command_id: &str) -> bool {
        self.accepted
            .get(command_id)
            .is_some_and(|receipt| receipt.accepted_at + RECEIPT_TTL > Instant::now())
    }

    /// 转发失败回滚预留。
    pub fn rollback(&mut self, command_id: &str) {
        self.accepted.remove(command_id);
        self.persist();
    }

    pub fn mark_running(&mut self, command_id: &str) {
        if let Some(receipt) = self.accepted.get_mut(command_id) {
            if receipt.state == RingingCommandState::Accepted {
                receipt.state = RingingCommandState::Running;
            }
            self.persist();
        }
    }

    /// 将带 causation_id 的可靠业务终态折叠进命令 receipt。ACK 只表示
    /// accepted；这里为断线后的 command-status 查询提供最终结果。
    pub fn observe_terminal_event(&mut self, envelope: &RingingEventEnvelope) {
        let Some(command_id) = envelope.causation_id.as_deref() else {
            return;
        };
        let terminal = match &envelope.event {
            RingingEvent::Control(deepx_domain::ControlEvent::OperationFailed {
                error, ..
            }) => Some((RingingCommandState::Failed, Some(error.code.clone()))),
            RingingEvent::Control(
                deepx_domain::ControlEvent::InteractionResolved { .. }
                | deepx_domain::ControlEvent::PlanReviewResolved { .. }
                | deepx_domain::ControlEvent::SkillsUpdated { .. }
                | deepx_domain::ControlEvent::SessionStateChanged { .. }
                | deepx_domain::ControlEvent::OperationCompleted { .. },
            ) => Some((RingingCommandState::Succeeded, None)),
            RingingEvent::Conversation(deepx_domain::ConversationEvent::TurnFailed {
                error,
                ..
            }) => Some((RingingCommandState::Failed, Some(error.code.clone()))),
            RingingEvent::Conversation(
                deepx_domain::ConversationEvent::TurnCompleted { .. }
                | deepx_domain::ConversationEvent::ConversationCancelled { .. },
            ) => Some((RingingCommandState::Succeeded, None)),
            RingingEvent::Conversation(deepx_domain::ConversationEvent::CompactFinished {
                status,
                ..
            }) => match status {
                deepx_domain::CompactStatus::Failed => {
                    Some((RingingCommandState::Failed, Some("compact_failed".into())))
                }
                _ => Some((RingingCommandState::Succeeded, None)),
            },
            RingingEvent::Tool(deepx_domain::ToolEvent::ToolFinished { .. }) => {
                Some((RingingCommandState::Succeeded, None))
            }
            RingingEvent::Tool(deepx_domain::ToolEvent::ToolFailed { error, .. }) => {
                Some((RingingCommandState::Failed, Some(error.code.clone())))
            }
            _ => None,
        };
        if let Some((state, error_code)) = terminal {
            self.mark_terminal(
                command_id,
                state,
                Some(envelope.event_id.clone()),
                error_code,
            );
        }
    }

    pub fn mark_terminal(
        &mut self,
        command_id: &str,
        state: RingingCommandState,
        event_id: Option<String>,
        error_code: Option<String>,
    ) {
        if let Some(receipt) = self.accepted.get_mut(command_id) {
            receipt.state = state;
            receipt.terminal_event_id = event_id;
            receipt.error_code = error_code;
            self.persist();
        }
    }

    pub fn status_for_session(
        &self,
        command_id: &str,
        client_session_id: &str,
    ) -> Option<RingingCommandStatus> {
        self.accepted.get(command_id).and_then(|receipt| {
            (receipt.accepted_at + RECEIPT_TTL > Instant::now()
                && receipt.client_session_id.as_deref() == Some(client_session_id))
            .then(|| RingingCommandStatus {
                command_id: command_id.to_string(),
                state: receipt.state,
                payload_fingerprint: receipt.fingerprint.clone(),
                terminal_event_id: receipt.terminal_event_id.clone(),
                error_code: receipt.error_code.clone(),
            })
        })
    }
}

const RECEIPT_TTL: Duration = Duration::from_secs(300);

fn unix_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis() as u64
}

impl RingingLeaseStore {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn open(&mut self, client_session_id: String, client_instance_id: String) {
        self.leases.insert(
            client_instance_id,
            LeaseEntry {
                client_session_id,
                expiry: Instant::now() + Duration::from_millis(RENEW_TTL_MS),
            },
        );
    }

    pub fn attach_seed(&mut self, client_session_id: &str, seed: &str) -> bool {
        if !self.is_active_session(client_session_id) || seed.is_empty() {
            return false;
        }
        self.seed_leases
            .entry(client_session_id.to_string())
            .or_default()
            .insert(seed.to_string());
        true
    }

    pub fn detach_seed(&mut self, client_session_id: &str, seed: &str) {
        if let Some(seeds) = self.seed_leases.get_mut(client_session_id) {
            seeds.remove(seed);
            if seeds.is_empty() {
                self.seed_leases.remove(client_session_id);
            }
        }
    }

    pub fn owns_seed(&mut self, client_session_id: &str, seed: &str) -> bool {
        self.expire();
        self.seed_leases
            .get(client_session_id)
            .is_some_and(|seeds| seeds.contains(seed))
    }

    /// 续租（按 client_session_id 反查）；过期/未知会话返回 false。
    pub fn renew(&mut self, client_session_id: &str) -> bool {
        let Some(entry) = self
            .leases
            .values_mut()
            .find(|e| e.client_session_id == client_session_id)
        else {
            return false;
        };
        if entry.expiry < Instant::now() {
            let victim = self
                .leases
                .iter()
                .find(|(_, e)| e.client_session_id == client_session_id)
                .map(|(k, _)| k.clone());
            if let Some(k) = victim {
                if let Some(entry) = self.leases.remove(&k) {
                    self.seed_leases.remove(&entry.client_session_id);
                }
            }
            return false;
        }
        entry.expiry = Instant::now() + Duration::from_millis(RENEW_TTL_MS);
        true
    }

    fn expire(&mut self) {
        let expired: HashSet<String> = self
            .leases
            .iter()
            .filter(|(_, entry)| entry.expiry < Instant::now())
            .map(|(instance, _)| instance.clone())
            .collect();
        for instance in expired {
            if let Some(entry) = self.leases.remove(&instance) {
                self.seed_leases.remove(&entry.client_session_id);
            }
        }
    }

    /// 活跃校验（按 client_instance_id；命令/切流端点使用）��
    #[cfg(test)]
    pub fn is_active(&self, client_instance_id: &str) -> bool {
        self.leases
            .get(client_instance_id)
            .is_some_and(|e| e.expiry >= Instant::now())
    }

    pub fn is_active_session(&self, client_session_id: &str) -> bool {
        self.leases.values().any(|entry| {
            entry.client_session_id == client_session_id && entry.expiry >= Instant::now()
        })
    }

    pub fn instance_for_session(&self, client_session_id: &str) -> Option<String> {
        self.leases.iter().find_map(|(instance, entry)| {
            (entry.client_session_id == client_session_id && entry.expiry >= Instant::now())
                .then(|| instance.clone())
        })
    }
}

/// 已解析的 HTTP 请求。
struct HttpRequest {
    method: String,
    path: String,
    headers: HashMap<String, String>,
    body: Vec<u8>,
}

impl HttpRequest {
    fn header(&self, name: &str) -> Option<&str> {
        self.headers
            .get(&name.to_ascii_lowercase())
            .map(|v| v.as_str())
    }
}

/// 读取并解析一个 HTTP 请求（请求行 + headers + Content-Length body）。
async fn read_request(stream: &mut TcpStream) -> Result<HttpRequest, String> {
    let mut buf = Vec::with_capacity(4096);
    let mut tmp = [0_u8; 2048];
    // 先找 header 结束���（\r\n\r\n）
    let header_end = loop {
        let text = String::from_utf8_lossy(&buf);
        if let Some(pos) = text.find("\r\n\r\n") {
            break pos + 4;
        }
        let n = stream.read(&mut tmp).await.map_err(stringify)?;
        if n == 0 {
            return Err("connection closed before headers".into());
        }
        buf.extend_from_slice(&tmp[..n]);
        if buf.len() > 64 * 1024 {
            return Err("request headers too large".into());
        }
    };

    let header_text = String::from_utf8_lossy(&buf[..header_end]).to_string();
    let mut lines = header_text.lines();
    let request_line = lines
        .next()
        .ok_or_else(|| "missing request line".to_string())?;
    let mut parts = request_line.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| "missing method".to_string())?
        .to_string();
    let path = parts
        .next()
        .ok_or_else(|| "missing path".to_string())?
        .to_string();
    let mut headers = HashMap::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    let content_length: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    if content_length > MAX_BODY_BYTES {
        return Err("body too large".into());
    }
    while buf.len() < header_end + content_length {
        let n = stream.read(&mut tmp).await.map_err(stringify)?;
        if n == 0 {
            return Err("connection closed during body".into());
        }
        buf.extend_from_slice(&tmp[..n]);
    }
    let body = buf[header_end..header_end + content_length].to_vec();
    Ok(HttpRequest {
        method,
        path,
        headers,
        body,
    })
}

async fn write_response(
    stream: &mut TcpStream,
    status: &str,
    content_type: &str,
    body: &[u8],
) -> Result<(), String> {
    let head = format!(
        "HTTP/1.1 {status}\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
        body.len()
    );
    stream.write_all(head.as_bytes()).await.map_err(stringify)?;
    stream.write_all(body).await.map_err(stringify)?;
    stream.flush().await.map_err(stringify)?;
    Ok(())
}

fn parse_channel(s: &str) -> Option<RingingChannel> {
    match s {
        "control" => Some(RingingChannel::Control),
        "conversation" => Some(RingingChannel::Conversation),
        "tool" => Some(RingingChannel::Tool),
        _ => None,
    }
}

/// SSE 事件帧：`id: <epoch>:<channel>:<stream_seq>` + `event:` + `data:`。
fn sse_frame(envelope: &deepx_ringing::RingingEventEnvelope) -> String {
    let event_type = serde_json::to_value(&envelope.event)
        .ok()
        .and_then(|v| v["type"].as_str().map(|s| s.to_string()))
        .unwrap_or_else(|| "message".into());
    // data 必须是**完整信封**（含 seed/stream_seq/event_id/event）：
    // 客户端按 RingingEventEnvelope 解析，缺 seed 将无法按会话路由，
    // 缺 event_id 将破坏 renderer 幂等。
    let data = serde_json::to_string(envelope).unwrap_or_else(|_| "{}".into());
    format!(
        "id: {}:{}:{}\nevent: {}\ndata: {}\n\n",
        envelope.server_epoch,
        envelope.channel.as_str(),
        envelope.stream_seq,
        event_type,
        data
    )
}

/// Ringing HTTP 入口（由 server.rs peek 分流调用）。
pub async fn handle_ringing_http(
    mut stream: TcpStream,
    _preview: &str,
    token: &str,
    hub: Arc<RingingHub>,
    leases: Arc<Mutex<RingingLeaseStore>>,
    service: DeepxService,
    pending: Arc<Mutex<PendingCommandStore>>,
) -> Result<(), String> {
    // server 仅用 TcpStream::peek 做协议分流；peek 不消费 socket 数据，且其
    // 2048-byte preview 可能只包含部分 body。这里必须始终从 stream 读取完整
    // Content-Length，不能把 preview 当作完整 HTTP 请求解析。
    let request = read_request(&mut stream).await?;

    // 鉴权：所有 Ringing 端点要求 Bearer token（SSE 不允许 query string 传 token）
    let authorized = request
        .header("authorization")
        .is_some_and(|v| v == format!("Bearer {token}"));
    if !authorized {
        return write_response(
            &mut stream,
            "401 Unauthorized",
            "text/plain",
            b"unauthorized",
        )
        .await;
    }

    let path = request.path.clone();
    let method = request.method.clone();

    if method == "POST" && path == format!("{RINGING_BASE_PATH}/clients/open") {
        return handle_open(&mut stream, &request.body, &leases, &hub).await;
    }
    if method == "POST" && path == format!("{RINGING_BASE_PATH}/leases/renew") {
        return handle_renew(
            &mut stream,
            request.header(CLIENT_SESSION_HEADER),
            &request.body,
            &leases,
        )
        .await;
    }
    let session_id = request
        .header(CLIENT_SESSION_HEADER)
        .filter(|id| !id.is_empty());
    let session_active = session_id.is_some_and(|id| {
        leases
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .is_active_session(id)
    });
    if !session_active {
        return write_response(
            &mut stream,
            "401 Unauthorized",
            "application/json",
            br#"{"code":"lease_required","message":"open a Ringing v2 client session first"}"#,
        )
        .await;
    }
    if method == "GET" && path.starts_with(&format!("{TIMELINE_V3_BASE_PATH}/sessions/")) {
        let rest = path.trim_start_matches(&format!("{TIMELINE_V3_BASE_PATH}/sessions/"));
        if let Some(seed) = rest.strip_suffix("/timeline") {
            return handle_timeline_snapshot(&mut stream, seed, session_id, &leases, &hub).await;
        }
        if let Some(seed) = rest.strip_suffix("/timeline/events") {
            let Some(session_id) = session_id else {
                return write_response(
                    &mut stream,
                    "401 Unauthorized",
                    "application/json",
                    br#"{"code":"lease_required","message":"client session header required"}"#,
                )
                .await;
            };
            return handle_timeline_sse(&mut stream, seed, &request, session_id, leases, hub).await;
        }
    }
    if method == "POST" && path.starts_with(&format!("{RINGING_BASE_PATH}/commands/")) {
        let channel = path.trim_start_matches(&format!("{RINGING_BASE_PATH}/commands/"));
        return handle_command(
            &mut stream,
            channel,
            &request.body,
            request.header(CLIENT_SESSION_HEADER),
            &leases,
            &service,
            &hub,
            &pending,
        )
        .await;
    }
    if method == "GET" && path.starts_with(&format!("{RINGING_BASE_PATH}/commands/")) {
        let command_id = path.trim_start_matches(&format!("{RINGING_BASE_PATH}/commands/"));
        return handle_command_status(&mut stream, command_id, session_id, &pending).await;
    }
    if method == "GET" && path.starts_with(&format!("{RINGING_BASE_PATH}/sessions/")) {
        let rest = path.trim_start_matches(&format!("{RINGING_BASE_PATH}/sessions/"));
        if let Some(seed) = rest.strip_suffix("/bootstrap") {
            return handle_bootstrap(&mut stream, seed, session_id, &leases, &hub).await;
        }
    }
    if method == "GET" && path.starts_with(&format!("{RINGING_BASE_PATH}/events/")) {
        let channel = path.trim_start_matches(&format!("{RINGING_BASE_PATH}/events/"));
        let Some(session_id) = session_id else {
            return write_response(
                &mut stream,
                "401 Unauthorized",
                "application/json",
                br#"{"code":"lease_required","message":"client session header required"}"#,
            )
            .await;
        };
        return handle_sse(&mut stream, channel, &request, session_id, leases, hub).await;
    }
    if method == "GET" && path.starts_with(&format!("{RINGING_BASE_PATH}/content/")) {
        return handle_content(&mut stream, &path, session_id, &leases, &hub).await;
    }
    if method == "POST" && path == format!("{RINGING_BASE_PATH}/content") {
        return handle_content_upload(
            &mut stream,
            request.header("content-type"),
            &request.body,
            session_id,
            &leases,
            &hub,
        )
        .await;
    }
    if method == "POST" && path.starts_with(&format!("{RINGING_BASE_PATH}/queries/")) {
        let name = path.trim_start_matches(&format!("{RINGING_BASE_PATH}/queries/"));
        return handle_query_post(
            &mut stream,
            name,
            &request.body,
            session_id,
            &leases,
            &service,
        )
        .await;
    }
    if method == "POST" && path.starts_with(&format!("{RINGING_BASE_PATH}/actions/")) {
        let name = path.trim_start_matches(&format!("{RINGING_BASE_PATH}/actions/"));
        return handle_action(
            &mut stream,
            name,
            &request.body,
            request.header(CLIENT_SESSION_HEADER),
            &leases,
            &service,
            &pending,
        )
        .await;
    }
    write_response(
        &mut stream,
        "404 Not Found",
        "text/plain",
        b"unknown ringing endpoint",
    )
    .await
}

/// GET /ringing/v2/content/{content_id}
///
/// 大内容外置读取（PLAN）：鉴权由统一 Bearer token 完成；seed 查询参数
/// 用于会话所有权校验（ContentStore 拒绝跨会话读取）。返回 200 + media_type
/// 或 404（不存在/过期/非本会话）。
async fn handle_content(
    stream: &mut TcpStream,
    path: &str,
    session_id: Option<&str>,
    leases: &Arc<Mutex<RingingLeaseStore>>,
    hub: &Arc<RingingHub>,
) -> Result<(), String> {
    let rest = path.trim_start_matches(&format!("{RINGING_BASE_PATH}/content/"));
    let (content_id, seed) = match rest.split_once('?') {
        Some((id, query)) => (id.to_string(), parse_query_param(query, "seed")),
        None => (rest.to_string(), None),
    };
    if content_id.is_empty() {
        return write_response(
            stream,
            "400 Bad Request",
            "text/plain",
            b"missing content_id",
        )
        .await;
    }
    let Some(seed) = seed else {
        return write_response(
            stream,
            "400 Bad Request",
            "text/plain",
            b"missing seed query param",
        )
        .await;
    };
    let owns_seed = session_id.is_some_and(|id| {
        leases
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .owns_seed(id, &seed)
    });
    if !owns_seed {
        return write_response(
            stream,
            "403 Forbidden",
            "application/json",
            br#"{"code":"content_forbidden","message":"content is not owned by this session"}"#,
        )
        .await;
    }
    match hub.get_content(&seed, &content_id) {
        Some(entry) => write_response(stream, "200 OK", &entry.media_type, &entry.bytes).await,
        None => {
            write_response(
                stream,
                "404 Not Found",
                "text/plain",
                b"content not found or expired",
            )
            .await
        }
    }
}

/// POST /ringing/v2/content
///
/// Electron main 上传本地附件；renderer 只传本地路径，绝不把路径或 token
/// 放入 Ringing 命令。使用受限 multipart 解析，内容上限由 read_request 执行。
async fn handle_content_upload(
    stream: &mut TcpStream,
    content_type: Option<&str>,
    body: &[u8],
    session_id: Option<&str>,
    leases: &Arc<Mutex<RingingLeaseStore>>,
    hub: &Arc<RingingHub>,
) -> Result<(), String> {
    let Some(content_type) = content_type else {
        return write_response(
            stream,
            "400 Bad Request",
            "text/plain",
            b"missing content type",
        )
        .await;
    };
    let Some(boundary) = content_type
        .split(';')
        .find_map(|part| part.trim().strip_prefix("boundary="))
        .map(|value| value.trim_matches('"').as_bytes().to_vec())
    else {
        return write_response(
            stream,
            "400 Bad Request",
            "text/plain",
            b"multipart boundary required",
        )
        .await;
    };
    let delimiter = [b"--".as_slice(), boundary.as_slice()].concat();
    let mut seed = None;
    let mut media_type = None;
    let mut content = None;
    // Split on the exact boundary without interpreting arbitrary binary bytes.
    let mut parts = Vec::new();
    let mut offset = 0;
    while let Some(relative) = body[offset..]
        .windows(delimiter.len())
        .position(|window| window == delimiter.as_slice())
    {
        parts.push(&body[offset..offset + relative]);
        offset += relative + delimiter.len();
    }
    parts.push(&body[offset..]);
    for part in parts {
        let part = part.strip_prefix(b"\r\n").unwrap_or(part);
        let part = part.strip_suffix(b"\r\n").unwrap_or(part);
        let Some(header_end) = part.windows(4).position(|window| window == b"\r\n\r\n") else {
            continue;
        };
        let headers = String::from_utf8_lossy(&part[..header_end]);
        let value = &part[header_end + 4..];
        let Some(name) = headers
            .split(';')
            .find_map(|piece| piece.trim().strip_prefix("name=\""))
            .and_then(|value| value.strip_suffix('"'))
        else {
            continue;
        };
        match name {
            "seed" => seed = String::from_utf8(value.to_vec()).ok(),
            "media_type" => media_type = String::from_utf8(value.to_vec()).ok(),
            "content" => content = Some(value.to_vec()),
            _ => {}
        }
    }
    let Some(seed) = seed.filter(|seed| !seed.is_empty()) else {
        return write_response(stream, "400 Bad Request", "text/plain", b"missing seed").await;
    };
    let Some(content) = content else {
        return write_response(stream, "400 Bad Request", "text/plain", b"missing content").await;
    };
    let owns_seed = session_id.is_some_and(|id| {
        leases
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .owns_seed(id, &seed)
    });
    if !owns_seed {
        return write_response(
            stream,
            "403 Forbidden",
            "application/json",
            br#"{"code":"content_forbidden","message":"content is not owned by this session"}"#,
        )
        .await;
    }
    let media_type = media_type
        .filter(|value| !value.is_empty())
        .unwrap_or_else(|| "application/octet-stream".into());
    let content_id = hub.put_content(&seed, &media_type, content, false);
    let response = serde_json::json!({
        "content_id": content_id.clone(),
        "media_type": media_type,
        "sha256": content_id,
        "truncated": false,
    });
    write_response(
        stream,
        "200 OK",
        "application/json",
        &serde_json::to_vec(&response).map_err(stringify)?,
    )
    .await
}

/// 从 query string 中取参数（`a=1&seed=xxx` 形式，无 URL 解码——content_id/seed
/// 均为十六进制/会话标识，不含保留字符）。
fn parse_query_param(query: &str, key: &str) -> Option<String> {
    query.split('&').find_map(|pair| {
        let (k, v) = pair.split_once('=')?;
        (k == key).then(|| v.to_string())
    })
}

/// 从 peek preview 解析请求（preview 已含完整 header；body 长度按 header 读取，
/// 不足部分由调用方保证已 peek 或本函数返回错误）。
#[cfg(test)]
fn parse_preview_request(preview: &str) -> Result<HttpRequest, String> {
    let header_end = preview
        .find("\r\n\r\n")
        .ok_or_else(|| "incomplete headers".to_string())?;
    let header_text = &preview[..header_end];
    let mut lines = header_text.lines();
    let request_line = lines
        .next()
        .ok_or_else(|| "missing request line".to_string())?;
    let mut parts = request_line.split_whitespace();
    let method = parts
        .next()
        .ok_or_else(|| "missing method".to_string())?
        .to_string();
    let path = parts
        .next()
        .ok_or_else(|| "missing path".to_string())?
        .to_string();
    let mut headers = HashMap::new();
    for line in lines {
        if let Some((k, v)) = line.split_once(':') {
            headers.insert(k.trim().to_ascii_lowercase(), v.trim().to_string());
        }
    }
    let content_length: usize = headers
        .get("content-length")
        .and_then(|v| v.parse().ok())
        .unwrap_or(0);
    // find 返回 \r 索引，+4 跳过 \r\n\r\n 到达 body 开头（与 read_request 一致）
    let body_start = header_end + 4;
    let body: Vec<u8> = preview[body_start..]
        .as_bytes()
        .iter()
        .copied()
        .take(content_length)
        .collect();
    if body.len() < content_length {
        return Err("body not fully peeked; use read_request".into());
    }
    Ok(HttpRequest {
        method,
        path,
        headers,
        body,
    })
}

async fn handle_open(
    stream: &mut TcpStream,
    body: &[u8],
    leases: &Arc<Mutex<RingingLeaseStore>>,
    hub: &Arc<RingingHub>,
) -> Result<(), String> {
    let req: ClientOpenRequest =
        serde_json::from_slice(body).map_err(|e| format!("invalid open request: {e}"))?;
    if req.schema != RINGING_SCHEMA || req.version != RINGING_VERSION {
        return write_response(
            stream,
            "426 Upgrade Required",
            "application/json",
            &serde_json::to_vec(&RingingCommandAck {
                command_id: String::new(),
                status: RingingCommandAckStatus::Rejected,
                code: Some("unsupported_version".into()),
                message: Some("unsupported Ringing schema/version".into()),
                retry_after_ms: None,
            })
            .unwrap_or_default(),
        )
        .await;
    }
    let client_session_id = random_hex();
    // v2 Desktop 只有在四项能力全部满足时才允许建立 Ringing backend。
    let supported: &[&str] = &[
        "Ringing_v2",
        "Ringing_batch_v2",
        "Ringing_bootstrap_v2",
        "Ringing_command_status_v2",
    ];
    let capabilities: Vec<String> = supported
        .iter()
        .filter(|capability| {
            req.capabilities
                .iter()
                .any(|candidate| candidate == *capability)
        })
        .map(|capability| (*capability).to_string())
        .collect();
    if capabilities.len() != supported.len() {
        return write_response(
            stream,
            "426 Upgrade Required",
            "application/json",
            br#"{"code":"missing_capability","message":"Ringing v2 capabilities are incomplete"}"#,
        )
        .await;
    }
    // lease 双键关联：client_instance_id（命令校验）+ client_session_id（renew）。
    leases
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .open(client_session_id.clone(), req.client_instance_id.clone());
    let resp = ClientOpenResponse {
        schema: RINGING_SCHEMA.into(),
        version: RINGING_VERSION,
        accepted: true,
        client_session_id: client_session_id.clone(),
        capabilities,
        server_epoch: hub.epoch().to_string(),
        lease_ttl_ms: RENEW_TTL_MS,
        renew_interval_ms: RENEW_INTERVAL_MS,
    };
    write_response(
        stream,
        "200 OK",
        "application/json",
        &serde_json::to_vec(&resp).map_err(stringify)?,
    )
    .await
}

async fn handle_renew(
    stream: &mut TcpStream,
    header_session_id: Option<&str>,
    body: &[u8],
    leases: &Arc<Mutex<RingingLeaseStore>>,
) -> Result<(), String> {
    let session_id = header_session_id
        .filter(|value| !value.is_empty())
        .ok_or_else(|| "missing client session header".to_string())?;
    let _ = body;
    let ok = leases
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .renew(session_id);
    if !ok {
        return write_response(
            stream,
            "401 Unauthorized",
            "text/plain",
            b"lease expired or unknown",
        )
        .await;
    }
    let resp = serde_json::json!({
        "ok": true,
        "lease_ttl_ms": RENEW_TTL_MS,
        "renew_interval_ms": RENEW_INTERVAL_MS,
    });
    write_response(
        stream,
        "200 OK",
        "application/json",
        &serde_json::to_vec(&resp).map_err(stringify)?,
    )
    .await
}

fn hydrate_attachment_previews(
    hub: &RingingHub,
    seed: &str,
    command: &mut deepx_ringing::RingingCommand,
) -> Result<(), String> {
    let deepx_ringing::RingingCommand::Conversation(
        deepx_domain::ConversationCommand::ConversationSendMessage {
            text, attachments, ..
        },
    ) = command
    else {
        return Ok(());
    };
    let Some(references) = attachments.take() else {
        return Ok(());
    };
    if references.is_empty() {
        return Ok(());
    }
    let mut parts = vec!["[Files]".to_string()];
    for reference in references {
        let entry = hub
            .get_content(seed, &reference.content_id)
            .ok_or_else(|| "attachment_not_found".to_string())?;
        if entry.sha256 != reference.sha256 || entry.media_type != reference.media_type {
            return Err("attachment_mismatch".into());
        }
        let preview = String::from_utf8_lossy(&entry.bytes)
            .lines()
            .take(10)
            .collect::<Vec<_>>()
            .join("\n")
            .chars()
            .take(1000)
            .collect::<String>();
        parts.push(format!(
            "\n{} ({}):\n{}",
            reference.content_id, reference.media_type, preview
        ));
    }
    parts.push(format!("\n\n[Message]\n{text}"));
    *text = parts.join("");
    Ok(())
}

async fn handle_command(
    stream: &mut TcpStream,
    channel: &str,
    body: &[u8],
    client_session_id: Option<&str>,
    leases: &Arc<Mutex<RingingLeaseStore>>,
    service: &DeepxService,
    hub: &Arc<RingingHub>,
    pending: &Arc<Mutex<PendingCommandStore>>,
) -> Result<(), String> {
    let Some(expected) = parse_channel(channel) else {
        return write_response(stream, "404 Not Found", "text/plain", b"unknown channel").await;
    };
    let env: RingingCommandEnvelope = match serde_json::from_slice(body) {
        Ok(env) => env,
        Err(e) => {
            let ack = RingingCommandAck {
                command_id: String::new(),
                status: RingingCommandAckStatus::Rejected,
                code: Some("invalid_body".into()),
                message: Some(format!("{e}")),
                retry_after_ms: None,
            };
            return write_response(
                stream,
                "400 Bad Request",
                "application/json",
                &serde_json::to_vec(&ack).map_err(stringify)?,
            )
            .await;
        }
    };
    if let Err(code) = env.validate() {
        let status = if code == "lease_required" {
            "401 Unauthorized"
        } else {
            "400 Bad Request"
        };
        let ack = RingingCommandAck {
            command_id: env.command_id.clone(),
            status: RingingCommandAckStatus::Rejected,
            code: Some(code.into()),
            message: Some("invalid Ringing v2 command envelope".into()),
            retry_after_ms: None,
        };
        return write_response(
            stream,
            status,
            "application/json",
            &serde_json::to_vec(&ack).map_err(stringify)?,
        )
        .await;
    }
    if env.channel != expected {
        let ack = RingingCommandAck {
            command_id: env.command_id.clone(),
            status: RingingCommandAckStatus::Rejected,
            code: Some("channel_mismatch".into()),
            message: Some(format!(
                "path channel {channel} != envelope channel {:?}",
                env.channel
            )),
            retry_after_ms: None,
        };
        return write_response(
            stream,
            "400 Bad Request",
            "application/json",
            &serde_json::to_vec(&ack).map_err(stringify)?,
        )
        .await;
    }
    let lease_ok = client_session_id.is_some_and(|session_id| {
        let leases = leases.lock().unwrap_or_else(|e| e.into_inner());
        leases.is_active_session(session_id)
            && leases.instance_for_session(session_id).as_deref()
                == Some(env.client_instance_id.as_str())
            && env.client_session_id == session_id
    });
    if !lease_ok {
        let ack = RingingCommandAck {
            command_id: env.command_id.clone(),
            status: RingingCommandAckStatus::Rejected,
            code: Some("lease_required".into()),
            message: Some("open a client session before sending commands".into()),
            retry_after_ms: Some(RENEW_INTERVAL_MS),
        };
        return write_response(
            stream,
            "401 Unauthorized",
            "application/json",
            &serde_json::to_vec(&ack).map_err(stringify)?,
        )
        .await;
    }
    let registry_command = matches!(
        &env.command,
        deepx_ringing::RingingCommand::Control(
            ControlCommand::SessionCreate { .. } | ControlCommand::SessionResume { .. }
        )
    );
    if !registry_command {
        let seed = env.seed.as_deref().unwrap_or_default();
        let owns_seed = client_session_id.is_some_and(|session_id| {
            leases
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .owns_seed(session_id, seed)
        });
        if !owns_seed {
            let ack = RingingCommandAck {
                command_id: env.command_id.clone(),
                status: RingingCommandAckStatus::Rejected,
                code: Some("lease_required".into()),
                message: Some("attach the session seed before sending this command".into()),
                retry_after_ms: Some(RENEW_INTERVAL_MS),
            };
            return write_response(
                stream,
                "401 Unauthorized",
                "application/json",
                &serde_json::to_vec(&ack).map_err(stringify)?,
            )
            .await;
        }
    }
    if matches!(
        &env.command,
        deepx_ringing::RingingCommand::Conversation(
            deepx_domain::ConversationCommand::ConversationLoadMore { .. }
        )
    ) {
        let ack = RingingCommandAck {
            command_id: env.command_id,
            status: RingingCommandAckStatus::Rejected,
            code: Some("unsupported_command".into()),
            message: Some(
                "Ringing v2 bootstrap already returns the complete persisted conversation history"
                    .into(),
            ),
            retry_after_ms: None,
        };
        return write_response(
            stream,
            "422 Unprocessable Entity",
            "application/json",
            &serde_json::to_vec(&ack).map_err(stringify)?,
        )
        .await;
    }
    // 幂等：accepted 后断线重试不得重复执行（PLAN 命令幂等硬规则）。
    // 锁内判断 + 预留记录，转发失败时回滚；锁不跨 await。
    let fingerprint_payload = serde_json::to_string(&serde_json::json!({
        "channel": env.channel,
        "seed": &env.seed,
        "expected_revision": env.expected_revision,
        "command": &env.command,
    }))
    .map_err(stringify)?;
    let fingerprint =
        deepx_runtime::ringing::content_store::sha256_hex(fingerprint_payload.as_bytes());
    let receipt_session_id = client_session_id.expect("validated Ringing session");
    let duplicate_check = {
        let mut pending = pending.lock().unwrap_or_else(|e| e.into_inner());
        match pending.record_fingerprint_for_session(
            &env.command_id,
            &fingerprint,
            receipt_session_id,
        ) {
            Ok(value) => Ok(!value),
            Err(()) => Err(()),
        }
    };
    if duplicate_check.is_err() {
        let ack = RingingCommandAck {
            command_id: env.command_id.clone(),
            status: RingingCommandAckStatus::Rejected,
            code: Some("duplicate_command_mismatch".into()),
            message: Some("command_id was already used with another payload".into()),
            retry_after_ms: None,
        };
        return write_response(
            stream,
            "409 Conflict",
            "application/json",
            &serde_json::to_vec(&ack).map_err(stringify)?,
        )
        .await;
    }
    let duplicate = duplicate_check.expect("duplicate check result");
    if duplicate {
        let ack = RingingCommandAck {
            command_id: env.command_id.clone(),
            status: RingingCommandAckStatus::Accepted,
            code: None,
            message: Some("duplicate command_id (already accepted)".into()),
            retry_after_ms: None,
        };
        return write_response(
            stream,
            "200 OK",
            "application/json",
            &serde_json::to_vec(&ack).map_err(stringify)?,
        )
        .await;
    }

    // SessionClose（契约 §2）：daemon 侧拦截，不转发 worker——
    // registry close + hub 发布 SessionStateChanged{Closed}（causation=command_id）；
    // 会话不存在同样 Accepted（幂等关闭）。无 seed → 400 并回滚幂等记录。
    if let deepx_ringing::RingingCommand::Control(ControlCommand::SessionClose {
        seed: close_seed,
    }) = &env.command
    {
        let close_seed = session_close_seed(close_seed, &env.seed);
        if close_seed.is_empty() {
            pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .rollback(&env.command_id);
            let ack = RingingCommandAck {
                command_id: env.command_id,
                status: RingingCommandAckStatus::Rejected,
                code: Some("missing_seed".into()),
                message: Some("SessionClose requires seed".into()),
                retry_after_ms: None,
            };
            return write_response(
                stream,
                "400 Bad Request",
                "application/json",
                &serde_json::to_vec(&ack).map_err(stringify)?,
            )
            .await;
        }
        if let Err(error) = service.close_session(&close_seed, Some(&env.command_id)) {
            pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .rollback(&env.command_id);
            let ack = RingingCommandAck {
                command_id: env.command_id,
                status: RingingCommandAckStatus::Rejected,
                code: Some("dispatch_failed".into()),
                message: Some(format!("{error}")),
                retry_after_ms: None,
            };
            return write_response(
                stream,
                "502 Bad Gateway",
                "application/json",
                &serde_json::to_vec(&ack).map_err(stringify)?,
            )
            .await;
        }
        if let Some(session_id) = client_session_id {
            leases
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .detach_seed(session_id, &close_seed);
        }
        pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .mark_terminal(&env.command_id, RingingCommandState::Succeeded, None, None);
        let ack = RingingCommandAck {
            command_id: env.command_id,
            status: RingingCommandAckStatus::Accepted,
            code: None,
            message: None,
            retry_after_ms: None,
        };
        return write_response(
            stream,
            "200 OK",
            "application/json",
            &serde_json::to_vec(&ack).map_err(stringify)?,
        )
        .await;
    }

    // SessionCreate/Resume are registry operations. They select the worker
    // instance for the whole connection and are not ordinary worker commands.
    match &env.command {
        deepx_ringing::RingingCommand::Control(ControlCommand::SessionCreate { .. }) => {
            let created_seed = match service.handle("session.new", &serde_json::json!({})) {
                Ok(value) => value,
                Err(error) => {
                    pending
                        .lock()
                        .unwrap_or_else(|e| e.into_inner())
                        .rollback(&env.command_id);
                    let ack = RingingCommandAck {
                        command_id: env.command_id,
                        status: RingingCommandAckStatus::Rejected,
                        code: Some("dispatch_failed".into()),
                        message: Some(format!("{error}")),
                        retry_after_ms: None,
                    };
                    return write_response(
                        stream,
                        "502 Bad Gateway",
                        "application/json",
                        &serde_json::to_vec(&ack).map_err(stringify)?,
                    )
                    .await;
                }
            };
            if let (Some(session_id), Some(seed)) = (client_session_id, created_seed.as_str()) {
                leases
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .attach_seed(session_id, seed);
                // SessionCreate is a registry operation, so it does not enter the
                // worker's command-causation scope. Publish the authoritative
                // creation event after the lease is attached; otherwise the SSE
                // filter could discard it before the newly created seed is owned
                // by this client session.
                publish_session_created(hub, seed, &env.command_id);
            }
            pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .mark_terminal(&env.command_id, RingingCommandState::Succeeded, None, None);
            let ack = RingingCommandAck {
                command_id: env.command_id,
                status: RingingCommandAckStatus::Accepted,
                code: None,
                message: None,
                retry_after_ms: None,
            };
            return write_response(
                stream,
                "200 OK",
                "application/json",
                &serde_json::to_vec(&ack).map_err(stringify)?,
            )
            .await;
        }
        deepx_ringing::RingingCommand::Control(ControlCommand::SessionResume { seed }) => {
            if let Err(error) = service.handle("session.resume", &serde_json::json!({"seed": seed}))
            {
                pending
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .rollback(&env.command_id);
                let ack = RingingCommandAck {
                    command_id: env.command_id,
                    status: RingingCommandAckStatus::Rejected,
                    code: Some("dispatch_failed".into()),
                    message: Some(format!("{error}")),
                    retry_after_ms: None,
                };
                return write_response(
                    stream,
                    "502 Bad Gateway",
                    "application/json",
                    &serde_json::to_vec(&ack).map_err(stringify)?,
                )
                .await;
            }
            if let Some(session_id) = client_session_id {
                leases
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .attach_seed(session_id, seed);
            }
            pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .mark_terminal(&env.command_id, RingingCommandState::Succeeded, None, None);
            let ack = RingingCommandAck {
                command_id: env.command_id,
                status: RingingCommandAckStatus::Accepted,
                code: None,
                message: None,
                retry_after_ms: None,
            };
            return write_response(
                stream,
                "200 OK",
                "application/json",
                &serde_json::to_vec(&ack).map_err(stringify)?,
            )
            .await;
        }
        _ => {}
    }

    let seed = env.seed.clone().unwrap_or_default();
    let mut worker_command = env.command.clone();
    if let Err(code) = hydrate_attachment_previews(hub, &seed, &mut worker_command) {
        pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .rollback(&env.command_id);
        let ack = RingingCommandAck {
            command_id: env.command_id,
            status: RingingCommandAckStatus::Rejected,
            code: Some(code.clone()),
            message: Some("attachment is unavailable or invalid".into()),
            retry_after_ms: None,
        };
        return write_response(
            stream,
            "400 Bad Request",
            "application/json",
            &serde_json::to_vec(&ack).map_err(stringify)?,
        )
        .await;
    }
    let worker_env = deepx_ringing::RingingWorkerCommandEnvelope::new(
        seed.as_str(),
        env.command_id.clone(),
        worker_command,
    )
    .with_expected_revision(env.expected_revision);
    if let Err(e) = service.send_ringing_command(&seed, &worker_env) {
        pending
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .rollback(&env.command_id);
        let ack = RingingCommandAck {
            command_id: env.command_id.clone(),
            status: RingingCommandAckStatus::Rejected,
            code: Some("dispatch_failed".into()),
            message: Some(format!("{e}")),
            retry_after_ms: None,
        };
        return write_response(
            stream,
            "502 Bad Gateway",
            "application/json",
            &serde_json::to_vec(&ack).map_err(stringify)?,
        )
        .await;
    }
    pending
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .mark_running(&env.command_id);
    let ack = RingingCommandAck {
        command_id: env.command_id,
        status: RingingCommandAckStatus::Accepted,
        code: None,
        message: None,
        retry_after_ms: None,
    };
    write_response(
        stream,
        "200 OK",
        "application/json",
        &serde_json::to_vec(&ack).map_err(stringify)?,
    )
    .await
}

async fn handle_command_status(
    stream: &mut TcpStream,
    command_id: &str,
    client_session_id: Option<&str>,
    pending: &Arc<Mutex<PendingCommandStore>>,
) -> Result<(), String> {
    let Some(client_session_id) = client_session_id else {
        return write_response(
            stream,
            "401 Unauthorized",
            "application/json",
            br#"{"code":"lease_required","message":"client session header required"}"#,
        )
        .await;
    };
    let Some(status) = pending
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .status_for_session(command_id, client_session_id)
    else {
        return write_response(
            stream,
            "404 Not Found",
            "application/json",
            br#"{"code":"command_not_found","message":"command receipt not found"}"#,
        )
        .await;
    };
    write_response(
        stream,
        "200 OK",
        "application/json",
        &serde_json::to_vec(&status).map_err(stringify)?,
    )
    .await
}

async fn handle_bootstrap(
    stream: &mut TcpStream,
    seed: &str,
    session_id: Option<&str>,
    leases: &Arc<Mutex<RingingLeaseStore>>,
    hub: &Arc<RingingHub>,
) -> Result<(), String> {
    if seed.is_empty() {
        return write_response(stream, "400 Bad Request", "text/plain", b"missing seed").await;
    }
    let owns_seed = session_id.is_some_and(|id| {
        leases
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .owns_seed(id, seed)
    });
    if !owns_seed {
        return write_response(
            stream,
            "401 Unauthorized",
            "application/json",
            br#"{"code":"lease_required","message":"attach the session seed before bootstrap"}"#,
        )
        .await;
    }
    let bootstrap = RingingSessionBootstrap::new(
        hub.epoch(),
        seed,
        hub.snapshot(RingingChannel::Control, seed),
        hub.conversation_snapshot(seed),
        hub.snapshot(RingingChannel::Tool, seed),
    );
    write_response(
        stream,
        "200 OK",
        "application/json",
        &serde_json::to_vec(&bootstrap).map_err(stringify)?,
    )
    .await
}

/// v3 transcript recovery state. It is intentionally separate from the v2
/// three-channel bootstrap: a Timeline client receives one materialized model
/// and one watermark only.
async fn handle_timeline_snapshot(
    stream: &mut TcpStream,
    seed: &str,
    session_id: Option<&str>,
    leases: &Arc<Mutex<RingingLeaseStore>>,
    hub: &Arc<RingingHub>,
) -> Result<(), String> {
    if seed.is_empty() {
        return write_response(stream, "400 Bad Request", "text/plain", b"missing seed").await;
    }
    let owns_seed = session_id.is_some_and(|id| {
        leases
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .owns_seed(id, seed)
    });
    if !owns_seed {
        return write_response(
            stream,
            "401 Unauthorized",
            "application/json",
            br#"{"code":"lease_required","message":"attach the session seed before reading timeline"}"#,
        )
        .await;
    }
    let snapshot = hub
        .timeline_snapshot(seed)
        .unwrap_or(deepx_domain::TimelineSnapshot {
            watermark: 0,
            turns: vec![],
        });
    let body = serde_json::json!({
        "schema": "deepx.Timeline",
        "version": 3,
        "server_epoch": hub.epoch(),
        "seed": seed,
        "snapshot": snapshot,
    });
    write_response(
        stream,
        "200 OK",
        "application/json",
        &serde_json::to_vec(&body).map_err(stringify)?,
    )
    .await
}

fn timeline_sse_frame(epoch: &str, seed: &str, entry: &deepx_domain::TimelineEntry) -> String {
    let data = serde_json::json!({
        "schema": "deepx.Timeline",
        "version": 3,
        "server_epoch": epoch,
        "seed": seed,
        "entry": entry,
    });
    format!(
        "id: {epoch}:timeline:{}\nevent: timeline.entry\ndata: {}\n\n",
        entry.timeline_seq,
        serde_json::to_string(&data).unwrap_or_else(|_| "{}".into())
    )
}

/// Per-session v3 Timeline SSE. The cursor is `epoch:timeline:timeline_seq`;
/// it cannot be compared with any v2 channel cursor.
async fn handle_timeline_sse(
    stream: &mut TcpStream,
    seed: &str,
    request: &HttpRequest,
    session_id: &str,
    leases: Arc<Mutex<RingingLeaseStore>>,
    hub: Arc<RingingHub>,
) -> Result<(), String> {
    if seed.is_empty()
        || !leases
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .owns_seed(session_id, seed)
    {
        return write_response(
            stream,
            "401 Unauthorized",
            "application/json",
            br#"{"code":"lease_required"}"#,
        )
        .await;
    }
    let after = request
        .header("last-event-id")
        .map(|cursor| parse_timeline_cursor(cursor, hub.epoch()))
        .unwrap_or(0);
    // Subscribe before replay so every entry has either the replay or live
    // path. Live duplicates at/below `after` are skipped below.
    let mut rx = hub.subscribe_timeline();
    let replay = hub.timeline_replay_since(seed, after);
    let replayed: HashSet<u64> = replay.iter().map(|entry| entry.timeline_seq).collect();
    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\n\r\n";
    stream.write_all(head.as_bytes()).await.map_err(stringify)?;
    for entry in &replay {
        if stream
            .write_all(timeline_sse_frame(hub.epoch(), seed, entry).as_bytes())
            .await
            .is_err()
        {
            return Ok(());
        }
    }
    stream.flush().await.map_err(stringify)?;
    let mut keepalive = tokio::time::interval(Duration::from_millis(SSE_KEEPALIVE_MS));
    keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = keepalive.tick() => {
                if !leases.lock().unwrap_or_else(|e| e.into_inner()).is_active_session(session_id) {
                    return Ok(());
                }
                if stream.write_all(b": keepalive\n\n").await.is_err() { return Ok(()); }
                let _ = stream.flush().await;
            }
            received = rx.recv() => match received {
                Ok(live) => {
                    if live.seed != seed || live.entry.timeline_seq <= after || replayed.contains(&live.entry.timeline_seq) {
                        continue;
                    }
                    if stream.write_all(timeline_sse_frame(hub.epoch(), seed, &live.entry).as_bytes()).await.is_err() {
                        return Ok(());
                    }
                    let _ = stream.flush().await;
                }
                // Never continue after a broadcast gap: the client reconnects
                // from the last parsed cursor and replays the lossless journal.
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => return Ok(()),
                Err(tokio::sync::broadcast::error::RecvError::Closed) => return Ok(()),
            }
        }
    }
}

fn parse_timeline_cursor(cursor: &str, epoch: &str) -> u64 {
    let mut parts = cursor.split(':');
    let received_epoch = parts.next().unwrap_or_default();
    let kind = parts.next().unwrap_or_default();
    let seq = parts.next().and_then(|value| value.parse::<u64>().ok());
    if received_epoch == epoch && kind == "timeline" && parts.next().is_none() {
        seq.unwrap_or(0)
    } else {
        0
    }
}

fn query_method(name: &str) -> Option<&'static str> {
    match name.trim_matches('/') {
        "session/list" | "session.list" => Some("session.list"),
        "session/meta" | "session.meta" => Some("session.meta"),
        "session/activity" | "session.activity" => Some("session.activity"),
        "session/dashboard" | "session.dashboard" => Some("session.dashboard"),
        "session/get_activity" | "session.get_activity" => Some("session.get_activity"),
        "workspace/get" | "workspace.get" => Some("workspace.get"),
        "workspace/status" | "workspace.status" => Some("workspace.status"),
        "config/load" | "config.load" => Some("config.load"),
        "skills/list_tools" | "skills.list_tools" => Some("skills.list_tools"),
        "todo/status" | "todo.status" => Some("todo.status"),
        "plan/read" | "plan.read" => Some("plan.read"),
        "plan/context_stats" | "plan.context_stats" => Some("plan.context_stats"),
        "stats/token_usage" | "stats.token_usage" => Some("stats.token_usage"),
        "git/diff" | "git.diff" => Some("git.diff"),
        "git/branch" | "git.branch" => Some("git.branch"),
        "git/branches" | "git.branches" => Some("git.branches"),
        "git/file_diff" | "git.file_diff" => Some("git.file_diff"),
        "daemon/version" | "daemon.version" => Some("daemon.version"),
        _ => None,
    }
}

async fn handle_query_post(
    stream: &mut TcpStream,
    name: &str,
    body: &[u8],
    session_id: Option<&str>,
    leases: &Arc<Mutex<RingingLeaseStore>>,
    service: &DeepxService,
) -> Result<(), String> {
    let Some(method) = query_method(name) else {
        return write_response(
            stream,
            "404 Not Found",
            "application/json",
            br#"{"code":"unknown_query","message":"unknown typed query"}"#,
        )
        .await;
    };
    let params: serde_json::Value = if body.is_empty() {
        serde_json::json!({})
    } else {
        serde_json::from_slice(body).map_err(|e| format!("invalid query body: {e}"))?
    };
    if query::requires_seed(method)
        && params
            .get("seed")
            .and_then(serde_json::Value::as_str)
            .is_none()
    {
        return write_response(
            stream,
            "400 Bad Request",
            "application/json",
            br#"{"code":"invalid_envelope","message":"seed is required"}"#,
        )
        .await;
    }
    if query::requires_seed(method) {
        let seed = params
            .get("seed")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default();
        let owns_seed = session_id.is_some_and(|id| {
            leases
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .owns_seed(id, seed)
        });
        if !owns_seed {
            return write_response(
                stream,
                "401 Unauthorized",
                "application/json",
                br#"{"code":"lease_required","message":"attach the session seed before querying"}"#,
            )
            .await;
        }
    }
    match query::query(service, method, &params) {
        Ok(value) => {
            write_response(
                stream,
                "200 OK",
                "application/json",
                &serde_json::to_vec(&value).map_err(stringify)?,
            )
            .await
        }
        Err(error) => {
            write_response(
                stream,
                "400 Bad Request",
                "application/json",
                &serde_json::to_vec(&query::error_response(&error)).map_err(stringify)?,
            )
            .await
        }
    }
}

async fn handle_action(
    stream: &mut TcpStream,
    name: &str,
    body: &[u8],
    session_id: Option<&str>,
    leases: &Arc<Mutex<RingingLeaseStore>>,
    service: &DeepxService,
    pending: &Arc<Mutex<PendingCommandStore>>,
) -> Result<(), String> {
    let method = name.trim_matches('/').replace('/', ".");
    let mut params: serde_json::Value = if body.is_empty() {
        serde_json::json!({})
    } else {
        serde_json::from_slice(body).map_err(|e| format!("invalid action body: {e}"))?
    };
    if !params.is_object() {
        return write_response(
            stream,
            "400 Bad Request",
            "text/plain",
            b"action body must be object",
        )
        .await;
    }
    let seed = params.get("seed").and_then(serde_json::Value::as_str);
    if let Some(seed) = seed {
        let lease_ok = session_id.is_some_and(|id| {
            leases
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .owns_seed(id, seed)
        });
        if !lease_ok {
            return write_response(stream, "401 Unauthorized", "text/plain", b"lease_required")
                .await;
        }
        if seed.is_empty() {
            return write_response(stream, "400 Bad Request", "text/plain", b"invalid seed").await;
        }
    }
    // Agent/session lifecycle and conversation commands must use Ringing command
    // envelopes; generic actions are limited to auxiliary typed services.
    let allowed = method.starts_with("git.")
        || method.starts_with("workspace.")
        || method.starts_with("config.")
        || method.starts_with("skills.")
        || method.starts_with("stats.")
        || method.starts_with("plan.")
        || method.starts_with("todo.");
    if !allowed {
        return write_response(
            stream,
            "400 Bad Request",
            "application/json",
            br#"{"code":"invalid_envelope","message":"agent commands must use Ringing commands"}"#,
        )
        .await;
    }
    let Some(action_id) = params.get("action_id").and_then(serde_json::Value::as_str) else {
        return write_response(
            stream,
            "400 Bad Request",
            "application/json",
            br#"{"code":"invalid_envelope","message":"action_id is required"}"#,
        )
        .await;
    };
    if action_id.is_empty() {
        return write_response(
            stream,
            "400 Bad Request",
            "application/json",
            br#"{"code":"invalid_envelope","message":"action_id is empty"}"#,
        )
        .await;
    }
    let supplied_fingerprint = params
        .get("fingerprint")
        .and_then(serde_json::Value::as_str);
    let mut fingerprint_params = params.clone();
    if let Some(object) = fingerprint_params.as_object_mut() {
        object.remove("action_id");
        object.remove("fingerprint");
    }
    let fingerprint_payload = serde_json::to_vec(&serde_json::json!({
        "method": method,
        "params": fingerprint_params,
    }))
    .map_err(stringify)?;
    let fingerprint = deepx_runtime::ringing::content_store::sha256_hex(&fingerprint_payload);
    if supplied_fingerprint.is_some_and(|value| value != fingerprint) {
        return write_response(
            stream,
            "400 Bad Request",
            "application/json",
            br#"{"code":"invalid_envelope","message":"action fingerprint mismatch"}"#,
        )
        .await;
    }
    let receipt_id = format!("action:{action_id}");
    let receipt_session_id = session_id.expect("validated Ringing session");
    let record_result = {
        let mut pending = pending.lock().unwrap_or_else(|e| e.into_inner());
        pending.record_fingerprint_for_session(&receipt_id, &fingerprint, receipt_session_id)
    };
    let duplicate = match record_result {
        Ok(value) => !value,
        Err(()) => {
            return write_response(
                stream,
                "409 Conflict",
                "application/json",
                br#"{"code":"duplicate_command_mismatch","message":"action_id was already used with another payload"}"#,
            )
            .await;
        }
    };
    if duplicate {
        return write_response(
            stream,
            "200 OK",
            "application/json",
            &serde_json::to_vec(&serde_json::json!({
                "status": "accepted",
                "action_id": action_id,
                "duplicate": true,
            }))
            .map_err(stringify)?,
        )
        .await;
    }
    if let Some(object) = params.as_object_mut() {
        object.remove("action");
        object.remove("action_id");
        object.remove("fingerprint");
    }
    match service.handle(&method, &params) {
        Ok(value) => {
            pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .mark_terminal(&receipt_id, RingingCommandState::Succeeded, None, None);
            write_response(
                stream,
                "200 OK",
                "application/json",
                &serde_json::to_vec(&value).map_err(stringify)?,
            )
            .await
        }
        Err(error) => {
            pending
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .rollback(&receipt_id);
            write_response(
                stream,
                "400 Bad Request",
                "application/json",
                &serde_json::to_vec(&query::error_response(&error)).map_err(stringify)?,
            )
            .await
        }
    }
}

/// SessionClose 的 seed 解析：命令 seed 优先，其次 envelope seed（无则空）。
fn session_close_seed(close_seed: &str, envelope_seed: &Option<String>) -> String {
    if !close_seed.is_empty() {
        close_seed.to_string()
    } else {
        envelope_seed.clone().unwrap_or_default()
    }
}

fn publish_session_created(hub: &RingingHub, seed: &str, command_id: &str) {
    let _ = hub.publish_with_causation(
        seed,
        deepx_domain::DomainEvent::Control(deepx_domain::ControlEvent::SessionStateChanged {
            seed: seed.to_string(),
            state: deepx_domain::SessionState::Created,
        }),
        Some(command_id),
    );
}

/// `ringing.reset_required` SSE 帧（cursor 超出保留窗口时发送）。
fn sse_reset_frame(reset: &RingingResetRequired) -> String {
    let data = serde_json::to_string(reset).unwrap_or_else(|_| "{}".into());
    format!("event: ringing.reset_required\ndata: {data}\n\n")
}

/// 单频道 SSE 长连接。
async fn handle_sse(
    stream: &mut TcpStream,
    channel: &str,
    request: &HttpRequest,
    session_id: &str,
    leases: Arc<Mutex<RingingLeaseStore>>,
    hub: Arc<RingingHub>,
) -> Result<(), String> {
    let Some(channel) = parse_channel(channel) else {
        return write_response(stream, "404 Not Found", "text/plain", b"unknown channel").await;
    };

    // Last-Event-ID：`epoch:channel:stream_seq`（只回放该频道可靠 tail）
    let last_event_id = request
        .header("last-event-id")
        .or_else(|| {
            request.path.split('?').nth(1).and_then(|q| {
                q.split('&')
                    .find_map(|kv| kv.strip_prefix("last_event_id="))
            })
        })
        .unwrap_or("");
    let after_seq = parse_sse_cursor(last_event_id, &hub.epoch(), channel);

    // 先订阅实时通道再回放 journal，避免回放期间新事件丢失；
    // 回放集合内的事件在实时循环中按 event_id 去重。
    let mut rx = hub.subscribe(channel);
    // A channel SSE is connection-scoped, but the hub aggregates multiple
    // seeds. Filter both replay and live fanout by the lease attached to this
    // client session; otherwise a client owning seed A could observe seed B.
    let replay = filter_replay_for_session(
        hub.replay_channel_since(channel, after_seq),
        session_id,
        &leases,
    );
    let replayed_ids: HashSet<String> = replay.events.iter().map(|e| e.event_id.clone()).collect();

    // 响应头
    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\n\r\n";
    stream.write_all(head.as_bytes()).await.map_err(stringify)?;
    stream.flush().await.map_err(stringify)?;

    // 可靠 tail + 当前 replaceable 值（PLAN：Last-Event-ID 有效时只回放可靠 tail）
    for env in &replay.events {
        if stream.write_all(sse_frame(env).as_bytes()).await.is_err() {
            return Ok(());
        }
        let _ = stream.flush().await;
    }
    // cursor 超出保留窗口的会话：客户端必须经 HTTP 读取权威 snapshot
    for reset in &replay.resets {
        if stream
            .write_all(sse_reset_frame(reset).as_bytes())
            .await
            .is_err()
        {
            return Ok(());
        }
        let _ = stream.flush().await;
    }

    let mut keepalive = tokio::time::interval(Duration::from_millis(SSE_KEEPALIVE_MS));
    keepalive.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tokio::select! {
            _ = keepalive.tick() => {
                // renewal 是 HTTP/SSE 这对连接的 client→server ACK。lease 已过期时
                // 不能继续保留一条只写的“幽灵 SSE”，客户端会立即重连并重新 open。
                if !leases
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .is_active_session(session_id)
                {
                    return Ok(());
                }
                if stream.write_all(b": keepalive\n\n").await.is_err() {
                    return Ok(()); // 客户端断开
                }
                let _ = stream.flush().await;
            }
            recv = rx.recv() => {
                match recv {
                    Ok(envelope) => {
                        if !leases
                            .lock()
                            .unwrap_or_else(|e| e.into_inner())
                            .owns_seed(session_id, &envelope.seed)
                        {
                            continue;
                        }
                        // 跳过回放已发送/连接前已确认的事件
                        if envelope.stream_seq <= after_seq
                            || replayed_ids.contains(&envelope.event_id)
                        {
                            continue;
                        }
                        if stream.write_all(sse_frame(&envelope).as_bytes()).await.is_err() {
                            return Ok(());
                        }
                        let _ = stream.flush().await;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Lagged(_)) => {
                        // Do not continue on this connection. The broadcast receiver has
                        // already skipped reliable events, while the Electron client may
                        // subsequently advance Last-Event-ID past them. Closing forces it
                        // to reconnect from its last *received* cursor and replay the gap
                        // from the reliable journal.
                        return Ok(());
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return Ok(()),
                }
            }
        }
    }
}

fn filter_replay_for_session(
    mut replay: deepx_runtime::ringing::hub::ChannelReplay,
    session_id: &str,
    leases: &Arc<Mutex<RingingLeaseStore>>,
) -> deepx_runtime::ringing::hub::ChannelReplay {
    let mut leases = leases.lock().unwrap_or_else(|e| e.into_inner());
    replay
        .events
        .retain(|event| leases.owns_seed(session_id, &event.seed));
    replay
        .resets
        .retain(|reset| leases.owns_seed(session_id, &reset.seed));
    replay
}

/// 解析 SSE cursor `epoch:channel:seq`（epoch/channel 不匹配视为 0）。
fn parse_sse_cursor(cursor: &str, epoch: &str, channel: RingingChannel) -> u64 {
    let mut parts = cursor.split(':');
    let e = parts.next().unwrap_or("");
    let c = parts.next().unwrap_or("");
    let seq = parts
        .next()
        .and_then(|s| s.parse::<u64>().ok())
        .unwrap_or(0);
    if e == epoch && c == channel.as_str() {
        seq
    } else {
        0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lease_lifecycle_ttl_renew_expiry() {
        let mut store = RingingLeaseStore::new();
        // open 关联双 id：client_instance_id（校验键）+ client_session_id（续租键）
        store.open("cs-1".into(), "ci-1".into());
        assert!(store.is_active("ci-1"));
        // 命令/切流端点用 client_instance_id 校验
        assert!(store.is_active("ci-1"));
        assert!(!store.is_active("unknown"));
        // renew 用 client_session_id 反查续期
        assert!(store.renew("cs-1"));
        // 过期模拟：直接改内部时间
        let inst = store.leases.get_mut("ci-1").expect("lease exists");
        inst.expiry = Instant::now() - Duration::from_secs(1);
        assert!(!store.is_active("ci-1"));
        assert!(!store.renew("cs-1"));
    }

    #[test]
    fn channel_parsing() {
        assert_eq!(parse_channel("control"), Some(RingingChannel::Control));
        assert_eq!(
            parse_channel("conversation"),
            Some(RingingChannel::Conversation)
        );
        assert_eq!(parse_channel("tool"), Some(RingingChannel::Tool));
        assert_eq!(parse_channel("bogus"), None);
    }

    #[test]
    fn sse_cursor_parsing() {
        assert_eq!(
            parse_sse_cursor("epoch-1:tool:42", "epoch-1", RingingChannel::Tool),
            42
        );
        assert_eq!(
            parse_sse_cursor("epoch-2:tool:42", "epoch-1", RingingChannel::Tool),
            0
        );
        assert_eq!(
            parse_sse_cursor("epoch-1:conversation:7", "epoch-1", RingingChannel::Tool),
            0
        );
        assert_eq!(
            parse_sse_cursor("garbage", "epoch-1", RingingChannel::Tool),
            0
        );
    }

    #[test]
    fn timeline_cursor_is_separate_from_v2_channel_cursors() {
        assert_eq!(parse_timeline_cursor("epoch-1:timeline:42", "epoch-1"), 42);
        assert_eq!(parse_timeline_cursor("epoch-1:tool:42", "epoch-1"), 0);
        assert_eq!(parse_timeline_cursor("epoch-2:timeline:42", "epoch-1"), 0);
        assert_eq!(
            parse_timeline_cursor("epoch-1:timeline:42:extra", "epoch-1"),
            0
        );
    }

    #[test]
    fn sse_frame_format_matches_plan() {
        let env = deepx_ringing::RingingEventEnvelope::new(
            "epoch-1",
            "s1",
            7,
            3,
            2,
            "e1",
            deepx_ringing::RingingEvent::Tool(deepx_domain::ToolEvent::ToolStarted {
                tool_call_id: "c".into(),
                turn_id: "t".into(),
                round_num: 0,
                name: "exec".into(),
            }),
        );
        let frame = sse_frame(&env);
        assert!(frame.starts_with("id: epoch-1:tool:7\nevent: tool_started\ndata: "));
        assert!(frame.ends_with("\n\n"));
        // data 必须是完整信封：含 seed（renderer 按会话路由）与 event_id（幂等）
        let data = frame
            .split("\ndata: ")
            .nth(1)
            .expect("data field")
            .trim_end_matches("\n\n");
        let parsed: serde_json::Value = serde_json::from_str(data).expect("data is JSON");
        assert_eq!(parsed["seed"], "s1");
        assert_eq!(parsed["event_id"], "e1");
        assert_eq!(parsed["stream_seq"], 7);
        assert_eq!(parsed["event"]["type"], "tool_started");
    }

    #[test]
    fn sse_reset_frame_format() {
        let reset = RingingResetRequired::new(RingingChannel::Tool, "s1", 7);
        let frame = sse_reset_frame(&reset);
        assert!(frame.starts_with("event: ringing.reset_required\ndata: "));
        assert!(frame.ends_with("\n\n"));
        assert!(frame.contains("\"seed\":\"s1\""));
        assert!(frame.contains("\"earliest_available_seq\":7"));
    }

    #[test]
    fn parse_preview_request_extracts_fields() {
        let preview = "POST /ringing/v2/commands/tool HTTP/1.1\r\nAuthorization: Bearer abc\r\nContent-Length: 7\r\n\r\n{\"a\":1}";
        let req = parse_preview_request(preview).expect("parse");
        assert_eq!(req.method, "POST");
        assert_eq!(req.path, "/ringing/v2/commands/tool");
        assert_eq!(req.header("authorization"), Some("Bearer abc"));
        assert_eq!(req.body, b"{\"a\":1}");
    }

    #[tokio::test]
    async fn read_request_waits_for_fragmented_body_beyond_router_preview() {
        use tokio::net::TcpListener;

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let server = tokio::spawn(async move {
            let (mut stream, _) = listener.accept().await.unwrap();
            read_request(&mut stream).await.unwrap()
        });

        let body = vec![b'x'; 4096];
        let mut client = TcpStream::connect(address).await.unwrap();
        let headers = format!(
            "POST /ringing/v2/content HTTP/1.1\r\nContent-Length: {}\r\n\r\n",
            body.len()
        );
        client.write_all(headers.as_bytes()).await.unwrap();
        client.write_all(&body[..512]).await.unwrap();
        tokio::task::yield_now().await;
        client.write_all(&body[512..]).await.unwrap();

        let request = server.await.unwrap();
        assert_eq!(request.method, "POST");
        assert_eq!(request.path, "/ringing/v2/content");
        assert_eq!(request.body, body);
    }

    #[test]
    fn pending_command_idempotency() {
        let mut store = PendingCommandStore::new();
        assert!(store.record("cmd-1"), "first accept");
        assert!(!store.record("cmd-1"), "duplicate within TTL rejected");
        assert!(store.is_known("cmd-1"));
        assert!(store.record("cmd-2"), "distinct id accepted");
        // 回滚后允许重试
        store.rollback("cmd-2");
        assert!(store.record("cmd-2"), "retry after rollback accepted");
    }

    #[test]
    fn command_receipts_are_scoped_to_the_owning_client_session() {
        let mut store = PendingCommandStore::new();
        assert!(
            store
                .record_fingerprint_for_session("cmd-owner", "fp", "session-a")
                .expect("first accept")
        );
        assert!(store.status_for_session("cmd-owner", "session-a").is_some());
        assert!(store.status_for_session("cmd-owner", "session-b").is_none());
        assert!(
            store
                .record_fingerprint_for_session("cmd-owner", "fp", "session-b")
                .is_err()
        );
    }

    #[test]
    fn causally_linked_terminal_event_completes_receipt_without_running_downgrade() {
        let mut store = PendingCommandStore::new();
        assert!(
            store
                .record_fingerprint_for_session("cmd-1", "fp", "session-a")
                .expect("accept")
        );
        let envelope = deepx_ringing::RingingEventEnvelope::new(
            "epoch",
            "seed",
            1,
            1,
            1,
            "event-1",
            deepx_ringing::RingingEvent::Tool(deepx_domain::ToolEvent::ToolFinished {
                tool_call_id: "call".into(),
                turn_id: "turn".into(),
                round_num: 0,
                result: deepx_domain::ToolResult {
                    success: true,
                    summary: "ok".into(),
                    output_ref: None,
                },
            }),
        )
        .with_causation("cmd-1");
        store.observe_terminal_event(&envelope);
        // A very fast worker can publish before handle_command calls
        // mark_running; that late transition must not overwrite terminal.
        store.mark_running("cmd-1");
        let status = store
            .status_for_session("cmd-1", "session-a")
            .expect("status");
        assert_eq!(status.state, RingingCommandState::Succeeded);
        assert_eq!(status.terminal_event_id.as_deref(), Some("event-1"));
    }

    #[test]
    fn session_close_seed_resolution_prefers_command_seed() {
        assert_eq!(
            session_close_seed("s-command", &Some("s-envelope".into())),
            "s-command"
        );
        assert_eq!(session_close_seed("s-command", &None), "s-command");
        assert_eq!(
            session_close_seed("", &Some("s-envelope".into())),
            "s-envelope"
        );
        assert_eq!(session_close_seed("", &None), "");
    }

    #[test]
    fn session_create_event_carries_command_causation() {
        let hub = RingingHub::new("epoch-1");
        publish_session_created(&hub, "s-created", "cmd-create");
        let replay = hub.replay_channel_since(RingingChannel::Control, 0);
        assert_eq!(replay.events.len(), 1);
        assert_eq!(replay.events[0].seed, "s-created");
        assert_eq!(replay.events[0].causation_id.as_deref(), Some("cmd-create"));
        assert!(matches!(
            &replay.events[0].event,
            deepx_ringing::RingingEvent::Control(deepx_domain::ControlEvent::SessionStateChanged {
                state: deepx_domain::SessionState::Created,
                ..
            })
        ));
    }

    #[test]
    fn parse_query_param_extracts_seed() {
        assert_eq!(parse_query_param("seed=abc", "seed"), Some("abc".into()));
        assert_eq!(
            parse_query_param("a=1&seed=xyz", "seed"),
            Some("xyz".into())
        );
        assert_eq!(parse_query_param("a=1", "seed"), None);
    }

    #[test]
    fn sse_replay_is_scoped_to_session_seed_leases() {
        let leases = Arc::new(Mutex::new(RingingLeaseStore::new()));
        leases
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .open("cs-1".into(), "ci-1".into());
        assert!(
            leases
                .lock()
                .unwrap_or_else(|e| e.into_inner())
                .attach_seed("cs-1", "seed-a")
        );

        let event_a = deepx_ringing::RingingEventEnvelope::new(
            "epoch-1",
            "seed-a",
            1,
            1,
            1,
            "event-a",
            deepx_ringing::RingingEvent::Tool(deepx_domain::ToolEvent::ToolStarted {
                tool_call_id: "call-a".into(),
                turn_id: "turn-a".into(),
                round_num: 0,
                name: "exec".into(),
            }),
        );
        let event_b = deepx_ringing::RingingEventEnvelope::new(
            "epoch-1",
            "seed-b",
            2,
            1,
            1,
            "event-b",
            deepx_ringing::RingingEvent::Tool(deepx_domain::ToolEvent::ToolStarted {
                tool_call_id: "call-b".into(),
                turn_id: "turn-b".into(),
                round_num: 0,
                name: "exec".into(),
            }),
        );
        let replay = deepx_runtime::ringing::hub::ChannelReplay {
            events: vec![event_a, event_b],
            resets: vec![
                RingingResetRequired::new(RingingChannel::Tool, "seed-a", 1),
                RingingResetRequired::new(RingingChannel::Tool, "seed-b", 2),
            ],
        };
        let filtered = filter_replay_for_session(replay, "cs-1", &leases);
        assert_eq!(filtered.events.len(), 1);
        assert_eq!(filtered.events[0].seed, "seed-a");
        assert_eq!(filtered.resets.len(), 1);
        assert_eq!(filtered.resets[0].seed, "seed-a");
    }
}
