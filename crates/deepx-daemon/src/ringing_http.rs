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

use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use deepx_domain::RingingChannel;
use deepx_ringing::{
    ClientOpenRequest, ClientOpenResponse, RingingChannelSnapshot, RingingCommandAck,
    RingingCommandAckStatus, RingingCommandEnvelope, RINGING_SCHEMA, RINGING_VERSION,
};
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

/// Ringing 逻辑 client session lease（绑定 client_session_id，TTL + renew）。
#[derive(Debug, Default)]
pub struct RingingLeaseStore {
    leases: HashMap<String, Instant>,
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

    pub fn open(&mut self, client_session_id: String) {
        self.leases
            .insert(client_session_id, Instant::now() + Duration::from_millis(RENEW_TTL_MS));
    }

    /// 续租；过期/未知会话返回 false。
    pub fn renew(&mut self, client_session_id: &str) -> bool {
        let Some(expiry) = self.leases.get_mut(client_session_id) else {
            return false;
        };
        if *expiry < Instant::now() {
            self.leases.remove(client_session_id);
            return false;
        }
        *expiry = Instant::now() + Duration::from_millis(RENEW_TTL_MS);
        true
    }

    pub fn is_active(&self, client_session_id: &str) -> bool {
        self.leases
            .get(client_session_id)
            .is_some_and(|e| *e >= Instant::now())
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
    let request_line = lines.next().ok_or_else(|| "missing request line".to_string())?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().ok_or_else(|| "missing method".to_string())?.to_string();
    let path = parts.next().ok_or_else(|| "missing path".to_string())?.to_string();
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
    let data = serde_json::to_string(&envelope.event)
        .unwrap_or_else(|_| "{}".into());
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
        return write_response(&mut stream, "401 Unauthorized", "text/plain", b"unauthorized").await;
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
        return handle_command(&mut stream, channel, &request.body, &leases, &service, &pending).await;
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
        // content store 在 T9 实现；当前显式 404
        return write_response(&mut stream, "404 Not Found", "text/plain", b"content store unavailable").await;
    }
    if method == "GET" && path.starts_with("/ringing/v1/query/") {
        // 只读查询 RPC 保留（PLAN：不伪装成 Command/Event）
        return write_response(&mut stream, "501 Not Implemented", "text/plain", b"query rpc reserved").await;
    }
    write_response(&mut stream, "404 Not Found", "text/plain", b"unknown ringing endpoint").await
}

/// 从 peek preview 解析请求（preview 已含完整 header；body 长度按 header 读取，
/// 不足部分由调用方保证已 peek 或本函数返回错误）。
fn parse_preview_request(preview: &str) -> Result<HttpRequest, String> {
    let header_end = preview
        .find("\r\n\r\n")
        .ok_or_else(|| "incomplete headers".to_string())?;
    let header_text = &preview[..header_end];
    let mut lines = header_text.lines();
    let request_line = lines.next().ok_or_else(|| "missing request line".to_string())?;
    let mut parts = request_line.split_whitespace();
    let method = parts.next().ok_or_else(|| "missing method".to_string())?.to_string();
    let path = parts.next().ok_or_else(|| "missing path".to_string())?.to_string();
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
    let req: ClientOpenRequest = serde_json::from_slice(body)
        .map_err(|e| format!("invalid open request: {e}"))?;
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
    leases
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .open(client_session_id.clone());
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
    write_response(stream, "200 OK", "application/json", &serde_json::to_vec(&resp).map_err(stringify)?)
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
    let req: RenewBody = serde_json::from_slice(body)
        .map_err(|e| format!("invalid renew request: {e}"))?;
    let ok = leases
        .lock()
        .unwrap_or_else(|e| e.into_inner())
        .renew(&req.client_session_id);
    if !ok {
        return write_response(stream, "401 Unauthorized", "text/plain", b"lease expired or unknown")
            .await;
    }
    let resp = serde_json::json!({
        "ok": true,
        "lease_ttl_ms": RENEW_TTL_MS,
        "renew_interval_ms": RENEW_INTERVAL_MS,
    });
    write_response(stream, "200 OK", "application/json", &serde_json::to_vec(&resp).map_err(stringify)?)
        .await
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
            message: Some(format!("path channel {channel} != envelope channel {:?}", env.channel)),
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
    write_response(stream, "200 OK", "application/json", &serde_json::to_vec(&ack).map_err(stringify)?)
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
    let snap: RingingChannelSnapshot = hub.snapshot(channel, seed);
    write_response(stream, "200 OK", "application/json", &serde_json::to_vec(&snap).map_err(stringify)?)
        .await
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
            request
                .path
                .split('?')
                .nth(1)
                .and_then(|q| q.split('&').find_map(|kv| kv.strip_prefix("last_event_id=")))
        })
        .unwrap_or("");
    let after_seq = parse_sse_cursor(last_event_id, &hub.epoch(), channel);

    // TODO(T9/T10)：SSE 跨 seed 聚合回放（多 session 共用频道流）。
    // 当前仅做 cursor 校验与实时推送；可靠 tail 回放待频道迁移时按 seed 聚合。
    let _ = after_seq;

    // 响应头
    let head = "HTTP/1.1 200 OK\r\nContent-Type: text/event-stream\r\nCache-Control: no-cache\r\nConnection: keep-alive\r\n\r\n";
    stream.write_all(head.as_bytes()).await.map_err(stringify)?;
    stream.flush().await.map_err(stringify)?;

    let mut rx = hub.subscribe(channel);
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
    let seq = parts.next().and_then(|s| s.parse::<u64>().ok()).unwrap_or(0);
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
        store.open("c1".into());
        assert!(store.is_active("c1"));
        assert!(store.renew("c1"));
        assert!(!store.is_active("unknown"));
        // 过期模拟：直接改内部时间
        store.leases.insert("c1".into(), Instant::now() - Duration::from_secs(1));
        assert!(!store.is_active("c1"));
        assert!(!store.renew("c1"));
    }

    #[test]
    fn channel_parsing() {
        assert_eq!(parse_channel("control"), Some(RingingChannel::Control));
        assert_eq!(parse_channel("conversation"), Some(RingingChannel::Conversation));
        assert_eq!(parse_channel("tool"), Some(RingingChannel::Tool));
        assert_eq!(parse_channel("bogus"), None);
    }

    #[test]
    fn sse_cursor_parsing() {
        assert_eq!(parse_sse_cursor("epoch-1:tool:42", "epoch-1", RingingChannel::Tool), 42);
        assert_eq!(parse_sse_cursor("epoch-2:tool:42", "epoch-1", RingingChannel::Tool), 0);
        assert_eq!(parse_sse_cursor("epoch-1:conversation:7", "epoch-1", RingingChannel::Tool), 0);
        assert_eq!(parse_sse_cursor("garbage", "epoch-1", RingingChannel::Tool), 0);
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
}
