//! 内置工具能力迁移表（spec 补充稿 §3.4；plan「ToolCapabilities 迁移表」项）。
//!
//! **已接线（P3-2）**：`ToolManager::register` 在构造 `ToolDescriptor` 时从本表
//! 注入 capabilities；执行调度仍暂用既有 conflict 算法，capabilities 切换调度
//! 留到显式行为变更切片。后续可将本表并入各工具注册处并删除本模块。
//!
//! ## 取值规则（逐项核对，评审依据）
//!
//! - `concurrency`：`Parallel` ⇔ 纯只读（category ∈ {Read, Net}）且无共享可变
//!   状态；`Exclusive` ⇔ 派生进程组（exec/process）或批量重写文件（journal
//!   replay）；其余保守 `Serial`。
//! - `streaming`：只有真实产生进度帧的工具为 `true`（现状仅 exec——
//!   `exec/handler.rs` 是唯一消费 `tx_progress` 的 handler）。
//! - `cancel_grace`：`1s` = 进程内、IO 边界轮询取消；`5s` = 派生进程组或批量
//!   重写（exec/process/journal；spec Q7 的兜底值，迁移时按实际收割路径标定）。
//! - `idempotent`：`true` ⇔ 纯读取（重放不改本地状态）；写类/交互类/exec 一律
//!   `false`（对齐 canonical ToolIntent「已准入执行」语义）。
//! - `workspace_bound`：触碰工作区文件的工具为 `true`（执行器须强制注入
//!   workspace_root）；纯交互（ask）、会话内存（todo_*）、网络（web_fetch）为 `false`。
//! - `interactive`：`ask` 为 `true`，允许 descriptor 用零 `default_timeout`
//!   表示“无默认超时”（Q9）。

use std::time::Duration;

use crate::tool_api::{Concurrency, ToolCapabilities};

/// 进程内快速工具：1s 内可观察到取消并返回。
const FAST_GRACE: Duration = Duration::from_secs(1);
/// 派生进程组 / 批量重写：5s 收割上限（spec Q7 兜底）。
const SLOW_GRACE: Duration = Duration::from_secs(5);

/// 纯只读（可并行、幂等、workspace 绑定）。
const READ_ONLY: ToolCapabilities = ToolCapabilities {
    concurrency: Concurrency::Parallel,
    streaming: false,
    cancel_grace: FAST_GRACE,
    idempotent: true,
    workspace_bound: true,
    interactive: false,
};

/// 纯只读、非 workspace 绑定（会话内存 / 网络）。
const READ_ONLY_UNBOUND: ToolCapabilities = ToolCapabilities {
    concurrency: Concurrency::Parallel,
    streaming: false,
    cancel_grace: FAST_GRACE,
    idempotent: true,
    workspace_bound: false,
    interactive: false,
};

/// 工作区变更（串行、非幂等、workspace 绑定）。
const MUTATING: ToolCapabilities = ToolCapabilities {
    concurrency: Concurrency::Serial,
    streaming: false,
    cancel_grace: FAST_GRACE,
    idempotent: false,
    workspace_bound: true,
    interactive: false,
};

/// 会话内存变更（串行、非幂等、非 workspace 绑定）。
const SESSION_MUTATING: ToolCapabilities = ToolCapabilities {
    concurrency: Concurrency::Serial,
    streaming: false,
    cancel_grace: FAST_GRACE,
    idempotent: false,
    workspace_bound: false,
    interactive: false,
};

/// 独占批次（派生进程组 / 批量重写）。
const EXCLUSIVE: ToolCapabilities = ToolCapabilities {
    concurrency: Concurrency::Exclusive,
    streaming: false,
    cancel_grace: SLOW_GRACE,
    idempotent: false,
    workspace_bound: true,
    interactive: false,
};

/// 阻塞交互（串行、非幂等、非 workspace 绑定）。
const INTERACTIVE: ToolCapabilities = ToolCapabilities {
    concurrency: Concurrency::Serial,
    streaming: false,
    cancel_grace: FAST_GRACE,
    idempotent: false,
    workspace_bound: false,
    interactive: true,
};

/// exec：独占 + 流式（唯一进度生产者）。
const EXEC: ToolCapabilities = ToolCapabilities {
    concurrency: Concurrency::Exclusive,
    streaming: true,
    cancel_grace: SLOW_GRACE,
    idempotent: false,
    workspace_bound: true,
    interactive: false,
};

/// 迁移表：19 项，顺序与注册表词表一致（`registration.rs` 的
/// `default_registry_exposes_the_formal_tool_vocabulary`）。
const TABLE: [(&str, ToolCapabilities); 19] = [
    ("apply_patch", MUTATING),
    ("ask", INTERACTIVE),
    ("confirm_apply", MUTATING),
    ("copy_range", MUTATING),
    ("delete", MUTATING),
    ("edit", MUTATING),
    ("exec", EXEC),
    ("glob", READ_ONLY),
    ("grep", READ_ONLY),
    ("journal", EXCLUSIVE),
    ("process", EXCLUSIVE),
    ("read", READ_ONLY),
    ("read_image", READ_ONLY),
    ("skills", MUTATING),
    ("todo_list", READ_ONLY_UNBOUND),
    ("todo_update", SESSION_MUTATING),
    ("todo_write", SESSION_MUTATING),
    ("web_fetch", READ_ONLY_UNBOUND),
    ("write", MUTATING),
];

