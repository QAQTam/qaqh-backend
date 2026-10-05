//! Ringing v2 wire contract.
//!
//! v2 is additive and uses a separate base path and version constant. The
//! crate root retains the live command/snapshot/worker envelopes; the v1
//! event bus and its wire types were removed in hub-fact-bus stages 3.2/3d.

mod cursor;
mod types;

pub use cursor::{CanonicalCursor, CursorError, CursorToken, END_OF_FACT};
pub use types::{
    RingingV2AskOutcome, RingingV2Bootstrap, RingingV2Capabilities, RingingV2ChannelSnapshot,
    RingingV2CommandAck, RingingV2CommandEnvelope, RingingV2CommandResult, RingingV2CommandStatus,
    RingingV2ContentValue, RingingV2ControlState, RingingV2Delivery, RingingV2DeviceWire,
    RingingV2DevicesResponse, RingingV2DriverClaimResponse, RingingV2DriverReleaseResponse,
    RingingV2DriverState, RingingV2EventEnvelope, RingingV2ExistingResult,
    RingingV2InteractionKind, RingingV2LeaseRenewResponse, RingingV2OpenRequest,
    RingingV2OpenResponse, RingingV2PairRequest, RingingV2PairResponse, RingingV2PairTokenRequest,
    RingingV2PairTokenResponse, RingingV2PendingInteraction, RingingV2PendingSet,
    RingingV2ResetReason, RingingV2ResetRequired, RingingV2StreamKey, events_path, open_path,
};

/// Ringing v2 wire version.
pub const RINGING_V2_VERSION: u32 = 2;

/// Ringing v2 HTTP/SSE base path.
pub const RINGING_V2_BASE_PATH: &str = "/ringing/v2";
