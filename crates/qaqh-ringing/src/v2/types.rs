//! Ringing v2 wire contract.
//!
//! RingingV2 command and transport wire types. Session state and business events
//! are defined in `qaqh-ringing-contract`.

use serde::{Deserialize, Serialize};
#[cfg(feature = "ts")]
use ts_rs::TS;

use crate::command::RingingCommand;
use crate::envelope::{RingingCommandAckStatus, RingingCommandState};
use crate::protocol::{RINGING_SCHEMA, is_safe_integer};
use crate::v2::{RINGING_V2_BASE_PATH, RINGING_V2_VERSION};

pub use qaqh_types::SettingValue;

/// 命令信封中的唯一目标位置。执行会话目标携带强类型 `ExecSessionId`;
/// 整数 registry ID 与系统 PID 不是操作凭据。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub enum CommandTarget {
    Global,
    Session { session_id: String },
    Workspace { workspace_id: String },
    ExecSession { id: qaqh_types::ExecSessionId },
}

/// 资源 revision 与 driver epoch 由命令信封携带。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub enum CommandPrecondition {
    SettingsRevision { revision: u64 },
    ConfigRevision { revision: u64 },
}

/// 命令目录代际。字段和语义变更需显式升级。
pub const COMMAND_CONTRACT_REVISION: &str = "ringing-commands-7";

/// 会话读模型/事件词表契约代际(spec §11)。管理事件与读模型的破坏性变化;
/// open 与 bootstrap 均携带,客户端必须严格校验。首个显式修订 = ringing-session-2,
/// 当前无该字段的读模型视为隐式第 1 代。
pub const SESSION_CONTRACT_REVISION: &str = "ringing-session-2";

/// RingingV2 唯一公共命令信封。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct RingingV2CommandEnvelope {
    pub schema: String,
    pub version: u32,
    pub contract_revision: String,
    pub command_id: String,
    pub client_instance_id: String,
    pub client_session_id: String,
    pub target: CommandTarget,
    pub command: RingingCommand,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub preconditions: Vec<CommandPrecondition>,
}

impl RingingV2CommandEnvelope {
    pub fn new(
        command_id: impl Into<String>,
        target: CommandTarget,
        command: RingingCommand,
    ) -> Self {
        Self {
            schema: RINGING_SCHEMA.to_string(),
            version: RINGING_V2_VERSION,
            contract_revision: COMMAND_CONTRACT_REVISION.to_string(),
            command_id: command_id.into(),
            client_instance_id: String::new(),
            client_session_id: String::new(),
            target,
            command,
            preconditions: Vec::new(),
        }
    }

    pub fn with_client_identity(
        mut self,
        client_instance_id: impl Into<String>,
        client_session_id: impl Into<String>,
    ) -> Self {
        self.client_instance_id = client_instance_id.into();
        self.client_session_id = client_session_id.into();
        self
    }

    pub fn with_precondition(mut self, precondition: CommandPrecondition) -> Self {
        self.preconditions.push(precondition);
        self
    }

    pub fn session_id(&self) -> Option<&str> {
        match &self.target {
            CommandTarget::Session { session_id } => Some(session_id),
            _ => None,
        }
    }

