//! PTC（programmatic tool calling）执行 seam。
//!
//! 本 crate 只有词汇与 trait：程序、宿主绑定、已解析执行输入、结果与失败分类。
//! 它不认识工具、会话、权限或进程——执行后端（bun / 未来的 python）与消费方
//! （`run_code` 工具）各自依赖本 crate，互不依赖。
//!
//! 契约要点（与 `packages/ptc-runtime` 的 seam 对齐）：
//!
//! - **失败是 `PtcRunResult::failure` 字段，不是 `Err`**。`run()` 只在调用方误用
//!   或后端无法启动时返回 [`PtcError`]；程序异常、超时、取消、输出超限都是结果。
//! - **绑定参数与返回值必须是 lossless JSON**（[`serde_json::Value`]），后端负责
//!   拒绝不可序列化的值，而不是悄悄降级成字符串。
//! - **取消是协作式的**：后端在取消后必须停止程序并结算，不承诺副作用已停止。
//! - 执行后端不解释工具语义；`run_code` 的消费方决定子调用怎么走权限漏斗。

#![deny(missing_docs)]

mod runtime;
mod types;

pub use runtime::PtcRuntime;
pub use types::{
    PtcBindingErrorClass, PtcBindingFn, PtcBindingMember, PtcBindingNamespace, PtcCancel, PtcError,
    PtcFailureKind, PtcLanguage, PtcProgramFormat, PtcRunFailure, PtcRunRequest, PtcRunResult,
    PtcRunSandbox, PtcRunSpec, PtcSandboxMode,
};
