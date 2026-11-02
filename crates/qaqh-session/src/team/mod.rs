//! Team task board canonical aggregate.
//!
//! 类型词表在 [`qaqh_ringing_contract::team`];本模块保留投影与自制文件存储
//! (store 将随 SQLite 业务存储切换删除)。

mod projection;
mod store;

pub mod board;

pub use board::{
    BOARD_COMMIT_FILE, BOARD_EVENTS_FILE, BOARD_IDENTITY_FILE, BOARD_IDENTITY_SCHEMA,
    BOARD_LOCK_FILE, BoardAppendOutcome, BoardProjection, BoardStore,
};
pub use projection::TaskBoardProjection;
pub use qaqh_ringing_contract::team::{
    BOARD_FACT_SCHEMA, BOARD_FACT_VERSION, BoardChannelView, BoardCreated, BoardDelta, BoardFact,
    BoardId, BoardPayload, BoardPostView, BoardSnapshot, BoardSubscriptionTarget,
    BoardSubscriptionView, BoardThreadView, ChannelCreated, ChannelId, MAX_BOARD_CHANNELS,
    MAX_BOARD_POSTS, MAX_BOARD_SUBSCRIPTIONS, MAX_BOARD_THREADS, PostCreated, PostId,
    SubscriptionChanged, TEAM_FACT_SCHEMA, TEAM_FACT_VERSION, TaskAcceptanceSet, TaskArtifact,
    TaskArtifactAttached, TaskBoardDelta, TaskBoardSnapshot, TaskCancelled, TaskClaimed,
    TaskClosed, TaskCompleted, TaskCreated, TaskDependencyAdded, TaskId, TaskReleased, TaskState,
    TaskView, TeamActor, TeamAgentResidency, TeamAgentSnapshot, TeamAgentStatus, TeamBoardChannel,
    TeamBoardPost, TeamBoardSnapshot, TeamBoardSubscription, TeamBoardSubscriptionTarget,
    TeamBoardThread, TeamCreated, TeamDelta, TeamError, TeamFact, TeamId, TeamInboxSummary,
    ThreadCreated, ThreadId,
    TeamPayload, TeamResult, TeamTaskArtifact, TeamTaskSnapshot, new_board_schema,
    new_team_schema,
};
pub use store::{
    TEAM_COMMIT_FILE, TEAM_EVENTS_FILE, TEAM_IDENTITY_FILE, TEAM_IDENTITY_SCHEMA, TEAM_LOCK_FILE,
    TeamAppendOutcome, TeamStore,
};