    pub fn precondition(&self, kind: &str) -> Option<u64> {
        self.preconditions
            .iter()
            .find_map(|item| match (kind, item) {
                ("settings_revision", CommandPrecondition::SettingsRevision { revision })
                | ("config_revision", CommandPrecondition::ConfigRevision { revision }) => {
                    Some(*revision)
                }
                _ => None,
            })
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        if self.schema != RINGING_SCHEMA || self.version != RINGING_V2_VERSION {
            return Err("invalid_command_envelope");
        }
        if self.contract_revision != COMMAND_CONTRACT_REVISION {
            return Err("unsupported_contract_revision");
        }
        if self.command_id.trim().is_empty()
            || self.client_instance_id.trim().is_empty()
            || self.client_session_id.trim().is_empty()
        {
            return Err("invalid_command_envelope");
        }
        if self.command.requires_session_target()
            && !matches!(self.target, CommandTarget::Session { .. })
        {
            return Err("target_mismatch");
        }
        if self.command.requires_exec_session_target()
            && !matches!(self.target, CommandTarget::ExecSession { .. })
        {
            return Err("target_mismatch");
        }
        if self.command.is_global_operation() && !matches!(self.target, CommandTarget::Global) {
            return Err("target_mismatch");
        }
        if matches!(
            self.command,
            RingingCommand::WorkspaceRename { .. } | RingingCommand::WorkspaceDelete {}
        ) && !matches!(self.target, CommandTarget::Workspace { .. })
        {
            return Err("target_mismatch");
        }
        if matches!(&self.command, RingingCommand::SessionRename { title } if title.trim().is_empty())
        {
            return Err("invalid_title");
        }
        let mut kinds = std::collections::HashSet::new();
        for precondition in &self.preconditions {
            let (kind, revision) = match precondition {
                CommandPrecondition::SettingsRevision { revision } => {
                    ("settings_revision", *revision)
                }
                CommandPrecondition::ConfigRevision { revision } => ("config_revision", *revision),
            };
            if !is_safe_integer(revision) {
                return Err("invalid_precondition_revision");
            }
            if !kinds.insert(kind) {
                return Err("duplicate_precondition");
            }
        }
        if self.command.requires_settings_revision()
            && self.precondition("settings_revision").is_none()
        {
            return Err("missing_settings_revision");
        }
        if self.command.requires_config_revision() && self.precondition("config_revision").is_none()
        {
            return Err("missing_config_revision");
        }
        Ok(())
    }
}

impl RingingCommand {
    pub fn requires_session_target(&self) -> bool {
        matches!(
            self,
            Self::SessionSettingsPatch { .. }
                | Self::SessionResume {}
                | Self::SessionAttach {}
                | Self::SessionClose {}
                | Self::SessionRename { .. }
                | Self::SessionArchive {}
                | Self::SessionUnarchive {}
                | Self::SessionDelete {}
                | Self::SessionShutdown
                | Self::AgentReloadConfig
                | Self::ConversationSendMessage(_)
                | Self::ConversationCancel { .. }
                | Self::InteractionAskRespond { .. }
                | Self::InteractionAskDismiss { .. }
                | Self::DriverClaim { .. }
                | Self::DriverRelease { .. }
                | Self::ToolPermissionRespond { .. }
                | Self::WorkspaceSet { .. }
                | Self::WorkspaceMoveSession { .. }
                | Self::WorkspaceDetach { .. }
                | Self::TodoSet { .. }
                | Self::TodoCancel { .. }
                | Self::PlanAction { .. }
        )
    }

    pub fn requires_settings_revision(&self) -> bool {
        matches!(self, Self::SessionSettingsPatch { .. })
    }

    /// 执行会话统一控制命令只接受 ExecSession target(spec §6.2)。
    pub fn requires_exec_session_target(&self) -> bool {
        matches!(
            self,
            Self::ExecSessionInterrupt {} | Self::ExecSessionTerminate {}
        )
    }

    pub fn requires_config_revision(&self) -> bool {
        matches!(
            self,
            Self::ProviderUpsert { .. }
                | Self::ProviderDelete { .. }
                | Self::ProviderModelUpsert { .. }
                | Self::ProviderModelDelete { .. }
                | Self::ModelSelectionSetDefault { .. }
        )
    }

