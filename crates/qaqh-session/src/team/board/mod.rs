//! Team message board canonical aggregate.

mod projection;
mod store;

pub use projection::BoardProjection;
pub use qaqh_ringing_contract::team::{
    BOARD_FACT_SCHEMA, BOARD_FACT_VERSION, BoardChannelView, BoardCreated, BoardDelta, BoardFact,
    BoardId, BoardPayload, BoardPostView, BoardSnapshot, BoardSubscriptionTarget,
    BoardSubscriptionView, BoardThreadView, ChannelCreated, ChannelId, MAX_BOARD_CHANNELS,
    MAX_BOARD_POSTS, MAX_BOARD_SUBSCRIPTIONS, MAX_BOARD_THREADS, PostCreated, PostId,
    SubscriptionChanged, ThreadCreated, ThreadId,
};
pub use store::{
    BOARD_COMMIT_FILE, BOARD_EVENTS_FILE, BOARD_IDENTITY_FILE, BOARD_IDENTITY_SCHEMA,
    BOARD_LOCK_FILE, BoardAppendOutcome, BoardStore,
};
