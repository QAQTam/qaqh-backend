//! 平面一(TokenPlane):受限令牌(WRITE_RESTRICTED)+ capability-SID DACL。
//!
//! 强制锚点见 docs/adr/0001;spec 定位见 docs/spec/。本 crate 不依赖任何
//! agent/审批概念(仓宪法:审批是调用方的事,沙箱只认识"这批路径可写")。

// 平台门控（v4.5 P2-1a 起为**逐模块**门控）：除 [`nt`] 外，本 crate 全量依赖
// windows crate（TokenPlane/ProjFS 都是 Windows 语义），非 Windows 下这些模块
// 不参与编译，使 `cargo check/clippy/test --workspace` 在 macOS/Linux 上可行。
// 依赖方（qaqh-sandbox / qaqh-workspace / qaqh-process-tools）本就按 target
// 门控依赖，不会在非 Windows 拉它。
// [`nt`]（原独立 crate，P2-1a 并入）是纯字符串路径归一——跨平台编译，
// NT 归一单测在 Linux 宿主照常运行。
pub mod nt;

#[cfg(windows)]
pub mod acceptance;
#[cfg(windows)]
pub mod acl;
#[cfg(windows)]
pub mod appcontainer;
#[cfg(windows)]
pub mod capability;
#[cfg(windows)]
pub mod console;
#[cfg(windows)]
pub mod desktop;
#[cfg(windows)]
pub mod env;
#[cfg(windows)]
pub mod events;
#[cfg(windows)]
pub mod feedback;
#[cfg(windows)]
pub mod policy;
#[cfg(windows)]
pub mod projfs;
#[cfg(windows)]
pub mod redirect;
#[cfg(windows)]
pub mod sid;
#[cfg(windows)]
pub mod sidstore;
#[cfg(windows)]
pub mod spawn;
#[cfg(windows)]
pub mod token;
