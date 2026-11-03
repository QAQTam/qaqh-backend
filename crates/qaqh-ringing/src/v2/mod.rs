//! Ringing v2 wire contract.
//!
//! v2 is additive and uses a separate base path and version constant. The
//! crate root retains the live command/snapshot/worker envelopes; the v1
//! event bus and its wire types were removed in hub-fact-bus stages 3.2/3d.

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
    SecretWrite, SessionCreateResult, SessionSettingsPatch, SettingValue, ToolsSettingPatch,
    events_path, open_path,
};

/// Ringing v2 wire version.
pub const RINGING_V2_VERSION: u32 = 2;

/// Ringing v2 HTTP/SSE base path.
pub const RINGING_V2_BASE_PATH: &str = "/ringing/v2";
