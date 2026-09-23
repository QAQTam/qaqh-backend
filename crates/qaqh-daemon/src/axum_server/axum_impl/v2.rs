//! Ringing v2 HTTP/SSE endpoints.
//!
//! The first cut exposes the canonical read path:
//! `open -> bootstrap -> since_cursor subscribe -> replay -> live`.

use qaqh_ringing::{
    RINGING_SCHEMA, RINGING_V2_VERSION, RINGING_VERSION, RingingCommandEnvelope,
    RingingV2Bootstrap, RingingV2Capabilities, RingingV2ChannelSnapshot, RingingV2CommandEnvelope,
    RingingV2LeaseRenewResponse, RingingV2OpenRequest, RingingV2OpenResponse,
};
use qaqh_runtime::ringing::V2StreamItem;
use qaqh_session::projection::{ControlSnapshot, ControlToolState, ConversationSnapshot};
use serde::Serialize;

use super::*;

#[derive(Debug, Clone, Serialize)]
struct V2ControlState {
    #[serde(flatten)]
    snapshot: ControlSnapshot,
    #[serde(skip_serializing_if = "Option::is_none")]
    driver: Option<qaqh_ringing::RingingV2DriverState>,
}

#[derive(Debug, Clone, Serialize)]
struct V2ToolState {
    tools: Vec<ControlToolState>,
}

#[derive(Deserialize)]
pub struct V2EventsQuery {
    pub since_cursor: Option<String>,
}

pub(crate) async fn handle_open_v2(
    State(state): State<AppState>,
    headers: HeaderMap,
    body: Bytes,
) -> Response {
    if !is_authorized(&headers, &state.token) {
        return unauthorized();
    }
    let request: RingingV2OpenRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(error) => {
            return api_error_response(
                StatusCode::BAD_REQUEST,
                "invalid_body",
                &format!("invalid v2 open request: {error}"),
            );
        }
    };
    if request.schema != RINGING_SCHEMA || request.version != RINGING_V2_VERSION {
        return api_error_response(
            StatusCode::UPGRADE_REQUIRED,
            "unsupported_version",
            "unsupported Ringing v2 schema/version",
        );
    }
    let client_session_id = random_hex();
    state
        .leases
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .open(client_session_id.clone(), request.client_instance_id);
    let response = RingingV2OpenResponse {
        schema: RINGING_SCHEMA.into(),
        version: RINGING_V2_VERSION,
        accepted: true,
        client_session_id,
        server_epoch: state.epoch.clone(),
        lease_ttl_ms: lease_ttl_ms(),
        renew_interval_ms: RENEW_INTERVAL_MS,
        capabilities: RingingV2Capabilities {
            subscribe: true,
            interact: true,
            drive: true,
            timeline: true,
            service: true,
            content: true,
        },
    };
    json_response(StatusCode::OK, &response)
}

pub(crate) async fn handle_renew_v2(State(state): State<AppState>, headers: HeaderMap) -> Response {
    if !is_authorized(&headers, &state.token) {
        return unauthorized();
    }
    let Some(session_id) = get_session_id(&headers) else {
        return lease_required_v2();
    };
    let ok = state
        .leases
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .renew(&session_id);
    if !ok {
        return api_error_response(
            StatusCode::UNAUTHORIZED,
            "lease_expired",
            "v2 lease expired or unknown",
        );
    }
    json_response(
        StatusCode::OK,
        &RingingV2LeaseRenewResponse {
            ok: true,
            lease_ttl_ms: lease_ttl_ms(),
            renew_interval_ms: RENEW_INTERVAL_MS,
        },
    )
}

pub(crate) async fn handle_bootstrap_v2(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(seed): Path<String>,
) -> Response {
    if !is_authorized(&headers, &state.token) {
        return unauthorized();
    }
    if require_v2_lease(&state, &headers).is_none() {
        return lease_required_v2();
    }
    if seed.trim().is_empty() {
        return api_error_response(StatusCode::BAD_REQUEST, "missing_seed", "missing seed");
    }
    let session_dir = qaqh_types::platform::sessions_dir().join(&seed);
    let bootstrap = match state.v2_hub.bootstrap(&session_dir, &seed) {
        Ok(bootstrap) => bootstrap,
        Err(error) => {
            return v2_hub_error_response(error);
        }
    };
    let control = V2ControlState {
        snapshot: bootstrap.projections.control.clone(),
        driver: None,
    };
    let conversation = bootstrap.projections.conversation.clone();
    let tool = V2ToolState {
        tools: bootstrap.projections.control.tools.clone(),
    };
    let tool_revision = bootstrap.projections.control.revision;
    let response: RingingV2Bootstrap<V2ControlState, ConversationSnapshot, V2ToolState> =
        RingingV2Bootstrap {
            schema: RINGING_SCHEMA.into(),
            version: RINGING_V2_VERSION,
            server_epoch: bootstrap.server_epoch,
            seed: bootstrap.seed,
            snapshot_cursor: bootstrap.snapshot_cursor,
            control: RingingV2ChannelSnapshot {
                channel: RingingChannel::Control,
                state_revision: control.snapshot.revision,
                snapshot_version: 1,
                state: control,
            },
            conversation: RingingV2ChannelSnapshot {
                channel: RingingChannel::Conversation,
                state_revision: conversation.revision,
                snapshot_version: 1,
                state: conversation,
            },
            tool: RingingV2ChannelSnapshot {
                channel: RingingChannel::Tool,
                state_revision: tool_revision,
                snapshot_version: 1,
                state: tool,
            },
        };
    json_response(StatusCode::OK, &response)
}

