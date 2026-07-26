use std::io;
use std::sync::{Arc, Mutex};

use futures_util::{SinkExt, StreamExt};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::{Semaphore, broadcast, mpsc, watch};
use tokio::time::{Duration, Instant, MissedTickBehavior};
use tokio_tungstenite::tungstenite::Message;
use tokio_tungstenite::tungstenite::handshake::server::{ErrorResponse, Request, Response};
use tokio_tungstenite::tungstenite::http::StatusCode;
use tokio_tungstenite::tungstenite::protocol::WebSocketConfig;

use deepx_proto::{
    COMPANION_PROTOCOL_VERSION, CompanionClientMessage, CompanionEvent, CompanionServerMessage,
};

use crate::CompanionState;

const HEARTBEAT_INTERVAL_MS: u64 = 15_000;
const MAX_FRAME_BYTES: usize = 1024 * 1024;
const MAX_CONNECTIONS: usize = 8;
const MAX_COMMANDS_PER_SECOND: u32 = 64;

fn handshake_timeout() -> Duration {
    if cfg!(test) {
        Duration::from_millis(100)
    } else {
        Duration::from_secs(5)
    }
}

pub struct CompanionHub;

#[derive(Clone)]
pub struct CompanionHubHandle {
    endpoint: String,
    token: Arc<str>,
    server_epoch: Arc<str>,
    state: Arc<Mutex<CompanionState>>,
    events: broadcast::Sender<CompanionServerMessage>,
    shutdown: watch::Sender<bool>,
}

impl CompanionHub {
    pub async fn bind_loopback(
        token: impl Into<String>,
        server_epoch: impl Into<String>,
    ) -> io::Result<(CompanionHubHandle, mpsc::Receiver<CompanionClientMessage>)> {
        let listener = TcpListener::bind("127.0.0.1:0").await?;
        let address = listener.local_addr()?;
        let token: Arc<str> = Arc::from(token.into());
        let server_epoch: Arc<str> = Arc::from(server_epoch.into());
        let state = Arc::new(Mutex::new(CompanionState::new(server_epoch.as_ref())));
        let (events, _) = broadcast::channel(256);
        let (commands_tx, commands_rx) = mpsc::channel(128);
        let (shutdown, shutdown_rx) = watch::channel(false);
        let handle = CompanionHubHandle {
            endpoint: format!("ws://{address}/companion/v1"),
            token: token.clone(),
            server_epoch: server_epoch.clone(),
            state: state.clone(),
            events: events.clone(),
            shutdown: shutdown.clone(),
        };
        tokio::spawn(run_accept_loop(
            listener,
            token,
            server_epoch,
            state,
            events,
            commands_tx,
            shutdown_rx,
        ));
        Ok((handle, commands_rx))
    }
}

impl CompanionHubHandle {
    pub fn endpoint(&self) -> &str {
        &self.endpoint
    }

    pub fn token(&self) -> &str {
        &self.token
    }

    #[cfg(test)]
    fn test_token(&self) -> &str {
        &self.token
    }

    pub fn publish(&self, event: CompanionEvent) {
        let published = self
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .publish(event);
        let _ = self.events.send(CompanionServerMessage::Event {
            server_epoch: published.server_epoch,
            seq: published.seq,
            event: published.event,
        });
    }

    pub fn command_result(
        &self,
        command_id: impl Into<String>,
        status: deepx_proto::CompanionCommandStatus,
        message: Option<String>,
    ) {
        let seq = self
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .next_sequence();
        let _ = self.events.send(CompanionServerMessage::CommandResult {
            server_epoch: self.server_epoch.to_string(),
            seq,
            command_id: command_id.into(),
            status,
            message,
        });
    }

    pub async fn shutdown(&self) {
        let seq = self
            .state
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .snapshot()
            .snapshot_seq
            .saturating_add(1);
        let _ = self.events.send(CompanionServerMessage::Shutdown {
            server_epoch: self.server_epoch.to_string(),
            seq,
            reason: "deepx_shutdown".into(),
        });
        let _ = self.shutdown.send(true);
        tokio::task::yield_now().await;
    }
}

