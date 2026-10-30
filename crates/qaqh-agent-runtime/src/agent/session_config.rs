//! 会话**有效配置**解析。
//!
//! 原 `engine_session.rs` 的 `SessionEngine`（create / resume / reload_config /
//! apply_config）全部以 `&mut AgentState` 为参数，随旧状态机一并删除。这里只剩
//! 两个纯函数：`spawn_agent`（首启/恢复）与配置热重载共同依赖它们。

/// 解析某 profile 的自带密钥。
///
/// 只有 profile 带 `"set"` 标记时才读 `secrets.toml`（`None` = 该 profile
/// 不自带 key → 继承主密钥）。带标记但取不回明文时返回**空串**而不是 `None`：
/// 宁可这一轮不带凭据报错，也不把主密钥发到别的端点去。
fn resolve_profile_key(cfg: &qaqh_config::Config, name: &str) -> Option<String> {
    if !cfg.profile_carries_key(name) {
        return None;
    }
    Some(
        qaqh_config::secrets::SecretStore::default_location()
            .load_profile_key(name)
            .unwrap_or_default(),
    )
}

/// 该会话的**有效配置** + 它选中的 profile 名。
///
/// `spawn_agent`（首启/恢复）与热重载共用这一个函数——两条路若各算一份，
/// 恢复出来的会话会先跑在全局 profile 上，直到下一次热重载才纠正。
///
/// `None` = 权威配置不可用（调用方按 `Config::default()` 兜底）。
pub fn session_effective_config(
    manager: Option<&qaqh_session::SessionManager>,
    session_id: &str,
) -> Option<(qaqh_config::Config, Option<String>)> {
    let global = qaqh_config::watch::authoritative()?;
    let profile = manager.and_then(|manager| manager.session_profile(session_id));
    let own_key = profile
        .as_deref()
        .and_then(|name| resolve_profile_key(&global, name));
    let mut cfg = qaqh_config::Config::for_session(&global, profile.as_deref(), own_key);
    if let Some(effort) = manager.and_then(|m| m.session_effort(session_id)) {
        cfg.reasoning_effort = effort;
    }
    Some((cfg, profile))
}
