//! `turn_lap` 的测试可见面（BUG-2026-09-13-08 回归用）。
//!
//! `turn_lap::admit` 是 `pub(crate)`：生产路径只从 loop 内部调用。集成测试
//! （`tests/`）无法触及 crate 私有模块，但取消收割语义必须在**真实**的
//! `execute_admitted_batch` 上验证（不能靠复制实现冒充）。这里只重导出
//! 该函数，不扩大其它内部面。

pub use crate::agent::turn_lap::admit::execute_admitted_batch;
