//! fact → 投影 的内部映射。
//!
//! 类型词表已迁至 [`qaqh_ringing_contract::fact`];本模块保留线上 delta
//! (ProjectionPayload)与 fact→delta 映射,将在统一事件模型落地时删除。

pub use qaqh_ringing_contract::fact::*;

mod projection_event;

pub use projection_event::*;
