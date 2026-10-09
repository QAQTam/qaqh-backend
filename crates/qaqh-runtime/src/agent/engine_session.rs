//! SessionEngine: session lifecycle management.
//!
//! Handles: create, resume, reload_config.
//! Delegates to: lifecycle.rs for core session operations.

use super::types::*;
use crate::agent::state::lifecycle;

/// Number of recent turns sent on session restore.
const INITIAL_LOAD_COUNT: usize = 20;

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
/// `spawn_agent`（首启/恢复）与 [`SessionEngine::reload_config`]（热重载）共用
/// 这一个函数——两条路若各算一份，恢复出来的会话会先跑在全局 profile 上，
/// 直到下一次 `AgentReloadConfig` 才纠正。
///
/// `None` = 权威配置不可用（调用方按 `Config::default()` 兜底）。
pub(crate) fn session_effective_config(
    manager: Option<&qaqh_session::SessionManager>,
    session_id: &str,
) -> Option<(qaqh_config::Config, Option<String>)> {
    let global = qaqh_config::watch::authoritative()?;
    let profile = manager.and_then(|manager| manager.session_profile(session_id));
    let own_key = profile
        .as_deref()
        .and_then(|name| resolve_profile_key(&global, name));
    let cfg = qaqh_config::Config::for_session(&global, profile.as_deref(), own_key);
    Some((cfg, profile))
}

pub struct SessionEngine;

impl Default for SessionEngine {
    fn default() -> Self {
        Self::new()
    }
}

impl SessionEngine {
    pub fn new() -> Self {
        Self
    }

    /// Create a new session with a fresh seed.
    pub fn create(
        &self,
        agent: &mut crate::agent::state::agent::AgentState,
        _cancel: &CancelToken,
    ) {
        lifecycle::create_session(agent);
    }

    /// Create a new session with a pre-set seed (from CLI --seed).
    pub fn create_with_session(
        &self,
        agent: &mut crate::agent::state::agent::AgentState,
        _cancel: &CancelToken,
    ) {
        lifecycle::create_session_with_session(agent);
    }

    /// Resume an existing session. Returns false if the session doesn't exist.
    pub fn resume(
        &self,
        agent: &mut crate::agent::state::agent::AgentState,
        session_id: &str,
        _cancel: &CancelToken,
    ) -> bool {
        log::info!("[SESSION] resume seed={session_id}");
        if lifecycle::init_session(agent, Some(session_id)) {
            // Restore persisted agent mode（0=Code 也重置：避免进程内已切
            // plan/code 后恢复默认会话仍停留在旧模式——前后端显示/拦截一致）。
            let saved_mode = agent.session.mode;
            qaqh_workspace::runtime::set_mode(saved_mode);

            // legacy SessionRestored has been retired; Ringing restore is
            // handled by the daemon bootstrap snapshot, not emitted here.
            let loaded = INITIAL_LOAD_COUNT.min(agent.msg.turn_count());
            log::info!(
                "[SESSION] restored, {} turns (has_more={})",
                loaded,
                agent.msg.turn_count() > INITIAL_LOAD_COUNT
            );
            true
        } else {
            log::info!("[SESSION] init_session returned false for {session_id}");
            false
        }
    }

    /// Reload config from disk and apply to agent.
    ///
    /// 会话若选定了 profile（`meta.profile`），在本会话的有效配置是
    /// "全局配置 + 该 profile 覆盖"，**不广播**、不影响其它会话。
    pub fn reload_config(
        &self,
        agent: &mut crate::agent::state::agent::AgentState,
        _cancel: &CancelToken,
    ) {
        // P2-D1：磁盘为权威源；磁盘读失败时回退单写口广播的最新镜像。
        // 权威读收敛到 config crate 单入口（PR-1-8）。
        let Some((cfg, profile)) =
            session_effective_config(agent.session_manager.as_deref(), &agent.session.session_id)
        else {
            return;
        };
        agent.session.profile = profile;
        Self::apply_config(cfg, agent);
        crate::agent::state::lifecycle::load_session_workspace(agent);
    }

