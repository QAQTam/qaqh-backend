//! Ringing v2 HTTP/SSE endpoints.
//!
//! The first cut exposes the canonical read path:
//! `open -> bootstrap -> since_cursor subscribe -> replay -> live`.

use qaqh_ringing::{
    RINGING_SCHEMA, RINGING_V2_VERSION, RingingCommandAckStatus, RingingV2AskOutcome,
    RingingV2Bootstrap, RingingV2Capabilities, RingingV2ChannelSnapshot, RingingV2CommandAck,
    RingingV2CommandEnvelope, RingingV2CommandResult, RingingV2DriverClaimResponse,
    RingingV2DriverReleaseResponse, RingingV2DriverState, RingingV2ExistingResult,
    RingingV2InteractionKind, RingingV2LeaseRenewResponse, RingingV2OpenRequest,
    RingingV2OpenResponse, RingingV2PendingInteraction,
};
use qaqh_runtime::ringing::V2StreamItem;
use qaqh_session::projection::{
    ControlInteractionState, ControlRoundState, ControlSubagentState, ControlToolState,
    ConversationSnapshot,
};
use qaqh_session::session_fact_v2::{
    ActivityState, ContentValue, InteractionDecision, InteractionKind, RecoveryOutcome, SessionId,
    ToolCallId, TurnId,
};
use serde::Serialize;

use super::*;

#[derive(Debug, Clone, Serialize)]
struct V2ControlState {
    session_id: Option<SessionId>,
    activity: ActivityState,
    current_turn_id: Option<TurnId>,
    current_call_id: Option<ToolCallId>,
    round: Option<ControlRoundState>,
    tools: Vec<ControlToolState>,
    subagents: Vec<ControlSubagentState>,
    last_recovery: Option<RecoveryOutcome>,
    revision: u64,
    last_fact_seq: u64,
    interactions: Vec<RingingV2PendingInteraction>,
    #[serde(skip_serializing_if = "Option::is_none")]
    driver: Option<RingingV2DriverState>,
}

#[derive(Debug, Clone, Serialize)]
struct V2ToolState {
    tools: Vec<ControlToolState>,
}

/// canonical `ContentValue` → wire 形态（#345）。
///
/// 两侧 serde 判别式一致（`kind` / `data`）；`Unavailable` 的 reason 结构按
/// 不透明 JSON 透传，不在 wire 层重复建模。
fn wire_content_value(
    value: &qaqh_session::session_fact_v2::ContentValue,
) -> RingingV2ContentValue {
    use qaqh_session::session_fact_v2::ContentValue;
    match value {
        ContentValue::Inline { text } => RingingV2ContentValue::Inline { text: text.clone() },
        ContentValue::Ref { content_ref } => RingingV2ContentValue::Ref {
            content_ref: content_ref.hash().as_str().to_string(),
        },
        ContentValue::Unavailable(reason) => RingingV2ContentValue::Unavailable(
            serde_json::to_value(reason).unwrap_or(serde_json::Value::Null),
        ),
    }
}

#[derive(Deserialize)]
pub struct V2EventsQuery {
    pub since_cursor: Option<String>,
}

pub(crate) async fn handle_open_v2(
    State(state): State<AppState>,
    Extension(identity): Extension<Identity>,
    body: Bytes,
) -> Response {
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
    // 身份由 token 反推：device 的 lease 以 `device_id` 派生身份登记，客户端自报的
    // `client_instance_id` 降级为纯诊断（不参与任何信任判定）。admin 维持旧行为。
    let instance_key = match &identity {
        Identity::Device { device_id, .. } => device_id.clone(),
        Identity::Admin => request.client_instance_id,
    };
    state
        .leases
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .open(client_session_id.clone(), instance_key);
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
            single_stream: true,
            pairing: true,
        },
    };
    json_response(StatusCode::OK, &response)
}

