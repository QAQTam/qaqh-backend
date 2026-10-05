//! deny-steer 反馈(spec §5.7):把内核拒绝翻译成模型可执行的导流指引。
//! 文案冻结;关键词启发式对齐 codex is_likely_sandbox_denied。

pub const DENIAL_FEEDBACK: &str = "[win-sbx] 写入受沙箱保护的路径被拒绝(内核 DACL 策略)。\n受保护的文件请改用受管控的文件工具(走权限审批),或先在 policy 的 writable_files / writable_roots 中声明目标。\n";

const KEYWORDS: &[&str] = &[
    "access is denied",
    "permission denied",
    "denied",
    "operation did not complete successfully",
    "拒绝访问",
    "权限不足",
];

/// 退出码非零且 stderr 命中拒绝特征 → 疑似沙箱拒绝(尽力而为,与 Linux 侧同构)。
pub fn is_likely_sandbox_denied(exit_code: u32, stderr_text: &str) -> bool {
    if exit_code == 0 {
        return false;
    }
    let hay = stderr_text.to_lowercase();
    KEYWORDS.iter().any(|k| hay.contains(k))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyword_hits() {
        assert!(is_likely_sandbox_denied(1, "Access is denied."));
        assert!(is_likely_sandbox_denied(1, "FATAL: 拒绝访问。"));
        assert!(!is_likely_sandbox_denied(0, "Access is denied."));
        assert!(!is_likely_sandbox_denied(1, "all good"));
    }
}
