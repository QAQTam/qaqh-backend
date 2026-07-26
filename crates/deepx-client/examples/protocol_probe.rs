use std::path::PathBuf;
use std::time::Duration;

use deepx_proto::{
    CONTROL_PROTOCOL_VERSION, ControlClientMessage, ControlServerMessage, DaemonDiscovery,
};
use futures_util::{SinkExt, StreamExt};
use tokio_tungstenite::tungstenite::client::IntoClientRequest;
use tokio_tungstenite::tungstenite::http::HeaderValue;
use tokio_tungstenite::tungstenite::protocol::Message;

#[tokio::main]
async fn main() -> Result<(), Box<dyn std::error::Error>> {
    let discovery_path = std::env::args()
        .nth(1)
        .map(PathBuf::from)
        .unwrap_or_else(deepx_types::platform::daemon_discovery_path);
    let discovery: DaemonDiscovery =
        serde_json::from_str(&std::fs::read_to_string(discovery_path)?)?;

    // The WebSocket upgrade itself must reject missing bearer credentials.
    assert!(
        tokio_tungstenite::connect_async(discovery.endpoint.clone())
            .await
            .is_err()
    );

    let mut wrong_version = connect_authenticated(&discovery).await?;
    wrong_version
        .send(json_message(&ControlClientMessage::ClientHello {
            protocol_version: CONTROL_PROTOCOL_VERSION + 1,
            client_version: "probe".into(),
            client_kind: "probe".into(),
            client_instance_id: "wrong-version".into(),
            after_epoch: None,
            after_seq: None,
        })?)
        .await?;
    let response = next_control(&mut wrong_version).await?;
    assert!(matches!(
        response,
        ControlServerMessage::Error { code, .. } if code == "protocol_version_mismatch"
    ));

    let mut oversized = connect_authenticated(&discovery).await?;
    oversized
        .send(json_message(&ControlClientMessage::ClientHello {
            protocol_version: CONTROL_PROTOCOL_VERSION,
            client_version: "probe".into(),
            client_kind: "probe".into(),
            client_instance_id: "oversized-message".into(),
            after_epoch: None,
            after_seq: None,
        })?)
        .await?;
    assert!(matches!(
        next_control(&mut oversized).await?,
        ControlServerMessage::ServerHello { .. }
    ));
    oversized
        .send(Message::Text("x".repeat(1024 * 1024 + 1).into()))
        .await?;
    let closed = tokio::time::timeout(Duration::from_secs(3), async {
        loop {
            match oversized.next().await {
                None | Some(Err(_)) | Some(Ok(Message::Close(_))) => break true,
                Some(Ok(_)) => continue,
            }
        }
    })
    .await
    .unwrap_or(false);
    assert!(closed, "daemon accepted an oversized control message");

    println!("authentication, version rejection, and message limit: ok");
    Ok(())
}

type Socket =
    tokio_tungstenite::WebSocketStream<tokio_tungstenite::MaybeTlsStream<tokio::net::TcpStream>>;

async fn connect_authenticated(
    discovery: &DaemonDiscovery,
) -> Result<Socket, Box<dyn std::error::Error>> {
    let mut request = discovery.endpoint.clone().into_client_request()?;
    request.headers_mut().insert(
        "authorization",
        HeaderValue::from_str(&format!("Bearer {}", discovery.token))?,
    );
    Ok(tokio_tungstenite::connect_async(request).await?.0)
}

fn json_message(message: &ControlClientMessage) -> Result<Message, serde_json::Error> {
    serde_json::to_string(message).map(|json| Message::Text(json.into()))
}

async fn next_control(
    socket: &mut Socket,
) -> Result<ControlServerMessage, Box<dyn std::error::Error>> {
    let frame = tokio::time::timeout(Duration::from_secs(3), socket.next())
        .await?
        .ok_or("connection closed")??;
    Ok(serde_json::from_str(frame.to_text()?)?)
}