    pub fn is_global_operation(&self) -> bool {
        matches!(
            self,
            Self::SessionCreate { .. }
                | Self::ProviderList { .. }
                | Self::ProviderPresets { .. }
                | Self::ProviderUpsert { .. }
                | Self::ProviderDelete { .. }
                | Self::ProviderModelUpsert { .. }
                | Self::ProviderModelDelete { .. }
                | Self::ModelSelectionSetDefault { .. }
                | Self::ConfigSave { .. }
                | Self::ConfigSetPermissionLevel { .. }
                | Self::WorkspaceCreate { .. }
                | Self::DiagnosticsMemoryStart { .. }
                | Self::DiagnosticsMemoryStop { .. }
                | Self::SubagentSpawn { .. }
        )
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct SessionSettingsPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model_selection: Option<SettingValue<qaqh_types::ModelSelection>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tools: Option<ToolsSettingPatch>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct SessionCreateResult {
    pub session_id: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "mode", rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub enum ToolsSettingPatch {
    Default,
    All,
    None,
    Custom { names: Vec<String> },
    Preset { name: String },
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct ProviderUpsertDto {
    pub name: String,
    pub base_url: String,
    pub wire: qaqh_types::Wire,
    pub credential: qaqh_types::CredentialSource,
    pub headers: std::collections::BTreeMap<String, String>,
    pub compat: Option<qaqh_types::EndpointCompat>,
    pub api_key: SecretWrite,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "operation", content = "value", rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub enum SecretWrite {
    #[default]
    Keep,
    Set(String),
    Clear,
}

/// v2 客户端自报种类(open 时声明;与 `client_instance_id` 同语义:纯
/// presence/诊断,不参与任何信任判定)。未声明即不出现在 wire 上。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
#[serde(rename_all = "snake_case")]
pub enum RingingV2ClientKind {
    Tui,
    Desktop,
    /// ACP 桥(Zed 等 ACP 宿主拉起的 stdio 进程)。
    Acp,
}

/// `clients.list` 的单条活跃连接视图。归属按稳定身份记账,同一身份的
/// 多条连接(多窗口)各占一行。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct RingingV2ClientView {
    pub client_session_id: String,
    /// 稳定身份(device token 反推的 `device_id`,admin 为自报 id)。
    pub instance_key: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_kind: Option<RingingV2ClientKind>,
    /// 该身份名下 attach 的会话数。
    pub attached_sessions: usize,
    /// 距 lease 过期剩余毫秒;调用方以此决定展示与轮询节奏。
    pub expires_in_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct RingingV2OpenRequest {
    pub schema: String,
    pub version: u32,
    pub client_instance_id: String,
    /// 客户端预期的命令目录代际(spec §11):服务端在分配 lease 前校验;
    /// 缺字段/不匹配的旧客户端在 open 阶段即失败,不进入半兼容消费。
    pub command_contract_revision: String,
    /// 客户端预期的读模型/事件词表代际;同样 open 阶段预检。
    pub session_contract_revision: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub client_kind: Option<RingingV2ClientKind>,
}

impl RingingV2OpenRequest {
    pub fn new(client_instance_id: impl Into<String>) -> Self {
        Self {
            schema: RINGING_SCHEMA.to_string(),
            version: RINGING_V2_VERSION,
            client_instance_id: client_instance_id.into(),
            command_contract_revision: COMMAND_CONTRACT_REVISION.to_string(),
            session_contract_revision: SESSION_CONTRACT_REVISION.to_string(),
            client_kind: None,
        }
    }

    /// 声明客户端种类(presence/诊断;不参与信任判定)。
    pub fn with_kind(mut self, kind: RingingV2ClientKind) -> Self {
        self.client_kind = Some(kind);
        self
    }

    /// Schema/version/revision gate mirroring the daemon open handler's
    /// `UPGRADE_REQUIRED` rejection; diagnostics-only `client_instance_id`
    /// must still be non-empty.
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.schema != RINGING_SCHEMA
            || self.version != RINGING_V2_VERSION
            || self.client_instance_id.trim().is_empty()
        {
            return Err("invalid_open_request");
        }
        if self.command_contract_revision != COMMAND_CONTRACT_REVISION
            || self.session_contract_revision != SESSION_CONTRACT_REVISION
        {
            return Err("unsupported_contract_revision");
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct RingingV2Capabilities {
    pub subscribe: bool,
    pub interact: bool,
    pub service: bool,
    pub content: bool,
    /// 单会话 SSE 流能力。
    ///
    /// `true` = `GET /ringing/v2/sessions/{seed}/events` 返回一条按序号递增的
    /// `SessionEventFrame` 流，业务类型由帧自身携带。
    ///
    pub single_stream: bool,
    /// QR pairing endpoints are available.
    pub pairing: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct RingingV2OpenResponse {
    pub schema: String,
    pub version: u32,
    pub accepted: bool,
    pub client_session_id: String,
    pub server_epoch: String,
    pub lease_ttl_ms: u64,
    pub renew_interval_ms: u64,
    pub capabilities: RingingV2Capabilities,
    /// 命令目录代际(B07):客户端在发送命令前校验;不匹配即给出可读错误,
    /// 而不是等到命令被 `unsupported_contract_revision` 拒绝才发现不兼容。
    pub command_contract_revision: String,
    /// 会话读模型/事件词表代际(`SESSION_CONTRACT_REVISION`):事件与
    /// bootstrap 形状由此管理;客户端严格校验,不匹配即拒绝连接。
    pub session_contract_revision: String,
}

impl RingingV2OpenResponse {
    pub fn validate(&self) -> Result<(), &'static str> {
        if self.schema != RINGING_SCHEMA
            || self.version != RINGING_V2_VERSION
            || self.client_session_id.trim().is_empty()
            || self.server_epoch.trim().is_empty()
            || self.command_contract_revision.trim().is_empty()
            || self.session_contract_revision.trim().is_empty()
            || !is_safe_integer(self.lease_ttl_ms)
            || !is_safe_integer(self.renew_interval_ms)
        {
            return Err("invalid_open_response");
        }
        if self.command_contract_revision != COMMAND_CONTRACT_REVISION
            || self.session_contract_revision != SESSION_CONTRACT_REVISION
        {
            return Err("unsupported_contract_revision");
        }
        Ok(())
    }
}

// ── 扫码配对（移动端 M0，spec-daemon-auth-devices §11）────────────────────────
//
// v2 冻结线**外追加**，不改既有帧格式。配对是唯一依赖桌面端在线的时刻。

/// `POST /ringing/v2/pairing/tokens` 请求（Admin）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct RingingV2PairTokenRequest {
    /// 桌面端决定的授予档位：`"view"` | `"interact"`。
    pub scope_grant: String,
    #[serde(default)]
    pub device_name: String,
    #[serde(default)]
    pub platform: String,
}

/// 配对令牌响应。`tls_fp` 为自签证书 SPKI 的 `sha256:<hex>`，供原生端 pinning。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct RingingV2PairTokenResponse {
    pub pairing_token: String,
    pub expires_in_ms: u64,
    #[serde(default)]
    pub tls_fp: String,
}

/// `POST /ringing/v2/pair` 请求（无 Bearer，消费一次性 `pairing_token`）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct RingingV2PairRequest {
    pub pairing_token: String,
    #[serde(default)]
    pub device_name: String,
    #[serde(default)]
    pub platform: String,
}

/// 配对成功响应。`device_token` 明文**仅此一次**出现，注册表只存摘要。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct RingingV2PairResponse {
    pub device_id: String,
    pub device_token: String,
    pub scope: String,
    pub daemon_version: String,
    pub protocol_version: u32,
}

/// 设备管理视图（**不含任何 token 材料**）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct RingingV2DeviceWire {
    pub device_id: String,
    pub name: String,
    pub platform: String,
    pub scope: String,
    pub created_at_ms: u64,
    pub last_seen_ms: u64,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct RingingV2DevicesResponse {
    pub devices: Vec<RingingV2DeviceWire>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct RingingV2LeaseRenewResponse {
    pub ok: bool,
    pub lease_ttl_ms: u64,
    pub renew_interval_ms: u64,
}

// ── 本地壳层审批投影 / content 上传响应（B02 handler 契约收口）────────────
//
// `GET /ringing/v2/sessions/{id}/approvals`（admin 桌面宿主面）与
// `POST /ringing/v2/content` 的响应此前由 daemon handler `json!` 拼装；
// 这里提取为一手 DTO，daemon 序列化与 TS 导出共用。device 侧 challenge
// 视图是另一种形状（opaque challenge），刻意不与 admin 面混用。

/// tool permission 待审批视图（admin 面）。
///
/// `tool_name`…`consequence` 是工具请求正文里的动态 JSON 值，契约只承诺
/// 「键存在、值为 JSON」；正文不可用时这些键为 `null`、`paths` 为 `[]`，
/// `details_unavailable` 如实标记。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct RingingV2PendingPermissionView {
    pub tool_call_id: String,
    pub tool_name: Option<serde_json::Value>,
    pub action_summary: Option<serde_json::Value>,
    pub reason: Option<serde_json::Value>,
    pub paths: serde_json::Value,
    pub category: Option<serde_json::Value>,
    pub level: Option<serde_json::Value>,
    pub risk: Option<serde_json::Value>,
    pub consequence: Option<serde_json::Value>,
    pub details_unavailable: bool,
}

/// ask / plan 待审批视图（admin 面）；`details` 为正文 JSON，不可用时为
/// `null`（键仍在）。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct RingingV2PendingInteractionView {
    pub id: String,
    pub kind: qaqh_ringing_contract::fact::InteractionKind,
    pub details: Option<serde_json::Value>,
}

/// `GET /ringing/v2/sessions/{id}/approvals`（admin）响应：两个槽位最多各
/// 一张待办卡；无待办时键仍在、值为 `null`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct RingingV2PendingApprovals {
    pub pending_permission: Option<RingingV2PendingPermissionView>,
    pub pending_interaction: Option<RingingV2PendingInteractionView>,
}

/// device 审批 challenge 的种类。它与 admin 面的 `InteractionKind` 使用两套词表：
/// challenge 侧 `plan` 不带
/// `_review` 后缀、permission 记作 `tool_permission`——wire 原样保留，
/// 不与 admin 混用。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub enum RingingV2DeviceApprovalKind {
    ToolPermission,
    Ask,
    Plan,
}

