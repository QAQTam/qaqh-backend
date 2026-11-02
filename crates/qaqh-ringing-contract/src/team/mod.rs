//! team / task-board / message-board 词表与线上快照(自 qaqh-session 迁入)。

pub mod board_types;
pub mod error;
pub mod snapshot;
pub mod task_types;

pub use board_types::*;
pub use error::{TeamError, TeamResult};
pub use snapshot::*;
pub use task_types::*;
