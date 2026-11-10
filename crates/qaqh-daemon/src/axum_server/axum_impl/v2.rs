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
use qaqh_ringing_contract::state::{InteractionView, PermissionRequestDetails, SessionBootstrap};
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
        .open(client_session_id.clone(), instance_key, request.client_kind);
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
/// 正文降级只有一种合法来源：**内容确实取不到**（`ContentValue::Unavailable`，或 ref
/// 指向的正文行已被淘汰）⇒ `details_unavailable: true` / `details: null`，条目仍在
/// （id 是答复所必需的）。正文存在却读不成 JSON、或 JSON 不合
/// `PermissionRequestDetails` 形状 ⇒ **带错误码的显式失败**，不再 `.ok()` 成 None：
/// 那种降级把形状漂移伪装成"没有详情"，HTTP 仍 200，没人知道契约断了（ADR-0004 §决策
/// 4；错误码表见 `ApprovalBodyError::code`，由 `coverage.json` 的 approvals.admin
/// `wire_notes` 钉住）。
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
                    error.code(),
                    &format!("pending permission details for {session_id}: {error}"),
                );
            }
        },
        None => None,
    };
    let interaction = interactions
        .iter()
        .filter(pending)
        .find(|interaction| interaction.kind == InteractionKind::Ask);
    let pending_interaction = match interaction {
        Some(interaction) => {
            let details = match interaction_body_value(&state, &session_id, &interaction.request) {
                Ok(details) => details,
                Err(error) => {
                    return api_error_response(
                        StatusCode::INTERNAL_SERVER_ERROR,
                        error.code(),
                        &format!("pending interaction body for {session_id}: {error}"),
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
    execute_command(&state, &headers, &identity, envelope).await
}

fn pending_permission_view(
    state: &AppState,
    session_id: &str,
    interaction: &InteractionView,
) -> Result<Option<RingingV2PendingPermissionView>, ApprovalBodyError> {
    let Some(tool_call_id) = interaction
        .call_id
        .as_ref()
        .map(|call_id| call_id.as_str().to_string())
    else {
        return Ok(None);
    };
    let body = interaction_body_value(state, session_id, &interaction.request)?;
    let details = permission_request_details(session_id, body)?;
    Ok(Some(RingingV2PendingPermissionView {
        tool_call_id,
        tool_name: details
            .as_ref()
            .map(|value| serde_json::json!(value.tool_name)),
        action_summary: details
            .as_ref()
            .and_then(|value| value.action_summary.as_ref())
            .map(|value| serde_json::json!(value)),
        reason: details
            .as_ref()
            .map(|value| serde_json::json!(value.reason)),
        paths: details
            .as_ref()
            .map(|value| serde_json::json!(value.paths))
            .unwrap_or_else(|| serde_json::json!([])),
        category: details
            .as_ref()
            .map(|value| serde_json::json!(value.category)),
        level: details.as_ref().map(|value| serde_json::json!(value.level)),
        risk: details.as_ref().map(|value| serde_json::json!(value.risk)),
        consequence: details
            .as_ref()
            .map(|value| serde_json::json!(value.consequence)),
        details_unavailable: details.is_none(),
    }))
}

fn interaction_body_value(
    state: &AppState,
    session_id: &str,
    request: &ContentValue,
) -> Result<Option<serde_json::Value>, ApprovalBodyError> {
    match request {
        // 生产端只写两种正文：permission 的 `PermissionRequestDetails`、ask 的
        // `NormalizedAsk`（都是 JSON）。读不成 JSON 就说明读写两侧形状不一致 =
        // 代码错误，报出来，不许 `.ok()` 成"没有详情"。
        ContentValue::Inline { text } => parse_inline_body(text).map(Some),
        ContentValue::Ref { content_ref } => {
            // canonical ref 是 `sha256:<hex>`，SQLite content id 是裸 hex。
            let store_id = content_ref.hash().as_str();
            let store_id = store_id.strip_prefix("sha256:").unwrap_or(store_id);
            let Some(entry) = state
                .hub
                .get_content(session_id, store_id)
                .map_err(ApprovalBodyError::Store)?
            else {
                // 正文行已被淘汰：这是唯一允许降级的一种"取不到"。
                return Ok(None);
            };
            parse_stored_body(&entry.bytes, store_id).map(Some)
        }
        ContentValue::Unavailable(_) => Ok(None),
    }
}

fn parse_inline_body(text: &str) -> Result<serde_json::Value, ApprovalBodyError> {
    serde_json::from_str(text).map_err(|source| ApprovalBodyError::InlineNotJson { source })
}

fn parse_stored_body(bytes: &[u8], store_id: &str) -> Result<serde_json::Value, ApprovalBodyError> {
    serde_json::from_slice(bytes).map_err(|source| ApprovalBodyError::StoredNotJson {
        store_id: store_id.to_owned(),
        source,
    })
}

/// 取回 permission 详情正文并投影成审批视图的字段来源。
///
/// `Ok(None)` 只有一种来源：正文**合法地取不到**（被淘汰 / `Unavailable`），此时视图
/// 以 `details_unavailable: true` 如实标记。例外是值域不合法（`is_valid()` 为假）——
/// 它形状对、正文也在，但 `details_unavailable` 已经是 fail-closed 闸（device 侧
/// approve/trust 会被 `permission_details_unavailable` 拒，只留 reject），所以走"类型化
/// 回退 + 带错误码的 warn"，而不是让整张待审批卡连带可答复的 id 一起消失。
fn permission_request_details(
    session_id: &str,
    body: Option<serde_json::Value>,
) -> Result<Option<PermissionRequestDetails>, ApprovalBodyError> {
    let Some(body) = body else {
        return Ok(None);
    };
    let details = serde_json::from_value::<PermissionRequestDetails>(body)
        .map_err(|source| ApprovalBodyError::PermissionShape { source })?;
    if details.is_valid() {
        return Ok(Some(details));
    }
    let error = ApprovalBodyError::PermissionIncomplete;
    log::warn!("[v2] session {session_id}: {} — {error}", error.code());
    Ok(None)
}

/// 审批投影读正文时的一切失败。分级即 ADR-0004 §决策 4 的边界：**存储侧取不到可以
/// 降级，形状侧读不通必须报错**。
#[derive(Debug)]
pub(crate) enum ApprovalBodyError {
    /// SQLite 正文行读不回来（存储故障，与 wire 形状无关）。
    Store(qaqh_session::db::DbError),
    /// `ContentValue::Inline` 的正文不是合法 JSON。
    InlineNotJson { source: serde_json::Error },
    /// content store 里的字节不是合法 JSON。
    StoredNotJson {
        store_id: String,
        source: serde_json::Error,
    },
    /// permission 正文不是 `PermissionRequestDetails` 的形状。该结构带
    /// `deny_unknown_fields` ⇒ 多一个字段也算漂移，正是原先被 `.ok()` 吞掉的那一类。
    PermissionShape { source: serde_json::Error },
    /// 形状对但值域不合法（`PermissionRequestDetails::is_valid()` 的语义原样保留）。
    PermissionIncomplete,
}

impl ApprovalBodyError {
    /// HTTP 面上的错误码。这张表由 `coverage.json` 的 approvals.admin `wire_notes`
    /// 钉住（导出器写，不手抄进金标准）。
    pub(crate) fn code(&self) -> &'static str {
        match self {
            Self::Store(_) => "content_unavailable",
            Self::InlineNotJson { .. } | Self::StoredNotJson { .. } => "interaction_body_not_json",
            Self::PermissionShape { .. } => "permission_details_shape_invalid",
            Self::PermissionIncomplete => "permission_details_incomplete",
        }
    }
}

impl std::fmt::Display for ApprovalBodyError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Store(error) => write!(formatter, "content store read failed: {error}"),
            Self::InlineNotJson { source } => {
                write!(formatter, "inline interaction body is not JSON: {source}")
            }
            Self::StoredNotJson { store_id, source } => write!(
                formatter,
                "stored interaction body {store_id} is not JSON: {source}"
            ),
            Self::PermissionShape { source } => write!(
                formatter,
                "permission body does not match PermissionRequestDetails: {source}"
            ),
            Self::PermissionIncomplete => write!(
                formatter,
                "permission body violates PermissionRequestDetails::is_valid()"
            ),
        }
    }
}

