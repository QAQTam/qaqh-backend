use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use deepx_proto::{
    CONTROL_PROTOCOL_VERSION, ControlClientMessage, ControlServerMessage, DaemonDiscovery,
};
use futures_util::{SinkExt, StreamExt};
use serde_json::Value;
use tokio::sync::{Mutex, broadcast, mpsc, oneshot};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::Message;

type Pending = Arc<Mutex<HashMap<String, oneshot::Sender<Result<Value, ClientError>>>>>;

#[derive(Debug, Clone)]
pub struct ClientError {
    pub code: String,
    pub message: String,
}

impl std::fmt::Display for ClientError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}: {}", self.code, self.message)
    }
}
impl std::error::Error for ClientError {}

#[derive(Clone)]
pub struct DeepxClient {
    outgoing: mpsc::Sender<ControlClientMessage>,
    events: broadcast::Sender<ControlServerMessage>,
    pending: Pending,
    next_id: Arc<AtomicU64>,
    initial_receiver: Arc<std::sync::Mutex<Option<broadcast::Receiver<ControlServerMessage>>>>,
    connected: Arc<AtomicBool>,
    connection_marker: Arc<()>,
    pub client_instance_id: Arc<str>,
}

impl DeepxClient {
    pub async fn connect_or_launch(
        client_kind: &str,
        daemon_path: Option<&Path>,
    ) -> Result<Self, ClientError> {
        Self::connect_or_launch_with_id(client_kind, daemon_path, random_hex()).await
    }

    pub async fn connect_or_launch_with_id(
        client_kind: &str,
        daemon_path: Option<&Path>,
        client_instance_id: String,
    ) -> Result<Self, ClientError> {
        Self::connect_or_launch_with_state(client_kind, daemon_path, client_instance_id, None, None)
            .await
    }

    pub async fn connect_or_launch_with_state(
        client_kind: &str,
        daemon_path: Option<&Path>,
        client_instance_id: String,
        after_epoch: Option<String>,
        after_seq: Option<u64>,
    ) -> Result<Self, ClientError> {
        let discovery = match read_discovery() {
            Ok(value) if endpoint_reachable(&value).await => value,
            _ => {
                launch_daemon(daemon_path).map_err(|e| client_error("daemon_launch_failed", e))?;
                wait_for_discovery().await?
            }
        };
        Self::connect_with_state(
            client_kind,
            discovery,
            client_instance_id,
            after_epoch,
            after_seq,
        )
        .await
    }

    pub async fn connect(
        client_kind: &str,
        discovery: DaemonDiscovery,
    ) -> Result<Self, ClientError> {
        Self::connect_with_id(client_kind, discovery, random_hex()).await
    }

    pub async fn connect_with_id(
        client_kind: &str,
        discovery: DaemonDiscovery,
        client_instance_id: String,
    ) -> Result<Self, ClientError> {
        Self::connect_with_state(client_kind, discovery, client_instance_id, None, None).await
    }