async fn run_accept_loop(
    listener: TcpListener,
    token: Arc<str>,
    server_epoch: Arc<str>,
    state: Arc<Mutex<CompanionState>>,
    events: broadcast::Sender<CompanionServerMessage>,
    commands: mpsc::Sender<CompanionClientMessage>,
    mut shutdown: watch::Receiver<bool>,
) {
    let connections = Arc::new(Semaphore::new(MAX_CONNECTIONS));
    loop {
        tokio::select! {
            accepted = listener.accept() => {
                let Ok((stream, _)) = accepted else { break };
                let Ok(permit) = connections.clone().try_acquire_owned() else { continue };
                let token = token.clone();
                let server_epoch = server_epoch.clone();
                let state = state.clone();
                let events = events.clone();
                let commands = commands.clone();
                let connection_shutdown = shutdown.clone();
                tokio::spawn(async move {
                    let _permit = permit;
                    handle_connection(
                        stream,
                        token.clone(),
                        server_epoch.clone(),
                        state.clone(),
                        events.clone(),
                        commands.clone(),
                        connection_shutdown,
                    ).await;
                });
            }
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() { break; }
            }
        }
    }
}

async fn handle_connection(
    stream: TcpStream,
    token: Arc<str>,
    server_epoch: Arc<str>,
    state: Arc<Mutex<CompanionState>>,
    events: broadcast::Sender<CompanionServerMessage>,
    commands: mpsc::Sender<CompanionClientMessage>,
    mut shutdown: watch::Receiver<bool>,
) {
    let expected = format!("Bearer {token}");
    let socket = tokio::time::timeout(
        handshake_timeout(),
        tokio_tungstenite::accept_hdr_async_with_config(
            stream,
            move |request: &Request, response: Response| {
                let authorized = request
                    .headers()
                    .get("authorization")
                    .and_then(|value| value.to_str().ok())
                    .is_some_and(|value| value == expected);
                let expected_path = request.uri().path() == "/companion/v1";
                if !expected_path {
                    let mut error = ErrorResponse::new(Some("not found".into()));
                    *error.status_mut() = StatusCode::NOT_FOUND;
                    Err(error)
                } else if authorized {
                    Ok(response)
                } else {
                    let mut error = ErrorResponse::new(Some("unauthorized".into()));
                    *error.status_mut() = StatusCode::UNAUTHORIZED;
                    Err(error)
                }
            },
            Some(
                WebSocketConfig::default()
                    .max_frame_size(Some(MAX_FRAME_BYTES))
                    .max_message_size(Some(MAX_FRAME_BYTES)),
            ),
        ),
    )
    .await;
    let Ok(Ok(mut socket)) = socket else { return };

    let Ok(Some(Ok(first))) = tokio::time::timeout(handshake_timeout(), socket.next()).await else {
        return;
    };
    let Ok(text) = first.to_text() else { return };
    let Ok(CompanionClientMessage::ClientHello {
        protocol_version, ..
    }) = serde_json::from_str::<CompanionClientMessage>(text)
    else {
        return;
    };
    if protocol_version != COMPANION_PROTOCOL_VERSION {
        return;
    }

    let mut event_rx = events.subscribe();
    if send_json(
        &mut socket,
        &CompanionServerMessage::ServerHello {
            protocol_version: COMPANION_PROTOCOL_VERSION,
            server_epoch: server_epoch.to_string(),
            heartbeat_interval_ms: HEARTBEAT_INTERVAL_MS,
            max_frame_bytes: MAX_FRAME_BYTES,
        },
    )
    .await
    .is_err()
    {
        return;
    }
    let snapshot = state
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .snapshot();
    if send_json(
        &mut socket,
        &CompanionServerMessage::Snapshot {
            server_epoch: server_epoch.to_string(),
            seq: snapshot.snapshot_seq,
            snapshot,
        },
    )
    .await
    .is_err()
    {
        return;
    }

    let heartbeat_period = Duration::from_millis(HEARTBEAT_INTERVAL_MS);
    let mut heartbeat =
        tokio::time::interval_at(Instant::now() + heartbeat_period, heartbeat_period);
    heartbeat.set_missed_tick_behavior(MissedTickBehavior::Skip);
    let mut command_window = Instant::now();
    let mut command_count = 0_u32;

    loop {
        tokio::select! {
            incoming = socket.next() => {
                let Some(Ok(frame)) = incoming else { break };
                let Ok(text) = frame.to_text() else { continue };
                let Ok(message) = serde_json::from_str::<CompanionClientMessage>(text) else { continue };
                match message {
                    CompanionClientMessage::ClientHello { .. } => break,
                    CompanionClientMessage::Heartbeat { .. } => {}
                    command => {
                        if command_window.elapsed() >= Duration::from_secs(1) {
                            command_window = Instant::now();
                            command_count = 0;
                        }
                        command_count = command_count.saturating_add(1);
                        if command_count > MAX_COMMANDS_PER_SECOND { break; }
                        if commands.send(command).await.is_err() { break; }
                    }
                }
            }
            event = event_rx.recv() => {
                match event {
                    Ok(event) => if send_json(&mut socket, &event).await.is_err() { break; },
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        let snapshot = state.lock().unwrap_or_else(|error| error.into_inner()).snapshot();
                        let message = CompanionServerMessage::Snapshot {
                            server_epoch: server_epoch.to_string(),
                            seq: snapshot.snapshot_seq,
                            snapshot,
                        };
                        if send_json(&mut socket, &message).await.is_err() { break; }
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
            _ = heartbeat.tick() => {
                let seq = state.lock().unwrap_or_else(|error| error.into_inner()).next_sequence();
                let response = CompanionServerMessage::Heartbeat {
                    server_epoch: server_epoch.to_string(),
                    seq,
                    nonce: seq,
                };
                if send_json(&mut socket, &response).await.is_err() { break; }
            }
            changed = shutdown.changed() => {
                if changed.is_err() || *shutdown.borrow() { break; }
            }
        }
    }
}

async fn send_json<S>(
    socket: &mut tokio_tungstenite::WebSocketStream<S>,
    message: &CompanionServerMessage,
) -> Result<(), tokio_tungstenite::tungstenite::Error>
where
    S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
{
    let json = serde_json::to_string(message)
        .map_err(|error| tokio_tungstenite::tungstenite::Error::Io(io::Error::other(error)))?;
    if json.len() > MAX_FRAME_BYTES {
        return Err(tokio_tungstenite::tungstenite::Error::Io(io::Error::new(
            io::ErrorKind::InvalidData,
            "companion frame exceeds 1 MiB",
        )));
    }
    socket.send(Message::Text(json.into())).await
}

#[cfg(test)]
mod tests {
    use futures_util::{SinkExt, StreamExt};
    use tokio::io::AsyncReadExt;
    use tokio::time::Duration;
    use tokio_tungstenite::tungstenite::Message;
    use tokio_tungstenite::tungstenite::client::IntoClientRequest;
    use tokio_tungstenite::tungstenite::http::HeaderValue;

    use deepx_proto::{
        COMPANION_PROTOCOL_VERSION, CompanionClientMessage, CompanionEvent, CompanionServerMessage,
        CompanionSession, SessionActivityState,
    };

    use super::CompanionHub;

    async fn authorized_socket(
        handle: &super::CompanionHubHandle,
    ) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>
    {
        let mut request = handle.endpoint().into_client_request().expect("request");
        request.headers_mut().insert(
            "authorization",
            HeaderValue::from_str(&format!("Bearer {}", handle.test_token())).expect("header"),
        );
        let (mut socket, _) = tokio_tungstenite::connect_async(request)
            .await
            .expect("connect");
        let hello = CompanionClientMessage::ClientHello {
            protocol_version: COMPANION_PROTOCOL_VERSION,
            client_version: "test".into(),
            capabilities: vec![],
        };
        socket
            .send(Message::Text(serde_json::to_string(&hello).unwrap().into()))
            .await
            .expect("send hello");
        socket
    }

    async fn authorized_socket_without_hello(
        handle: &super::CompanionHubHandle,
    ) -> tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>
    {
        let mut request = handle.endpoint().into_client_request().expect("request");
        request.headers_mut().insert(
            "authorization",
            HeaderValue::from_str(&format!("Bearer {}", handle.test_token())).expect("header"),
        );
        tokio_tungstenite::connect_async(request)
            .await
            .expect("connect")
            .0
    }

    async fn read_server_message<S>(
        socket: &mut tokio_tungstenite::WebSocketStream<S>,
    ) -> CompanionServerMessage
    where
        S: tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin,
    {
        let frame = socket.next().await.expect("message").expect("valid frame");
        serde_json::from_str(frame.to_text().expect("text")).expect("server message")
    }

    #[tokio::test]
    async fn rejects_connections_without_bearer_token() {
        let (handle, _commands) = CompanionHub::bind_loopback("secret", "epoch-1")
            .await
            .expect("bind");
        assert!(
            tokio_tungstenite::connect_async(handle.endpoint())
                .await
                .is_err()
        );
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn closes_clients_that_never_send_a_websocket_handshake() {
        let (handle, _commands) = CompanionHub::bind_loopback("secret", "epoch-1")
            .await
            .expect("bind");
        let address = handle
            .endpoint()
            .trim_start_matches("ws://")
            .split('/')
            .next()
            .expect("address");
        let mut stream = tokio::net::TcpStream::connect(address)
            .await
            .expect("connect");
        let mut byte = [0_u8; 1];
        let read = tokio::time::timeout(Duration::from_millis(500), stream.read(&mut byte)).await;
        assert_eq!(
            read.expect("server should close slow handshake").unwrap(),
            0
        );
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn closes_authenticated_clients_that_never_send_client_hello() {
        let (handle, _commands) = CompanionHub::bind_loopback("secret", "epoch-1")
            .await
            .expect("bind");
        let mut socket = authorized_socket_without_hello(&handle).await;
        assert!(
            tokio::time::timeout(Duration::from_millis(500), socket.next())
                .await
                .is_ok()
        );
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn disconnects_clients_that_exceed_the_command_rate_limit() {
        let (handle, _commands) = CompanionHub::bind_loopback("secret", "epoch-1")
            .await
            .expect("bind");
        let mut socket = authorized_socket(&handle).await;
        let _ = read_server_message(&mut socket).await;
        let _ = read_server_message(&mut socket).await;
        for index in 0..=super::MAX_COMMANDS_PER_SECOND {
            let command = CompanionClientMessage::FocusSession {
                command_id: format!("flood-{index}"),
                seed: "deadbeef".into(),
            };
            socket
                .send(Message::Text(
                    serde_json::to_string(&command).unwrap().into(),
                ))
                .await
                .expect("send command");
        }
        assert!(
            tokio::time::timeout(Duration::from_millis(500), socket.next())
                .await
                .is_ok()
        );
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn rejects_authenticated_connections_on_an_unknown_path() {
        let (handle, _commands) = CompanionHub::bind_loopback("secret", "epoch-1")
            .await
            .expect("bind");
        let mut request = handle
            .endpoint()
            .replace("/companion/v1", "/other")
            .into_client_request()
            .expect("request");
        request.headers_mut().insert(
            "authorization",
            HeaderValue::from_str(&format!("Bearer {}", handle.test_token())).expect("header"),
        );
        assert!(tokio_tungstenite::connect_async(request).await.is_err());
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn sends_hello_and_snapshot_before_incremental_events() {
        let (handle, _commands) = CompanionHub::bind_loopback("secret", "epoch-1")
            .await
            .expect("bind");
        let mut socket = authorized_socket(&handle).await;

        assert!(matches!(
            read_server_message(&mut socket).await,
            CompanionServerMessage::ServerHello {
                protocol_version: 1,
                ..
            }
        ));
        assert!(matches!(
            read_server_message(&mut socket).await,
            CompanionServerMessage::Snapshot { seq: 0, .. }
        ));

        handle.publish(CompanionEvent::SessionActivity {
            session: CompanionSession {
                seed: "deadbeef".into(),
                title: None,
                workspace: None,
                state: SessionActivityState::Working,
                visual_state: deepx_proto::CompanionVisualState::Working,
                turn_id: Some("turn-1".into()),
                session_seq: 1,
                updated_at: 10,
            },
        });
        assert!(matches!(
            read_server_message(&mut socket).await,
            CompanionServerMessage::Event { seq: 1, .. }
        ));
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn forwards_authenticated_client_commands_to_the_host() {
        let (handle, mut commands) = CompanionHub::bind_loopback("secret", "epoch-1")
            .await
            .expect("bind");
        let mut socket = authorized_socket(&handle).await;
        let _ = read_server_message(&mut socket).await;
        let _ = read_server_message(&mut socket).await;
        let command = CompanionClientMessage::FocusSession {
            command_id: "focus-1".into(),
            seed: "deadbeef".into(),
        };
        socket
            .send(Message::Text(
                serde_json::to_string(&command).unwrap().into(),
            ))
            .await
            .expect("send command");
        assert_eq!(commands.recv().await, Some(command));
        handle.shutdown().await;
    }

    #[tokio::test]
    async fn broadcasts_command_results_with_the_next_sequence() {
        let (handle, _commands) = CompanionHub::bind_loopback("secret", "epoch-1")
            .await
            .expect("bind");
        let mut socket = authorized_socket(&handle).await;
        let _ = read_server_message(&mut socket).await;
        let _ = read_server_message(&mut socket).await;
        handle.command_result(
            "command-1",
            deepx_proto::CompanionCommandStatus::Accepted,
            None,
        );
        assert!(matches!(
            read_server_message(&mut socket).await,
            CompanionServerMessage::CommandResult { seq: 1, command_id, .. } if command_id == "command-1"
        ));
        handle.shutdown().await;
    }
}
