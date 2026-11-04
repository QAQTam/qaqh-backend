//! Ringing v2 HTTP/SSE endpoints.
//!
//! 读取面:`open -> bootstrap(SessionState) -> since_cursor subscribe -> live`。
//! 状态与事件都出自会话 owner(SQLite 业务表 + 单流 seq 广播)。

use qaqh_ringing::{
    RINGING_SCHEMA, RINGING_V2_VERSION, RingingV2ApprovalRespondRequest, RingingV2AskOutcome,
    RingingV2Capabilities, RingingV2CommandEnvelope, RingingV2CommandResult,
    RingingV2DeviceChallenges, RingingV2ExistingResult, RingingV2LeaseRenewResponse,
    RingingV2OpenRequest, RingingV2OpenResponse, RingingV2PendingApprovals,
    RingingV2PendingInteractionView, RingingV2PendingPermissionView,
};
use qaqh_ringing_contract::fact::{ContentValue, InteractionDecision, InteractionKind};
use qaqh_ringing_contract::state::{InteractionView, SessionBootstrap};
use serde::Serialize;

use super::*;

// bootstrap 的 state 是 qaqh_ringing_contract 的 SessionState(与 qaqh-client、
// TS 契约导出共用同一份定义);本文件只做业务库→wire 的装配。
use qaqh_ringing_contract::{TEAM_SCHEMA, TeamResponse};

#[derive(Deserialize)]
pub struct V2EventsQuery {
    pub since_cursor: Option<String>,
}

/// Cadence for the admin-only memory diagnostics frames that ride the session
/// SSE. Sampling is opt-in (`diagnostics.memory.start`), so this only ticks
/// while a probe window is open.
const MEMORY_PUSH_INTERVAL: Duration = Duration::from_secs(1);

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
    // 契约代际预检(spec §11):旧客户端在 open 阶段明确失败,不进入半兼容消费。
    if request.command_contract_revision != qaqh_ringing::COMMAND_CONTRACT_REVISION
        || request.session_contract_revision != qaqh_ringing::SESSION_CONTRACT_REVISION
    {
        return api_error_response(
            StatusCode::UPGRADE_REQUIRED,
            "unsupported_contract_revision",
            &format!(
                "client expects command/session revision {}/{}; daemon provides {}/{}",
                request.command_contract_revision,
                request.session_contract_revision,
                qaqh_ringing::COMMAND_CONTRACT_REVISION,
                qaqh_ringing::SESSION_CONTRACT_REVISION,
            ),
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
            service: true,
            content: true,
            single_stream: true,
            pairing: true,
        },
        // B07:命令目录代际随握手下发,客户端在发命令前即可校验兼容性。
        // spec §11:读模型/事件词表代际同批下发,客户端严格校验。
        command_contract_revision: qaqh_ringing::COMMAND_CONTRACT_REVISION.into(),
        session_contract_revision: qaqh_ringing::SESSION_CONTRACT_REVISION.into(),
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

/// 业务会话存在性:不建 owner(避免为不存在的会话泄漏泵线程)。
async fn require_business_session(
    owners: &qaqh_session::owner::OwnerDirectory,
    session_id: &str,
) -> Result<(), Response> {
    let owned = session_id.to_string();
    let exists = owners
        .db()
        .read_blocking(move |conn| qaqh_session::db::store::session_exists(conn, &owned))
        .map_err(|error| {
            api_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "state_unavailable",
                &format!("business lookup failed: {error}"),
            )
        })?;
    if !exists {
        return Err(api_error_response(
            StatusCode::NOT_FOUND,
            "session_not_found",
            "business session row not found",
        ));
    }
    Ok(())
}

pub(crate) async fn handle_bootstrap_v2(
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
    if let Err(response) = require_business_session(state.service.owners(), &session_id).await {
        return response;
    }
    let owner = state.service.owners().get_or_create(&session_id);
    let mut snapshot = match owner.snapshot().await {
        Ok(snapshot) => snapshot,
        Err(error) => {
            return api_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "state_unavailable",
                &format!("business state for {session_id}: {error}"),
            );
        }
    };
    // driver 席位归属 daemon(内存);overlay 进快照,claim/release 经 owner
    // 发 DriverChanged 保持实时一致。
    snapshot.driver = state.drivers.get(&session_id);
    let response = SessionBootstrap {
        schema: RINGING_SCHEMA.into(),
        version: RINGING_V2_VERSION,
        session_contract_revision: qaqh_ringing::SESSION_CONTRACT_REVISION.into(),
        server_epoch: state.epoch.clone(),
        session_id: session_id.clone(),
        state: snapshot,
    };
    json_response(StatusCode::OK, &response)
}

