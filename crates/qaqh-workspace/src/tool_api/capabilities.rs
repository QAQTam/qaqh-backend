//! 工具能力声明（09-19 补充稿 §3）：驱动 loop 的并发/串行/取消/流式决策。
//!
//! 与 `category` / `risk` 正交——后者是**权限输入**（谁需要审批），本结构是
//! **编排输入**（批次怎么跑）。loop 不得再硬编码并发/串行判断。

use std::time::Duration;

/// 并发档位。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Concurrency {
    /// 可与同批任意工具并行（只读类目标档）。
    Parallel,
    /// 与同批其它工具串行（默认；保守迁移档）。
    #[default]
    Serial,
    /// 独占整个批次（如 exec 长进程、process 管理）。
    Exclusive,
}

/// 工具能力声明（随 `ToolDescriptor` 注册）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ToolCapabilities {
    /// 并发档位；批次调度器据此分区。
    pub concurrency: Concurrency,
    /// 是否声明支持进度流（`ToolCallContext.progress` 非空时才可能收到帧）。
    pub streaming: bool,
    /// 观察到取消后承诺的有界返回时间（base spec §6.4 的数值来源）。
    pub cancel_grace: Duration,
    /// 是否幂等（重试/重放安全的依据）。
    pub idempotent: bool,
    /// 是否 workspace 绑定（cwd 敏感工具必须为 true，执行器强制注入
    /// `ToolCallContext.workspace_root`）。
    pub workspace_bound: bool,
    /// 是否阻塞等待用户交互。
    ///
    /// 交互工具可以声明 `default_timeout = 0` 表示“无默认超时”；
    /// 非交互工具仍必须给出正数默认超时。
    pub interactive: bool,
}

impl Default for ToolCapabilities {
    fn default() -> Self {
        Self {
            concurrency: Concurrency::Serial,
            streaming: false,
            cancel_grace: Duration::from_secs(5),
            idempotent: false,
            workspace_bound: true,
            interactive: false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn defaults_are_conservative() {
        let caps = ToolCapabilities::default();
        assert_eq!(
            caps.concurrency,
            Concurrency::Serial,
            "默认串行（迁移保守档）"
        );
        assert!(!caps.streaming);
        assert_eq!(
            caps.cancel_grace,
            Duration::from_secs(5),
            "5s 兜底（补充稿 Q7）"
        );
        assert!(!caps.idempotent);
        assert!(caps.workspace_bound);
        assert!(!caps.interactive);
    }

    #[test]
    fn concurrency_default_is_serial() {
        assert_eq!(Concurrency::default(), Concurrency::Serial);
    }
}
