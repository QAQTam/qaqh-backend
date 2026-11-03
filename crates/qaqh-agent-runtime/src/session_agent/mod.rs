//! 会话装配与系统提示词:SessionLoop 的两个支撑面。
//!
//! 旧 SessionAgent/AgentHost/agentloop 执行框架已删除;回合执行职责在
//! [`crate::session_loop::SessionLoop`]。

pub mod assembly;
pub mod prompt;
