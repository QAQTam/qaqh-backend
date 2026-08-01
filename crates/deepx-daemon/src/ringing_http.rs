//! Ringing HTTP command/query + 三 SSE 事件流（daemon 侧传输层，T5）。
//!
//! 端点（PLAN 固定）：
//! ```text
//! POST /ringing/v1/clients/open
//! POST /ringing/v1/leases/renew
//! POST /ringing/v1/commands/{control|conversation|tool}
//! GET  /ringing/v1/snapshots/{channel}/{seed}
//! GET  /ringing/v1/content/{content_id}
//! GET  /ringing/v1/query/...
//! GET  /ringing/v1/events/{control|conversation|tool}   (SSE)
//! ```
//!
//! 硬规则：
//! - SSE 断开只表示该频道退化，**不撤销** session lease（TTL + renew 维护）。
//! - token 只经 `Authorization` header，不进 query string。
//! - HTTP command ack 只表达 accepted/rejected；业务完成由对应频道可靠终态表达。

use std::collections::{HashMap, HashSet};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use deepx_domain::{ControlCommand, RingingChannel};
use deepx_ringing::{
    ClientOpenRequest, ClientOpenResponse, RINGING_SCHEMA, RINGING_VERSION, RingingChannelSnapshot,
    RingingCommandAck, RingingCommandAckStatus, RingingCommandEnvelope, RingingResetRequired,
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
const MAX_BODY_BYTES: usize = 16 * 1024 * 1024;
const SSE_KEEPALIVE_MS: u64 = 15_000;

/// Ringing 逻辑 client session lease。
///
/// 键 = 客户端自生成的 `client_instance_id`（命令/切流端点校验字段）；
/// 值记录服务端签发的 `client_session_id`（renew 端点���用）。
/// open 时双 id 关联，renew 按 client_session_id 反查续期。
#[derive(Debug, Default)]
pub struct RingingLeaseStore {
    leases: HashMap<String, LeaseEntry>,
}

#[derive(Debug, Clone)]
struct LeaseEntry {
    client_session_id: String,
    expiry: Instant,
}

/// 已 accepted 命令的幂等表（有界 TTL；accepted 后断线重试不得重复执行）。
#[derive(Debug, Default)]
pub struct PendingCommandStore {
    accepted: HashMap<String, Instant>,
    max_entries: usize,
}

impl PendingCommandStore {
    pub fn new() -> Self {
        Self {
            accepted: HashMap::new(),
            max_entries: 4096,
        }
    }

    /// 记录 accepted。返回 false 表示重复（已 accepted 且未过期）。
    pub fn record(&mut self, command_id: &str) -> bool {
        let now = Instant::now();
        if let Some(at) = self.accepted.get(command_id) {
            if *at + Duration::from_secs(300) > now {
                return false; // 重复：已接受且在 TTL 内
            }
        }
        self.accepted.insert(command_id.to_string(), now);
        while self.accepted.len() > self.max_entries {
            let victim = self
                .accepted
                .iter()
                .min_by_key(|(_, at)| **at)
                .map(|(id, _)| id.clone())
                .expect("non-empty");
            self.accepted.remove(&victim);
        }
        true
    }

    pub fn is_known(&self, command_id: &str) -> bool {
        self.accepted
            .get(command_id)
            .is_some_and(|at| *at + Duration::from_secs(300) > Instant::now())
    }

    /// 转发失败回滚预留。
    pub fn rollback(&mut self, command_id: &str) {
        self.accepted.remove(command_id);
    }
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
                self.leases.remove(&k);
            }
            return false;
        }
        entry.expiry = Instant::now() + Duration::from_millis(RENEW_TTL_MS);
        true
    }

    /// 活跃校验（按 client_instance_id；命令/切流端点使用）��
    pub fn is_active(&self, client_instance_id: &str) -> bool {
        self.leases
            .get(client_instance_id)
            .is_some_and(|e| e.expiry >= Instant::now())
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
    preview: &str,
    token: &str,
    hub: Arc<RingingHub>,
    leases: Arc<Mutex<RingingLeaseStore>>,
    service: DeepxService,
    pending: Arc<Mutex<PendingCommandStore>>,
) -> Result<(), String> {
    // preview 已含请求头（2048 字节足够）；补读 body 时 read_request 需要流状态——
    // 简化：若 preview 完整（含 \r\n\r\n）则解析 preview，否则走 read_request 全量读取。
    // （真实实现中 preview 与流共享数据；这里用 preview 构造请求）
    let request = if preview.contains("\r\n\r\n") {
        parse_preview_request(preview)?
    } else {
        read_request(&mut stream).await?
    };

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

    if method == "POST" && path == "/ringing/v1/clients/open" {
        return handle_open(&mut stream, &request.body, &leases, &hub).await;
    }
    if method == "POST" && path == "/ringing/v1/leases/renew" {
        return handle_renew(&mut stream, &request.body, &leases).await;
    }
    if method == "POST" && path.starts_with("/ringing/v1/commands/") {
        let channel = path.trim_start_matches("/ringing/v1/commands/");
        return handle_command(
            &mut stream,
            channel,
            &request.body,
            &leases,
            &service,
            &pending,
        )
        .await;
    }
    if method == "POST" && path.starts_with("/ringing/v1/cutover/events/") {
        let channel = path.trim_start_matches("/ringing/v1/cutover/events/");
        return handle_cutover_events(&mut stream, channel, &request.body, &leases, &hub).await;
    }
    if method == "POST" && path.starts_with("/ringing/v1/cutover/commands/") {
        let channel = path.trim_start_matches("/ringing/v1/cutover/commands/");
        return handle_cutover_commands(&mut stream, channel, &request.body, &leases, &hub).await;
    }
    if method == "GET" && path.starts_with("/ringing/v1/snapshots/") {
        let rest = path.trim_start_matches("/ringing/v1/snapshots/");
        return handle_snapshot(&mut stream, rest, &hub).await;
    }
    if method == "GET" && path.starts_with("/ringing/v1/events/") {
        let channel = path.trim_start_matches("/ringing/v1/events/");
        return handle_sse(&mut stream, channel, &request, hub).await;
    }
    if method == "GET" && path.starts_with("/ringing/v1/content/") {
        return handle_content(&mut stream, &path, &hub).await;
    }
    if method == "GET" && path.starts_with("/ringing/v1/query/") {
        let rest = path.trim_start_matches("/ringing/v1/query/");
        return handle_query(&mut stream, rest, &service).await;
    }
    write_response(
        &mut stream,
        "404 Not Found",
        "text/plain",
        b"unknown ringing endpoint",
    )
    .await
}

