use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use deepx_proto::{
    CONTROL_PROTOCOL_VERSION, ControlClientMessage, ControlServerMessage, ControlSnapshot,
    DaemonDiscovery,
};
use deepx_runtime::{DeepxService, EventBus, LeaseDecision, LeaseManager};
use futures_util::{SinkExt, StreamExt};
use deepx_runtime::RingingHub;
use deepx_runtime::{WorkspaceMode, WorkspaceSupervisor};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, mpsc, watch};
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::protocol::{Message, WebSocketConfig};

/// Maximum time to wait for a channel send inside a `select!` branch body.
///
/// Must be well under the heartbeat interval (5 s) so a stuck writer is
/// detected by the writer-monitor arm, not by a permanent branch-body block.
const SEND_TIMEOUT_MS: u64 = 500;
const MAX_FRAME_BYTES: usize = 1024 * 1024;
const HEARTBEAT_INTERVAL_MS: u64 = 5_000;
const LEASE_TIMEOUT_MS: u64 = 15_000;
const MAX_CONNECTIONS: usize = 32;
const OUTBOUND_QUEUE_CAPACITY: usize = 8_192;
const REQUEST_QUEUE_CAPACITY: usize = 64;

struct RequestJob {
    request_id: String,
    method: String,
    params: serde_json::Value,
}

fn daemon_channel() -> String {
    std::env::var("DEEPX_CHANNEL").unwrap_or_else(|_| {
        if cfg!(debug_assertions) {
            "dev".into()
        } else {
            "stable".into()
        }
    })
}

pub async fn run() -> Result<(), String> {
    deepx_types::platform::ensure_data_root().map_err(stringify)?;
    let _lock = acquire_single_instance()?;
    let token = random_hex();
    let epoch = random_hex();
    let listener = TcpListener::bind("127.0.0.1:0").await.map_err(stringify)?;
    let address = listener.local_addr().map_err(stringify)?;
    let discovery = DaemonDiscovery {
        endpoint: format!("ws://{address}/control/v1"),
        token: token.clone(),
        pid: std::process::id(),
        server_epoch: epoch.clone(),
        protocol_version: CONTROL_PROTOCOL_VERSION,
        daemon_version: env!("CARGO_PKG_VERSION").into(),
        build_id: env!("DEEPX_BUILD_ID").into(),
        channel: daemon_channel(),
        executable: std::env::current_exe()
            .ok()
            .and_then(|path| path.canonicalize().ok().or(Some(path)))
            .map(|path| path.to_string_lossy().into_owned())
            .unwrap_or_default(),
    };
    write_discovery(&discovery)?;
    let events = EventBus::new(epoch);
    let hub = Arc::new(RingingHub::new(events.epoch().to_string()));
    let service = DeepxService::init(events);
    service.attach_ringing(hub.clone());
    // 工具套件运行环境：config `[workspace] mode`（local 默认 / wsl 可选）。
    // 拉起失败不阻塞 daemon——worker 回退进程内工具执行。
    let workspace_mode = deepx_config::Config::load()
        .map(|config| WorkspaceMode::parse(&config.workspace.mode))
        .unwrap_or(WorkspaceMode::Local);
    let workspace = WorkspaceSupervisor::start(workspace_mode).ok();
    if let Some(ref ws) = workspace {
        service.attach_workspace(ws.endpoint.clone(), ws.token.clone());
        service.attach_workspace_state(deepx_runtime::WorkspaceRuntimeState {
            configured_mode: workspace_mode.label().into(),
            active_mode: ws.mode_label().into(),
            endpoint: ws.endpoint.clone(),
        });
    } else {
        service.attach_workspace_state(deepx_runtime::WorkspaceRuntimeState {
            configured_mode: workspace_mode.label().into(),
            active_mode: "disabled".into(),
            endpoint: String::new(),
        });
    }
    let leases = Arc::new(Mutex::new(LeaseManager::default()));
    let ringing_leases = Arc::new(Mutex::new(
        crate::ringing_http::RingingLeaseStore::new(),
    ));
    let pending_commands = Arc::new(Mutex::new(
        crate::ringing_http::PendingCommandStore::new(),
    ));
    let connections = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    let (shutdown, mut shutdown_rx) = watch::channel(false);
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else { break };
                let Ok(permit) = connections.clone().try_acquire_owned() else {
                    drop(stream);
                    continue;
                };
                let service = service.clone(); let leases = leases.clone(); let token = token.clone();
                let hub = hub.clone(); let ringing_leases = ringing_leases.clone();
                let pending_commands = pending_commands.clone();
                let shutdown = shutdown.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    if let Err(error)=handle_connection(stream,token,service,leases,shutdown,hub,ringing_leases,pending_commands).await { log::warn!("control connection: {error}"); }
                });
            }
            changed = shutdown_rx.changed() => if changed.is_err() || *shutdown_rx.borrow() { break },
        }
    }
    service.shutdown();
    if let Some(ref ws) = workspace {
        ws.stop();
    }
    let _ = std::fs::remove_file(deepx_types::platform::daemon_discovery_path());
    let _ = std::fs::remove_file(deepx_types::platform::daemon_lock_path());
    Ok(())
}

