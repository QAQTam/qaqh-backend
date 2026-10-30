//! 服务面 handler：一个大函数按领域拆成四个模块。
//!
//! 每个模块只认自己的强类型命令枚举（[`crate::service::command`]），
//! `QaqhService::handle` 只剩「解码 + 分派」。

pub(crate) mod session;
pub(crate) mod system;
pub(crate) mod task;
pub(crate) mod workspace;