    pub async fn connect_with_state(
        client_kind: &str,
        discovery: DaemonDiscovery,
        client_instance_id: String,
        after_epoch: Option<String>,
        after_seq: Option<u64>,
    ) -> Result<Self, ClientError> {
        if discovery.protocol_version != CONTROL_PROTOCOL_VERSION {
            return Err(ClientError {
                code: "protocol_version_mismatch".into(),
                message: format!("daemon protocol is {}", discovery.protocol_version),
            });
        }
        let mut request = discovery
            .endpoint
            .clone()
            .into_client_request()
            .map_err(|e| client_error("invalid_endpoint", e))?;
        request.headers_mut().insert(
            "authorization",
            HeaderValue::from_str(&format!("Bearer {}", discovery.token))
                .map_err(|e| client_error("invalid_token", e))?,
        );
        let (mut socket, _) = tokio_tungstenite::connect_async(request)
            .await
            .map_err(|e| client_error("connect_failed", e))?;
        let hello = ControlClientMessage::ClientHello {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            client_version: env!("CARGO_PKG_VERSION").into(),
            client_kind: client_kind.into(),
            client_instance_id: client_instance_id.clone(),
            after_epoch,
            after_seq,
        };
        socket
            .send(Message::Text(
                serde_json::to_string(&hello)
                    .map_err(|e| client_error("serialize", e))?
                    .into(),
            ))
            .await
            .map_err(|e| client_error("send_failed", e))?;
        let first = tokio::time::timeout(Duration::from_secs(5), socket.next())
            .await
            .map_err(|_| ClientError {
                code: "hello_timeout".into(),
                message: "daemon did not complete the control handshake".into(),
            })?
            .ok_or_else(|| ClientError {
                code: "disconnected".into(),
                message: "daemon closed during the control handshake".into(),
            })?
            .map_err(|e| client_error("handshake_failed", e))?;
        let hello: ControlServerMessage = serde_json::from_str(
            first
                .to_text()
                .map_err(|e| client_error("handshake_failed", e))?,
        )
        .map_err(|e| client_error("handshake_failed", e))?;
        match hello {
            ControlServerMessage::ServerHello {
                protocol_version, ..
            } if protocol_version == CONTROL_PROTOCOL_VERSION => {}
            ControlServerMessage::ServerHello {
                protocol_version, ..
            } => {
                return Err(ClientError {
                    code: "protocol_version_mismatch".into(),
                    message: format!("daemon selected protocol {protocol_version}"),
                });
            }
            ControlServerMessage::Error { code, message, .. } => {
                return Err(ClientError { code, message });
            }
            _ => {
                return Err(ClientError {
                    code: "invalid_server_hello".into(),
                    message: "daemon did not send server_hello first".into(),
                });
            }
        }
        let (mut sink, mut source) = socket.split();
        let (outgoing_tx, mut outgoing_rx) = mpsc::channel::<ControlClientMessage>(256);
        let (events, _) = broadcast::channel(4096);
        // Keep a receiver alive before the reader task starts. The daemon sends
        // Snapshot immediately after ServerHello, and dropping that first
        // message would make cold clients reconstruct an incomplete session.
        let initial_receiver = Arc::new(std::sync::Mutex::new(Some(events.subscribe())));
        let pending: Pending = Arc::new(Mutex::new(HashMap::new()));
        let connected = Arc::new(AtomicBool::new(true));
        let writer_connected = connected.clone();
        let writer_events = events.clone();
        tokio::spawn(async move {
            let mut heartbeat = tokio::time::interval(Duration::from_secs(5));
            let mut nonce = 0_u64;
            loop {
                tokio::select! {
                    message=outgoing_rx.recv()=>{let Some(message)=message else{break};let Ok(json)=serde_json::to_string(&message)else{continue};if sink.send(Message::Text(json.into())).await.is_err(){break;}}
                    _=heartbeat.tick()=>{nonce=nonce.saturating_add(1);let message=ControlClientMessage::Heartbeat{nonce};let Ok(json)=serde_json::to_string(&message)else{continue};if sink.send(Message::Text(json.into())).await.is_err(){break;}}
                }
            }
            if writer_connected.swap(false, Ordering::AcqRel) {
                let _ = writer_events.send(ControlServerMessage::Error {
                    request_id: None,
                    code: "disconnected".into(),
                    message: "daemon connection closed".into(),
                });
            }
        });
        let reader_events = events.clone();
        let reader_pending = pending.clone();
        let reader_connected = connected.clone();
        tokio::spawn(async move {
            while let Some(Ok(frame)) = source.next().await {
                let Ok(text) = frame.to_text() else { continue };
                let Ok(message) = serde_json::from_str::<ControlServerMessage>(text) else {
                    continue;
                };
                match &message {
                    ControlServerMessage::Response { request_id, result } => {
                        if let Some(tx) = reader_pending.lock().await.remove(request_id) {
                            let _ = tx.send(Ok(result.clone()));
                        }
                    }
                    ControlServerMessage::Error {
                        request_id: Some(request_id),
                        code,
                        message: detail,
                    } => {
                        if let Some(tx) = reader_pending.lock().await.remove(request_id) {
                            let _ = tx.send(Err(ClientError {
                                code: code.clone(),
                                message: detail.clone(),
                            }));
                        }
                    }
                    ControlServerMessage::LeaseDenied {
                        request_id,
                        seed,
                        owner_kind,
                        retry_after_ms,
                    } => {
                        if let Some(tx) = reader_pending.lock().await.remove(request_id) {
                            let _=tx.send(Err(ClientError{code:"session_busy".into(),message:format!("session {seed} is used by {owner_kind}; retry in {retry_after_ms}ms")}));
                        }
                    }
                    _ => {}
                }
                let _ = reader_events.send(message);
            }
            let mut pending = reader_pending.lock().await;
            for (_, tx) in pending.drain() {
                let _ = tx.send(Err(ClientError {
                    code: "disconnected".into(),
                    message: "daemon connection closed".into(),
                }));
            }
            if reader_connected.swap(false, Ordering::AcqRel) {
                let _ = reader_events.send(ControlServerMessage::Error {
                    request_id: None,
                    code: "disconnected".into(),
                    message: "daemon connection closed".into(),
                });
            }
        });
        Ok(Self {
            outgoing: outgoing_tx,
            events,
            pending,
            next_id: Arc::new(AtomicU64::new(1)),
            initial_receiver,
            connected,
            connection_marker: Arc::new(()),
            client_instance_id: Arc::from(client_instance_id),
        })
    }