#[cfg(test)]
mod permission_request_details_tests {
    use super::{
        ApprovalBodyError, parse_inline_body, parse_stored_body, permission_request_details,
    };
    use serde_json::json;

    fn valid_body() -> serde_json::Value {
        json!({
            "tool_name": "exec_command",
            "action_summary": "command: \"cargo check\"",
            "reason": "Execution requires confirmation",
            "paths": [],
            "category": "exec",
            "level": 1,
            "risk": "high",
            "consequence": "May execute arbitrary actions."
        })
    }

    #[test]
    fn permission_details_require_the_ringing_v2_body_shape() {
        let details = permission_request_details("session-fixture", Some(valid_body()))
            .expect("structured permission details should project")
            .expect("a valid body projects to details");
        assert_eq!(details.tool_name, "exec_command");
        assert_eq!(
            details.action_summary.as_deref(),
            Some("command: \"cargo check\"")
        );
        assert_eq!(details.level, 1);
    }

    /// 形状漂移（旧 v1 的裸字符串正文、`deny_unknown_fields` 撞上的多余字段）必须
    /// 报错，不许降级成"没有详情"——这正是 ADR-0004 §发现 里那个"多一个字段就整块
    /// 消失但 HTTP 200"的 fail-open。
    #[test]
    fn v2_permission_shape_drift_is_a_loud_error() {
        let error = permission_request_details(
            "session-fixture",
            Some(json!("Execution requires confirmation")),
        )
        .expect_err("a bare string is not PermissionRequestDetails");
        assert_eq!(error.code(), "permission_details_shape_invalid");

        // 生产端不会写 `normalized_args`（完整工具入参可能含密钥,刻意排除在外）；
        // `deny_unknown_fields` 因此把它判成形状漂移。
        let drifted = json!({
            "tool_name": "exec_command",
            "action_summary": "command: \"cargo check\"",
            "reason": "Execution requires confirmation",
            "paths": [],
            "category": "exec",
            "level": 1,
            "risk": "high",
            "consequence": "May execute arbitrary actions.",
            "normalized_args": {},
        });
        let error = permission_request_details("session-fixture", Some(drifted))
            .expect_err("deny_unknown_fields makes an extra field shape drift");
        assert_eq!(error.code(), "permission_details_shape_invalid");
    }