/// GET /ringing/v1/content/{content_id}?seed={seed}
///
/// 大内容外置读取（PLAN）：鉴权由统一 Bearer token 完成；seed 查询参数
/// 用于会话所有权校验（ContentStore 拒绝跨会话读取）。返回 200 + media_type
/// 或 404（不存在/过期/非本会话）。
async fn handle_content(
    stream: &mut TcpStream,
    path: &str,
    hub: &Arc<RingingHub>,
) -> Result<(), String> {
    let rest = path.trim_start_matches("/ringing/v1/content/");
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
            "400 Bad Request",
            "application/json",
            &serde_json::to_vec(&RingingCommandAck {
                command_id: String::new(),
                status: RingingCommandAckStatus::Rejected,
                code: Some("schema_or_version_mismatch".into()),
                message: Some("unsupported Ringing schema/version".into()),
                retry_after_ms: None,
            })
            .unwrap_or_default(),
        )
        .await;
    }
    let client_session_id = random_hex();
    // lease 双键关联：client_instance_id（命令/切流校验）+ client_session_id（renew）
    leases
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .open(client_session_id.clone(), req.client_instance_id.clone());
    // 支持的能力 = 请求 ∩ 服务端支持
    let supported: &[&str] = &[
        "Ringing_v1",
        "Ringing_session_cutover_v1",
        "Ringing_batch_v1",
    ];
    let capabilities: Vec<String> = req
        .capabilities
        .iter()
        .filter(|c| supported.contains(&c.as_str()))
        .cloned()
        .collect();
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
    body: &[u8],
    leases: &Arc<Mutex<RingingLeaseStore>>,
) -> Result<(), String> {
    #[derive(serde::Deserialize)]
    struct RenewBody {
        client_session_id: String,
    }
    let req: RenewBody =
        serde_json::from_slice(body).map_err(|e| format!("invalid renew request: {e}"))?;
    let ok = leases
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .renew(&req.client_session_id);
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

/// GET /ringing/v1/query/{path}?seed=...
///
/// 只读查询（契约 §1）：`session/list`、`session/meta`、`session/activity`；
/// 未知路径 → 404 + JSON 错误；`session/meta` 缺 seed → 400。
async fn handle_query(
    stream: &mut TcpStream,
    path_with_query: &str,
    service: &DeepxService,
) -> Result<(), String> {
    let (query_path, query_string) = match path_with_query.split_once('?') {
        Some((path, query)) => (path, Some(query)),
        None => (path_with_query, None),
    };
    let seed = query_string.and_then(|query| parse_query_param(query, "seed"));
    let method = match query_path {
        // 同时接受斜杠（URL 路径）与点号（legacy RPC 方法名）两种形式
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
        "daemon/version" | "daemon.version" => Some("daemon.version"),
        _ => None,
    };
    let Some(method) = method else {
        let body = serde_json::to_vec(&query::error_response(&format!(
            "unknown query path {query_path}"
        )))
        .map_err(stringify)?;
        return write_response(stream, "404 Not Found", "application/json", &body).await;
    };
    if query::requires_seed(method) && seed.is_none() {
        let body = serde_json::to_vec(&query::error_response("missing seed query param"))
            .map_err(stringify)?;
        return write_response(stream, "400 Bad Request", "application/json", &body).await;
    }
    let params = serde_json::json!({ "seed": seed });
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
            let body = serde_json::to_vec(&query::error_response(&error)).map_err(stringify)?;
            write_response(stream, "400 Bad Request", "application/json", &body).await
        }
    }
}

