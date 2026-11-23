// P0-2：折叠策略的作用域契约——策略不再经 `CURRENT_POLICY` TLS 或
// `ToolExecutionScope` 跨线程搬运，而是由调用方显式传给 `tool_side_fold::apply`。
// 并发线程各自持有独立策略引用，零交叉污染。

use qaqh_workspace::tool_side_fold::{self, NoFoldPolicy, StandardPolicy, ToolResultFoldPolicy};

/// 标准策略下 `exec` 的字符上限是 8K；`ToolResult::ok` 的模型面硬顶是 24K。
const STANDARD_FOLD_UPPER_BOUND: usize = 12_000;

fn folded_exec_chars(policy: &dyn ToolResultFoldPolicy) -> usize {
    let body = "line of output\n".repeat(2_000); // ~30K chars，远超 8K 命令输出限额
    let mut result = qaqh_types::ToolResult::ok(body);
    tool_side_fold::apply(policy, "exec_command", &mut result);
    result.model_text().chars().count()
}

#[test]
fn explicit_fold_policies_never_cross_contaminate_concurrent_threads() {
    let (standard_chars, no_fold_chars) = std::thread::scope(|scope| {
        let std_handle = scope.spawn(|| {
            let first = folded_exec_chars(&StandardPolicy);
            let second = folded_exec_chars(&StandardPolicy);
            (first, second)
        });
        let no_fold_handle = scope.spawn(|| folded_exec_chars(&NoFoldPolicy));
        (
            std_handle.join().expect("join standard"),
            no_fold_handle.join().expect("join no-fold"),
        )
    });

    assert!(
        standard_chars.0 < STANDARD_FOLD_UPPER_BOUND,
        "StandardPolicy 下 exec_command 输出应被折叠到 8K 量级，实际 {} 字符",
        standard_chars.0
    );
    assert_eq!(
        standard_chars.0, standard_chars.1,
        "并发线程使用 NoFoldPolicy 不得影响本线程 StandardPolicy 结果"
    );
    assert!(
        no_fold_chars > STANDARD_FOLD_UPPER_BOUND,
        "NoFoldPolicy 应透传至 24K 硬顶，实际 {no_fold_chars} 字符"
    );
}
