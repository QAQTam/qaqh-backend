//! QAQ-Harness Ringing v2 daemon client (HTTP/SSE).
//!
//! Shared transport for the TUI and desktop shells: discovery, lease
//! negotiation/renewal, the v2 single-stream event feed, plus commands,
//! queries, bootstrap and graceful stop.
//!
//! The public API uses the canonical `qaqh-domain` and `qaqh-ringing`
//! contracts. HTTP/SSE JSON is decoded at this boundary and never becomes a
//! renderer-facing compatibility protocol.

#![cfg_attr(not(test), forbid(unsafe_code))]
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
pub mod types;
pub mod v2;
pub mod v2_stream;

pub use client::{
    Client, ClientHandlers, ClientOptions, PairScope, RemoteEndpoint, StopStatus, runtime_handle,
};
pub use discovery::{DaemonDiscovery, DiscoveryExt, ensure_daemon_running, read_discovery};
pub use endpoint::QueryRequest;
pub use error::{ClientError, Result};
pub use projection::sanitize_session_list;
/// 领域状态类型（`qaqh_domain::state`）。
///
/// 纯 v2 之后**不再**经 v1 bootstrap 的 `state` 访问器取用：v2 bootstrap 直接返回
/// typed 三频道快照（[`ClientV2Bootstrap`]）。这里保留导出只因为 v1 领域投影仍在
/// runtime 内部使用，壳层不应再依赖。
pub use qaqh_domain::state::{
    ControlState, ConversationState, InteractionKind, LastFailure, LastRound, PendingInteraction,
    RunningTool, ToolState,
};
/// 设备配对 / 设备管理的 wire 类型（`Client::issue_pairing_token` 等方法的返回值）。
///
/// 壳层（TUI / 桌面）不直接依赖 `qaqh-ringing`，这三个类型必须经本 crate 出口给出去，
/// 否则壳层就只能手解 JSON——那正是本客户端存在的理由要反对的事。
pub use qaqh_ringing::v2::{RingingV2DeviceWire, RingingV2PairTokenResponse};
pub use qaqh_ringing::{
    CommandPrecondition, CommandTarget, ProviderUpsertDto, SecretWrite, SessionCreateResult,
    SessionSettingsPatch, ToolsSettingPatch, RingingV2CommandEnvelope, COMMAND_CONTRACT_REVISION,
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
    AgentLifecycleState, AskAnswer, AskMode, AskResolution, CLIENT_SESSION_HEADER, Channel,
    CommandOptions, CompactStatus, ContentRef, ControlEvent, ConversationSendMessage,
    ConversationEvent, DashboardDocument,
    DashboardTask, DomainActivityState, DomainAskQuestion, DomainDashboardSnapshot, DomainError,
    DomainSessionState, ErrorScope, ImageBlock, InputPurpose, MAX_SAFE_INTEGER, NoticeLevel,
    PermissionCategory, PermissionRisk, PlanReviewItem, ProviderToolState, RINGING_SCHEMA,
    ReconnectReason, RingingCommand, RingingCommandAckStatus, RingingCommandState,
    RoundDeltaKind, SessionActivity,
    SessionListEntry, SessionMeta, SessionRunStatus, SkillInfo, SkillRuntimeInfo, SkillsStatus,
    ToolContinuation, ToolError, ToolEvent, ToolImage, ToolModelPayload, ToolResult,
    ToolStatus, UsageInfo, is_safe_integer,
};
pub use v2::{
    CLIENT_V2_END_OF_FACT, ClientV2Bootstrap, ClientV2Event, ClientV2EventEnvelope,
    ClientV2ResyncRequired, ClientV2SessionState, ClientV2Subscription, ClientV2SubscriptionEvent,
    ClientV2TeamResponse, RINGING_V2_BASE_PATH, RINGING_V2_VERSION,
};
/// The v2 contract exposes the session fact model under its real module
/// names; there is no `ClientV2*` renaming layer.
pub use v2::{contract_events, contract_state, fact, ringing, ringing_v2};
pub use v2_stream::{V2Stream, V2StreamHandlers, V2StreamStatus};