    pub fn is_connected(&self) -> bool {
        self.connected.load(Ordering::Acquire)
    }

    pub fn same_connection(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.connection_marker, &other.connection_marker)
    }

    pub fn subscribe(&self) -> broadcast::Receiver<ControlServerMessage> {
        self.initial_receiver
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .take()
            .unwrap_or_else(|| self.events.subscribe())
    }
    pub async fn request(
        &self,
        method: impl Into<String>,
        params: Value,
    ) -> Result<Value, ClientError> {
        let request_id = self.request_id();
        self.round_trip(
            request_id.clone(),
            ControlClientMessage::Request {
                request_id,
                method: method.into(),
                params,
            },
        )
        .await
    }
    pub async fn attach(&self, seed: impl Into<String>) -> Result<Value, ClientError> {
        let request_id = self.request_id();
        self.round_trip(
            request_id.clone(),
            ControlClientMessage::SessionAttach {
                request_id,
                seed: seed.into(),
            },
        )
        .await
    }
    pub async fn detach(&self, seed: impl Into<String>) -> Result<Value, ClientError> {
        let request_id = self.request_id();
        self.round_trip(
            request_id.clone(),
            ControlClientMessage::SessionDetach {
                request_id,
                seed: seed.into(),
            },
        )
        .await
    }

    async fn round_trip(
        &self,
        request_id: String,
        message: ControlClientMessage,
    ) -> Result<Value, ClientError> {
        if !self.is_connected() {
            return Err(ClientError {
                code: "disconnected".into(),
                message: "daemon connection closed".into(),
            });
        }
        let (tx, rx) = oneshot::channel();
        self.pending.lock().await.insert(request_id.clone(), tx);
        if self.outgoing.send(message).await.is_err() {
            self.pending.lock().await.remove(&request_id);
            return Err(ClientError {
                code: "disconnected".into(),
                message: "daemon writer stopped".into(),
            });
        }
        tokio::time::timeout(Duration::from_secs(30), rx)
            .await
            .map_err(|_| ClientError {
                code: "timeout".into(),
                message: "daemon request timed out".into(),
            })?
            .map_err(|_| ClientError {
                code: "disconnected".into(),
                message: "request channel closed".into(),
            })?
    }
    fn request_id(&self) -> String {
        format!(
            "{}-{}",
            &self.client_instance_id[..8],
            self.next_id.fetch_add(1, Ordering::Relaxed)
        )
    }
}

fn read_discovery() -> Result<DaemonDiscovery, std::io::Error> {
    let content = std::fs::read_to_string(deepx_types::platform::daemon_discovery_path())?;
    serde_json::from_str(&content).map_err(std::io::Error::other)
}
async fn endpoint_reachable(discovery: &DaemonDiscovery) -> bool {
    if !deepx_types::platform::process_is_running(discovery.pid) {
        return false;
    }
    let address = discovery
        .endpoint
        .trim_start_matches("ws://")
        .split('/')
        .next()
        .unwrap_or_default();
    tokio::net::TcpStream::connect(address).await.is_ok()
}
async fn wait_for_discovery() -> Result<DaemonDiscovery, ClientError> {
    for _ in 0..50 {
        if let Ok(value) = read_discovery() {
            if endpoint_reachable(&value).await {
                return Ok(value);
            }
        }
        tokio::time::sleep(Duration::from_millis(100)).await;
    }
    Err(ClientError {
        code: "daemon_start_timeout".into(),
        message: "daemon did not publish discovery in 5 seconds".into(),
    })
}
fn launch_daemon(explicit: Option<&Path>) -> Result<(), std::io::Error> {
    let path = explicit
        .map(Path::to_path_buf)
        .unwrap_or_else(default_daemon_path);
    let mut command = std::process::Command::new(path);
    command
        .arg("run")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x00000008 | 0x08000000);
    }
    command.spawn().map(|_| ())
}
fn default_daemon_path() -> PathBuf {
    let mut path = std::env::current_exe().unwrap_or_else(|_| PathBuf::from("deepx-daemon"));
    path.set_file_name(if cfg!(windows) {
        "deepx-daemon.exe"
    } else {
        "deepx-daemon"
    });
    path
}
fn random_hex() -> String {
    rand::random::<[u8; 16]>()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
fn client_error(code: &'static str, error: impl std::fmt::Display) -> ClientError {
    ClientError {
        code: code.into(),
        message: error.to_string(),
    }
}
