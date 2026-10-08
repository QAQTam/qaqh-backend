//! 平面一(TokenPlane):受限令牌(WRITE_RESTRICTED)+ capability-SID DACL。
//!
//! 强制锚点见 docs/adr/0001;spec 定位见 docs/spec/。本 crate 不依赖任何
//! agent/审批概念(仓宪法:审批是调用方的事,沙箱只认识"这批路径可写")。

// 平台门控：本 crate 全量依赖 windows crate（TokenPlane/ProjFS 都是 Windows 语义），
// 非 Windows 下编译成空 lib，使 `cargo check/clippy/test --workspace` 在 macOS/Linux
// 上可行。依赖方（qaqh-sandbox / qaqh-workspace / qaqh-process-tools）本就按
// target 门控依赖，不会在非 Windows 拉它。
#![cfg(windows)]

pub mod acceptance;
pub mod acl;
pub mod appcontainer;
pub mod capability;
pub mod console;
pub mod desktop;
pub mod env;
pub mod events;
pub mod feedback;
pub mod policy;
pub mod projfs;
pub mod redirect;
pub mod sid;
pub mod sidstore;
pub mod spawn;
pub mod token;