/// device（原生 app，半可信）视角的一张待审批卡：`challenge_id` 是
/// **不透明**的一次性 token（canonical id 不出 daemon），`details` 是有界
/// 展示白名单。与 admin 面的 [`RingingV2PendingApprovals`] 刻意不同形。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct RingingV2DeviceChallenge {
    pub challenge_id: String,
    pub kind: RingingV2DeviceApprovalKind,
    /// challenge 剩余有效期（秒）；过期后应答会被拒，须重新拉取 pending。
    pub expires_in: u64,
    pub details: serde_json::Value,
}

/// device 身份 `GET .../approvals` 的响应：不透明 challenge 列表。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct RingingV2DeviceChallenges {
    pub challenges: Vec<RingingV2DeviceChallenge>,
}

/// `POST /ringing/v2/sessions/{id}/approvals/respond`（device 侧）请求。
///
/// `decision` 是自由字符串：未知 decision 的拒绝语义（`invalid approval
/// decision`）由 daemon 侧映射负责，不在 DTO 收紧判别——那会把可观察的
/// 错误码改掉。`payload` 缺省为 `null`。
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct RingingV2ApprovalRespondRequest {
    pub challenge_id: String,
    pub decision: String,
    #[serde(default)]
    pub payload: serde_json::Value,
}

