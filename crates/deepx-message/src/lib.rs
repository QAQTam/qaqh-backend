//! deepx-message: structured conversation state with state-machine lifecycle.
//!
//! `MessageStore` is the single source of truth for messages.
//! Every `push_*` returns an [`Effect`] telling the caller what to do next.

pub mod effect;
pub mod store;

pub use effect::{Effect, PendingTool, ToolExecReport, ToolExecRequest, ToolExecutorFn};
pub use store::{MessageStore, Turn};
