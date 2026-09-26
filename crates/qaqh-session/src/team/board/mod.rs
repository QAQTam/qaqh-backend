//! Team message board canonical aggregate.

mod projection;
mod store;
mod types;

pub use projection::{
    BoardChannelView, BoardDelta, BoardPostView, BoardProjection, BoardSnapshot,
    BoardSubscriptionView, BoardThreadView,
};
pub use store::{
    BOARD_COMMIT_FILE, BOARD_EVENTS_FILE, BOARD_IDENTITY_FILE, BOARD_IDENTITY_SCHEMA,
    BOARD_LOCK_FILE, BoardAppendOutcome, BoardStore,
};
pub use types::{
    BOARD_FACT_SCHEMA, BOARD_FACT_VERSION, BoardCreated, BoardFact, BoardId, BoardPayload,
    BoardSubscriptionTarget, ChannelCreated, ChannelId, MAX_BOARD_CHANNELS, MAX_BOARD_POSTS,
    MAX_BOARD_SUBSCRIPTIONS, MAX_BOARD_THREADS, PostCreated, PostId, SubscriptionChanged,
    ThreadCreated, ThreadId, new_board_schema,
};
