//! 引擎 [`Config`](crate::Config) ↔ wire DTO（`qaqh-config-api`）双向映射层。
//!
//! 当前架构见 docs/current/architecture.md：service 层不再手拼 config json，
//! 所有字段同步收敛到本模块的**穷举字面量**里——任何一端新增字段而另一端
//! 未映射，在此处直接编译失败（S2「人肉同步」病根由编译器接管）。
//!
//! 守卫语义冻结（与 2026-08 前 `update_string/update_u32` 行为对齐，见
//! qaqh-config-api `ConfigPatch` 文档）：
//! - apiKey / subagent.apiKey：`"****"` 或空串 = 保持现值（显式删除走专用接口）；
//! - model / baseUrl / providerId / endpoint / reasoningEffort：空串 = 保持
//!   （防未加载草稿整包写回时清空用户配置——2026-08 根因 1 同类防御）；
//! - tokenizerPath：空串 = 清除（None，未设置）；
//! - exec.defaultShell：空串 / "auto" = 回到平台自动探测（None）；
//! - 数值：validate() 已保证值域；u64→u32 饱和转换防回绕。

use crate::config::Config;
use qaqh_config_api::{
    ConfigDto, ConfigPatch, ExecDto, LspDto, LspServerDto, McpDto, McpServerDto, ProfileSummaryDto,
    SandboxDto, SubagentDto,
};

/// 引擎配置 → 读模型。api_key 按契约掩码：非空一律 `"****"`（明文永不出 daemon）。
pub fn to_dto(cfg: &Config) -> ConfigDto {
    // BYOK：没有 provider 目录可下发——设置面就是这六个字段自身。
    ConfigDto {
        model: cfg.model.clone(),
        base_url: cfg.base_url.clone(),
        wire: cfg.wire.as_str().to_string(),
        credential: credential_wire(cfg.credential).to_string(),
        max_tokens: u64::from(cfg.max_tokens),
        context_length: u64::from(cfg.context_length),
        reasoning_effort: cfg.reasoning_effort.clone(),
        auto_compact_threshold: cfg.auto_compact_threshold,
        permission_level: cfg.permission_level,
        api_key: masked_or_empty(&cfg.api_key),
        lang: cfg.lang.clone(),
        font_family: cfg.font_family.clone(),
        theme: cfg.theme.clone(),
        notifications_enabled: cfg.notifications_enabled.unwrap_or(true),
        active_profile: cfg.active_profile.clone(),
        profiles: cfg.profiles.keys().cloned().collect(),
        profiles_detail: cfg
            .profiles
            .iter()
            .map(|(name, p)| {
                (
                    name.clone(),
                    ProfileSummaryDto {
                        name: name.clone(),
                        model: p.model.clone(),
                        base_url: p.base_url.clone(),
                        wire: p.wire.as_str().to_string(),
                        effort: p.effort.clone(),
                        context_length: u64::from(p.context_length),
                        max_tokens: u64::from(p.max_tokens),
                        api_key_set: p.api_key.is_some(),
                        credential: credential_wire(p.credential).to_string(),
                    },
                )
            })
            .collect(),
        compliance_enabled: cfg.compliance_enabled,
        subagent: SubagentDto {
            model: cfg.subagent.model.clone(),
            base_url: cfg.subagent.base_url.clone(),
            api_key: masked_or_empty(&cfg.subagent.api_key),
            api_key_set: !cfg.subagent.api_key.is_empty(),
            max_tokens: u64::from(cfg.subagent.max_tokens),
            timeout_secs: cfg.subagent.timeout_secs,
            default_tools: cfg.subagent.default_tools.clone(),
            max_depth: u64::from(cfg.subagent.max_depth),
            message_in_flight_per_pair: cfg.subagent.message_in_flight_per_pair,
            message_outbound_per_sender: cfg.subagent.message_outbound_per_sender,
        },
        mcp: McpDto {
            enabled: cfg.mcp.enabled,
            idle_shutdown_secs: cfg.mcp.idle_shutdown_secs,
            servers: cfg
                .mcp
                .servers
                .iter()
                .map(|(name, s)| McpServerDto {
                    name: name.clone(),
                    transport: match s.transport {
                        crate::config::McpTransportKind::Stdio => "stdio".into(),
                        crate::config::McpTransportKind::Http => "http".into(),
                    },
                    command: s.command.clone(),
                    args: s.args.clone(),
                    env: s.env.clone(),
                    url: s.url.clone(),
                    headers: s.headers.clone(),
                    tools: s.tools.clone(),
                    resources_enabled: s.resources_enabled,
                    default_timeout_secs: s.default_timeout_secs,
                    max_concurrent_calls: s.max_concurrent_calls,
                    cwd: s.cwd.clone(),
                })
                .collect(),
        },
        lsp: LspDto {
            enabled: cfg.lsp.enabled,
            idle_shutdown_secs: cfg.lsp.idle_shutdown_secs,
            servers: cfg
                .lsp
                .servers
                .iter()
                .map(|(name, s)| LspServerDto {
                    name: name.clone(),
                    command: s.command.clone(),
                    args: s.args.clone(),
                    env: s.env.clone(),
                    extensions: s.extensions.clone(),
                    startup_timeout_secs: s.startup_timeout_secs,
                    default_timeout_secs: s.default_timeout_secs,
                })
                .collect(),
        },
        tokenizer_path: cfg.tokenizer_path.clone(),
        exec: ExecDto {
            default_shell: cfg.exec.default_shell.clone(),
        },
        sandbox: SandboxDto {
            enabled: cfg.sandbox.enabled,
        },
        session_idle_unload_secs: cfg.session_idle_unload_secs,
        tool_allowlist_enabled: cfg.tool_allowlist_enabled,
        tool_allowlist: cfg.tool_allowlist.clone(),
    }
}

