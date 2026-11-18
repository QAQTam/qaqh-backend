//! 会话级 exec 生命周期内核。

pub mod capture;
pub mod manager;
mod tool;

pub use manager::{
    ExecManagerError, ExecManagerErrorKind, ExecSessionManager, ExecSessionPoll,
    ExecSessionPublisher, ExecSessionResult, ExecSessionStart, StopResult,
};
pub use tool::register;
// crate 内测试面：ShellName/ExecCommandArgs 的白名单反序列化断言（exec/tests.rs）。
#[cfg(test)]
pub(crate) use tool::{ExecCommandArgs, ShellName};
