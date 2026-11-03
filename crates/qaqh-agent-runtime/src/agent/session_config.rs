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

/// 新 BYOK 目录体系：把解析出的模型选择覆盖到运行配置上。
///
/// 覆盖面 = 端点身份（base_url/wire/compat/headers/credential）+ 模型身份
/// （model/max_tokens/context_window）+ 能力位与档位。凭据：`api_key` 型
/// provider 用自己的加密槽（`secrets.providers.<id>`），未存储时**继承**现有
/// cfg（旧主密钥语义）；账号型（chatgpt_plan / deepseek_account）凭据由账号
/// 链路注入，此处不动。解析失败（provider/model 已删、档位非法）时告警并
/// 保持现有配置——宁可沿用上一轮可用配置，也不把会话打成空配置。
fn apply_model_selection(
    cfg: &mut qaqh_config::Config,
    catalog: &qaqh_profile::ProfilesDocument,
    selection: &qaqh_profile::ModelSelection,
) {
    let Ok(effective) = qaqh_profile::resolve_model_selection(catalog, selection) else {
        log::warn!(
            "[session_config] model selection {}/{} failed to resolve; keeping current config",
            selection.provider_id,
            selection.model_id
        );
        return;
    };
    cfg.provider_id = effective.provider_id.clone();
    cfg.provider_name = effective.provider_id.clone();
    cfg.model = effective.model_id.clone();
    cfg.base_url = effective.base_url.clone();
    cfg.wire = effective.wire;
    cfg.compat = effective.compat.clone().unwrap_or_default();
    cfg.credential = effective.credential;
    cfg.provider_headers = effective.headers.clone();
    cfg.max_tokens = effective.max_output_tokens;
    cfg.context_length = effective.context_window;
    cfg.reasoning_effort = effective.reasoning_effort.clone().unwrap_or_default();
    cfg.model_capabilities = effective.capabilities;
    cfg.model_reasoning_levels = effective.reasoning_levels.clone();
    cfg.model_native_web_search = effective.native_web_search;
    if effective.credential == qaqh_types::CredentialSource::ApiKey {
        let secrets = qaqh_config::secrets::SecretStore::default_location();
        if secrets.has_provider_key(&effective.provider_id) {
            // 带槽取不到明文 → 空串（同 profile key 纪律：宁可报错也不串密钥）。
            cfg.api_key = secrets
                .load_provider_key(&effective.provider_id)
                .unwrap_or_default();
        }
        // 未存储 → 继承 cfg.api_key（main 密钥），与旧 profile 语义一致。
    }
}

/// 该会话的**有效配置** + 它选中的 profile 名。
///
/// `spawn_agent`（首启/恢复）与热重载共用这一个函数——两条路若各算一份，
/// 恢复出来的会话会先跑在全局 profile 上，直到下一次热重载才纠正。
///
/// 配置来源优先级（新 BYOK 目录体系落地后的次序）：
/// 1. 会话级 `model_selection`（`session.settings.patch` / 创建时锁定）；
/// 2. 全局 `default_model_selection`；
/// 3. 旧 profile 名链路（废除过渡期保留）。
///
/// `None` = 权威配置不可用（调用方按 `Config::default()` 兜底）。
pub fn session_effective_config(
    owners: Option<&qaqh_session::owner::OwnerDirectory>,
    session_id: &str,
) -> Option<(qaqh_config::Config, Option<String>)> {
    use qaqh_types::SettingValue;
    let global = qaqh_config::watch::authoritative()?;
    let owned = session_id.to_string();
    let row = owners.and_then(|owners| {
        owners
            .db()
            .read_blocking(move |conn| qaqh_session::db::store::session_row(conn, &owned))
            .ok()?
    });
    let profile = row
        .as_ref()
        .and_then(|row| row.profile.clone())
        .filter(|name| !name.is_empty());
    let own_key = profile
        .as_deref()
        .and_then(|name| resolve_profile_key(&global, name));
    let mut cfg = qaqh_config::Config::for_session(&global, profile.as_deref(), own_key);
    if let Some(effort) = row.as_ref().and_then(|row| row.effort.clone()).filter(|e| !e.is_empty()) {
        cfg.reasoning_effort = effort;
    }
    let selection = row
        .as_ref()
        .and_then(|row| row.model_selection.clone())
        .map(|selection| SettingValue::Use { value: selection })
        .or_else(|| {
            global
                .default_model_selection
                .clone()
                .map(|value| qaqh_types::SettingValue::Use { value })
        });
    if let Some(selection) = selection {
        let selection = match selection {
            SettingValue::Use { value } => value,
            SettingValue::Inherit => return Some((cfg, profile)),
        };
        apply_model_selection(&mut cfg, &global.profiles_catalog, &selection);
    }
    Some((cfg, profile))
}