/// `GET /ringing/v2/sessions/{seed}/team` — team roster + task/message board snapshots.
///
/// 花名册由业务库血缘重建:根会话 + `sessions.parent_session_id` 直指的子代理,
/// 状态读各自的业务行;任务板/留言板仍来自 team aggregates。增量在
/// SSE 单流上由 `SessionEvent` 承载。
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
    let owners = state.service.owners();
    let children = match owners.session_children(&session_id) {
        Ok(children) => children,
        Err(error) => {
            return api_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "team_unavailable",
                &format!("business children for {session_id}: {error}"),
            );
        }
    };
    let root_path = qaqh_ringing_contract::fact::AgentPath::root();
    let mut agents = vec![qaqh_ringing_contract::team::TeamAgentSnapshot {
        agent_id: qaqh_ringing_contract::fact::SessionId::new(session_id.clone()),
        agent_path: root_path.clone(),
        nickname: None,
        role: None,
        status: qaqh_ringing_contract::team::TeamAgentStatus::Running,
        residency: qaqh_ringing_contract::team::TeamAgentResidency::Loaded,
        parent_agent_path: None,
        current_task_id: None,
    }];
    for child in &children {
        let status = match owners.session_run_state(child) {
            Ok(qaqh_session::owner::SessionRunState::Running) => {
                qaqh_ringing_contract::team::TeamAgentStatus::Running
            }
            Ok(qaqh_session::owner::SessionRunState::Waiting) => {
                qaqh_ringing_contract::team::TeamAgentStatus::WaitingUser
            }
            Ok(qaqh_session::owner::SessionRunState::Cancelling) => {
                qaqh_ringing_contract::team::TeamAgentStatus::Running
            }
            Ok(qaqh_session::owner::SessionRunState::Idle) => {
                qaqh_ringing_contract::team::TeamAgentStatus::Running
            }
            Ok(qaqh_session::owner::SessionRunState::NeverStarted) => {
                qaqh_ringing_contract::team::TeamAgentStatus::PendingInit
            }
            Err(_) => qaqh_ringing_contract::team::TeamAgentStatus::PendingInit,
        };
        let path = root_path.child(child).unwrap_or_else(|_| {
            // 血缘 id 不满足 path 段规则时按原样平铺;花名册展示不因
            // 命名规则失败而缺席。
            qaqh_ringing_contract::fact::AgentPath::parse_absolute(&format!("/root/{child}"))
                .unwrap_or(root_path.clone())
        });
        agents.push(qaqh_ringing_contract::team::TeamAgentSnapshot {
            agent_id: qaqh_ringing_contract::fact::SessionId::new(child.clone()),
            agent_path: path.clone(),
            nickname: None,
            role: None,
            status,
            residency: qaqh_ringing_contract::team::TeamAgentResidency::Unloaded,
            parent_agent_path: Some(root_path.clone()),
            current_task_id: None,
        });
    }
    let team = qaqh_ringing_contract::team::TeamSnapshot {
        root_session_id: Some(qaqh_ringing_contract::fact::SessionId::new(
            session_id.clone(),
        )),
        agents,
        unread_messages: Vec::new(),
        revision: 0,
        last_fact_seq: 0,
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
    let response = TeamResponse {
        schema: TEAM_SCHEMA.to_owned(),
        session_id,
        team,
        tasks,
        board,
    };
    json_response(StatusCode::OK, &response)
}