async fn handle_connection(
    mut stream: TcpStream,
    token: String,
    service: DeepxService,
    leases: Arc<Mutex<LeaseManager>>,
    shutdown: watch::Sender<bool>,
    hub: Arc<RingingHub>,
    ringing_leases: Arc<Mutex<crate::ringing_http::RingingLeaseStore>>,
    pending_commands: Arc<Mutex<crate::ringing_http::PendingCommandStore>>,
) -> Result<(), String> {
    let mut peek = [0_u8; 2048];
    let count = stream.peek(&mut peek).await.map_err(stringify)?;
    let preview = String::from_utf8_lossy(&peek[..count]);
    if preview.starts_with("POST /control/v1/stop ")
        || preview.starts_with("POST /control/v1/stop-if-idle ")
    {
        use tokio::io::AsyncWriteExt;
        let authorized = preview
            .lines()
            .any(|line| line.eq_ignore_ascii_case(&format!("Authorization: Bearer {token}")));
        let idle_required = preview.starts_with("POST /control/v1/stop-if-idle ");
        let busy = idle_required && service.has_active_work();
        let response = if !authorized {
            b"HTTP/1.1 401 Unauthorized\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
                .as_slice()
        } else if busy {
            b"HTTP/1.1 409 Conflict\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".as_slice()
        } else {
            b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n".as_slice()
        };
        stream.write_all(response).await.map_err(stringify)?;
        if authorized && !busy {
            let _ = shutdown.send(true);
        }
        return Ok(());
    }

    // Ringing HTTP/SSE 分流（PLAN：legacy WS 与 Ringing HTTP/SSE 并行、互不嵌套）
    if preview.starts_with("POST /ringing/") || preview.starts_with("GET /ringing/") {
        return crate::ringing_http::handle_ringing_http(
            stream,
            &preview,
            &token,
            hub,
            ringing_leases,
            service,
            pending_commands,
        )
        .await;
    }

    // Debug 只读页：静态服务前端产物（浏览器调试入口，无需 Electron）
    if preview.starts_with("GET /debug") {
        return crate::debug_http::handle_debug_http(stream, &preview, &token).await;
    }

    let expected = format!("Bearer {token}");
    let socket = tokio::time::timeout(
        Duration::from_secs(5),
        tokio_tungstenite::accept_hdr_async_with_config(
            stream,
            move |request: &Request, response: Response| {
                let authorized = request
                    .headers()
                    .get("authorization")
                    .and_then(|v| v.to_str().ok())
                    .is_some_and(|v| v == expected);
                if request.uri().path() != "/control/v1" {
                    return Err(error_response(StatusCode::NOT_FOUND, "not found"));
                }
                if !authorized {
                    return Err(error_response(StatusCode::UNAUTHORIZED, "unauthorized"));
                }
                Ok(response)
            },
            Some(
                WebSocketConfig::default()
                    .max_frame_size(Some(MAX_FRAME_BYTES))
                    .max_message_size(Some(MAX_FRAME_BYTES)),
            ),
        ),
    )
    .await
    .map_err(|_| "handshake timeout".to_string())?
    .map_err(stringify)?;
    let (mut sink, mut source) = socket.split();
    let first = tokio::time::timeout(Duration::from_secs(5), source.next())
        .await
        .map_err(|_| "hello timeout".to_string())?
        .ok_or_else(|| "connection closed".to_string())?
        .map_err(stringify)?;
    let hello: ControlClientMessage =
        serde_json::from_str(first.to_text().map_err(stringify)?).map_err(stringify)?;
    let ControlClientMessage::ClientHello {
        protocol_version,
        client_kind,
        client_instance_id,
        after_epoch,
        after_seq,
        ..
    } = hello
    else {
        return Err("first message must be client_hello".into());
    };
    if protocol_version != CONTROL_PROTOCOL_VERSION {
        send(
            &mut sink,
            &ControlServerMessage::Error {
                request_id: None,
                code: "protocol_version_mismatch".into(),
                message: format!("server requires protocol {CONTROL_PROTOCOL_VERSION}"),
                retry_after_ms: None,
            },
        )
        .await?;
        return Ok(());
    }
    let connection_id = random_hex();
    send(
        &mut sink,
        &ControlServerMessage::ServerHello {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            server_version: env!("CARGO_PKG_VERSION").into(),
            server_epoch: service.events().epoch().into(),
            heartbeat_interval_ms: HEARTBEAT_INTERVAL_MS,
            lease_timeout_ms: LEASE_TIMEOUT_MS,
            max_frame_bytes: MAX_FRAME_BYTES,
        },
    )
    .await?;
    // Subscribe before taking a replay/snapshot boundary so events published
    // concurrently with initial synchronization cannot fall through a gap.
    let mut events = service.events().subscribe();
    let replay = after_epoch
        .as_deref()
        .zip(after_seq)
        .and_then(|(epoch, seq)| service.events().replay_after(epoch, seq));
    let mut delivered_seq = after_seq.unwrap_or_default();
    if let Some(messages) = replay {
        for message in messages {
            if let ControlServerMessage::Event { seq, .. } = &message {
                delivered_seq = delivered_seq.max(*seq);
            }
            if event_allowed(&message, &leases, &client_instance_id, &client_kind) {
                send(&mut sink, &message).await?;
            }
        }
    } else {
        let attached = leases
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .attached_for(&client_instance_id, Instant::now());
        let snapshot_seq = service.events().current_seq();
        send(
            &mut sink,
            &ControlServerMessage::Snapshot {
                server_epoch: service.events().epoch().into(),
                seq: snapshot_seq,
                snapshot: build_snapshot(&service, attached).await?,
            },
        )
        .await?;
        delivered_seq = snapshot_seq;
    }
    let mut heartbeat = tokio::time::interval(Duration::from_millis(HEARTBEAT_INTERVAL_MS));
    let mut daemon_shutdown = shutdown.subscribe();
    let mut command_window = Instant::now();
    let mut command_count = 0_u32;
    // One ordered outbound queue is deliberate. Splitting critical, standard,
    // and bulk events allowed CompactEnd/TurnStart to overtake CompactDelta
    // and made the frontend replay a sequence that never existed in EventBus.
    let (outbound_tx, mut outbound_rx) = mpsc::channel(OUTBOUND_QUEUE_CAPACITY);
    let mut writer = tokio::spawn(async move {
        while let Some(message) = outbound_rx.recv().await {
            send(&mut sink, &message).await?;
        }
        Ok::<(), String>(())
    });
    let (request_tx, mut request_rx) = mpsc::channel::<RequestJob>(REQUEST_QUEUE_CAPACITY);
    let (git_tx, mut git_rx) = mpsc::channel::<RequestJob>(REQUEST_QUEUE_CAPACITY);
    let request_service = service.clone();
    let git_service = request_service.clone();
    let request_outbound = outbound_tx.clone();
    let git_outbound = outbound_tx.clone();
    let git_worker = tokio::spawn(async move {
        let git_semaphore = Arc::new(tokio::sync::Semaphore::new(4));
        while let Some(job) = git_rx.recv().await {
            let RequestJob { request_id, method, params } = job;
            let service = git_service.clone();
            let outbound = git_outbound.clone();
            let permit = git_semaphore.clone().acquire_owned().await;
            tokio::spawn(async move {
                let _permit = permit;
                let handled = tokio::task::spawn_blocking(move || service.handle(&method, &params)).await;
                let message = match handled {
                    Ok(Ok(result)) => ControlServerMessage::Response { request_id, result },
                    Ok(Err(message)) => ControlServerMessage::Error {
                        request_id: Some(request_id),
                        code: "request_failed".into(),
                        message,
                        retry_after_ms: None,
                    },
                    Err(error) => ControlServerMessage::Error {
                        request_id: Some(request_id),
                        code: "runtime_failed".into(),
                        message: error.to_string(),
                        retry_after_ms: None,
                    },
                };
                send_or_drop(&outbound, message).await;
            });
        }
    });
    let request_worker = tokio::spawn(async move {
        while let Some(job) = request_rx.recv().await {
            let RequestJob {
                request_id,
                method,
                params,
            } = job;
            let service = request_service.clone();
            let handled =
                tokio::task::spawn_blocking(move || service.handle(&method, &params)).await;
            let message = match handled {
                Ok(Ok(result)) => ControlServerMessage::Response { request_id, result },
                Ok(Err(message)) => ControlServerMessage::Error {
                    request_id: Some(request_id),
                    code: "request_failed".into(),
                    message,
                    retry_after_ms: None,
                },
                Err(error) => ControlServerMessage::Error {
                    request_id: Some(request_id),
                    code: "runtime_failed".into(),
                    message: error.to_string(),
                    retry_after_ms: None,
                },
            };
            // Timeout-bounded: a stuck writer must not permanently strand the
            // RPC reply. The request still executes (e.g. session.send_message
            // still writes agent stdin), so user input reaches the API even if
            // this reply is dropped after the timeout.
            send_or_drop(&request_outbound, message).await;
        }
    });
    loop {
        tokio::select! {
            incoming=source.next()=>{
                let Some(Ok(frame))=incoming else{break}; let Ok(text)=frame.to_text() else{continue};
                let Ok(message)=serde_json::from_str::<ControlClientMessage>(text) else{continue};
                // Heartbeat and SessionAttach are essential liveness / reconnection
                // signals and must not be counted toward the per-second rate limit.
                let counted = !matches!(message,
                    ControlClientMessage::Heartbeat{..} | ControlClientMessage::SessionAttach{..}
                );
                if counted {
                    if command_window.elapsed()>=Duration::from_secs(1){command_window=Instant::now();command_count=0;}
                    command_count=command_count.saturating_add(1);
                    if command_count>100{
                        log::warn!("control connection: rate limit exceeded ({command_count} commands/s), closing connection");
                        break;
                    }
                }
                match message {
                    ControlClientMessage::Heartbeat{nonce}=>{
                        leases.lock().unwrap_or_else(|e|e.into_inner()).renew_connection(&connection_id,Instant::now());
                        // Timeout-bounded: a dead writer drops the receiver
                        // and send returns Err immediately. A slow-but-alive
                        // writer blocks at most SEND_TIMEOUT_MS, then the
                        // message is dropped and the writer-monitor arm can
                        // fire on the next select! iteration.
                        send_or_drop(&outbound_tx, ControlServerMessage::Heartbeat{server_epoch:service.events().epoch().into(),seq:service.events().current_seq(),nonce}).await;
                    }
                    ControlClientMessage::SessionAttach{request_id,seed}=>{
                        let decision=if client_kind=="clawd"{
                            LeaseDecision::Acquired
                        }else{
                            leases.lock().unwrap_or_else(|e|e.into_inner()).attach(&seed,&client_instance_id,&client_kind,&connection_id,Instant::now())
                        };
                        match decision {
                            LeaseDecision::Acquired|LeaseDecision::Resumed=>{
                                send_or_drop(&outbound_tx, ControlServerMessage::Response{request_id,result:serde_json::json!({"seed":seed})}).await;
                                // An attach only needs the canonical state for
                                // that session. Rebuilding every previously
                                // attached session here turns multi-session
                                // reconnect into O(N^2) snapshots and can make
                                // the event consumer lag during heavy streams.
                                let attached=vec![seed];
                                let snapshot_seq=service.events().current_seq();
                                if let Ok(snapshot)=build_snapshot(&service,attached).await {
                                    send_or_drop(&outbound_tx, ControlServerMessage::Snapshot{server_epoch:service.events().epoch().into(),seq:snapshot_seq,snapshot}).await;
                                    delivered_seq=snapshot_seq;
                                }
                            }
                            LeaseDecision::Denied{owner_kind,retry_after_ms}=>{
                                send_or_drop(&outbound_tx, ControlServerMessage::LeaseDenied{request_id,seed,owner_kind,retry_after_ms}).await;
                            }
                        }
                    }
                    ControlClientMessage::SessionDetach{request_id,seed}=>{
                        let detached=leases.lock().unwrap_or_else(|e|e.into_inner()).detach(&seed,&client_instance_id);
                        send_or_drop(&outbound_tx, ControlServerMessage::Response{request_id,result:serde_json::json!({"detached":detached})}).await;
                    }
                    ControlClientMessage::Request{request_id,method,params}=>{
                        if DeepxService::session_scoped(&method){
                            let seed=params.get("seed").and_then(serde_json::Value::as_str).unwrap_or_default();
                            if !leases.lock().unwrap_or_else(|e|e.into_inner()).owns(seed,&client_instance_id,Instant::now()){
                                send_or_drop(&outbound_tx, ControlServerMessage::Error{request_id:Some(request_id),code:"session_lease_required".into(),message:format!("session {seed} is not attached"),retry_after_ms:None}).await;
                                continue;
                            }
                        }
                        let job = RequestJob{request_id: request_id.clone(), method, params};
                        let target = if job.method.starts_with("git.") { &git_tx } else { &request_tx };
                        if let Err(error)=target.try_send(job) {
                            let code=match error {
                                mpsc::error::TrySendError::Full(_)=>"request_queue_full",
                                mpsc::error::TrySendError::Closed(_)=>"runtime_failed",
                            };
                            send_or_drop(&outbound_tx, ControlServerMessage::Error{request_id:Some(request_id),code:code.into(),message:"daemon request worker is unavailable".into(),retry_after_ms:Some(5_000)}).await;
                        }
                    }
                    ControlClientMessage::ClientHello{..}=>break,
                }
            }
            event=events.recv()=>match event{
                Ok(ControlServerMessage::Event{seq,..}) if seq<=delivered_seq=>{},
                Ok(ref message @ ControlServerMessage::Event{seq,..})=>{
                    delivered_seq=seq;
                    if !event_allowed(message,&leases,&client_instance_id,&client_kind){ continue; }
                    send_or_drop(&outbound_tx, message.clone()).await;
                },
                Ok(message)=>{send_or_drop(&outbound_tx, message).await;}
                Err(tokio::sync::broadcast::error::RecvError::Lagged(_))=>{
                    let attached=leases.lock().unwrap_or_else(|e|e.into_inner()).attached_for(&client_instance_id,Instant::now());
                    let snapshot_seq=service.events().current_seq();
                    let snapshot=build_snapshot(&service,attached).await?;
                    send_or_drop(&outbound_tx, ControlServerMessage::Snapshot{server_epoch:service.events().epoch().into(),seq:snapshot_seq,snapshot}).await;
                    delivered_seq=snapshot_seq;
                }
                Err(_)=>break,
            },
            _=heartbeat.tick()=>{
                send_or_drop(&outbound_tx, ControlServerMessage::Heartbeat{server_epoch:service.events().epoch().into(),seq:service.events().current_seq(),nonce:service.events().current_seq()}).await;
            }
            changed=daemon_shutdown.changed()=>{
                if changed.is_err() || *daemon_shutdown.borrow() {
                    send_or_drop(&outbound_tx, ControlServerMessage::Shutdown{server_epoch:service.events().epoch().into(),seq:service.events().current_seq(),reason:"daemon_stop_requested".into()}).await;
                    break;
                }
            },
            result=&mut writer=>{
                match result {
                    Ok(Ok(()))=>{},
                    Ok(Err(error))=>return Err(error),
                    Err(error)=>return Err(error.to_string()),
                }
                break;
            },
        }
    }
    request_worker.abort();
    git_worker.abort();
    writer.abort();
    Ok(())
}

