//! Ringing v2 wire contract.
//!
//! RingingV2 command and transport wire types on the versioned base path.

mod shell_stream;
mod types;

pub use types::{
    COMMAND_CONTRACT_REVISION, CommandPrecondition, CommandRejection, CommandTarget,
    ProviderUpsertDto, RingingV2ApprovalRespondRequest, RingingV2AskOutcome, RingingV2Capabilities,
    RingingV2ClientKind, RingingV2ClientView, RingingV2CommandAck, RingingV2CommandEnvelope,
    RingingV2CommandResult, RingingV2CommandStatus, RingingV2ContentUploadResponse,
    RingingV2DeviceApprovalKind, RingingV2DeviceChallenge, RingingV2DeviceChallenges,
    RingingV2DeviceWire, RingingV2DevicesResponse, RingingV2ExistingResult,
    RingingV2LeaseRenewResponse, RingingV2OpenRequest, RingingV2OpenResponse, RingingV2PairRequest,
    RingingV2PairResponse, RingingV2PairTokenRequest, RingingV2PairTokenResponse,
    RingingV2PendingApprovals, RingingV2PendingInteractionView, RingingV2PendingPermissionView,
    RingingV2ResyncRequired, RingingV2SyncResult, SESSION_CONTRACT_REVISION, SecretWrite,
    SessionCreateResult, SessionSettingsPatch, SettingValue, ToolsSettingPatch, events_path,
    open_path,
};
pub use shell_stream::{ShellStreamControl, ShellStreamEvent, shell_stream_path};

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
///
/// 最后一条是**唯一**的 ws 流(用户 shell 终端数据面,ADR-0028 §D3 裁定=候选 B)：
/// 登记的 method 写 `GET` 而不是 `WS`，因为升级请求按 HTTP 事实就是 GET +
/// `Upgrade: websocket`；`router.route(path, get(..))` 与本表的 method 列必须同
/// 一面镜子（写 `WS` 会让本表的列不再等于 HTTP 方法，B02 闸也就失去意义）。
/// 「这是一条 ws 流、不是普通 JSON GET」这件事写在 `coverage.json` 的能力记录里
/// （含 4 个控制帧的词表），不靠版本猜测。
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
    ("GET", "/ringing/v2/shells/{shell_id}/stream"),
];