/// `POST /ringing/v2/content` 响应。`content_id` 与 `sha256` 都是裸 hex；
/// SDK 组装规范 `ContentRef` 时会加上 `sha256:` 前缀。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct RingingV2ContentUploadResponse {
    pub content_id: String,
    pub media_type: String,
    pub sha256: String,
    pub size: u64,
    pub truncated: bool,
}

/// Server notification that the client must fetch a fresh session bootstrap.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct RingingV2ResyncRequired {
    pub session_id: String,
    pub reason: String,
}

/// v2 command acknowledgement.
///
/// `existing` is populated only when the daemon replays a receipt that is
/// still inside the idempotency TTL — that is, when the caller re-submitted a
/// `command_id` it may have lost the ACK for.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct RingingV2CommandAck {
    pub command_id: String,
    pub status: RingingCommandAckStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub retry_after_ms: Option<u64>,
    /// Already-recorded outcome this submission collided with.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub existing: Option<RingingV2ExistingResult>,
    /// Completed synchronous outcome (统一命令面的 completed 结果；通道命令
    /// 不携带)。JSON 形态由各操作的 result DTO 决定。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<serde_json::Value>,
    /// Operation name (for completed status)。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub operation: Option<String>,
    /// 操作后观察到的资源 revision（配置/设置类操作回带，供客户端对账）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<u64>,
}

/// Why a v2 submission collided with an already-recorded outcome.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub enum RingingV2ExistingResult {
    /// The same `command_id` was submitted again inside the receipt TTL
    /// (typically because the first ACK was lost).
    CommandReceipt {
        state: RingingCommandState,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        terminal_event_id: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error_code: Option<String>,
        /// Typed terminal payload, when the command produced one.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        result: Option<RingingV2CommandResult>,
        /// 完整同步结果回放(B05):首次执行的 result/operation/revision 原样
        /// 恢复;`succeeded` 而此字段缺失 = 引擎类命令,不携带同步结果。
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sync_result: Option<RingingV2SyncResult>,
    },
    /// A different command answered an interaction that was already resolved
    /// (first-answer-wins). Carries the winning verdict so the loser can
    /// reconcile without string-matching `code`.
    InteractionResolved { result: RingingV2CommandResult },
}

