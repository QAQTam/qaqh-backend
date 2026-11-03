//! `qaqh-agent-runtime::agent` — 会话级共享件的薄层。
//!
//! 旧 Loop 状态机与其取消树/liveness 随 SessionLoop 切换删除;本模块只剩
//! SessionLoop 仍需要的两件事:
//!
//! | 模块 | 职责 |
//! |---|---|
//! | `session_config.rs` | 会话有效配置解析(profile / BYOK / effort) |
//! | `prompt.rs` | 启动期 OS / 工具信息探测槽(系统提示词在 session_agent/prompt) |

pub mod prompt;
pub mod session_config;