pub(crate) async fn handle_renew_v2(State(state): State<AppState>, headers: HeaderMap) -> Response {
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
    Extension(identity): Extension<Identity>,
    headers: HeaderMap,
    Path(session_id): Path<String>,
) -> Response {
    let caller = match require_lease_on_session(&state, &headers, &identity, &session_id) {
        Ok(caller) => caller,
        Err(response) => return response,
    };
    if session_id.trim().is_empty() {
        return api_error_response(
            StatusCode::BAD_REQUEST,
            "missing_session_id",
            "missing seed",
        );
    }
    let session_dir = qaqh_types::platform::sessions_dir().join(&session_id);
    let bootstrap = match state.v2_hub.bootstrap(&session_dir, &session_id) {
        Ok(bootstrap) => bootstrap,
        Err(error) => {
            return v2_hub_error_response(error);
        }
    };
    let control_snapshot = &bootstrap.projections.control;
    let interactions = control_snapshot
        .interactions
        .iter()
        .filter(|interaction| {
            interaction.resolution.is_none() && interaction.expired_reason.is_none()
        })
        .map(|interaction| RingingV2PendingInteraction {
            interaction_id: interaction.interaction_id.as_str().to_string(),
            call_id: interaction
                .call_id
                .as_ref()
                .map(|call_id| call_id.as_str().to_string())
                .unwrap_or_default(),
            turn_id: interaction
                .turn_id
                .as_ref()
                .map(|turn_id| turn_id.as_str().to_string())
                .unwrap_or_default(),
            kind: match interaction.kind {
                InteractionKind::Permission => RingingV2InteractionKind::Permission,
                InteractionKind::Ask => RingingV2InteractionKind::Ask,
                InteractionKind::Plan => RingingV2InteractionKind::PlanReview,
            },
            // #345 / 2026-09-24 修订：ask / plan / permission 的 modal 正文
            // 都走 content store（ref 指向正文）。permission 在旧实现里靠
            // tool 频道快照 / timeline 卡兜底；纯 v2 单流下该快照已不存在，
            // 因此也必须暴露 canonical 正文 ref。
            request: Some(wire_content_value(&interaction.request)),
        })
        .collect();
    let driver = {
        let driver = canonical_driver_state(&state, &session_id);
        let holder = driver.as_ref().and_then(|driver| driver.holder.clone());
        let driver_epoch = driver
            .as_ref()
            .map(|driver| driver.driver_epoch)
            .unwrap_or(0);
        // A recorded holder whose lease is gone is presented as vacant; the
        // next claim takes over via `stale_holder`.
        if holder.is_some() {
            // Observers keep the seat on the reclaim scan list too, so an
            // expired holder is released even if nobody claims afterwards.
            watch_driver_seat(&state, &session_id);
        }
        let effective_holder = holder.filter(|holder| holder_is_live(&state, holder));
        Some(RingingV2DriverState {
            can_claim: effective_holder.as_deref() != Some(caller.as_str()),
            holder: effective_holder,
            driver_epoch,
        })
    };
    let control = V2ControlState {
        session_id: control_snapshot.session_id.clone(),
        activity: control_snapshot.activity,
        current_turn_id: control_snapshot.current_turn_id.clone(),
        current_call_id: control_snapshot.current_call_id.clone(),
        round: control_snapshot.round.clone(),
        tools: control_snapshot.tools.clone(),
        subagents: control_snapshot.subagents.clone(),
        last_recovery: control_snapshot.last_recovery,
        revision: control_snapshot.revision,
        last_fact_seq: control_snapshot.last_fact_seq,
        interactions,
        driver,
    };
    let conversation = bootstrap.projections.conversation.clone();
    let tool = V2ToolState {
        tools: control_snapshot.tools.clone(),
    };
    let tool_revision = control_snapshot.revision;
    let response: RingingV2Bootstrap<V2ControlState, ConversationSnapshot, V2ToolState> =
        RingingV2Bootstrap {
            schema: RINGING_SCHEMA.into(),
            version: RINGING_V2_VERSION,
            server_epoch: bootstrap.server_epoch,
            session_id: bootstrap.session_id,
            snapshot_cursor: bootstrap.snapshot_cursor,
            control: RingingV2ChannelSnapshot {
                channel: RingingChannel::Control,
                state_revision: control.revision,
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

/// `GET /ringing/v2/sessions/{seed}/team` — team roster + task/message board snapshots.
///
/// Team agents are rebuilt from the session canonical log; tasks and board
/// entries come from the root tree's separate team aggregates. Deltas are
/// delivered on the per-seed single stream as `ProjectionPayload::TeamDelta`.
pub(crate) async fn handle_team_snapshot_v2(
    State(state): State<AppState>,
    Extension(identity): Extension<Identity>,
    headers: HeaderMap,
    Path(session_id): Path<String>,
) -> Response {
    if let Err(response) = require_lease_on_session(&state, &headers, &identity, &session_id) {
        return response;
    }
    if session_id.trim().is_empty() {
        return api_error_response(
            StatusCode::BAD_REQUEST,
            "missing_session_id",
            "missing seed",
        );
    }
    let session_dir = qaqh_types::platform::sessions_dir().join(&session_id);
    let team = match state.v2_hub.bootstrap(&session_dir, &session_id) {
        Ok(bootstrap) => bootstrap.projections.team,
        Err(error) => return v2_hub_error_response(error),
    };
    let tasks = match state.service.task_board_snapshot(&session_id) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            return api_error_response(StatusCode::BAD_REQUEST, "team_unavailable", &error);
        }
    };
    let board = match state.service.board_snapshot(&session_id) {
        Ok(snapshot) => snapshot,
        Err(error) => {
            return api_error_response(StatusCode::BAD_REQUEST, "board_unavailable", &error);
        }
    };
    json_response(
        StatusCode::OK,
        &serde_json::json!({
            "schema": "qaqh.ringing.team/v1",
            "session_id": session_id,
            "team": team,
            "tasks": tasks,
            "board": board,
        }),
    )
}

/// `GET /ringing/v2/sessions/{seed}/approvals` — 本地壳层审批投影。
///
/// 消费方是桌面壳宿主（`qaqh-webui-app` 经 `qaqh-client::pending_approvals`）:
/// 宿主把这里的 canonical id 映射为不透明 challenge 再交给渲染层（防御纵深,
/// 见 plan-webui-tauri D3 决策）。webui 浏览器网关已随 Tauri 化移除。
///
/// 纯 v2：pending 集合来自 canonical control 投影（只取未 resolved / 未 expired 的
/// 条目），正文来自 canonical content ref（`request`）。形状保持稳定：
///
/// ```json
/// { "pending_permission": {...}|null, "pending_interaction": {...}|null }
/// ```
///
/// 返回的 id 是 **canonical** 形态（`call_<ULID>` / `int_<ULID>`）；运行时侧已接受
/// canonical 与 wire 两种形态，故宿主直接透传即可。
pub(crate) async fn handle_pending_approvals_v2(
    State(state): State<AppState>,
    Extension(identity): Extension<Identity>,
    headers: HeaderMap,
    Path(session_id): Path<String>,
) -> Response {
    if let Err(response) = require_lease_on_session(&state, &headers, &identity, &session_id) {
        return response;
    }
    if session_id.trim().is_empty() {
        return api_error_response(
            StatusCode::BAD_REQUEST,
            "missing_session_id",
            "missing seed",
        );
    }
    let session_dir = qaqh_types::platform::sessions_dir().join(&session_id);
    let bootstrap = match state.v2_hub.bootstrap(&session_dir, &session_id) {
        Ok(bootstrap) => bootstrap,
        Err(error) => return v2_hub_error_response(error),
    };
    let interactions = &bootstrap.projections.control.interactions;
    let pending = |interaction: &&ControlInteractionState| {
        interaction.resolution.is_none() && interaction.expired_reason.is_none()
    };

    let pending_permission = interactions
        .iter()
        .filter(pending)
        .find(|interaction| interaction.kind == InteractionKind::Permission)
        .and_then(|interaction| pending_permission_view(&state, interaction));
    // 引擎的模态优先级是 plan 先于 ask（PlanReview 挂起期间 ask 尚不可应答），
    // 而同一挂起内 fact 的落盘序是 ask 在 plan 之前——选取必须按引擎优先级
    // 而非 fact 顺序，否则 ask 卡会遮住真正可应答的 plan 卡。
    let pending_interaction = interactions
        .iter()
        .filter(pending)
        .find(|interaction| interaction.kind == InteractionKind::Plan)
        .or_else(|| {
            interactions
                .iter()
                .filter(pending)
                .find(|interaction| interaction.kind == InteractionKind::Ask)
        })
        .map(|interaction| {
            serde_json::json!({
                "id": interaction.interaction_id.as_str(),
                "kind": match interaction.kind {
                    InteractionKind::Ask => "ask",
                    InteractionKind::Plan => "plan",
                    InteractionKind::Permission => "permission",
                },
                // 与 permission 的 `details_unavailable` 同语义：正文取不到时
                // 仍保留可答复的 id/kind，details 降级为 null。
                "details": interaction_body_value(&state, &interaction.request)
                    .unwrap_or(serde_json::Value::Null),
            })
        });

    let pending_json = serde_json::json!({
        "pending_permission": pending_permission.unwrap_or(serde_json::Value::Null),
        "pending_interaction": pending_interaction.unwrap_or(serde_json::Value::Null),
    });
    // admin（桌面宿主）：维持现状透传 canonical id（宿主已在壳侧做映射）。
    if identity.is_admin() {
        return json_response(StatusCode::OK, &pending_json);
    }
    // device（原生 app，半可信）：canonical id 不出 daemon，改发不透明 challenge。
    match state.challenges.issue_views(&session_id, &pending_json) {
        Ok(views) => json_response(
            StatusCode::OK,
            &serde_json::json!({ "challenges": views }),
        ),
        Err(code) => api_error_response(
            StatusCode::BAD_GATEWAY,
            code,
            "approval projection failed",
        ),
    }
}

/// `POST /ringing/v2/sessions/{seed}/approvals/respond`（S5）— device 侧审批应答。
///
/// body：`{ "challenge_id": str, "decision": str, "payload": {...} }`。
/// device 身份：一次性消费 challenge（scope = 本会话）→ `command_for` 映射回 canonical
/// 命令 → 进 `execute_command`（保留「审批应答不受 driver 门控」语义）。
/// **admin 不使用本端点**——桌面宿主维持 canonical 透传命令通道。
///
/// spec §9 描述了本流程但未定 wire 端点，此为本 spec 的补充设计。
#[derive(Deserialize)]
pub(crate) struct ApprovalRespondRequest {
    challenge_id: String,
    decision: String,
    #[serde(default)]
    payload: serde_json::Value,
}

pub(crate) async fn handle_approval_respond_v2(
    State(state): State<AppState>,
    Extension(identity): Extension<Identity>,
    headers: HeaderMap,
    Path(session_id): Path<String>,
    body: Bytes,
) -> Response {
    if let Err(response) = require_scope(&identity, Scope::Interact) {
        return response;
    }
    let caller = match require_lease_on_session(&state, &headers, &identity, &session_id) {
        Ok(caller) => caller,
        Err(response) => return response,
    };
    let request: ApprovalRespondRequest = match serde_json::from_slice(&body) {
        Ok(request) => request,
        Err(error) => {
            return api_error_response(
                StatusCode::BAD_REQUEST,
                "invalid_body",
                &format!("invalid approval respond request: {error}"),
            );
        }
    };
    let challenge = match state.challenges.consume(&request.challenge_id, &session_id) {
        Ok(challenge) => challenge,
        Err(code) => {
            return api_error_response(StatusCode::FORBIDDEN, code, "challenge rejected");
        }
    };
    let mut command = match challenge::command_for(&challenge, &request.decision, &request.payload)
    {
        Ok(command) => command,
        Err(code) => {
            return api_error_response(
                StatusCode::BAD_REQUEST,
                code,
                "invalid approval decision",
            );
        }
    };
    // §9.5 远程 `trust_folder` 默认拒：非 admin 提交的永久扩界降级为单次放行。
    if !identity.is_admin()
        && let qaqh_ringing::RingingCommand::Tool(
            qaqh_domain::ToolCommand::ToolPermissionRespond { trust_folder, .. },
        ) = &mut command
    {
        *trust_folder = false;
    }
    let instance_key = state
        .leases
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .instance_for_session(&caller)
        .unwrap_or_default();
    let envelope = RingingV2CommandEnvelope::new(
        qaqh_session::canonical::generate_ulid(),
        instance_key,
        command,
    )
    .with_client_session_id(caller)
    .with_session_id(&session_id);
    let (status, ack) = execute_command(&state, &headers, &identity, envelope).await;
    json_response(status, &ack)
}

/// 从 canonical interaction 的 `request` 取回 permission 详情正文，映射回旧 v1
/// 端点暴露的 `pending_permission_details` 形状。
///
/// 正文取不到（被淘汰）时仍返回条目本身——id 是答复所必需的，详情缺失只能降级
/// 展示，不能让待审批项从 UI 上消失。
fn pending_permission_view(
    state: &AppState,
    interaction: &ControlInteractionState,
) -> Option<serde_json::Value> {
    let call_id = interaction.call_id.as_ref()?.as_str().to_string();
    let body = interaction_body_value(state, &interaction.request);
    Some(serde_json::json!({
        "tool_call_id": call_id,
        "tool_name": body.as_ref().and_then(|b| b.get("tool_name")).cloned().unwrap_or(serde_json::Value::Null),
        "action_summary": body.as_ref().and_then(|b| b.get("action_summary")).cloned().unwrap_or(serde_json::Value::Null),
        "reason": body.as_ref().and_then(|b| b.get("reason")).cloned().unwrap_or(serde_json::Value::Null),
        "paths": body.as_ref().and_then(|b| b.get("paths")).cloned().unwrap_or(serde_json::json!([])),
        "category": body.as_ref().and_then(|b| b.get("category")).cloned().unwrap_or(serde_json::Value::Null),
        "level": body.as_ref().and_then(|b| b.get("level")).cloned().unwrap_or(serde_json::Value::Null),
        "risk": body.as_ref().and_then(|b| b.get("risk")).cloned().unwrap_or(serde_json::Value::Null),
        "consequence": body.as_ref().and_then(|b| b.get("consequence")).cloned().unwrap_or(serde_json::Value::Null),
        "details_unavailable": body.is_none(),
    }))
}

/// 解析 interaction `request` 的正文 JSON（Inline 直接解，Ref 走 content store）。
fn interaction_body_value(state: &AppState, request: &ContentValue) -> Option<serde_json::Value> {
    match request {
        ContentValue::Inline { text } => serde_json::from_str(text).ok(),
        ContentValue::Ref { content_ref } => {
            // canonical ref 是 `sha256:<hex>`，content store 的 id 是裸 hex。
            let store_id = content_ref.hash().as_str();
            let store_id = store_id.strip_prefix("sha256:").unwrap_or(store_id);
            let entry = state.hub.get_content_any(store_id)?;
            serde_json::from_slice(&entry.bytes).ok()
        }
        ContentValue::Unavailable(_) => None,
    }
}

/// 每 seed 一条 SSE（2026-09-24 冻结修订）。
///
/// 事件带 `stream_key`，客户端自行 demux；per-channel 的 `events/{channel}`
/// 已硬切删除。reset 在单流上只发一次。
pub(crate) async fn handle_events_v2(
    State(state): State<AppState>,
    Extension(identity): Extension<Identity>,
    headers: HeaderMap,
    Path(session_id): Path<String>,
    Query(query): Query<V2EventsQuery>,
) -> Response {
    let caller = match require_lease_on_session(&state, &headers, &identity, &session_id) {
        Ok(caller) => caller,
        Err(response) => return response,
    };
    if session_id.trim().is_empty() {
        return api_error_response(
            StatusCode::BAD_REQUEST,
            "missing_session_id",
            "missing seed",
        );
    }
    let cursor = query
        .since_cursor
        .as_deref()
        .map(qaqh_ringing::CursorToken::from_opaque);
    let session_dir = qaqh_types::platform::sessions_dir().join(&session_id);
    let mut subscription = match state
        .v2_hub
        .subscribe(&session_dir, &session_id, cursor.as_ref())
    {
        Ok(subscription) => subscription,
        Err(error) => return v2_hub_error_response(error),
    };

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, Infallible>>(64);
    // 设备吊销（`revoke_device` 摘 lease）必须**杀掉在途 SSE**（spec §6）：订阅本身
    // 与 lease 生命周期无关，故循环内每个事件前复查租约存活；失效即下发终止帧并
    // 断流。与 `sse.rs::handle_timeline_events` 的逐事件存活检查同款。
    let leases = state.leases.clone();
    let stream_session = session_id.clone();
    tokio::spawn(async move {
        loop {
            match subscription.next().await {
                V2StreamItem::Event(event) => {
                    if !leases
                        .lock()
                        .unwrap_or_else(|error| error.into_inner())
                        .is_active_session(&caller)
                    {
                        log::info!(
                            "[ringing-v2] lease {caller} revoked; terminating in-flight SSE for {stream_session}"
                        );
                        let frame = Event::default().event("ringing.stream_terminated").data(
                            serde_json::json!({
                                "code": "revoked",
                                "session_id": stream_session.as_str(),
                                "message": "client lease revoked or expired; reconnect after re-auth",
                            })
                            .to_string(),
                        );
                        let _ = tx.send(Ok(frame)).await;
                        break;
                    }
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

/// Canonical driver seat, or `None` when the session has no `DriverChanged`
/// fact (or no canonical log at all).
pub(crate) fn canonical_driver_state(
    state: &AppState,
    session_id: &str,
) -> Option<qaqh_session::projection::ControlDriverState> {
    let session_dir = qaqh_types::platform::sessions_dir().join(session_id);
    state
        .v2_hub
        .driver_state(&session_dir, session_id)
        .ok()
        .flatten()
}

/// Whether a recorded driver still holds a live daemon lease.
pub(crate) fn holder_is_live(state: &AppState, holder: &str) -> bool {
    state
        .leases
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .is_active_session(holder)
}

fn unix_millis() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as u64)
        .unwrap_or_default()
}

/// Remember a seed whose seat is worth scanning for lease expiry.
fn watch_driver_seat(state: &AppState, session_id: &str) {
    state
        .driver_watch
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .insert(session_id);
}

fn unwatch_driver_seat(state: &AppState, session_id: &str) {
    state
        .driver_watch
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .remove(session_id);
}

/// Reclaim driver seats whose holder's lease has expired (spec §9.3 自动移交).
///
/// The daemon cannot append canonical facts, so this forwards a privileged
/// `DriverRelease` to the session actor with an `expected_epoch` CAS. If the
/// seat moved on in the meantime the release is a no-op instead of kicking the
/// new holder.
pub(crate) fn reclaim_dead_driver_seats(state: &AppState) {
    let sessions = state
        .driver_watch
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .sessions();
    for session_id in sessions {
        let Some(driver) = canonical_driver_state(state, &session_id) else {
            // Session gone (or no canonical log): nothing left to scan.
            unwatch_driver_seat(state, &session_id);
            continue;
        };
        let Some(holder) = driver.holder.clone() else {
            // Seat already vacant: stop scanning this seed.
            unwatch_driver_seat(state, &session_id);
            continue;
        };
        if holder_is_live(state, &holder) {
            continue;
        }
        let now_ms = unix_millis();
        if !state
            .driver_watch
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .reclaim_due(&session_id, now_ms)
        {
            continue;
        }
        let envelope = qaqh_ringing::RingingWorkerCommandEnvelope::new(
            session_id.as_str(),
            qaqh_session::canonical::generate_ulid(),
            qaqh_ringing::RingingCommand::Control(qaqh_domain::ControlCommand::DriverRelease {
                client_session_id: holder,
                expected_epoch: Some(driver.driver_epoch),
            }),
        );
        state
            .driver_watch
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .note_reclaim_dispatch(&session_id, now_ms);
        match state.service.send_ringing_command(&session_id, &envelope) {
            Ok(()) => log::info!(
                "[ringing-v2] reclaiming driver seat for {session_id}: holder lease expired at epoch {}",
                driver.driver_epoch
            ),
            Err(error) => {
                // The session cannot be reached at all (deleted meta, spawn
                // failure). Retrying every tick would only spam the log; the
                // seed is re-registered on the next claim/bootstrap touch.
                log::warn!("[ringing-v2] driver seat reclaim dropped for {session_id}: {error}");
                unwatch_driver_seat(state, &session_id);
            }
        }
    }
}

/// Forward a daemon-normalized driver command to the session actor.
///
/// The actor's `ToolLedger` is the canonical single writer, so it — not the
/// daemon — allocates `driver_epoch` and appends `DriverChanged`. The HTTP
/// response below is therefore "requested"; the authoritative seat reaches
/// clients as the reliable `DriverChanged` event.
async fn forward_driver_command(
    state: &AppState,
    headers: &HeaderMap,
    identity: &Identity,
    session_id: &str,
    caller: &str,
    command: qaqh_domain::ControlCommand,
) -> Response {
    let client_instance_id = state
        .leases
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .instance_for_session(caller)
        .unwrap_or_default();
    let envelope = RingingV2CommandEnvelope::new(
        qaqh_session::canonical::generate_ulid(),
        client_instance_id,
        qaqh_ringing::RingingCommand::Control(command),
    )
    .with_client_session_id(caller)
    .with_session_id(session_id);
    let (status, ack) = execute_command(state, headers, identity, envelope).await;
    json_response(status, &ack)
}

pub(crate) async fn handle_driver_claim_v2(
    State(state): State<AppState>,
    Extension(identity): Extension<Identity>,
    headers: HeaderMap,
    Path(session_id): Path<String>,
) -> Response {
    let Some(caller) = require_v2_lease(&state, &headers) else {
        return lease_required_v2();
    };
    if session_id.trim().is_empty() {
        return api_error_response(
            StatusCode::BAD_REQUEST,
            "missing_session_id",
            "missing seed",
        );
    }
    watch_driver_seat(&state, &session_id);
    let current = canonical_driver_state(&state, &session_id);
    let holder = current.as_ref().and_then(|driver| driver.holder.clone());
    let driver_epoch = current
        .as_ref()
        .map(|driver| driver.driver_epoch)
        .unwrap_or(0);
    if let Some(holder) = holder.clone() {
        if holder == caller {
            return json_response(
                StatusCode::OK,
                &RingingV2DriverClaimResponse {
                    accepted: true,
                    holder: Some(holder),
                    driver_epoch,
                    reason: "already_holder".into(),
                },
            );
        }
        if holder_is_live(&state, &holder) {
            return json_response(
                StatusCode::OK,
                &RingingV2DriverClaimResponse {
                    accepted: false,
                    holder: Some(holder),
                    driver_epoch,
                    reason: "driver_busy".into(),
                },
            );
        }
    }
    // Seat is free, or its recorded holder's lease has expired.
    let stale_holder = holder
        .clone()
        .filter(|holder| !holder_is_live(&state, holder));
    let dispatch = forward_driver_command(
        &state,
        &headers,
        &identity,
        &session_id,
        &caller,
        qaqh_domain::ControlCommand::DriverClaim {
            client_session_id: caller.clone(),
            stale_holder,
        },
    )
    .await;
    if !dispatch.status().is_success() {
        return dispatch;
    }
    json_response(
        StatusCode::OK,
        &RingingV2DriverClaimResponse {
            accepted: true,
            // State at request time; the new seat arrives via `DriverChanged`.
            holder,
            driver_epoch,
            reason: "claim_requested".into(),
        },
    )
}

pub(crate) async fn handle_driver_release_v2(
    State(state): State<AppState>,
    Extension(identity): Extension<Identity>,
    headers: HeaderMap,
    Path(session_id): Path<String>,
) -> Response {
    let Some(caller) = require_v2_lease(&state, &headers) else {
        return lease_required_v2();
    };
    if session_id.trim().is_empty() {
        return api_error_response(
            StatusCode::BAD_REQUEST,
            "missing_session_id",
            "missing seed",
        );
    }
    let current = canonical_driver_state(&state, &session_id);
    let holder = current.as_ref().and_then(|driver| driver.holder.clone());
    let driver_epoch = current
        .as_ref()
        .map(|driver| driver.driver_epoch)
        .unwrap_or(0);
    if holder.as_deref() != Some(caller.as_str()) {
        return json_response(
            StatusCode::OK,
            &RingingV2DriverReleaseResponse {
                accepted: false,
                holder,
                driver_epoch,
                reason: "not_driver".into(),
            },
        );
    }
    let dispatch = forward_driver_command(
        &state,
        &headers,
        &identity,
        &session_id,
        &caller,
        qaqh_domain::ControlCommand::DriverRelease {
            client_session_id: caller.clone(),
            expected_epoch: Some(driver_epoch),
        },
    )
    .await;
    if !dispatch.status().is_success() {
        return dispatch;
    }
    json_response(
        StatusCode::OK,
        &RingingV2DriverReleaseResponse {
            accepted: true,
            holder,
            driver_epoch,
            reason: "release_requested".into(),
        },
    )
}

/// Commands that mutate the session and therefore require the driver seat.
///
/// Interaction answers (permission / ask / plan) are deliberately excluded:
/// a non-driver must still be able to resolve a pending interaction.
fn driver_gated(command: &qaqh_ringing::RingingCommand) -> bool {
    use qaqh_domain::ControlCommand;
    matches!(
        command,
        qaqh_ringing::RingingCommand::Conversation(_)
            | qaqh_ringing::RingingCommand::Control(
                ControlCommand::SessionClose { .. }
                    | ControlCommand::SessionArchive { .. }
                    | ControlCommand::SessionUnarchive { .. }
                    | ControlCommand::SessionDelete { .. }
                    | ControlCommand::SessionShutdown
                    | ControlCommand::AgentReloadConfig
                    | ControlCommand::SetToolMode { .. }
                    | ControlCommand::SkillsActivate { .. }
                    | ControlCommand::SkillsReload
                    | ControlCommand::SkillsOperation { .. },
            )
    )
}

/// Driver admission for a gated command.
///
/// Returns `Some(rejection)` when the command must not reach the worker.
/// A session with no claimed driver stays permissive: gating only kicks in
/// once somebody holds the seat, so unclaimed sessions keep working while the
/// TUI has not adopted driver claiming yet.
fn driver_admission(
    state: &AppState,
    headers: &HeaderMap,
    session_id: &str,
    command: &qaqh_ringing::RingingCommand,
    driver_epoch: Option<u64>,
) -> Option<RingingV2CommandAck> {
    if !driver_gated(command) {
        return None;
    }
    let client_session_id = require_v2_lease(state, headers)?;
    let driver = canonical_driver_state(state, session_id)?;
    let holder = driver.holder?;
    if !holder_is_live(state, &holder) {
        return None;
    }
    let epoch = driver.driver_epoch;
    if holder != client_session_id {
        return Some(RingingV2CommandAck {
            command_id: String::new(),
            status: RingingCommandAckStatus::Rejected,
            code: Some("not_driver".into()),
            message: Some("another client holds the driver seat".into()),
            retry_after_ms: None,
            existing: None,
        });
    }
    if let Some(expected) = driver_epoch
        && expected != epoch
    {
        return Some(RingingV2CommandAck {
            command_id: String::new(),
            status: RingingCommandAckStatus::Rejected,
            code: Some("stale_driver_epoch".into()),
            message: Some(format!(
                "driver_epoch {expected} is stale; current epoch is {epoch}"
            )),
            retry_after_ms: None,
            existing: None,
        });
    }
    None
}

pub(crate) async fn handle_command_v2(
    State(state): State<AppState>,
    Extension(identity): Extension<Identity>,
    headers: HeaderMap,
    Path(id): Path<String>,
    body: Bytes,
) -> Response {
    // scope 按命令分级，在 `execute_command` 内裁决（`SessionAttach` 仅需 view），
    // 故此处不设路由级 scope 门。
    let mut envelope: RingingV2CommandEnvelope = match serde_json::from_slice(&body) {
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
    // Idempotent replay: a command_id already inside the receipt TTL must not
    // re-enter the worker. v2 answers with the recorded terminal outcome
    // (including the typed payload) instead of the v1 "already accepted"
    // message, so a client that lost the first ACK can reconcile directly.
    let fingerprint = command_fingerprint(
        envelope.channel,
        envelope.session_id.as_deref(),
        envelope.expected_revision,
        envelope.driver_epoch,
        &envelope.command,
    );
    if let Some(existing) = existing_v2_receipt(&state, &headers, &envelope.command_id) {
        if existing.payload_fingerprint != fingerprint {
            return json_response(
                StatusCode::CONFLICT,
                &RingingV2CommandAck {
                    command_id: envelope.command_id.clone(),
                    status: RingingCommandAckStatus::Rejected,
                    code: Some("duplicate_command_mismatch".into()),
                    message: Some("command_id was already used with another payload".into()),
                    retry_after_ms: None,
                    existing: None,
                },
            );
        }
        let message = match existing.state {
            qaqh_ringing::RingingCommandState::Succeeded => {
                "duplicate command_id (already completed)"
            }
            qaqh_ringing::RingingCommandState::Failed => "duplicate command_id (already failed)",
            qaqh_ringing::RingingCommandState::Rejected => {
                "duplicate command_id (already rejected)"
            }
            qaqh_ringing::RingingCommandState::Accepted
            | qaqh_ringing::RingingCommandState::Running => "duplicate command_id (in flight)",
        };
        return json_response(
            StatusCode::OK,
            &RingingV2CommandAck {
                command_id: envelope.command_id.clone(),
                status: RingingCommandAckStatus::Accepted,
                code: None,
                message: Some(message.into()),
                retry_after_ms: None,
                existing: Some(existing.into_existing()),
            },
        );
    }
    // First-answer-wins: if the canonical control projection already carries a
    // structured verdict for the targeted interaction, answer synchronously
    // with the winning result instead of dispatching a command the worker can
    // only reject with a bare `interaction_already_resolved`.
    if require_v2_lease(&state, &headers).is_some()
        && let Some(session_id) = envelope.session_id.as_deref()
        && let Some(existing) = resolved_interaction_existing(&state, session_id, &envelope.command)
    {
        return json_response(
            StatusCode::OK,
            &RingingV2CommandAck {
                command_id: envelope.command_id.clone(),
                status: RingingCommandAckStatus::Rejected,
                code: Some("interaction_already_resolved".into()),
                message: Some("interaction was already resolved".into()),
                retry_after_ms: None,
                existing: Some(existing),
            },
        );
    }
    if let Some(session_id) = envelope.session_id.as_deref()
        && let Some(mut rejection) = driver_admission(
            &state,
            &headers,
            session_id,
            &envelope.command,
            envelope.driver_epoch,
        )
    {
        rejection.command_id = envelope.command_id.clone();
        return json_response(StatusCode::OK, &rejection);
    }
    // Driver commands are daemon-normalized: identity fields on the wire are
    // never trusted, so a direct `driver_claim` submission cannot claim a seat
    // on behalf of another lease.
    if let Some(caller) = require_v2_lease(&state, &headers) {
        let session_id = envelope.session_id.clone();
        match &mut envelope.command {
            qaqh_ringing::RingingCommand::Control(qaqh_domain::ControlCommand::DriverClaim {
                client_session_id,
                stale_holder,
            }) => {
                *client_session_id = caller;
                *stale_holder = session_id
                    .as_deref()
                    .and_then(|session_id| canonical_driver_state(&state, session_id))
                    .and_then(|driver| driver.holder)
                    .filter(|holder| !holder_is_live(&state, holder));
            }
            qaqh_ringing::RingingCommand::Control(qaqh_domain::ControlCommand::DriverRelease {
                client_session_id,
                expected_epoch,
            }) => {
                *client_session_id = caller;
                *expected_epoch = session_id
                    .as_deref()
                    .and_then(|session_id| canonical_driver_state(&state, session_id))
                    .map(|driver| driver.driver_epoch);
            }
            _ => {}
        }
    }
    // 进程内命令入口直调（spec 阶段 1.2）：v2 信封直接进引擎，
    // 不再序列化成 v1 信封 JSON 绕已无路由的 v1 handler。
    let (status, ack) = execute_command(&state, &headers, &identity, envelope).await;
    json_response(status, &ack)
}

/// Interaction addressed by an interaction-resolution command.
enum InteractionTarget<'a> {
    /// ask / plan commands carry the canonical `int_...` interaction id.
    InteractionId(&'a str),
    /// permission commands carry the canonical `call_...` tool call id.
    CallId(&'a str),
}

fn interaction_target(command: &qaqh_ringing::RingingCommand) -> Option<InteractionTarget<'_>> {
    use qaqh_domain::{ControlCommand, ToolCommand};
    match command {
        qaqh_ringing::RingingCommand::Tool(ToolCommand::ToolPermissionRespond {
            tool_call_id,
            ..
        }) => Some(InteractionTarget::CallId(tool_call_id)),
        qaqh_ringing::RingingCommand::Control(
            ControlCommand::InteractionAskRespond { interaction_id, .. }
            | ControlCommand::InteractionAskDismiss { interaction_id }
            | ControlCommand::PlanReviewRespond { interaction_id, .. },
        ) => Some(InteractionTarget::InteractionId(interaction_id)),
        _ => None,
    }
}

/// Structured verdict of an already-resolved interaction, if the canonical
/// control projection has one. Legacy facts carry no verdict → `None`, and the
/// command falls through to the worker's existing rejection path.
fn resolved_interaction_existing(
    state: &AppState,
    session_id: &str,
    command: &qaqh_ringing::RingingCommand,
) -> Option<RingingV2ExistingResult> {
    let target = interaction_target(command)?;
    let session_dir = qaqh_types::platform::sessions_dir().join(session_id);
    let interactions = state
        .v2_hub
        .control_interactions(&session_dir, session_id)
        .ok()?;
    let interaction = match target {
        InteractionTarget::InteractionId(id) => interactions
            .iter()
            .find(|interaction| interaction.interaction_id.as_str() == id),
        InteractionTarget::CallId(id) => interactions.iter().find(|interaction| {
            interaction
                .call_id
                .as_ref()
                .is_some_and(|call_id| call_id.as_str() == id)
        }),
    }?;
    let verdict = interaction.resolution.as_ref()?.verdict?;
    let result = command_result_for(
        interaction.kind,
        interaction.interaction_id.as_str(),
        verdict,
    )?;
    Some(RingingV2ExistingResult::InteractionResolved { result })
}

fn command_result_for(
    kind: InteractionKind,
    interaction_id: &str,
    verdict: InteractionDecision,
) -> Option<RingingV2CommandResult> {
    let interaction_id = interaction_id.to_string();
    match (kind, verdict) {
        (InteractionKind::Permission, InteractionDecision::Approved) => {
            Some(RingingV2CommandResult::PermissionResolved {
                interaction_id,
                approved: true,
            })
        }
        (InteractionKind::Permission, InteractionDecision::Rejected) => {
            Some(RingingV2CommandResult::PermissionResolved {
                interaction_id,
                approved: false,
            })
        }
        (InteractionKind::Ask, InteractionDecision::Answered) => {
            Some(RingingV2CommandResult::AskResolved {
                interaction_id,
                outcome: RingingV2AskOutcome::Answered,
            })
        }
        (InteractionKind::Ask, InteractionDecision::Dismissed) => {
            Some(RingingV2CommandResult::AskResolved {
                interaction_id,
                outcome: RingingV2AskOutcome::Dismissed,
            })
        }
        (InteractionKind::Plan, InteractionDecision::Approved) => {
            Some(RingingV2CommandResult::PlanReviewResolved {
                interaction_id,
                approved: true,
            })
        }
        (InteractionKind::Plan, InteractionDecision::Rejected) => {
            Some(RingingV2CommandResult::PlanReviewResolved {
                interaction_id,
                approved: false,
            })
        }
        _ => None,
    }
}

/// Look up a replayable receipt for the lease named by the request header.
///
/// Returns `None` when the header is absent/inactive, so the caller falls
/// through to the shared command path and its canonical error response.
fn existing_v2_receipt(
    state: &AppState,
    headers: &HeaderMap,
    command_id: &str,
) -> Option<qaqh_runtime::ringing::ExistingCommandReceipt> {
    let session_id = get_session_id(headers)?;
    let active = state
        .leases
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .is_active_session(&session_id);
    if !active {
        return None;
    }
    state
        .pending
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .existing_receipt_for_session(command_id, &session_id)
}

pub(crate) async fn handle_command_status_v2(
    State(state): State<AppState>,
    headers: HeaderMap,
    Path(command_id): Path<String>,
) -> Response {
    let Some(session_id) = get_session_id(&headers) else {
        return api_error_response(
            StatusCode::UNAUTHORIZED,
            "lease_required",
            "client session header required",
        );
    };
    let Some(status) = state
        .pending
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .v2_status_for_session(&command_id, &session_id)
    else {
        return api_error_response(
            StatusCode::NOT_FOUND,
            "command_not_found",
            "command receipt not found",
        );
    };
    json_response(StatusCode::OK, &status)
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

fn forbidden_not_owner(session_id: &str) -> Response {
    api_error_response(
        StatusCode::FORBIDDEN,
        "forbidden_not_owner",
        &format!("lease does not own session {session_id}"),
    )
}

/// 解析调用方 lease，并**绑定到身份**：device 身份的 lease 必须由本设备建立
/// （lease 的 `client_instance_id` == 本 token 反推的 `device_id`）——否则即便
/// 提供了他人的有效 `client_session_id` 也视为无 lease。堵住「拿到别人的 cs 即
/// 可冒充其 lease」（A3：身份由 token 反推）。admin 维持旧行为（自报 instance）。
pub(crate) fn caller_lease_bound_to_identity(
    state: &AppState,
    headers: &HeaderMap,
    identity: &Identity,
) -> Option<String> {
    let caller = require_v2_lease(state, headers)?;
    match identity {
        Identity::Admin => Some(caller),
        Identity::Device { device_id, .. } => {
            let bound = state
                .leases
                .lock()
                .unwrap_or_else(|error| error.into_inner())
                .instance_for_session(&caller)
                .as_deref()
                == Some(device_id.as_str());
            bound.then_some(caller)
        }
    }
}

/// 会话归属校验（S1）：无 lease → 401 `lease_required`；有 lease 但非归属 →
/// 403 `forbidden_not_owner`。admin 豁免（桌面壳 / CLI / TUI / 探针行为不变）。
///
/// 归属只由 `SessionAttach` 建立；移动端读某 seed 前须先 attach。
fn require_lease_on_session(
    state: &AppState,
    headers: &HeaderMap,
    identity: &Identity,
    session_id: &str,
) -> Result<String, Response> {
    let Some(caller) = caller_lease_bound_to_identity(state, headers, identity) else {
        return Err(lease_required_v2());
    };
    if identity.is_admin() {
        return Ok(caller);
    }
    let owns = state
        .leases
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .owns_session(&caller, session_id);
    if !owns {
        return Err(forbidden_not_owner(session_id));
    }
    Ok(caller)
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

pub(crate) fn api_error_response(status: StatusCode, code: &str, message: &str) -> Response {
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

pub(crate) fn json_response<T: Serialize>(status: StatusCode, value: &T) -> Response {
    (
        status,
        [(header::CONTENT_TYPE, "application/json")],
        serde_json::to_vec(value).unwrap_or_default(),
    )
        .into_response()
}