    /// 把磁盘配置的热同步字段整体拷入运行中 agent（[`Self::reload_config`] 的
    /// 唯一实现，取所有权避免逐字段 clone）。
    ///
    /// 2026-08-25 复盘（docs/current/architecture.md）：此前手工逐字段
    /// 拷贝漏掉 `auto_compact_threshold`，压缩 gate（engine_turn.rs:1326）读的
    /// 又是 `agent.config.auto_compact_threshold` → 活会话永远按旧阈值提前
    /// 压缩，而 UI/磁盘均已是新值（三方不一致，极难排查）。
    /// 单测 `applies_all_hot_fields` 锁定字段清单：**新增热同步字段时必须
    /// 同步补本测试断言**，否则该字段在运行中会话上静默不生效。
    /// （P2-D1 已落地 watch；待 `From<&Config>` 整体快照赋值落地后本函数将被取代。）
    pub(crate) fn apply_config(
        cfg: qaqh_config::Config,
        agent: &mut crate::agent::state::agent::AgentState,
    ) {
        agent.config.api_key = cfg.api_key;
        agent.config.model = cfg.model;
        agent.config.base_url = cfg.base_url;
        // BYOK：端点记录自身（wire/compat）随配置刷新，不再有预设坐标。
        agent.config.wire = cfg.wire;
        agent.config.compat = cfg.compat;
        agent.config.reasoning_effort = cfg.reasoning_effort;
        agent.config.max_tokens = cfg.max_tokens;
        agent.config.context_length = cfg.context_length;
        agent.config.auto_compact_threshold = cfg.auto_compact_threshold;
        agent.config.permission_level = cfg.permission_level;
        agent.config.exec = cfg.exec;
        // compliance_enabled 按回合读（engine_input.rs）；subagent.* 在 spawn 时
        // 读（spawn.rs 的 apply_subagent_config）——两者都不在早期清单里，漏同步
        // 会让设置页改完对已开会话不生效。
        agent.config.compliance_enabled = cfg.compliance_enabled;
        agent.config.subagent = cfg.subagent;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 锁定 reload 热同步字段清单（R1 回归守卫：曾漏 auto_compact_threshold，
    /// 导致活会话永远用旧阈值提前压缩）。新增同步字段 → 同步补断言。
    #[test]
    fn applies_all_hot_fields() {
        let new_cfg = qaqh_config::Config {
            api_key: "sk-new".into(),
            model: "model-new".into(),
            base_url: "https://new.example/v1".into(),
            wire: qaqh_types::Wire::Responses,
            compat: qaqh_types::EndpointCompat {
                responses_effort_max: "max".into(),
                ..Default::default()
            },
            reasoning_effort: "max".into(),
            max_tokens: 123_456,
            context_length: 2_000_000,
            auto_compact_threshold: 0.95,
            permission_level: 3,
            exec: qaqh_config::config::ExecConfig {
                default_shell: Some("zsh".into()),
            },
            // 按回合读（engine_input.rs）：必须与旧值不同以证明确实同步。
            compliance_enabled: false,
            // spawn 时读（spawn.rs）：至少要改 model / max_tokens 以证明整体同步。
            subagent: qaqh_config::config::SubagentConfig {
                model: "sub-new".into(),
                max_tokens: 8192,
                ..Default::default()
            },
            ..Default::default()
        };
        let mut agent = crate::agent::state::agent::AgentState::new(qaqh_config::Config {
            api_key: "sk-old".into(),
            model: "model-old".into(),
            base_url: "https://old.example/v1".into(),
            reasoning_effort: "low".into(),
            max_tokens: 4096,
            context_length: 10_000,
            auto_compact_threshold: 0.3,
            permission_level: 1,
            compliance_enabled: true,
            subagent: qaqh_config::config::SubagentConfig {
                model: "sub-old".into(),
                max_tokens: 1024,
                ..Default::default()
            },
            ..Default::default()
        });

        SessionEngine::apply_config(new_cfg, &mut agent);

        assert_eq!(agent.config.api_key, "sk-new");
        assert_eq!(agent.config.model, "model-new");
        assert_eq!(agent.config.base_url, "https://new.example/v1");
        assert_eq!(agent.config.reasoning_effort, "max");
        assert_eq!(agent.config.max_tokens, 123_456);
        // BYOK：端点自述的 wire/compat 也是热字段（reload 后请求形状必须跟着换）。
        assert_eq!(agent.config.wire, qaqh_types::Wire::Responses);
        assert_eq!(agent.config.compat.responses_effort_max, "max");
        // 单一压缩分母（软阈值与硬 pre-flight 同源）。
        assert_eq!(agent.config.context_length, 2_000_000);
        // R1 主角：阈值必须随 reload 同步到运行中会话。
        assert!((agent.config.auto_compact_threshold - 0.95).abs() < f64::EPSILON);
        assert_eq!(agent.config.permission_level, 3);
        assert_eq!(agent.config.exec.default_shell.as_deref(), Some("zsh"));
        // compliance_enabled 按回合读；subagent.* 在 spawn 时读——漏同步会让
        // 设置页改完对已开会话不生效。
        assert!(!agent.config.compliance_enabled);
        assert_eq!(agent.config.subagent.model, "sub-new");
        assert_eq!(agent.config.subagent.max_tokens, 8192);
    }
}
