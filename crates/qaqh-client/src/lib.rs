//! QAQ-Harness Ringing v2 daemon client (HTTP/SSE).
//!
//! Shared transport for the TUI and desktop shells: discovery, lease
//! negotiation/renewal, the v2 single-stream event feed, plus commands,
//! queries, bootstrap and graceful stop.
//!
//! The public API uses the `qaqh-ringing-contract` and `qaqh-ringing`
//! contracts. HTTP/SSE JSON is decoded at this boundary and never becomes a
//! renderer-facing compatibility protocol.

#![cfg_attr(not(test), deny(unsafe_code))]
pub mod client;
pub mod discovery;
pub mod endpoint;
pub mod error;
pub mod projection;
pub mod remote_path;
pub mod service;
pub mod service_types;
pub mod session;
mod sse_decoder;
mod tls_pin;
pub mod types;
pub mod v2;
pub mod v2_stream;

pub use client::{
    Client, ClientHandlers, ClientOptions, PairScope, RemoteEndpoint, StopStatus, pair_remote,
    runtime_handle,
};
pub use discovery::{DaemonDiscovery, DiscoveryExt, ensure_daemon_running, read_discovery};
pub use endpoint::QueryRequest;
pub use error::{ClientError, Result};
pub use projection::sanitize_session_list;
pub use qaqh_ringing::RingingV2ResyncRequired;
/// 设备配对 / 设备管理的 wire 类型（`Client::issue_pairing_token` 等方法的返回值）。
///
/// 壳层（TUI / 桌面）不直接依赖 `qaqh-ringing`，这三个类型必须经本 crate 出口给出去，
/// 否则壳层就只能手解 JSON——那正是本客户端存在的理由要反对的事。
pub use qaqh_ringing::v2::{
    RingingV2ApprovalRespondRequest, RingingV2ClientKind, RingingV2ClientView,
    RingingV2ContentUploadResponse, RingingV2DeviceApprovalKind, RingingV2DeviceChallenge,
    RingingV2DeviceChallenges, RingingV2DeviceWire, RingingV2PairResponse, RingingV2PairTokenResponse,
};
pub use qaqh_ringing::{
    COMMAND_CONTRACT_REVISION, CommandPrecondition, CommandTarget, ProviderUpsertDto,
    RingingV2CommandEnvelope, SESSION_CONTRACT_REVISION, SecretWrite, SessionCreateResult,
    SessionSettingsPatch, ToolsSettingPatch,
};
/// 公开字段背后的**类型名**：`ConfigDto.credential` 的取值域由
/// `CredentialSource` 定义，`ConfigDto.wire` 与 `ConfigPatch::wire` 的规范拼写
/// 由 `Wire` 定义。
///
/// 这些类型本来就在 `qaqh-types` 里，只是没经本 crate 出口：壳层于是只能
/// 「看得见字段、拿不到类型名」——要么 match 时不写类型名，要么为了一行
/// `ReasoningKind::is_displayable()` 直接 path 依赖 `qaqh-types`。二者都是
/// 本客户端不该逼出来的选择（零成本、零新代码，只是补出口）。
pub use qaqh_types::{CredentialSource, ReasoningKind, Wire};
pub use remote_path::{display_host, display_path, remote_path_from_display};
pub use service::{SERVICE_METHODS, is_allowed};
/// `siwc.*` / `dsaccount.*` 服务面响应的 typed DTO（形状见
/// `docs/spec-service-surface-siwc-dsaccount.md`）。
///
/// 壳层拿到 [`Client::service_call`] 的 `Value` 之后，直接
/// `serde_json::from_value::<SiwcStatus>(value)` 即可——不必再手抄一份
/// 「结构对齐的本地类型」（那正是桌面端已经付过一次的代价）。
pub use service_types::{
    DsAccountBalance, DsAccountBonus, DsAccountFlow, DsAccountModel, DsAccountProfile,
    DsAccountSignInStart, DsAccountSignInStatus, DsAccountStatus, DsAccountVerify, DsFlowState,
    DsRevokeOutcome, DsTotalCost, DsWallet, ServiceError, SiwcConnectionState, SiwcFlowState,
    SiwcModel, SiwcProfile, SiwcProfileState, SiwcProfiles, SiwcSignInStart, SiwcSignInStatus,
    SiwcSignOutResult, SiwcStatus,
};
pub use session::RingingSession;
pub use types::{
    AskAnswer, CLIENT_SESSION_HEADER, CommandOptions, CompactStatus, ContentRef,
    ConversationSendMessage, ImageBlock, InputPurpose, MAX_SAFE_INTEGER, RINGING_SCHEMA,
    ReconnectReason, RingingCommand, RingingCommandAckStatus, RingingCommandState, SessionActivity,
    SessionActivityState, SessionListEntry, SessionMeta, SessionRunStatus, ToolContinuation,
    ToolError, ToolImage, ToolModelPayload, ToolResult, ToolStatus, UsageInfo, is_safe_integer,
};
pub use v2::{
    ClientV2SessionState, ClientV2Subscription, ClientV2SubscriptionEvent, RINGING_V2_BASE_PATH,
    RINGING_V2_VERSION, SessionBootstrap, SessionEventFrame, SessionState, TeamResponse,
};
/// The v2 contract exposes the session fact model under its real module
/// names; there is no `ClientV2*` renaming layer.
pub use v2::{contract_events, contract_state, fact, ringing, ringing_v2};
pub use v2_stream::{V2Stream, V2StreamHandlers, V2StreamStatus};