async fn handle_command(
    stream: &mut TcpStream,
    channel: &str,
    body: &[u8],
    leases: &Arc<Mutex<RingingLeaseStore>>,
    service: &DeepxService,
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
    if !leases
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_active(&env.client_instance_id)
    {
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
    // 幂等：accepted 后断线重试不得重复执行（PLAN 命令幂等硬规则）。
    // 锁内判断 + 预留记录，转发失败时回滚；锁不跨 await。
    let duplicate = {
        let mut pending = pending.lock().unwrap_or_else(|e| e.into_inner());
        !pending.record(&env.command_id)
    };
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

    let seed = env.seed.clone().unwrap_or_default();
    let worker_env = deepx_ringing::RingingWorkerCommandEnvelope::new(
        seed.as_str(),
        env.command_id.clone(),
        env.command.clone(),
    );
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

/// SessionClose 的 seed 解析：命令 seed 优先，其次 envelope seed（无则空）。
fn session_close_seed(close_seed: &str, envelope_seed: &Option<String>) -> String {
    if !close_seed.is_empty() {
        close_seed.to_string()
    } else {
        envelope_seed.clone().unwrap_or_default()
    }
}

/// POST /ringing/v1/cutover/events/{channel}
///
/// 事件切流���两阶段提交，PLAN）：
/// body: `{"action": "prepare"|"commit"|"abort", "seed": "...", "client_instance_id": "..."}`
/// - prepare：进入 Preparing（SSE boundary 建立 + snapshot + 缓冲阶段），
///   事件协议仍为 legacy；
/// - commit：原子切换 event owner 为 Ringing（必须 precede prepare）；
/// - abort：prepare 失败/超时/断线，保持 legacy。
///
/// 响应：`{ok, event_protocol, command_protocol}`；AlreadyRinging /
/// NotPreparing → 409 Conflict。
async fn handle_cutover_events(
    stream: &mut TcpStream,
    channel: &str,
    body: &[u8],
    leases: &Arc<Mutex<RingingLeaseStore>>,
    hub: &Arc<RingingHub>,
) -> Result<(), String> {
    let Some(channel) = parse_channel(channel) else {
        return write_response(stream, "404 Not Found", "text/plain", b"unknown channel").await;
    };
    #[derive(serde::Deserialize)]
    struct CutoverEventsBody {
        action: String,
        seed: String,
        client_instance_id: String,
    }
    let req: CutoverEventsBody = match serde_json::from_slice(body) {
        Ok(req) => req,
        Err(e) => {
            return write_response(
                stream,
                "400 Bad Request",
                "text/plain",
                format!("invalid cutover request: {e}").as_bytes(),
            )
            .await;
        }
    };
    if !leases
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_active(&req.client_instance_id)
    {
        return write_response(
            stream,
            "401 Unauthorized",
            "text/plain",
            b"lease required: open a client session before cutover",
        )
        .await;
    }
    let result = match req.action.as_str() {
        "prepare" => hub.cutover_prepare(&req.seed, channel),
        "commit" => hub.cutover_commit(&req.seed, channel),
        "abort" => {
            hub.cutover_abort(&req.seed, channel);
            Ok(())
        }
        other => {
            return write_response(
                stream,
                "400 Bad Request",
                "text/plain",
                format!("unknown cutover action: {other}").as_bytes(),
            )
            .await;
        }
    };
    match result {
        Ok(()) => {
            let resp = serde_json::json!({
                "ok": true,
                "event_protocol": if hub.event_is_ringing(&req.seed, channel) { "ringing" } else { "legacy" },
                "command_protocol": if hub.command_is_ringing(&req.seed, channel) { "ringing" } else { "legacy" },
            });
            write_response(
                stream,
                "200 OK",
                "application/json",
                &serde_json::to_vec(&resp).map_err(stringify)?,
            )
            .await
        }
        Err(e) => {
            let (code, message) = match e {
                deepx_runtime::ringing::cutover::CutoverError::AlreadyRinging => {
                    ("already_ringing", "channel already switched to Ringing")
                }
                deepx_runtime::ringing::cutover::CutoverError::NotPreparing => {
                    ("not_preparing", "commit/abort requires a preceding prepare")
                }
            };
            let resp = serde_json::json!({
                "ok": false,
                "code": code,
                "message": message,
            });
            write_response(
                stream,
                "409 Conflict",
                "application/json",
                &serde_json::to_vec(&resp).map_err(stringify)?,
            )
            .await
        }
    }
}

/// POST /ringing/v1/cutover/commands/{channel}
///
/// 命令切流（单阶段，PLAN command_mode_prepare）：
/// body: `{"protocol": "ringing"|"legacy", "seed": "...", "client_instance_id": "..."}`
/// 服务端返回命令协议已切换；此后该 seed+channel 的命令应走 Ringing command。
async fn handle_cutover_commands(
    stream: &mut TcpStream,
    channel: &str,
    body: &[u8],
    leases: &Arc<Mutex<RingingLeaseStore>>,
    hub: &Arc<RingingHub>,
) -> Result<(), String> {
    let Some(channel) = parse_channel(channel) else {
        return write_response(stream, "404 Not Found", "text/plain", b"unknown channel").await;
    };
    #[derive(serde::Deserialize)]
    struct CutoverCommandsBody {
        protocol: String,
        seed: String,
        client_instance_id: String,
    }
    let req: CutoverCommandsBody = match serde_json::from_slice(body) {
        Ok(req) => req,
        Err(e) => {
            return write_response(
                stream,
                "400 Bad Request",
                "text/plain",
                format!("invalid cutover request: {e}").as_bytes(),
            )
            .await;
        }
    };
    if !leases
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .is_active(&req.client_instance_id)
    {
        return write_response(
            stream,
            "401 Unauthorized",
            "text/plain",
            b"lease required: open a client session before cutover",
        )
        .await;
    }
    let ringing = match req.protocol.as_str() {
        "ringing" => true,
        "legacy" => false,
        other => {
            return write_response(
                stream,
                "400 Bad Request",
                "text/plain",
                format!("unknown protocol: {other}").as_bytes(),
            )
            .await;
        }
    };
    hub.cutover_switch_command(&req.seed, channel, ringing);
    let resp = serde_json::json!({
        "ok": true,
        "command_protocol": req.protocol,
        "event_protocol": if hub.event_is_ringing(&req.seed, channel) { "ringing" } else { "legacy" },
    });
    write_response(
        stream,
        "200 OK",
        "application/json",
        &serde_json::to_vec(&resp).map_err(stringify)?,
    )
    .await
}

async fn handle_snapshot(
    stream: &mut TcpStream,
    rest: &str,
    hub: &Arc<RingingHub>,
) -> Result<(), String> {
    let mut parts = rest.split('/');
    let channel = parts.next().unwrap_or("");
    let seed = parts.next().unwrap_or("");
    let Some(channel) = parse_channel(channel) else {
        return write_response(stream, "404 Not Found", "text/plain", b"unknown channel").await;
    };
    if seed.is_empty() {
        return write_response(stream, "400 Bad Request", "text/plain", b"missing seed").await;
    }
    let snap: RingingChannelSnapshot = match channel {
        RingingChannel::Conversation => hub.conversation_snapshot(seed),
        _ => hub.snapshot(channel, seed),
    };
    write_response(
        stream,
        "200 OK",
        "application/json",
        &serde_json::to_vec(&snap).map_err(stringify)?,
    )
    .await
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
    let replay = hub.replay_channel_since(channel, after_seq);
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
                if stream.write_all(b": keepalive\n\n").await.is_err() {
                    return Ok(()); // 客户端断开
                }
                let _ = stream.flush().await;
            }
            recv = rx.recv() => {
                match recv {
                    Ok(envelope) => {
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
                        // 慢消费者落后：跳过（reliable 由 cursor 重连兜底）
                        continue;
                    }
                    Err(tokio::sync::broadcast::error::RecvError::Closed) => return Ok(()),
                }
            }
        }
    }
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
        let preview = "POST /ringing/v1/commands/tool HTTP/1.1\r\nAuthorization: Bearer abc\r\nContent-Length: 7\r\n\r\n{\"a\":1}";
        let req = parse_preview_request(preview).expect("parse");
        assert_eq!(req.method, "POST");
        assert_eq!(req.path, "/ringing/v1/commands/tool");
        assert_eq!(req.header("authorization"), Some("Bearer abc"));
        assert_eq!(req.body, b"{\"a\":1}");
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
    fn parse_query_param_extracts_seed() {
        assert_eq!(parse_query_param("seed=abc", "seed"), Some("abc".into()));
        assert_eq!(
            parse_query_param("a=1&seed=xyz", "seed"),
            Some("xyz".into())
        );
        assert_eq!(parse_query_param("a=1", "seed"), None);
    }
}