    /// `is_valid()` 的语义原样保留：形状对、值域不合法仍是 fail-closed 的
    /// "详情不可用"（卡与可答复的 id 都还在），只是不再无声。
    #[test]
    fn v2_incomplete_body_still_degrades_fail_closed() {
        // `level` 的合法值域是 1..=4（唯一权威是 is_valid()，此处不重抄表）。
        let incomplete = json!({
            "tool_name": "exec_command",
            "reason": "Execution requires confirmation",
            "paths": [],
            "category": "exec",
            "level": 0,
            "risk": "high",
            "consequence": "May execute arbitrary actions."
        });
        let details = permission_request_details("session-fixture", Some(incomplete))
            .expect("a well-shaped body is not an HTTP failure");
        assert!(
            details.is_none(),
            "incomplete body must not project as details"
        );
    }

    #[test]
    fn v2_absent_body_is_the_only_legitimate_degradation() {
        assert!(
            permission_request_details("session-fixture", None)
                .expect("absent body is not an error")
                .is_none()
        );
    }

    #[test]
    fn v2_non_json_bodies_report_their_origin() {
        let error = parse_inline_body("Execution requires confirmation")
            .expect_err("inline text must be JSON interaction body");
        assert_eq!(error.code(), "interaction_body_not_json");
        assert!(matches!(error, ApprovalBodyError::InlineNotJson { .. }));

        let error = parse_stored_body(b"{not json", "deadbeef")
            .expect_err("stored bytes must be JSON interaction body");
        assert_eq!(error.code(), "interaction_body_not_json");
        assert!(
            error.to_string().contains("deadbeef"),
            "错误消息要能指回具体的正文行：{error}"
        );
        assert!(matches!(error, ApprovalBodyError::StoredNotJson { .. }));
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
        | qaqh_ringing::RingingCommand::InteractionAskDismiss { interaction_id } => {
            Some(InteractionTarget::InteractionId(interaction_id))
        }
        _ => None,
    }
}

