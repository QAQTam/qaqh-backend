//! team / 任务板词表与线上快照(自 qaqh-session 迁入)。ADR-0033:留言板词表退役。

pub mod error;
pub mod snapshot;
pub mod task_types;

pub use error::{TeamError, TeamResult};
pub use snapshot::*;
pub use task_types::*;