/// `GET /ringing/v2/sessions/{seed}/approvals` — 本地壳层审批投影。
///
/// 消费方是桌面壳宿主（`qaqh-electron` 经 `qaqh-client::pending_approvals`）:
/// 宿主把这里的 canonical id 映射为不透明 challenge 再交给渲染层（防御纵深）。
/// webui 浏览器网关已移除。
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
    if let Err(response) = require_business_session(state.service.owners(), &session_id).await {
        return response;
    }
    let owner = state.service.owners().get_or_create(&session_id);
    let snapshot = match owner.snapshot().await {
        Ok(snapshot) => snapshot,
        Err(error) => {
            return api_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                "state_unavailable",
                &format!("business state for {session_id}: {error}"),
            );
        }
    };
    let interactions = &snapshot.interactions;
    let pending = |interaction: &&InteractionView| interaction.resolution.is_none();

    let permission = interactions
        .iter()
        .filter(pending)
        .find(|interaction| interaction.kind == InteractionKind::Permission);
    let pending_permission = match permission {
        Some(interaction) => match pending_permission_view(&state, &session_id, interaction) {
            Ok(view) => view,
            Err(error) => {
                return api_error_response(
                    StatusCode::INTERNAL_SERVER_ERROR,
                    "content_unavailable",
                    &format!("pending interaction content for {session_id}: {error}"),
                );
            }
        },
        None => None,
    };
    // 引擎的模态优先级是 plan 先于 ask（PlanReview 挂起期间 ask 尚不可应答），
    // 而同一挂起内 fact 的落盘序是 ask 在 plan 之前——选取必须按引擎优先级
    // 而非 fact 顺序，否则 ask 卡会遮住真正可应答的 plan 卡。
    let interaction = interactions
        .iter()
        .filter(pending)
        .find(|interaction| interaction.kind == InteractionKind::Plan)
        .or_else(|| {
            interactions
                .iter()
                .filter(pending)
                .find(|interaction| interaction.kind == InteractionKind::Ask)
        });
    let pending_interaction = match interaction {
        Some(interaction) => {
            let details = match interaction_body_value(&state, &session_id, &interaction.request) {
                Ok(details) => details,
                Err(error) => {
                    return api_error_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        "content_unavailable",
                        &format!("pending interaction content for {session_id}: {error}"),
                    );
                }
            };
            Some(RingingV2PendingInteractionView {
                id: interaction.interaction_id.as_str().to_string(),
                kind: interaction.kind,
                // 与 permission 的 `details_unavailable` 同语义：正文取不到时
                // 仍保留可答复的 id/kind，details 降级为 null。
                details,
            })
        }
        None => None,
    };

    // wire 形状（键恒在、空槽为 null）由 `RingingV2PendingApprovals` DTO 锁定；
    // device 面从这里序列化后派发 opaque challenge（形状刻意不与 admin 混用）。
    let pending_json = RingingV2PendingApprovals {
        pending_permission,
        pending_interaction,
    };
    // admin（桌面宿主）：维持现状透传 canonical id（宿主已在壳侧做映射）。
    if identity.is_admin() {
        return json_response(StatusCode::OK, &pending_json);
    }
    // device（原生 app，半可信）：canonical id 不出 daemon，改发不透明 challenge。
    // wire 形状（challenge_id/kind/expires_in/details）由 `RingingV2DeviceChallenges`
    // DTO 锁定，刻意不与 admin 面混用。
    match state.challenges.issue_views(&session_id, &pending_json) {
        Ok(views) => json_response(
            StatusCode::OK,
            &RingingV2DeviceChallenges { challenges: views },
        ),
        Err(code) => {
            api_error_response(StatusCode::BAD_GATEWAY, code, "approval projection failed")
        }
    }
}

/// `POST /ringing/v2/sessions/{seed}/approvals/respond`（S5）— device 侧审批应答。
///
/// body：`RingingV2ApprovalRespondRequest`（challenge_id/decision/payload）。
/// device 身份：一次性消费 challenge（scope = 本会话）→ `command_for` 映射回 canonical
/// 命令 → 进 `execute_command`（保留「审批应答不受 driver 门控」语义）。
/// **admin 不使用本端点**——桌面宿主维持 canonical 透传命令通道。
///
/// spec §9 描述了本流程但未定 wire 端点，此为本 spec 的补充设计。
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
    let request: RingingV2ApprovalRespondRequest = match serde_json::from_slice(&body) {
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
            return api_error_response(StatusCode::BAD_REQUEST, code, "invalid approval decision");
        }
    };
    // §9.5 远程 `trust_folder` 默认拒：非 admin 提交的永久扩界降级为单次放行。
    if !identity.is_admin()
        && let qaqh_ringing::RingingCommand::ToolPermissionRespond { trust_folder, .. } =
            &mut command
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
        qaqh_ringing_contract::ulid::generate_ulid(),
        qaqh_ringing::CommandTarget::Session {
            session_id: session_id.clone(),
        },
        command,
    )
    .with_client_identity(instance_key, caller);
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
    session_id: &str,
    interaction: &InteractionView,
) -> Result<Option<RingingV2PendingPermissionView>, qaqh_session::db::DbError> {
    let Some(tool_call_id) = interaction
        .call_id
        .as_ref()
        .map(|call_id| call_id.as_str().to_string())
    else {
        return Ok(None);
    };
    let body = interaction_body_value(state, session_id, &interaction.request)?;
    let field = |name: &str| body.as_ref().and_then(|b| b.get(name)).cloned();
    Ok(Some(RingingV2PendingPermissionView {
        tool_call_id,
        tool_name: field("tool_name"),
        action_summary: field("action_summary"),
        reason: field("reason"),
        paths: field("paths").unwrap_or_else(|| serde_json::json!([])),
        category: field("category"),
        level: field("level"),
        risk: field("risk"),
        consequence: field("consequence"),
        details_unavailable: body.is_none(),
    }))
}