/// 已裁决交互的三种结局。**读不出投影 ≠ 没有裁决**：把读取故障压成"没有裁决"再往下放，
/// 等于在存储故障时可能重复执行一次副作用命令——而幂等语义要求"未知就说未知"
/// （`coverage.json` 的 `receipt_policy.outcome_unknown`）。原来的
/// `owner.snapshot().await.ok()?` 干的正是这件事。
pub(super) enum ResolvedInteraction {
    /// 有结构化裁决结论，直接作为 existing 回放（重放路径）。
    Verdict(RingingV2ExistingResult),
    /// 投影读到了，但这条交互没有裁决（legacy facts 或未裁决）⇒ 交给 worker。
    None,
    /// 投影读不出来 ⇒ 调用方必须按"结果未知"回答，**且不得写终态回执**。
    Unavailable(String),
}

pub(super) async fn resolved_interaction_existing(
    state: &AppState,
    session_id: &str,
    command: &qaqh_ringing::RingingCommand,
) -> ResolvedInteraction {
    let Some(target) = interaction_target(command) else {
        return ResolvedInteraction::None;
    };
    let owner = state.service.owners().get_or_create(session_id);
    let snapshot = match owner.snapshot().await {
        Ok(snapshot) => snapshot,
        Err(error) => return ResolvedInteraction::Unavailable(error.to_string()),
    };
    let interactions = snapshot.interactions;
    let found = match target {
        InteractionTarget::InteractionId(id) => interactions
            .iter()
            .find(|interaction| interaction.interaction_id.as_str() == id),
        InteractionTarget::CallId(id) => interactions.iter().find(|interaction| {
            interaction
                .call_id
                .as_ref()
                .is_some_and(|call_id| call_id.as_str() == id)
        }),
    };
    let Some(interaction) = found else {
        return ResolvedInteraction::None;
    };
    let Some(resolution) = interaction.resolution.as_ref() else {
        return ResolvedInteraction::None;
    };
    let Some(result) = command_result_for(
        interaction.kind,
        interaction.interaction_id.as_str(),
        resolution.decision,
    ) else {
        return ResolvedInteraction::None;
    };
    ResolvedInteraction::Verdict(RingingV2ExistingResult::InteractionResolved { result })
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
        // 错误体只有两个字符串，编码不可能失败；真失败了就是 serde_json 自身出事。
        // 这里绝不允许 `unwrap_or_default()` —— 那会发一个"状态码说错了、内容说不出"
        // 的响应，客户端连 code 都读不到（宪法 5：报错别静默）。
        .expect("error body is two JSON strings; encoding cannot fail"),
    )
        .into_response()
}