/// 查询内置工具的能力声明。
///
/// 非内置工具（动态 MCP/LSP、外部注册器注入）返回 `None`——调用方回退保守
/// 默认档 [`ToolCapabilities::default`]。
pub fn builtin_capabilities(name: &str) -> Option<ToolCapabilities> {
    TABLE
        .iter()
        .find(|(tool, _)| *tool == name)
        .map(|(_, capabilities)| capabilities.clone())
}

/// 迁移表覆盖的全部工具名（与注册表词表逐项一致）。
pub fn table_tool_names() -> Vec<&'static str> {
    TABLE.iter().map(|(tool, _)| *tool).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::permission::ToolCategory;
    use crate::registration::build_tool_manager;

    #[test]
    fn table_covers_registry_exactly() {
        let registry: Vec<String> = build_tool_manager(&[])
            .all_defs()
            .into_iter()
            .map(|def| def.function.name)
            .collect();
        let table: Vec<String> = table_tool_names()
            .iter()
            .map(|name| (*name).to_owned())
            .collect();
        assert_eq!(
            table, registry,
            "迁移表必须与注册表词表逐项一致（新增/删除工具必须同步本表）"
        );
    }

    #[test]
    fn parallel_tools_are_pure_reads() {
        let manager = build_tool_manager(&[]);
        for name in table_tool_names() {
            let capabilities = builtin_capabilities(name).expect("表内条目");
            if capabilities.concurrency == Concurrency::Parallel {
                let category = manager.category_of(name).expect("注册工具");
                assert!(
                    matches!(category, ToolCategory::Read | ToolCategory::Net),
                    "{name} 声明 Parallel 但 category={category:?}（只读才可并行）"
                );
            }
        }
    }

    #[test]
    fn exclusive_tools_are_process_or_rewrite() {
        let manager = build_tool_manager(&[]);
        let exclusive: Vec<&str> = table_tool_names()
            .into_iter()
            .filter(|name| {
                builtin_capabilities(name).expect("表内条目").concurrency == Concurrency::Exclusive
            })
            .collect();
        assert_eq!(
            exclusive,
            vec!["exec", "journal", "process"],
            "独占档变更必须显式审查（spec §3.4）"
        );
        for name in exclusive {
            let category = manager.category_of(name).expect("注册工具");
            assert!(
                matches!(category, ToolCategory::Exec | ToolCategory::Write),
                "{name} 独占但 category={category:?}"
            );
        }
    }

    #[test]
    fn streaming_is_only_where_progress_frames_exist() {
        let streaming: Vec<&str> = table_tool_names()
            .into_iter()
            .filter(|name| builtin_capabilities(name).expect("表内条目").streaming)
            .collect();
        assert_eq!(
            streaming,
            vec!["exec"],
            "新增流式工具时必须同步本断言与进度通道接线"
        );
    }

    #[test]
    fn idempotent_tools_are_pure_reads() {
        let manager = build_tool_manager(&[]);
        for name in table_tool_names() {
            let capabilities = builtin_capabilities(name).expect("表内条目");
            if capabilities.idempotent {
                let category = manager.category_of(name).expect("注册工具");
                assert!(
                    matches!(category, ToolCategory::Read | ToolCategory::Net),
                    "{name} 声明幂等但 category={category:?}（重放安全仅限纯读取）"
                );
            }
        }
    }

    #[test]
    fn workspace_unbound_tools_are_pinned() {
        let unbound: Vec<&str> = table_tool_names()
            .into_iter()
            .filter(|name| {
                !builtin_capabilities(name)
                    .expect("表内条目")
                    .workspace_bound
            })
            .collect();
        assert_eq!(
            unbound,
            vec!["ask", "todo_list", "todo_update", "todo_write", "web_fetch"],
            "非 workspace 绑定工具必须显式审查（执行器不注入 workspace_root）"
        );
    }

    #[test]
    fn cancel_grace_is_declared_and_bounded() {
        for name in table_tool_names() {
            let capabilities = builtin_capabilities(name).expect("表内条目");
            assert!(
                capabilities.cancel_grace >= FAST_GRACE,
                "{name} 取消宽限小于 1s 下限"
            );
            assert!(
                capabilities.cancel_grace <= SLOW_GRACE,
                "{name} 取消宽限超过 spec Q7 兜底上限（5s）"
            );
        }
    }

    #[test]
    fn golden_rows_are_pinned() {
        let exec = builtin_capabilities("exec").expect("表内条目");
        assert_eq!(exec.concurrency, Concurrency::Exclusive);
        assert!(exec.streaming);
        assert_eq!(exec.cancel_grace, SLOW_GRACE);
        assert!(!exec.idempotent);
        assert!(exec.workspace_bound);

        let read = builtin_capabilities("read").expect("表内条目");
        assert_eq!(read.concurrency, Concurrency::Parallel);
        assert!(read.idempotent);
        assert_eq!(read.cancel_grace, FAST_GRACE);
        assert!(read.workspace_bound);

        let ask = builtin_capabilities("ask").expect("表内条目");
        assert_eq!(ask.concurrency, Concurrency::Serial);
        assert!(!ask.idempotent);
        assert!(!ask.workspace_bound);
        assert!(ask.interactive, "ask 必须显式声明交互，才能使用零默认超时");

        assert_eq!(
            builtin_capabilities("mcp__fs__read_file"),
            None,
            "非内置工具不落表（回退默认档）"
        );
    }
}