/// Parse an interaction body from inline JSON or its session-scoped SQLite content row.
fn interaction_body_value(
    state: &AppState,
    session_id: &str,
    request: &ContentValue,
) -> Result<Option<serde_json::Value>, qaqh_session::db::DbError> {
    match request {
        ContentValue::Inline { text } => Ok(serde_json::from_str(text).ok()),
        ContentValue::Ref { content_ref } => {
            // canonical ref 是 `sha256:<hex>`，SQLite content id 是裸 hex。
            let store_id = content_ref.hash().as_str();
            let store_id = store_id.strip_prefix("sha256:").unwrap_or(store_id);
            let Some(entry) = state.hub.get_content(session_id, store_id)? else {
                return Ok(None);
            };
            Ok(serde_json::from_slice(&entry.bytes).ok())
        }
        ContentValue::Unavailable(_) => Ok(None),
    }
}

/// 每 seed 一条 SSE(报告二 §5)。
///
/// 帧载荷是类型化 `SessionEventFrame`,`id: {epoch}:{seq}` 即同步游标。
/// owner 广播无回放尾:`since_cursor` 落后于当前水位即下发
/// `ringing.resync_required` 要求客户端重新 bootstrap;lagged 同理。
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
    // 游标格式 `{epoch}:{seq}`;解析为 (epoch 是否匹配, seq)。裸 seq 视为
    // 当前 epoch 的水位;不可解析 = 无游标(live,重叠由客户端幂等吸收)。
    if let Err(response) = require_business_session(state.service.owners(), &session_id).await {
        return response;
    }
    let (cursor_epoch_matches, since_seq) = match query.since_cursor.as_deref() {
        None => (true, None),
        Some(cursor) => match cursor.rsplit_once(':') {
            Some((cursor_epoch, seq)) => (cursor_epoch == state.epoch, seq.parse::<u64>().ok()),
            None => (true, cursor.parse::<u64>().ok()),
        },
    };
    let owner = state.service.owners().get_or_create(&session_id);
    let mut subscription = owner.subscribe();
    // resync 判定(B01):订阅后读实时位置 P(`current_seq` = 下一个将分配的
    // seq,纯内存原子,不查库)。游标 epoch 不匹配 → 客户端持旧代状态,必须
    // resync;`since < P` → 订阅窗口外有缺口(无回放尾)→ resync;`since >= P`
    // 或无游标 → live 直推,重叠由客户端幂等消费吸收。
    let live_position = owner.current_seq();
    let needs_resync = if !cursor_epoch_matches {
        true
    } else {
        since_seq.is_some_and(|since| since < live_position)
    };

    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Event, Infallible>>(64);
    // 设备吊销(`revoke_device` 摘 lease)必须**杀掉在途 SSE**(spec §6):订阅本身
    // 与 lease 生命周期无关,故循环内每个事件前复查租约存活;失效即下发终止帧并
    // 断流。与 `sse.rs::handle_timeline_events` 的逐事件存活检查同款。
    let leases = state.leases.clone();
    let stream_session = session_id.clone();
    let stream_epoch = state.epoch.clone();
    // B:诊断内存快照直接混进这条会话流,但**只对 admin 订阅者**追加——非 admin
    // 连 ticker 都不建,越权面为零。帧用独立 event 名 `diagnostics.memory`;采集
    // 开关仍由 `diagnostics.memory.start/stop` 控制。
    let diagnostics_service = state.service.clone();
    let diagnostics_admin = identity.is_admin();
    tokio::spawn(async move {
        if needs_resync {
            let data = serde_json::json!({
                "session_id": stream_session,
                "reason": "cursor_gap",
            });
            let frame = Event::default()
                .event("ringing.resync_required")
                .data(data.to_string());
            let _ = tx.send(Ok(frame)).await;
            return;
        }
        let mut diagnostics_tick = diagnostics_admin.then(|| {
            let mut interval = tokio::time::interval(MEMORY_PUSH_INTERVAL);
            interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
            interval
        });
        if let Some(interval) = diagnostics_tick.as_mut() {
            interval.tick().await; // 消费 interval 的立即首跳
        }
        let mut diagnostics_cursor: Option<u64> = None;
        loop {
            // 取消安全:`next()` 只 await 在 broadcast `recv()` 上,select 丢弃
            // 另一个分支不会丢事件。非 admin 时 ticker 分支永久 pending。
            let tick = async {
                match diagnostics_tick.as_mut() {
                    Some(interval) => {
                        interval.tick().await;
                    }
                    None => std::future::pending::<()>().await,
                }
            };
            let item = tokio::select! {
                item = subscription.recv() => Some(item),
                _ = tick => None,
            };
            match item {
                None => {
                    if diagnostics_service.diagnostics_memory_enabled()
                        && let Ok(snapshot) =
                            diagnostics_service.diagnostics_memory_snapshot(diagnostics_cursor)
                    {
                        diagnostics_cursor = snapshot
                            .get("latest_phase_sequence")
                            .and_then(serde_json::Value::as_u64);
                        let frame = Event::default()
                            .event("diagnostics.memory")
                            .data(snapshot.to_string());
                        if tx.send(Ok(frame)).await.is_err() {
                            break;
                        }
                    }
                }
                Some(Ok(frame_event)) => {
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
                    let data = serde_json::to_string(&frame_event).unwrap_or_else(|_| "{}".into());
                    let frame = Event::default()
                        .id(format!("{}:{}", stream_epoch, frame_event.seq))
                        .event("ringing.event")
                        .data(data);
                    if tx.send(Ok(frame)).await.is_err() {
                        break;
                    }
                }
                // lagged:缺口出现,要求客户端重新同步。
                Some(Err(tokio::sync::broadcast::error::RecvError::Lagged(_))) => {
                    let data = serde_json::json!({
                        "session_id": stream_session,
                        "reason": "stream_lagged",
                    });
                    let frame = Event::default()
                        .event("ringing.resync_required")
                        .data(data.to_string());
                    let _ = tx.send(Ok(frame)).await;
                    break;
                }
                Some(Err(tokio::sync::broadcast::error::RecvError::Closed)) => break,
            }
        }
    });

    Sse::new(ReceiverStream::new(rx))
        .keep_alive(KeepAlive::new().interval(Duration::from_secs(15)))
        .into_response()
}