pub(crate) fn json_response<T: Serialize>(status: StatusCode, value: &T) -> Response {
    match serde_json::to_vec(value) {
        Ok(body) => (status, [(header::CONTENT_TYPE, "application/json")], body).into_response(),
        Err(error) => {
            // 编码失败绝不等于"发一个 200 空体"：客户端只会拿到一段无法解析的 JSON，
            // 而所有守卫看不见任何东西（ADR-0004 §决策 4 的同族，只是它在出站这一侧）。
            // 可达性（本轮实测，纠正我自己写错过一次的判断）：非有限浮点**不走这条 Err 路** ——
            // `to_value(NaN)` = `Ok(Null)`，`to_string({ratio: NaN})` = `Ok("{\"ratio\":null}")`，
            // ∞ 同理（`ser.rs` 的 `FpCategory::Nan | Infinite => write_null`）。所以这条分支
            // 只服务真正的编码器故障（非法 map 键、自定义 `Serialize` 报错），有测试用合成
            // 编码器钉住它。NaN 的洞因此**不是**"发空体"，而是"静默换成 null"：见
            // ADR-0004 的 `config.load` 值域校验待办。
            let code = "response_serialization_failed";
            log::error!("[v2] {code}: {error}");
            api_error_response(
                StatusCode::INTERNAL_SERVER_ERROR,
                code,
                &format!("failed to encode response body: {error}"),
            )
        }
    }
}

#[cfg(test)]
mod response_encoding_tests {
    use super::{api_error_response, json_response};
    use axum::http::StatusCode;

    /// 合成一个"编码器自己报错"的类型：这才是 `json_response` 的 Err 分支的真触发条件。
    /// （不要用 NaN 当触发器 —— 它压根不报错，见下面的 null 测试。）
    struct Broken;

    impl serde::Serialize for Broken {
        fn serialize<S: serde::Serializer>(&self, _serializer: S) -> Result<S::Ok, S::Error> {
            Err(serde::ser::Error::custom("synthetic encoder failure"))
        }
    }

    /// 编码失败必须显式失败，不许塌成"200 + 空体"。
    #[tokio::test]
    async fn v2_json_response_never_returns_an_empty_success_body() {
        let response = json_response(StatusCode::OK, &Broken);
        assert_eq!(
            response.status(),
            StatusCode::INTERNAL_SERVER_ERROR,
            "编码器报错时必须是失败，不是看起来成功的空体"
        );
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .expect("body");
        assert!(!bytes.is_empty(), "错误体必须真的带内容");
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).expect("error body is JSON");
        assert_eq!(parsed["code"], "response_serialization_failed");
    }

    /// 我上一版把这条写反了（断成 `to_vec(NaN)` 会报错 ⇒ CI 打红）。实测正确的形状是
    /// **两条路都成功、都把非有限浮点写成 `null`** —— 这条测试留着，就是为了让下一个
    /// 人不再凭记忆写它，同时它给出 ADR-0004 `config.load` 待办的真正症状：静默换值。
    #[test]
    fn v2_to_value_collapses_non_finite_floats_to_null() {
        #[derive(serde::Serialize)]
        struct Gauge {
            ratio: f64,
        }
        assert_eq!(
            serde_json::to_value(f64::NAN).expect("to_value accepts any f64"),
            serde_json::Value::Null
        );
        assert_eq!(
            serde_json::to_string(&Gauge { ratio: f64::NAN }).expect("NaN encodes"),
            r#"{"ratio":null}"#,
            "NaN 不报错，被静默换成 null"
        );
        assert_eq!(
            serde_json::to_string(&Gauge {
                ratio: f64::NEG_INFINITY
            })
            .expect("∞ encodes"),
            r#"{"ratio":null}"#
        );
    }

    /// 错误体本身不可为空的另一例：`api_error_response` 只拼两个字符串，编码不可失败，
    /// 所以它用 `expect` 而不是发一段空 body（客户端连 code 都读不到）。
    #[tokio::test]
    async fn v2_api_error_response_always_carries_a_code() {
        let response = api_error_response(StatusCode::BAD_GATEWAY, "state_unavailable", "boom");
        let bytes = axum::body::to_bytes(response.into_body(), 4096)
            .await
            .expect("body");
        let parsed: serde_json::Value = serde_json::from_slice(&bytes).expect("error body is JSON");
        assert_eq!(parsed["code"], "state_unavailable");
    }
}