async fn build_snapshot(
    service: &DeepxService,
    attached: Vec<String>,
) -> Result<ControlSnapshot, String> {
    let service = service.clone();
    tokio::task::spawn_blocking(move || service.snapshot(attached))
        .await
        .map_err(|error| format!("snapshot worker failed: {error}"))
}

fn event_allowed(
    message: &ControlServerMessage,
    leases: &Arc<Mutex<LeaseManager>>,
    client_instance_id: &str,
    client_kind: &str,
) -> bool {
    // 观察者客户端（clawd 等）接收所有事件，不检查租约
    if client_kind == "clawd" {
        return true;
    }
    match message {
        ControlServerMessage::Event { event: deepx_proto::Agent2Ui::Error { .. }, .. } => true,
        ControlServerMessage::Event { seed, .. } => leases
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .owns(seed, client_instance_id, Instant::now()),
        _ => true,
    }
}

async fn send<S>(sink: &mut S, message: &ControlServerMessage) -> Result<(), String>
where
    S: futures_util::Sink<Message> + Unpin,
    S::Error: std::fmt::Display,
{
    sink.send(Message::Text(
        serde_json::to_string(message).map_err(stringify)?.into(),
    ))
    .await
    .map_err(stringify)
}

/// Send a message to an mpsc channel with a bounded timeout.
///
/// **Why this exists:** `tokio::select!` does not poll other branches while
/// the chosen branch's body is still awaiting.  An unbounded `tx.send(msg).await`
/// inside a branch body therefore blocks the entire `select!` — including the
/// `result = &mut writer` death-monitor branch — when the writer is slow but
/// alive (e.g. TCP send window exhausted on a half-open connection).  The
/// writer task never exits in that state, so the receiver is never dropped,
/// and `send` never returns `Err`.
///
/// The timeout caps the worst-case block at `SEND_TIMEOUT_MS`, after which the
/// message is dropped and the `select!` can poll the writer-monitor branch.
/// A *dead* writer (receiver dropped) still makes `send` return immediately,
/// so the timeout only kicks in for the stuck-but-alive case.
///
/// This is the root-cause fix for the "old sessions work, new sessions can't
/// send messages" regression introduced by d2f66da (which replaced the
/// previous `try_send().map_err()?` with unbounded `send().await`).
async fn send_or_drop<T>(tx: &mpsc::Sender<T>, msg: T) {
    match tokio::time::timeout(Duration::from_millis(SEND_TIMEOUT_MS), tx.send(msg)).await {
        Ok(Ok(())) | Ok(Err(_)) => {}
        Err(_) => log::warn!("control outbound: writer stalled >{SEND_TIMEOUT_MS}ms, dropping message"),
    }
}
fn error_response(status: StatusCode, text: &str) -> ErrorResponse {
    let mut response = ErrorResponse::new(Some(text.into()));
    *response.status_mut() = status;
    response
}
pub fn random_hex() -> String {
    rand::random::<[u8; 32]>()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
fn stringify(error: impl std::fmt::Display) -> String {
    error.to_string()
}

fn acquire_single_instance() -> Result<File, String> {
    let path = deepx_types::platform::daemon_lock_path();
    match OpenOptions::new().create_new(true).write(true).open(&path) {
        Ok(mut file) => {
            writeln!(file, "{}", std::process::id()).map_err(stringify)?;
            Ok(file)
        }
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {
            if discovery_is_reachable() {
                return Err("another daemon instance is already running".into());
            }
            std::fs::remove_file(&path).map_err(|e| format!("remove stale daemon lock: {e}"))?;
            let _ = std::fs::remove_file(deepx_types::platform::daemon_discovery_path());
            let mut file = OpenOptions::new()
                .create_new(true)
                .write(true)
                .open(&path)
                .map_err(stringify)?;
            writeln!(file, "{}", std::process::id()).map_err(stringify)?;
            Ok(file)
        }
        Err(error) => Err(error.to_string()),
    }
}
fn discovery_is_reachable() -> bool {
    let Ok(content) = std::fs::read_to_string(deepx_types::platform::daemon_discovery_path())
    else {
        return false;
    };
    let Ok(discovery) = serde_json::from_str::<DaemonDiscovery>(&content) else {
        return false;
    };
    let lock_pid = std::fs::read_to_string(deepx_types::platform::daemon_lock_path())
        .ok()
        .and_then(|value| value.trim().parse::<u32>().ok());
    if lock_pid != Some(discovery.pid) {
        return false;
    }
    if !deepx_types::platform::process_is_running(discovery.pid) {
        return false;
    }
    let address = discovery
        .endpoint
        .trim_start_matches("ws://")
        .split('/')
        .next()
        .unwrap_or_default();
    address.parse().ok().is_some_and(|address| {
        std::net::TcpStream::connect_timeout(&address, Duration::from_millis(300)).is_ok()
    })
}
fn write_discovery(discovery: &DaemonDiscovery) -> Result<(), String> {
    let target = deepx_types::platform::daemon_discovery_path();
    let temp = target.with_extension("json.tmp");
    let mut file = File::create(&temp).map_err(stringify)?;
    serde_json::to_writer_pretty(&mut file, discovery).map_err(stringify)?;
    file.flush().map_err(stringify)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o600))
            .map_err(stringify)?;
    }
    if target.exists() {
        std::fs::remove_file(&target).map_err(stringify)?;
    }
    std::fs::rename(temp, &target).map_err(stringify)?;
    restrict_discovery_permissions(&target)
}

#[cfg(windows)]
fn restrict_discovery_permissions(path: &std::path::Path) -> Result<(), String> {
    use std::os::windows::process::CommandExt;

    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    let output = std::process::Command::new("whoami")
        .creation_flags(CREATE_NO_WINDOW)
        .output()
        .map_err(|error| format!("resolve current Windows identity: {error}"))?;
    if !output.status.success() {
        return Err("resolve current Windows identity: whoami failed".into());
    }
    let identity = String::from_utf8_lossy(&output.stdout).trim().to_string();
    let status = std::process::Command::new("icacls")
        .arg(path)
        .args(["/inheritance:r", "/grant:r", &format!("{identity}:(F)")])
        .creation_flags(CREATE_NO_WINDOW)
        .status()
        .map_err(|error| format!("restrict daemon discovery ACL: {error}"))?;
    status
        .success()
        .then_some(())
        .ok_or_else(|| "restrict daemon discovery ACL: icacls failed".into())
}

#[cfg(not(windows))]
fn restrict_discovery_permissions(_path: &std::path::Path) -> Result<(), String> {
    Ok(())
}
