//! Ringing v2 wire contract.
//!
//! RingingV2 command and transport wire types on the versioned base path.

mod types;

pub use types::{
    COMMAND_CONTRACT_REVISION, CommandPrecondition, CommandTarget, ProviderUpsertDto,
    RingingV2ApprovalRespondRequest, RingingV2AskOutcome, RingingV2Capabilities,
    RingingV2CommandAck, RingingV2CommandEnvelope, RingingV2CommandResult, RingingV2CommandStatus,
    RingingV2ContentUploadResponse, RingingV2DeviceApprovalKind, RingingV2DeviceChallenge,
    RingingV2DeviceChallenges, RingingV2DeviceWire, RingingV2DevicesResponse,
    RingingV2DriverClaimResponse, RingingV2DriverReleaseResponse, RingingV2ExistingResult,
    RingingV2LeaseRenewResponse, RingingV2OpenRequest, RingingV2OpenResponse, RingingV2PairRequest,
    RingingV2PairResponse, RingingV2PairTokenRequest, RingingV2PairTokenResponse,
    RingingV2PendingApprovals, RingingV2PendingInteractionView, RingingV2PendingPermissionView,
    RingingV2ResyncRequired, RingingV2SyncResult, SecretWrite, SessionCreateResult,
    SessionSettingsPatch, SettingValue, ToolsSettingPatch, events_path, open_path,
};

/// Ringing v2 wire version.
pub const RINGING_V2_VERSION: u32 = 2;

/// Ringing v2 HTTP/SSE base path.
pub const RINGING_V2_BASE_PATH: &str = "/ringing/v2";

/// Ringing v2 规范 HTTP 路由表(B02 漂移闸的契约侧真源)。
///
/// - daemon `build_router` 遍历本表注册 handler:表里有而 handler match 缺 arm
///   = 启动即 panic(响亮失败,不允许静默 404);
/// - contract exporter 校验 coverage 声明的路由 ⊆ 本表,双侧不得再漂移。
/// `/health`、`/activity`、`/control/v1/*` 是控制面观测/停机端点,不属于
/// Ringing 契约面,不在本表。
pub const RINGING_V2_HTTP_ROUTES: &[(&str, &str)] = &[
    ("POST", "/ringing/v2/clients/open"),
    ("POST", "/ringing/v2/leases/renew"),
    ("GET", "/ringing/v2/sessions/{session_id}/bootstrap"),
    ("GET", "/ringing/v2/sessions/{session_id}/team"),
    ("GET", "/ringing/v2/sessions/{session_id}/events"),
    ("POST", "/ringing/v2/commands"),
    ("GET", "/ringing/v2/commands/{command_id}"),
    ("GET", "/ringing/v2/sessions/{session_id}/approvals"),
    ("POST", "/ringing/v2/sessions/{session_id}/approvals/respond"),
    ("POST", "/ringing/v2/content"),
    ("GET", "/ringing/v2/content/{content_id}"),
    ("POST", "/ringing/v2/service/{method}"),
    ("POST", "/ringing/v2/pairing/tokens"),
    ("POST", "/ringing/v2/pair"),
    ("GET", "/ringing/v2/devices"),
    ("POST", "/ringing/v2/devices/{device_id}/revoke"),
    ("GET", "/ringing/v2/exec-sessions"),
    ("POST", "/ringing/v2/exec-sessions/{id}/kill"),
];
