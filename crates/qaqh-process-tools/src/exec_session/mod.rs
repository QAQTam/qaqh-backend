//! 会话级 exec 生命周期内核。

pub mod capture;
pub mod manager;
mod tool;

pub use manager::{
    ExecManagerError, ExecManagerErrorKind, ExecSessionManager, ExecSessionPoll,
    ExecSessionPublisher, ExecSessionResult, ExecSessionStart, StopResult,
};
pub use tool::register;
