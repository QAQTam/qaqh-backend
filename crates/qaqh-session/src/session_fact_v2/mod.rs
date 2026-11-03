//! qaqh-session 的 fact 类型门面:统一从 [`qaqh_ringing_contract::fact`]
//! 再导出(报告一 §12:契约独立,session 不再拥有第二份 fact 词表)。

pub use qaqh_ringing_contract::fact::*;