/// 把 Merge Patch 应用到引擎配置（`Config::update` 单写口的 mutate 体）。
/// 值域校验先行；逐字段守卫语义见模块文档。
pub fn apply_patch(cfg: &mut Config, patch: &ConfigPatch) -> Result<(), String> {
    patch.validate()?;
    // 历史「空串 = 保持」语义：过滤掩码与空串，仅显式新值放行。
    let meaningful = |s: &Option<String>| {
        s.as_deref()
            .is_some_and(|v| !v.is_empty() && v != "****")
            .then(|| s.clone().expect("checked some"))
    };
    // 一致性判定必须在**任何赋值之前**做：半途返回 Err 会留下半个改动，而调用方
    // 未必有回滚。判据用「补丁应用后的形状」——补丁给了就用补丁的，没给就用现值，
    // 这样「两边任一被改动」都查得到：只查「本次补丁带没带 credential」会漏掉
    // 「已有 plan 的用户把 wire 改走」那一半。
    let next_credential = match patch.credential.as_deref() {
        // 值域已由 validate() 收口；这里只解析，**不给未知值兜底**——静默改回
        // ApiKey 正是「用户以为在用套餐、实际打自己的 key」那类最难查的问题。
        Some("api_key") => qaqh_types::CredentialSource::ApiKey,
        Some("chatgpt_plan") => qaqh_types::CredentialSource::ChatgptPlan,
        Some("deepseek_account") => qaqh_types::CredentialSource::DeepseekAccount,
        Some(other) => return Err(format!("无法识别的 credential: {other}")),
        None => cfg.credential,
    };
    // 初次切到 DeepSeek 账号且未指定 wire 时，把通用默认 Chat 换成
    // Responses；OAuth 路由只维护 Responses，显式 Chat 会在下面被拒绝。
    let next_wire = match meaningful(&patch.wire) {
        Some(v) => qaqh_types::Wire::parse(&v).ok_or_else(|| format!("无法识别的 wire: {v}"))?,
        None if next_credential == qaqh_types::CredentialSource::DeepseekAccount
            && cfg.credential != qaqh_types::CredentialSource::DeepseekAccount
            && cfg.wire == qaqh_types::Wire::Chat =>
        {
            qaqh_types::Wire::Responses
        }
        None => cfg.wire,
    };
    // 套餐直连只提供 Responses API：保存期就拦掉，别等到第一回合请求才报。
    //
    // 官方 base_url 那一半刻意不在这里查：权威判定在 qaqh-runtime 的
    // `apply_chatgpt_plan`（gate.rs），它持有 `qaqh_siwc::API_BASE`；本 crate
    // 不依赖 qaqh-siwc，在这里再写一份 URL 字面量就是第二个真相源。
    if next_credential == qaqh_types::CredentialSource::ChatgptPlan
        && next_wire != qaqh_types::Wire::Responses
    {
        return Err(format!(
            "credential=chatgpt_plan 必须配 responses 协议（wire={}）：套餐直连只提供 Responses API",
            next_wire.as_str()
        ));
    }
    // DeepSeek 账号只维护 Responses：OAuth 路由不再保留 Chat 分支。
    //
    // Responses 也是官方明确支持「在对话中间插入工具调用」的 OpenAI 兼容面；
    // Anthropic 面在本仓的账号凭据头形态下不可用。base_url 的权威校验仍在
    // `apply_deepseek_account`，这里只收口 wire 值域。
    if next_credential == qaqh_types::CredentialSource::DeepseekAccount
        && next_wire != qaqh_types::Wire::Responses
    {
        return Err(format!(
            "credential=deepseek_account 只支持 responses 协议（wire={}）：OAuth 路由不再维护 chat/anthropic 面",
            next_wire.as_str()
        ));
    }
    if let Some(v) = meaningful(&patch.api_key) {
        cfg.api_key = v;
    }
    if let Some(v) = meaningful(&patch.model) {
        cfg.model = v;
    }
    if let Some(v) = meaningful(&patch.base_url) {
        cfg.base_url = v;
    }
    cfg.wire = next_wire;
    if patch.credential.is_some() {
        // 解析与一致性判定已在上方（赋值之前）完成，这里只落地。
        cfg.credential = next_credential;
    }
    if let Some(v) = patch.max_tokens {
        cfg.max_tokens = u32::try_from(v).unwrap_or(u32::MAX);
    }
    if let Some(v) = patch.context_length {
        cfg.context_length = u32::try_from(v).unwrap_or(u32::MAX);
    }
    if let Some(v) = meaningful(&patch.reasoning_effort) {
        cfg.reasoning_effort = v;
    }
    if let Some(v) = patch.auto_compact_threshold {
        cfg.auto_compact_threshold = v;
    }
    if let Some(v) = patch.compliance_enabled {
        cfg.compliance_enabled = v;
    }
    if let Some(v) = patch.permission_level {
        // validate() 已保证 1..=3（BUG-2026-09-13-15；三档制 2026-10-03）。
        cfg.permission_level = u8::try_from(v).unwrap_or(1);
    }
    if let Some(v) = &patch.tokenizer_path {
        cfg.tokenizer_path = (!v.is_empty()).then(|| v.clone());
    }
    if let Some(exec) = &patch.exec
        && let Some(v) = &exec.default_shell
    {
        // 与 load 路径同口径归一化（trim + 小写）；空串 / "auto" = 自动探测。
        let shell = v.trim().to_ascii_lowercase();
        cfg.exec.default_shell = (!shell.is_empty() && shell != "auto").then_some(shell);
    }
    if let Some(v) = patch.session_idle_unload_secs {
        cfg.session_idle_unload_secs = v;
    }
    if let Some(v) = patch.tool_allowlist_enabled {
        cfg.tool_allowlist_enabled = v;
    }
    if let Some(v) = &patch.tool_allowlist {
        cfg.tool_allowlist = v.clone();
    }
    if let Some(sandbox) = &patch.sandbox
        && let Some(enabled) = sandbox.enabled
    {
        cfg.sandbox.enabled = enabled;
    }
    if let Some(sub) = &patch.subagent {
        if let Some(v) = meaningful(&sub.model) {
            cfg.subagent.model = v;
        }
        if let Some(v) = meaningful(&sub.base_url) {
            cfg.subagent.base_url = v;
        }
        if let Some(v) = meaningful(&sub.api_key) {
            cfg.subagent.api_key = v;
        }
        if let Some(v) = sub.max_tokens {
            cfg.subagent.max_tokens = u32::try_from(v).unwrap_or(u32::MAX);
        }
        if let Some(v) = sub.timeout_secs {
            cfg.subagent.timeout_secs = v;
        }
        if let Some(v) = &sub.default_tools {
            // 允许空数组（= 全部工具可用）；工具名归一化在 load 路径统一做。
            cfg.subagent.default_tools = v.clone();
        }
        if let Some(v) = sub.max_depth {
            cfg.subagent.max_depth = u32::try_from(v).unwrap_or(u32::MAX);
        }
        if let Some(v) = sub.message_in_flight_per_pair {
            cfg.subagent.message_in_flight_per_pair = v;
        }
        if let Some(v) = sub.message_outbound_per_sender {
            cfg.subagent.message_outbound_per_sender = v;
        }
    }
    Ok(())
}

