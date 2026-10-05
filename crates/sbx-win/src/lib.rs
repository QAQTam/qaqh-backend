//! 平面一(TokenPlane):受限令牌(WRITE_RESTRICTED)+ capability-SID DACL。
//!
//! 强制锚点见 docs/adr/0001;spec 定位见 docs/spec/。本 crate 不依赖任何
//! agent/审批概念(仓宪法:审批是调用方的事,沙箱只认识"这批路径可写")。

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