/// Interaction addressed by an interaction-resolution command.
enum InteractionTarget<'a> {
    /// ask / plan commands carry the canonical `int_...` interaction id.
    InteractionId(&'a str),
    /// permission commands carry the canonical `call_...` tool call id.
    CallId(&'a str),
}

fn interaction_target(command: &qaqh_ringing::RingingCommand) -> Option<InteractionTarget<'_>> {
    match command {
        qaqh_ringing::RingingCommand::ToolPermissionRespond { tool_call_id, .. } => {
            Some(InteractionTarget::CallId(tool_call_id))
        }
        qaqh_ringing::RingingCommand::InteractionAskRespond { interaction_id, .. }
        | qaqh_ringing::RingingCommand::InteractionAskDismiss { interaction_id }
        | qaqh_ringing::RingingCommand::PlanReviewRespond { interaction_id, .. } => {
            Some(InteractionTarget::InteractionId(interaction_id))
        }
        _ => None,
    }
}

/// Structured verdict of an already-resolved interaction, if the canonical
/// control projection has one. Legacy facts carry no verdict → `None`, and the
/// command falls through to the worker's existing rejection path.
pub(super) async fn resolved_interaction_existing(
    state: &AppState,
    session_id: &str,
    command: &qaqh_ringing::RingingCommand,
) -> Option<RingingV2ExistingResult> {
    let target = interaction_target(command)?;
    let owner = state.service.owners().get_or_create(session_id);
    let snapshot = owner.snapshot().await.ok()?;
    let interactions = snapshot.interactions;
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
    let decision = interaction.resolution.as_ref()?.decision;
    let result = command_result_for(
        interaction.kind,
        interaction.interaction_id.as_str(),
        decision,
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
pub(super) fn existing_v2_receipt(
    state: &AppState,
    headers: &HeaderMap,
    command_id: &str,
) -> Option<qaqh_ringing_runtime::ExistingCommandReceipt> {
    let session_id = get_session_id(headers)?;
    // §5：按稳定身份键查回执——换连接（重开 lease）后仍能对账原命令。
    let owner_key = state
        .leases
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .instance_for_session(&session_id)?;
    state
        .pending
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .existing_receipt_for_session(command_id, &owner_key)
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
    // §5：按稳定身份键查回执（换连接后仍可对账原命令）。
    let owner_key = state
        .leases
        .lock()
        .unwrap_or_else(|error| error.into_inner())
        .instance_for_session(&session_id);
    let Some(status) = owner_key.and_then(|owner_key| {
        state
            .pending
            .lock()
            .unwrap_or_else(|error| error.into_inner())
            .v2_status_for_session(&command_id, &owner_key)
    }) else {
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