pub(crate) async fn handle_events_v2(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path((seed, channel)): Path<(String, String)>,
    Query(query): Query<V2EventsQuery>,
) -> Response {
    if !is_authorized(&headers, &state.token) {
        return unauthorized();
    }
    if require_v2_lease(&state, &headers).is_none() {
        return lease_required_v2();
    }
    let Some(channel) = parse_channel(&channel) else {
        return api_error_response(
            StatusCode::BAD_REQUEST,
            "invalid_channel",
            "invalid channel",
        );
    };
    if seed.trim().is_empty() {
        return api_error_response(StatusCode::BAD_REQUEST, "missing_seed", "missing seed");
    }
    let cursor = query
        .since_cursor
        .as_deref()
        .map(qaqh_ringing::CursorToken::from_opaque);
    let session_dir = qaqh_types::platform::sessions_dir().join(&seed);
    let mut subscription =
        match state
            .v2_hub
            .subscribe(&session_dir, &seed, channel, cursor.as_ref())
        {
            Ok(subscription) => subscription,
            Err(error) => return v2_hub_error_response(error),
        };

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, Infallible>>(64);
    tokio::spawn(async move {
        loop {
            match subscription.next().await {
                V2StreamItem::Event(event) => {
                    let data =
                        serde_json::to_string(event.as_ref()).unwrap_or_else(|_| "{}".into());
                    let frame = Event::default()
                        .id(format!("v2:{}:{}", event.server_epoch, event.event_id))
                        .event("ringing.event")
                        .data(data);
                    if tx.send(Ok(frame)).await.is_err() {
                        break;
                    }
                }
                V2StreamItem::Reset(reset) => {
                    let data = serde_json::to_string(&reset).unwrap_or_else(|_| "{}".into());
                    let frame = Event::default().event("ringing.reset_required").data(data);
                    let _ = tx.send(Ok(frame)).await;
                    break;
                }
            }
        }
    });

    Sse::new(ReceiverStream::new(rx))
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response()
}

pub(crate) async fn handle_command_v2(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    if !is_authorized(&headers, &state.token) {
        return unauthorized();
    }
    let envelope: RingingV2CommandEnvelope = match serde_json::from_slice(&body) {
        Ok(envelope) => envelope,
        Err(error) => {
            return api_error_response(
                StatusCode::BAD_REQUEST,
                "invalid_body",
                &format!("invalid v2 command envelope: {error}"),
            );
        }
    };
    if let Err(code) = envelope.validate() {
        return api_error_response(StatusCode::BAD_REQUEST, code, "invalid v2 command envelope");
    }
    if envelope.channel.as_str() != id {
        return api_error_response(
            StatusCode::BAD_REQUEST,
            "channel_mismatch",
            "path channel does not match v2 command envelope",
        );
    }
    let v1 = RingingCommandEnvelope {
        schema: RINGING_SCHEMA.into(),
        version: RINGING_VERSION,
        channel: envelope.channel,
        command_id: envelope.command_id,
        client_instance_id: envelope.client_instance_id,
        client_session_id: envelope.client_session_id,
        seed: envelope.seed,
        expected_revision: envelope.expected_revision,
        command: envelope.command,
    };
    let body = match serde_json::to_vec(&v1) {
        Ok(body) => body,
        Err(error) => {
            return api_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "encode_error",
                &error.to_string(),
            );
        }
    };
    handle_command(State(state), headers, Path(id), Bytes::from(body)).await
}

pub(crate) async fn handle_command_status_v2(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(command_id): Path<String>,
) -> Response {
    handle_command_status(State(state), headers, Path(command_id)).await
}

fn require_v2_lease(state: &AppState, headers: &HeaderMap) -> Option<String> {
    let session_id = get_session_id(headers)?;
    let leases = state
        .leases
        .lock()
        .unwrap_or_else(|error| error.into_inner());
    leases.is_active_session(&session_id).then_some(session_id)
}

fn lease_required_v2() -> Response {
    api_error_response(
        StatusCode::UNAUTHORIZED,
        "lease_required",
        "open a Ringing v2 client session first",
    )
}

fn v2_hub_error_response(error: qaqh_runtime::ringing::V2HubError) -> Response {
    match error {
        qaqh_runtime::ringing::V2HubError::SessionMissing(_) => api_error_response(
            StatusCode::NOT_FOUND,
            "session_not_found",
            "canonical session log not found",
        ),
        qaqh_runtime::ringing::V2HubError::SnapshotMissing(_) => api_error_response(
            StatusCode::CONFLICT,
            "snapshot_missing",
            "canonical snapshot cursor is unavailable",
        ),
        qaqh_runtime::ringing::V2HubError::InvalidCursor(message) => {
            api_error_response(StatusCode::BAD_REQUEST, "cursor_expired", &message)
        }
        qaqh_runtime::ringing::V2HubError::Canonical(message) => api_error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "canonical_error",
            &message,
        ),
        qaqh_runtime::ringing::V2HubError::InvalidEvent(message) => api_error_response(
            StatusCode::INTERNAL_SERVER_ERROR,
            "invalid_projection_event",
            &message,
        ),
    }
}

fn api_error_response(status: StatusCode, code: &str, message: &str) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::to_vec(&serde_json::json!({
            "code": code,
            "message": message,
        }))
        .unwrap_or_default(),
    )
        .into_response()
}

fn json_response<T: Serialize>(status: StatusCode, value: &T) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::to_vec(value).unwrap_or_default(),
    )
        .into_response()
}