fn masked_or_empty(key: &str) -> String {
    if key.is_empty() {
        String::new()
    } else {
        "****".to_string()
    }
}

/// `CredentialSource` → wire 字面量。与 `ConfigPatch::credential` 的值域同源：
/// 读写两侧都由 `qaqh_types::CredentialSource` 的 serde 表示定义。
fn credential_wire(source: qaqh_types::CredentialSource) -> &'static str {
    match source {
        qaqh_types::CredentialSource::ApiKey => "api_key",
        qaqh_types::CredentialSource::ChatgptPlan => "chatgpt_plan",
        qaqh_types::CredentialSource::DeepseekAccount => "deepseek_account",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 穷举映射回归：改任一字段后此测试必须同步更新（K1 编译期+运行期双保险）。
    #[test]
    fn to_dto_masks_secrets_and_projects_all_sections() {
        let mut cfg = Config {
            api_key: "sk-secret".into(),
            ..Default::default()
        };
        cfg.model = "m".into();
        cfg.context_length = 1_000_000;
        cfg.auto_compact_threshold = 0.95;
        cfg.subagent.api_key = "sk-sub".into();
        cfg.profiles.insert(
            "default".into(),
            qaqh_types::ProfileConfig {
                model: "m".into(),
                max_tokens: 4096,
                effort: Some("high".into()),
                context_length: 128_000,
                base_url: String::new(),
                wire: qaqh_types::Wire::Chat,
                compat: None,
                api_key: None,
                credential: qaqh_types::CredentialSource::ApiKey,
                preset_endpoint: None,
                context_limit: None,
                context_window: None,
            },
        );

        let dto = to_dto(&cfg);
        assert_eq!(dto.api_key, "****");
        assert_eq!(dto.subagent.api_key, "****");
        assert!(dto.subagent.api_key_set);
        assert_eq!(dto.context_length, 1_000_000);
        assert!((dto.auto_compact_threshold - 0.95).abs() < f64::EPSILON);
        assert!(dto.profiles.contains(&"default".to_string()));
        let empty = to_dto(&Config::default());
        assert_eq!(empty.api_key, "");
        assert!(!empty.subagent.api_key_set);
    }

    #[test]
    fn tool_allowlist_patch_and_projection() {
        let mut cfg = Config::default();
        let patch: ConfigPatch = serde_json::from_value(serde_json::json!({
            "toolAllowlistEnabled": true, "toolAllowlist": ["read"]
        }))
        .unwrap();
        apply_patch(&mut cfg, &patch).unwrap();
        let dto = to_dto(&cfg);
        assert!(dto.tool_allowlist_enabled);
        assert_eq!(dto.tool_allowlist, vec!["read"]);
        apply_patch(
            &mut cfg,
            &serde_json::from_value(serde_json::json!({"toolAllowlist": []})).unwrap(),
        )
        .unwrap();
        assert!(cfg.tool_allowlist_enabled);
        assert!(cfg.tool_allowlist.is_empty());
    }

    #[test]
    fn apply_patch_updates_fields_and_keeps_untouched() {
        let mut cfg = Config {
            model: "old".into(),
            auto_compact_threshold: 0.3,
            ..Default::default()
        };
        let patch = ConfigPatch {
            model: Some("new".into()),
            context_length: Some(2_000_000),
            auto_compact_threshold: Some(0.95),
            ..Default::default()
        };
        apply_patch(&mut cfg, &patch).expect("apply");
        assert_eq!(cfg.model, "new");
        assert_eq!(cfg.context_length, 2_000_000);
        assert!((cfg.auto_compact_threshold - 0.95).abs() < f64::EPSILON);
    }

    #[test]
    fn apply_patch_keeps_secret_and_empty_semantics() {
        let mut cfg = Config {
            api_key: "sk-keep".into(),
            ..Default::default()
        };
        cfg.model = "m".into();
        // 掩码/空串 = 保持现值。
        let patch = ConfigPatch {
            api_key: Some("****".into()),
            model: Some(String::new()),
            ..Default::default()
        };
        apply_patch(&mut cfg, &patch).expect("apply");
        assert_eq!(cfg.api_key, "sk-keep");
        assert_eq!(cfg.model, "m");
        // 显式新值 = 替换。
        let replace = ConfigPatch {
            api_key: Some("sk-new".into()),
            ..Default::default()
        };
        apply_patch(&mut cfg, &replace).expect("apply");
        assert_eq!(cfg.api_key, "sk-new");
    }

    /// exec / session_idle_unload_secs 是「后端有、前端新补」的写字段。
    #[test]
    fn apply_patch_writes_exec_and_idle_unload() {
        let mut cfg = Config::default();
        assert_eq!(cfg.exec.default_shell, None);
        let patch = ConfigPatch {
            exec: Some(qaqh_config_api::ExecPatch {
                default_shell: Some("Bash".into()),
            }),
            session_idle_unload_secs: Some(900),
            ..Default::default()
        };
        apply_patch(&mut cfg, &patch).expect("apply");
        // 与 load 同口径归一化：小写。
        assert_eq!(cfg.exec.default_shell.as_deref(), Some("bash"));
        assert_eq!(cfg.session_idle_unload_secs, 900);
        // 空串 / "auto" = 回到自动探测。
        let auto = ConfigPatch {
            exec: Some(qaqh_config_api::ExecPatch {
                default_shell: Some("auto".into()),
            }),
            session_idle_unload_secs: Some(0),
            ..Default::default()
        };
        apply_patch(&mut cfg, &auto).expect("apply");
        assert_eq!(cfg.exec.default_shell, None);
        assert_eq!(cfg.session_idle_unload_secs, 0);
    }

    #[test]
    fn apply_patch_writes_sandbox() {
        let mut cfg = Config::default();
        assert!(cfg.sandbox.enabled);
        let patch = ConfigPatch {
            sandbox: Some(qaqh_config_api::SandboxPatch {
                enabled: Some(false),
            }),
            ..Default::default()
        };
        apply_patch(&mut cfg, &patch).expect("apply sandbox false");
        assert!(!cfg.sandbox.enabled);
        assert!(!to_dto(&cfg).sandbox.enabled);

        let patch_on = ConfigPatch {
            sandbox: Some(qaqh_config_api::SandboxPatch {
                enabled: Some(true),
            }),
            ..Default::default()
        };
        apply_patch(&mut cfg, &patch_on).expect("apply sandbox true");
        assert!(cfg.sandbox.enabled);
        assert!(to_dto(&cfg).sandbox.enabled);
    }

    #[test]
    fn apply_patch_subagent_partial_and_empty_tools_allowed() {
        let mut cfg = Config::default();
        cfg.subagent.timeout_secs = 120;
        cfg.subagent.default_tools = vec!["read".into()];
        let patch = ConfigPatch {
            subagent: Some(qaqh_config_api::SubagentPatch {
                timeout_secs: Some(240),
                default_tools: Some(vec![]),
                ..Default::default()
            }),
            ..Default::default()
        };
        apply_patch(&mut cfg, &patch).expect("apply");
        assert_eq!(cfg.subagent.timeout_secs, 240);
        assert!(cfg.subagent.default_tools.is_empty());
    }

    #[test]
    fn apply_patch_rejects_invalid_before_mutating() {
        let mut cfg = Config {
            auto_compact_threshold: 0.75,
            ..Default::default()
        };
        let bad = ConfigPatch {
            auto_compact_threshold: Some(1.5),
            ..Default::default()
        };
        assert!(apply_patch(&mut cfg, &bad).is_err());
        assert!((cfg.auto_compact_threshold - 0.75).abs() < f64::EPSILON);
    }

    #[test]
    fn apply_patch_saturates_u32_overflow() {
        let mut cfg = Config::default();
        let patch = ConfigPatch {
            context_length: Some(u64::MAX),
            ..Default::default()
        };
        apply_patch(&mut cfg, &patch).expect("apply");
        assert_eq!(cfg.context_length, u32::MAX);
    }

    /// `credential` 是「显式声明额度来源」的唯一入口：必须能经 patch 落进配置，
    /// 且**不因后续补丁而静默改回 BYOK**。
    #[test]
    fn apply_patch_sets_credential_and_never_resets_it() {
        let mut cfg = Config {
            wire: qaqh_types::Wire::Responses,
            ..Default::default()
        };
        apply_patch(
            &mut cfg,
            &ConfigPatch {
                credential: Some("chatgpt_plan".into()),
                ..Default::default()
            },
        )
        .expect("apply");
        assert_eq!(cfg.credential, qaqh_types::CredentialSource::ChatgptPlan);

        // 只改模型的补丁不得动额度来源（这正是「静默改回 BYOK」的回归位）。
        apply_patch(
            &mut cfg,
            &ConfigPatch {
                model: Some("m2".into()),
                ..Default::default()
            },
        )
        .expect("apply");
        assert_eq!(cfg.credential, qaqh_types::CredentialSource::ChatgptPlan);

        // 显式切回 BYOK 才是唯一能改回去的方式。
        apply_patch(
            &mut cfg,
            &ConfigPatch {
                credential: Some("api_key".into()),
                ..Default::default()
            },
        )
        .expect("apply");
        assert_eq!(cfg.credential, qaqh_types::CredentialSource::ApiKey);
    }

    /// 配错组合在**保存期**就报，并带出该怎么改；拒绝时不得留半个改动。
    #[test]
    fn apply_patch_rejects_plan_on_a_non_responses_wire() {
        let mut cfg = Config {
            wire: qaqh_types::Wire::Chat,
            ..Default::default()
        };
        let err = apply_patch(
            &mut cfg,
            &ConfigPatch {
                credential: Some("chatgpt_plan".into()),
                ..Default::default()
            },
        )
        .expect_err("openai wire + plan must be rejected");
        assert!(err.contains("responses"), "{err}");
        assert_eq!(
            cfg.credential,
            qaqh_types::CredentialSource::ApiKey,
            "拒绝即不落地"
        );

        // 同一次补丁里把 wire 一起改对就该通过（判定用应用后的 cfg.wire）。
        apply_patch(
            &mut cfg,
            &ConfigPatch {
                wire: Some("responses".into()),
                credential: Some("chatgpt_plan".into()),
                ..Default::default()
            },
        )
        .expect("apply");
        assert_eq!(cfg.credential, qaqh_types::CredentialSource::ChatgptPlan);
    }

    #[test]
    fn deepseek_account_defaults_to_responses_and_rejects_chat() {
        use qaqh_types::{CredentialSource, Wire};

        let mut cfg = Config::default();
        apply_patch(
            &mut cfg,
            &ConfigPatch {
                credential: Some("deepseek_account".into()),
                ..Default::default()
            },
        )
        .expect("select account");
        assert_eq!(cfg.wire, Wire::Responses);
        assert_eq!(cfg.credential, CredentialSource::DeepseekAccount);
        assert_eq!(to_dto(&cfg).wire, "responses");

        let err = apply_patch(
            &mut cfg,
            &ConfigPatch {
                wire: Some("chat".into()),
                ..Default::default()
            },
        )
        .expect_err("OAuth must reject chat");
        assert!(err.contains("只支持 responses"), "{err}");
        assert_eq!(cfg.wire, Wire::Responses, "拒绝即不落地");

        let mut explicit = Config::default();
        let err = apply_patch(
            &mut explicit,
            &ConfigPatch {
                credential: Some("deepseek_account".into()),
                wire: Some("chat".into()),
                ..Default::default()
            },
        )
        .expect_err("account with explicit chat must be rejected");
        assert!(err.contains("只支持 responses"), "{err}");
        assert_eq!(
            explicit.credential,
            CredentialSource::ApiKey,
            "拒绝即不落地"
        );
        assert_eq!(explicit.wire, Wire::Chat, "拒绝即不落地");
    }

    /// DeepSeek 账号：配错 wire 在保存期就报，且**双向**都拦（与套餐那条同规矩）。
    ///
    /// 值域只有 responses：OAuth 路由不再维护 Chat；Anthropic 面拿不到账号
    /// 凭据头的形态，故也不开放。
    #[test]
    fn apply_patch_rejects_account_on_the_anthropic_wire_and_vice_versa() {
        let mut cfg = Config {
            wire: qaqh_types::Wire::Messages,
            ..Default::default()
        };
        let err = apply_patch(
            &mut cfg,
            &ConfigPatch {
                credential: Some("deepseek_account".into()),
                ..Default::default()
            },
        )
        .expect_err("anthropic wire + account must be rejected");
        assert!(err.contains("只支持 responses"), "{err}");
        assert_eq!(
            cfg.credential,
            qaqh_types::CredentialSource::ApiKey,
            "拒绝即不落地"
        );

        // 同一次补丁里把 wire 一起改到 responses 即通过：Responses 是账号
        // 额度唯一能拿到「对话中间工具调用」的面。
        apply_patch(
            &mut cfg,
            &ConfigPatch {
                wire: Some("responses".into()),
                credential: Some("deepseek_account".into()),
                ..Default::default()
            },
        )
        .expect("responses wire must be accepted for the account credential");
        assert_eq!(cfg.wire, qaqh_types::Wire::Responses);
        assert_eq!(
            cfg.credential,
            qaqh_types::CredentialSource::DeepseekAccount
        );
        // 读回与写出必须同源（`credential_wire` 的穷尽 match 挡住漏改）。
        assert_eq!(to_dto(&cfg).credential, "deepseek_account");

        // 反向：已有账号凭据的用户把 wire 改走，同样在保存期报错。
        let err = apply_patch(
            &mut cfg,
            &ConfigPatch {
                wire: Some("anthropic".into()),
                ..Default::default()
            },
        )
        .expect_err("moving the wire away must be rejected");
        assert!(err.contains("只支持 responses"), "{err}");
        assert_eq!(
            cfg.credential,
            qaqh_types::CredentialSource::DeepseekAccount,
            "拒绝即不落地"
        );
        assert_eq!(
            cfg.wire,
            qaqh_types::Wire::Responses,
            "拒绝即不落地（wire 也不能半改）"
        );
    }

    /// 反向也要拦：已有 plan 的用户把 wire 改走，同样在保存期报错——只查「本次
    /// 补丁带了 credential」会漏掉这一半。
    #[test]
    fn apply_patch_rejects_moving_the_wire_away_from_plan() {
        let mut cfg = Config {
            wire: qaqh_types::Wire::Responses,
            credential: qaqh_types::CredentialSource::ChatgptPlan,
            ..Default::default()
        };
        let err = apply_patch(
            &mut cfg,
            &ConfigPatch {
                wire: Some("openai".into()),
                ..Default::default()
            },
        )
        .expect_err("plan + openai must be rejected");
        assert!(err.contains("responses"), "{err}");
        assert_eq!(cfg.wire, qaqh_types::Wire::Responses, "拒绝即不落地");
    }

    /// 读模型把额度来源投影成 wire 字面量（前端据此显示「本机套餐」）。
    #[test]
    fn to_dto_projects_the_credential_source() {
        let byok = Config {
            credential: qaqh_types::CredentialSource::ApiKey,
            ..Default::default()
        };
        let plan = Config {
            credential: qaqh_types::CredentialSource::ChatgptPlan,
            ..Default::default()
        };
        assert_eq!(to_dto(&byok).credential, "api_key");
        assert_eq!(to_dto(&plan).credential, "chatgpt_plan");
    }

    /// R3 验收：`ConfigPatch { wire: Some("chat") }` 被接受且落成 `Wire::Chat`，
    /// 读模型回显 `"chat"`——读写同一套规范拼写，客户端不再需要桥。
    #[test]
    fn canonical_wire_flows_through_patch_and_dto() {
        let mut cfg = Config::default();
        apply_patch(
            &mut cfg,
            &ConfigPatch {
                wire: Some("chat".into()),
                ..Default::default()
            },
        )
        .expect("规范名 chat 必须被接受");
        assert_eq!(cfg.wire, qaqh_types::Wire::Chat);
        assert_eq!(to_dto(&cfg).wire, "chat", "读回也必须是规范名");

        apply_patch(
            &mut cfg,
            &ConfigPatch {
                wire: Some("messages".into()),
                ..Default::default()
            },
        )
        .expect("规范名 messages 必须被接受");
        assert_eq!(cfg.wire, qaqh_types::Wire::Messages);
        assert_eq!(to_dto(&cfg).wire, "messages");
    }

    /// **R3 镜像闸**：`qaqh_config_api::canonical_wire` 是「只依赖 serde」的
    /// 拼写镜像（K1），真源是 [`qaqh_types::Wire::parse`]。本测试逐个拼写对拍
    /// 两边，任何一边改名而另一边没跟就红——镜像可以不共享代码，但不能漂移。
    ///
    /// 同时钉住「读出的规范名再写回去必须被接受」这条闭环（round-trip）。
    #[test]
    fn canonical_wire_mirrors_qaqh_types() {
        let spellings = [
            "chat",
            "openai",
            "openai-compatible",
            "chat_completions",
            "chat/completions",
            "responses",
            "openai-responses",
            "messages",
            "anthropic",
            "claude",
            "gemini",
            "google",
            "generatecontent",
            "generate_content",
            "gemini-generate-content",
            "  CHAT ",
            " Messages ",
            "grpc",
            "",
            "gpt-5",
        ];
        for spelling in spellings {
            let expected = qaqh_types::Wire::parse(spelling).map(qaqh_types::Wire::as_str);
            assert_eq!(
                qaqh_config_api::canonical_wire(spelling),
                expected,
                "拼写 {spelling:?} 在镜像与 Wire::parse 之间不一致"
            );
        }
        // 读 → 写闭环：读模型给出的规范名，直接写回去必须落成同一个 Wire。
        for wire in [
            qaqh_types::Wire::Chat,
            qaqh_types::Wire::Responses,
            qaqh_types::Wire::Messages,
            qaqh_types::Wire::Gemini,
        ] {
            let shown = wire.as_str();
            assert_eq!(
                qaqh_types::Wire::parse(shown),
                Some(wire),
                "读出的 {shown} 必须能被写侧解析回同一个 Wire"
            );
            assert_eq!(qaqh_config_api::canonical_wire(shown), Some(shown));
        }
    }
}