/// 同步命令结果的 receipt 回放形状(B05):ACK 丢失后,重放原 `command_id`
/// 或轮询 status 都能拿回首次执行承诺的 result/operation/revision。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct RingingV2SyncResult {
    pub operation: String,
    pub result: serde_json::Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub revision: Option<u64>,
}

/// Typed terminal payload of a command.
///
/// Only outcomes a client must be able to reconcile after losing an ACK are
/// represented. The reliable `causation_id = command_id` event remains the
/// source of truth; this payload exists so idempotent replay and status polling
/// do not degrade to string matching on `code`/`message`.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub enum RingingV2CommandResult {
    /// ask_user interaction was resolved by the command that owns this receipt.
    AskResolved {
        interaction_id: String,
        outcome: RingingV2AskOutcome,
    },
    /// tool permission was resolved by the command that owns this receipt.
    PermissionResolved {
        interaction_id: String,
        approved: bool,
    },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub enum RingingV2AskOutcome {
    Answered,
    Dismissed,
}

/// v2 command status. Superset of [`crate::RingingCommandStatus`] with the
/// typed terminal payload.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct RingingV2CommandStatus {
    pub command_id: String,
    pub state: RingingCommandState,
    pub payload_fingerprint: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub terminal_event_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error_code: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub result: Option<RingingV2CommandResult>,
    /// 同步命令结果回放(B05):succeeded 且缺失 = 引擎类命令,不携带。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sync_result: Option<RingingV2SyncResult>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct RingingV2DriverClaimResponse {
    pub accepted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub holder: Option<String>,
    pub driver_epoch: u64,
    pub reason: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub struct RingingV2DriverReleaseResponse {
    pub accepted: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub holder: Option<String>,
    pub driver_epoch: u64,
    pub reason: String,
}

pub fn open_path() -> String {
    format!("{RINGING_V2_BASE_PATH}/clients/open")
}

