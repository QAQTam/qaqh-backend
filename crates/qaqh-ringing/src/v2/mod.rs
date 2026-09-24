//! Ringing v2 wire contract.
//!
//! v1 remains available in the crate root for the 2.0 compatibility window.
//! v2 is additive and uses a separate base path and version constant.

mod cursor;
mod types;

pub use cursor::{CanonicalCursor, CursorError, CursorToken, END_OF_FACT};
pub use types::{
    RingingV2AskOutcome, RingingV2Bootstrap, RingingV2Capabilities, RingingV2ChannelSnapshot,
    RingingV2CommandAck, RingingV2CommandEnvelope, RingingV2CommandResult, RingingV2CommandStatus,
    RingingV2ControlState, RingingV2Delivery, RingingV2DriverClaimResponse,
    RingingV2DriverReleaseResponse, RingingV2DriverState, RingingV2EventEnvelope,
    RingingV2ExistingResult, RingingV2InteractionKind, RingingV2LeaseRenewResponse,
    RingingV2OpenRequest, RingingV2OpenResponse, RingingV2PendingInteraction, RingingV2PendingSet,
    RingingV2ResetReason, RingingV2ResetRequired, RingingV2StreamKey, events_path, open_path,
};

/// Ringing v2 wire version.
pub const RINGING_V2_VERSION: u32 = 2;

/// Ringing v2 HTTP/SSE base path.
pub const RINGING_V2_BASE_PATH: &str = "/ringing/v2";
