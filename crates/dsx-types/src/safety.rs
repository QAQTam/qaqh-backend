// ── Safety levels for tool execution ──

#[derive(Debug, Clone, PartialEq)]
pub enum SafetyLevel {
    Safe,     // auto-execute (read, list, search, git-status)
    Confirm,  // ask user (write, edit, git-commit, curl, build)
    Danger,   // ask user with warning (sudo, rm, chmod, dd)
}