/// 单流订阅路径（每 session 一条 SSE，帧直接携带类型化事件与序号）。
pub fn events_path(session_id: &str) -> String {
    format!("{RINGING_V2_BASE_PATH}/sessions/{session_id}/events")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn session_create_round_trips_model_selection() {
        let command = RingingCommand::SessionCreate {
            cwd: Some("/workspace".into()),
            title: Some("Session".into()),
            model_selection: Some(qaqh_types::ModelSelection::default()),
            tool_mode: None,
            custom_tools: None,
        };
        let value = serde_json::to_value(command).expect("serialize command");
        assert!(value["model_selection"].is_object(), "{value}");

        let decoded: RingingCommand = serde_json::from_value(value).expect("deserialize command");
        match decoded {
            RingingCommand::SessionCreate {
                model_selection, ..
            } => {
                assert!(model_selection.is_some());
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn session_settings_patch_requires_settings_revision() {
        let envelope = RingingV2CommandEnvelope {
            schema: RINGING_SCHEMA.into(),
            version: RINGING_V2_VERSION,
            contract_revision: COMMAND_CONTRACT_REVISION.into(),
            command_id: "cmd-settings".into(),
            client_instance_id: "instance-1".into(),
            client_session_id: "lease-1".into(),
            target: CommandTarget::Session {
                session_id: "ses_01ARZ3NDEKTSV4RRFFQ69G5FAV".into(),
            },
            command: RingingCommand::SessionSettingsPatch {
                patch: SessionSettingsPatch::default(),
            },
            preconditions: vec![],
        };
        assert_eq!(envelope.validate(), Err("missing_settings_revision"));

        let current = RingingV2CommandEnvelope {
            preconditions: vec![CommandPrecondition::SettingsRevision { revision: 0 }],
            ..envelope
        };
        current.validate().expect("settings revision precondition");
    }

    #[test]
    fn session_settings_patch_round_trips_tools() {
        let command = RingingCommand::SessionSettingsPatch {
            patch: SessionSettingsPatch {
                tools: Some(ToolsSettingPatch::All),
                ..Default::default()
            },
        };
        let value = serde_json::to_value(&command).expect("serialize settings patch");
        let decoded: RingingCommand = serde_json::from_value(value).expect("decode patch");
        match decoded {
            RingingCommand::SessionSettingsPatch { patch } => {
                assert_eq!(patch.tools, Some(ToolsSettingPatch::All));
            }
            other => panic!("unexpected command: {other:?}"),
        }
    }

    #[test]
    fn v2_command_ack_carries_typed_existing_result() {
        let ack = RingingV2CommandAck {
            command_id: "cmd-1".into(),
            status: RingingCommandAckStatus::Accepted,
            code: None,
            message: Some("duplicate command_id (already completed)".into()),
            retry_after_ms: None,
            result: None,
            operation: None,
            revision: None,
            existing: Some(RingingV2ExistingResult::CommandReceipt {
                state: RingingCommandState::Succeeded,
                terminal_event_id: Some("evt-1".into()),
                error_code: None,
                result: Some(RingingV2CommandResult::AskResolved {
                    interaction_id: "i1".into(),
                    outcome: RingingV2AskOutcome::Answered,
                }),
                sync_result: None,
            }),
        };
        let json = serde_json::to_value(&ack).expect("serialize");
        assert_eq!(json["existing"]["source"], "command_receipt");
        assert_eq!(json["existing"]["state"], "succeeded");
        assert_eq!(json["existing"]["result"]["kind"], "ask_resolved");
        assert_eq!(json["existing"]["result"]["outcome"], "answered");
        let back: RingingV2CommandAck = serde_json::from_value(json).expect("deserialize");
        assert_eq!(back, ack);
    }

    #[test]
    fn v2_command_ack_carries_winning_interaction_verdict() {
        let ack = RingingV2CommandAck {
            command_id: "cmd-2".into(),
            status: RingingCommandAckStatus::Rejected,
            code: Some("interaction_already_resolved".into()),
            message: Some("interaction was already resolved".into()),
            retry_after_ms: None,
            result: None,
            operation: None,
            revision: None,
            existing: Some(RingingV2ExistingResult::InteractionResolved {
                result: RingingV2CommandResult::PermissionResolved {
                    interaction_id: "int_1".into(),
                    approved: false,
                },
            }),
        };
        let json = serde_json::to_value(&ack).expect("serialize");
        assert_eq!(json["existing"]["source"], "interaction_resolved");
        assert_eq!(json["existing"]["result"]["kind"], "permission_resolved");
        assert_eq!(json["existing"]["result"]["approved"], false);
        let back: RingingV2CommandAck = serde_json::from_value(json).expect("deserialize");
        assert_eq!(back, ack);
    }

    #[test]
    fn v2_command_status_round_trips_typed_result() {
        let status = RingingV2CommandStatus {
            command_id: "cmd-2".into(),
            state: RingingCommandState::Failed,
            payload_fingerprint: "fp".into(),
            terminal_event_id: Some("evt-2".into()),
            error_code: Some("interaction_already_resolved".into()),
            result: Some(RingingV2CommandResult::PermissionResolved {
                interaction_id: "i2".into(),
                approved: false,
            }),
            sync_result: None,
        };
        let json = serde_json::to_value(&status).expect("serialize");
        assert_eq!(json["result"]["kind"], "permission_resolved");
        assert_eq!(json["result"]["approved"], false);
        let back: RingingV2CommandStatus = serde_json::from_value(json).expect("deserialize");
        assert_eq!(back, status);
    }

    #[test]
    fn v2_constants_remain_unchanged() {
        assert_eq!(RINGING_V2_VERSION, 2);
        assert_eq!(RINGING_V2_BASE_PATH, "/ringing/v2");
        assert_eq!(open_path(), "/ringing/v2/clients/open");
    }

    /// 单流修订（2026-09-24）：SSE 路径不再带 channel 段。
    #[test]
    fn events_path_is_per_session_single_stream() {
        assert_eq!(events_path("s1"), "/ringing/v2/sessions/s1/events");
        assert!(
            !events_path("s1").contains("/events/"),
            "不得再出现 per-channel 的 events/{{channel}} 形态"
        );
    }

    #[test]
    fn command_envelope_requires_v2_identity() {
        let command = RingingCommand::SessionResume {};
        let mut envelope = RingingV2CommandEnvelope::new(
            "cmd-1",
            CommandTarget::Session {
                session_id: "seed-1".into(),
            },
            command,
        );
        assert_eq!(envelope.validate(), Err("invalid_command_envelope"));
        envelope = envelope.with_client_identity("instance-1", "session-1");
        envelope.validate().expect("valid v2 command envelope");
    }
}
