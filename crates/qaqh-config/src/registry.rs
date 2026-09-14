//! Provider registry — known providers and their endpoints.
//!
//! Architecture:
//!   Provider (e.g. DeepSeek) has 1..N Endpoints (all OpenAI-compatible for now).
//!   User selects (provider_id, endpoint_id) → protocol + base_url auto-fill.
//!   Model list is fetched from endpoint's /models URL at runtime.
//!
//! T9 TOML 优先：内置能力基线来自 `assets/providers.toml`（include_str! 版本化，
//! 由 example `export_providers` 生成），用户覆盖按优先级合并：
//!   override 文件（`providers.override.toml`）> config.toml `[providers]` 段
//!   （兼容旧路径）> assets baseline。
//! 覆盖面是稀疏 patch（全 Option），只改声明了的字段；`remove` 可隐藏端点。
//! 对外查找 API（find_provider/find_endpoint/image_tool_enabled…）不变。
//!
//! Backward compat: old provider_id "deepseek-openai"/"deepseek-anthropic" are
//! auto-migrated to provider_id="deepseek" + endpoint="openai".

use qaqh_types::{
    EndpointPatch, EndpointPatchRef, EndpointSpec, ProviderPatch, ProviderSpec, ProvidersFile,
    ProvidersOverrideFile,
};

/// assets/providers.toml 的字节快照（编译期嵌入）。
const BASELINE_TOML: &str = include_str!("../../../assets/providers.toml");

fn builtin_providers() -> Vec<ProviderSpec> {
    // T9: 内置基线已迁至 assets/providers.toml（include_str! 随 crate 版本化，
    // 由 example `export_providers` 生成）；原 13 个手工构造函数删除，避免双源漂移。
    parse_providers_toml(BASELINE_TOML).unwrap_or_else(|e| {
        // 编译期由 tests::baseline_toml_roundtrip 保证；运行期解析失败只能是
        // 构建产物损坏，panic 是合理处置（配置源不可信）。
        panic!("assets/providers.toml baseline parse failed: {e}")
    })
}

fn parse_providers_toml(raw: &str) -> Result<Vec<ProviderSpec>, String> {
    let file: ProvidersFile = toml::from_str(raw).map_err(|e| format!("TOML parse: {e}"))?;
    Ok(file.providers)
}

// ── 用户覆盖合并（T9） ──

/// 全局合并结果缓存：进程内只读盘一次（override + config.toml 段），
/// 后续查找全部走内存表。`invalidate_merged()` 失效后下次查找重建
/// （config 单写口提交/文件热重载时触发）。
static MERGED: std::sync::RwLock<Option<std::sync::Arc<Vec<ProviderSpec>>>> =
    std::sync::RwLock::new(None);

/// 失效合并缓存（T9 热重载挂钩）。
pub fn invalidate_merged() {
    *MERGED.write().unwrap_or_else(|e| e.into_inner()) = None;
}

/// 校验覆盖面声明的 base_url：必须可解析且 scheme ∈ {https,http}；
/// `http` 仅允许 localhost/127.0.0.1（设计文档 §7 风险项）。
fn validate_override_base_url(url: &str) -> Result<(), String> {
    if url.is_empty() {
        return Ok(());
    }
    // 轻量校验，不引入 url crate：按 `scheme://rest` 切分。
    let Some((scheme, rest)) = url.split_once("://") else {
        return Err(format!("base_url 缺少 scheme: {url}"));
    };
    match scheme {
        "https" => {}
        "http" => {
            let host = rest.split(['/', ':']).next().unwrap_or("");
            if host != "localhost" && host != "127.0.0.1" {
                return Err(format!(
                    "base_url http 仅允许 localhost/127.0.0.1（生产端点必须 https）: {url}"
                ));
            }
        }
        _ => return Err(format!("base_url scheme 必须是 https/http: {url}")),
    }
    Ok(())
}

/// 把一个 ProviderPatch 合入基线表：按 id 定位，缺失时在尾部新建 provider。
fn apply_provider_patch(baseline: &mut Vec<ProviderSpec>, patch: &ProviderPatch) {
    let Some(patch_id) = patch.id.clone().filter(|s| !s.is_empty()) else {
        log::warn!("[registry] override provider 缺少 id，忽略");
        return;
    };
    let entry = match baseline.iter_mut().find(|p| p.id == patch_id) {
        Some(p) => p,
        None => {
            baseline.push(ProviderSpec {
                id: patch_id,
                display: patch.display.clone().unwrap_or_default(),
                endpoints: Vec::new(),
            });
            baseline.last_mut().expect("just pushed")
        }
    };
    if let Some(d) = &patch.display {
        entry.display = d.clone();
    }
    for ep_ref in &patch.endpoints {
        apply_endpoint_patch(entry, ep_ref);
    }
    for remove_id in &patch.remove {
        entry.endpoints.retain(|e| &e.id != remove_id);
    }
}

fn apply_endpoint_patch(provider: &mut ProviderSpec, ep_ref: &EndpointPatchRef) {
    let patch: &EndpointPatch = &ep_ref.patch;
    // 先校验 base_url（若有声明）。
    if let Some(url) = &patch.base_url
        && let Err(e) = validate_override_base_url(url)
    {
        log::warn!(
            "[registry] override {}/{} base_url 被拒绝: {e}",
            provider.id,
            ep_ref.id
        );
        return;
    }
    match provider.endpoints.iter_mut().find(|e| e.id == ep_ref.id) {
        Some(ep) => patch.apply_to(ep),
        None => {
            // 新增端点：从 Default 出发，仅叠加声明字段（id/protocol/base_url
            // 是新增端点的最小必需集，缺 protocol 默认 openai）。
            let mut ep = EndpointSpec {
                id: ep_ref.id.clone(),
                ..Default::default()
            };
            patch.apply_to(&mut ep);
            if ep.base_url.is_empty() {
                log::warn!(
                    "[registry] override 新增端点 {}/{} 缺少 base_url，忽略",
                    provider.id,
                    ep_ref.id
                );
                return;
            }
            provider.endpoints.push(ep);
        }
    }
}

/// 执行一次完整合并：baseline + 逐 patch（按声明顺序叠加）。
fn merged_providers() -> std::sync::Arc<Vec<ProviderSpec>> {
    // 快路径：读锁命中缓存。
    if let Some(hit) = MERGED
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .as_ref()
        .cloned()
    {
        return hit;
    }
    // 慢路径：重建 + 写回（并发下重复重建无害，最终一致）。
    let mut baseline = builtin_providers();
    for raw in user_override_tomls() {
        match parse_override_toml(&raw) {
            Ok(patches) => {
                for patch in &patches {
                    apply_provider_patch(&mut baseline, patch);
                }
            }
            Err(e) => {
                // 用户覆盖面解析失败只降级告警，不 panic（baseline 仍可用）。
                log::warn!("[registry] 用户 provider 覆盖解析失败（已忽略该文件）: {e}");
            }
        }
    }
    let arc = std::sync::Arc::new(baseline);
    *MERGED.write().unwrap_or_else(|e| e.into_inner()) = Some(arc.clone());
    arc
}

/// 用户覆盖 TOML 原文列表，按优先级从低到高：config.toml `[providers]` 段、
/// override 文件。均可能不存在（返回空）。
fn user_override_tomls() -> Vec<String> {
    let mut out = Vec::new();
    // ① config.toml 的 [providers] / [[providers]] 段（兼容旧路径）。
    let config_path = qaqh_types::platform::config_path();
    if let Ok(text) = std::fs::read_to_string(&config_path)
        && let Some(seg) = extract_providers_section(&text)
    {
        out.push(seg);
    }
    // ② override 文件（同目录 providers.override.toml）。
    let override_path = config_path.with_file_name("providers.override.toml");
    if let Ok(text) = std::fs::read_to_string(&override_path) {
        out.push(text);
    }
    out
}

/// 从 config.toml 原文中提取 `[providers]`（含其子表）段落原文。
/// 没有该段则返回 None。段落以行首 `[providers` 开始，下一个非子表的
/// 顶级表头结束。文本级近似：段落内字符串值含换行/顶级表头形态的极端
/// 输入不被支持（provider 配置段不含此类值，可接受）。
fn extract_providers_section(text: &str) -> Option<String> {
    let mut out = String::new();
    let mut inside = false;
    for line in text.lines() {
        let trimmed = line.trim_start();
        if trimmed.starts_with("[providers]") || trimmed.starts_with("[[providers]]") {
            inside = true;
        } else if inside
            && trimmed.starts_with('[')
            && !trimmed.starts_with("[providers.")
            && !trimmed.starts_with("[[providers.")
        {
            inside = false;
        }
        if inside {
            out.push_str(line);
            out.push('\n');
        }
    }
    if out.is_empty() { None } else { Some(out) }
}

/// 解析用户覆盖面：`[[providers]]` 平铺数组形态（patch 内带 id）。
fn parse_override_toml(raw: &str) -> Result<Vec<ProviderPatch>, String> {
    let file: ProvidersOverrideFile =
        toml::from_str(raw).map_err(|e| format!("TOML parse: {e}"))?;
    let mut patches = Vec::new();
    for p in file.providers {
        if p.id.as_deref().unwrap_or("").is_empty() {
            log::warn!("[registry] override [[providers]] 缺少 id，忽略该条");
            continue;
        }
        patches.push(p);
    }
    Ok(patches)
}

fn providers() -> Vec<ProviderSpec> {
    (*merged_providers()).clone()
}

// ── Lookup ──

pub fn all_providers() -> Vec<ProviderSpec> {
    providers()
}

pub fn find_provider(id: &str) -> Option<ProviderSpec> {
    providers().into_iter().find(|p| p.id == id)
}

pub fn find_endpoint(provider_id: &str, endpoint_id: &str) -> Option<EndpointSpec> {
    find_provider(provider_id).and_then(|p| p.endpoints.into_iter().find(|e| e.id == endpoint_id))
}

pub fn first_endpoint_for(provider_id: &str) -> Option<EndpointSpec> {
    find_provider(provider_id).and_then(|p| p.endpoints.into_iter().next())
}

/// Whether the endpoint accepts image input (gates the `read_image` tool).
pub fn image_tool_enabled(provider_id: &str, endpoint_id: &str) -> bool {
    find_endpoint(provider_id, endpoint_id).is_some_and(|e| e.supports_image_tool)
}

/// Whether a specific model accepts image input on this endpoint.
///
/// Layers [`image_tool_enabled`] with the optional per-model allowlist
/// (`EndpointSpec::image_models`): routers serve heterogeneous models, so the
/// endpoint flag alone would let `read_image` attach pixels to text-only
/// models and fail upstream with an opaque 400.
pub fn image_model_supported(provider_id: &str, endpoint_id: &str, model: &str) -> bool {
    let Some(ep) = find_endpoint(provider_id, endpoint_id) else {
        return false;
    };
    if !ep.supports_image_tool {
        return false;
    }
    match &ep.image_models {
        None => true,
        Some(list) => {
            let model = model.to_lowercase();
            list.iter().any(|pattern| match pattern.strip_suffix('*') {
                Some(prefix) => model.starts_with(&prefix.to_lowercase()),
                None => model == pattern.to_lowercase(),
            })
        }
    }
}

pub fn first_provider_endpoint() -> (String, String) {
    let providers = all_providers();
    let p = providers.first();
    let pid = p.map(|p| p.id.clone()).unwrap_or_else(|| "deepseek".into());
    let ep = first_endpoint_for(&pid)
        .map(|e| e.id.clone())
        .unwrap_or_else(|| "openai".into());
    (pid, ep)
}

// ── Model discovery ──

pub fn models_url_for(provider_id: &str, endpoint_id: &str) -> Option<String> {
    let ep = find_endpoint(provider_id, endpoint_id)?;
    let base = ep.models_url.as_deref().unwrap_or(&ep.base_url);
    // Most presets store a base URL, but OpenRouter's model discovery needs
    // documented query filters. Treat an explicit /models URL as complete.
    if base.contains("/models") {
        return Some(base.to_string());
    }
    let stripped = base.trim_end_matches('/');
    Some(format!("{}/models", stripped))
}

pub fn default_model_for(provider_id: &str, endpoint_id: &str) -> String {
    find_endpoint(provider_id, endpoint_id)
        .map(|e| e.default_model.clone())
        .unwrap_or_default()
}

pub fn protocol_for(provider_id: &str, endpoint_id: &str) -> String {
    find_endpoint(provider_id, endpoint_id)
        .map(|e| e.protocol.clone())
        .unwrap_or_else(|| "openai".into())
}

pub fn base_url_for(provider_id: &str, endpoint_id: &str) -> String {
    find_endpoint(provider_id, endpoint_id)
        .map(|e| e.base_url.clone())
        .unwrap_or_default()
}

// ── Backward compatibility ──

pub fn migrate_provider_id(old_pid: &str) -> (String, String) {
    if find_provider(old_pid).is_some() {
        let ep = first_endpoint_for(old_pid)
            .map(|e| e.id.clone())
            .unwrap_or_else(|| "openai".into());
        (old_pid.to_string(), ep)
    } else {
        ("deepseek".into(), "openai".into())
    }
}

/// Resolve the endpoint spec for an already-loaded [`crate::Config`]
/// (PR-1-9 / B7): the loop resolves once at config-assembly/reload time and
/// engines read the stored field instead of walking the registry per call.
pub fn resolve_for_config(cfg: &crate::Config) -> Option<EndpointSpec> {
    find_endpoint(&cfg.provider_id, &cfg.endpoint)
}

#[cfg(test)]
mod tests {
    use super::*;
    use qaqh_types::CacheTokenField;

    // ── T9: baseline round-trip / merge / override ──

    #[test]
    fn baseline_toml_roundtrip() {
        let baseline = builtin_providers();
        assert_eq!(baseline.len(), 13, "baseline 应有 13 个 provider");
        // 再序列化回 TOML 再解析，确认无信息丢失。
        let doc = ProvidersFile {
            providers: baseline.clone(),
        };
        let text = toml::to_string_pretty(&doc).expect("serialize");
        let reparsed = parse_providers_toml(&text).expect("reparse");
        assert_eq!(reparsed.len(), baseline.len());
        for (a, b) in baseline.iter().zip(reparsed.iter()) {
            assert_eq!(a.id, b.id);
            assert_eq!(a.endpoints.len(), b.endpoints.len(), "provider {}", a.id);
            for (ea, eb) in a.endpoints.iter().zip(b.endpoints.iter()) {
                assert_eq!(ea.id, eb.id);
                assert_eq!(ea.base_url, eb.base_url);
                assert_eq!(ea.supports_thinking, eb.supports_thinking);
                assert_eq!(ea.retry, eb.retry);
            }
        }
    }

    #[test]
    fn override_patch_updates_existing_endpoint() {
        let mut baseline = builtin_providers();
        let patch: ProviderPatch = toml::from_str(
            r#"
            id = "deepseek"
            [[endpoints]]
            id = "openai"
            supports_thinking = false
            stateful = true
            retry = { max_retries = 8, base_delay_secs = 2, max_delay_secs = 60, idle_timeout_secs = 600 }
            "#,
        )
        .expect("parse patch");
        apply_provider_patch(&mut baseline, &patch);
        let ep = baseline
            .iter()
            .find(|p| p.id == "deepseek")
            .and_then(|p| p.endpoints.iter().find(|e| e.id == "openai"))
            .expect("endpoint");
        assert!(!ep.supports_thinking);
        assert!(ep.stateful);
        let retry = ep.retry.as_ref().expect("retry spec");
        assert_eq!(retry.max_retries, 8);
        assert_eq!(retry.base_delay_secs, 2);
        // 未声明字段保持 baseline 值。
        assert!(ep.include_stream_usage, "未声明字段不应被覆盖");
    }

    #[test]
    fn override_new_provider_and_endpoint() {
        let mut baseline = builtin_providers();
        let patch: ProviderPatch = toml::from_str(
            r#"
            id = "my-proxy"
            display = "本地代理"
            [[endpoints]]
            id = "openai"
            protocol = "openai"
            base_url = "http://127.0.0.1:8787/v1"
            "#,
        )
        .expect("parse patch");
        apply_provider_patch(&mut baseline, &patch);
        let p = baseline
            .iter()
            .find(|p| p.id == "my-proxy")
            .expect("new provider");
        assert_eq!(p.endpoints.len(), 1);
        assert_eq!(p.endpoints[0].base_url, "http://127.0.0.1:8787/v1");
        assert_eq!(p.endpoints[0].protocol, "openai");
    }

    #[test]
    fn override_rejects_non_local_http_base_url() {
        let mut baseline = builtin_providers();
        let before = baseline
            .iter()
            .find(|p| p.id == "deepseek")
            .and_then(|p| p.endpoints.iter().find(|e| e.id == "openai"))
            .map(|e| e.base_url.clone())
            .expect("endpoint");
        let patch: ProviderPatch = toml::from_str(
            r#"
            id = "deepseek"
            [[endpoints]]
            id = "openai"
            base_url = "http://evil.example.com"
            "#,
        )
        .expect("parse patch");
        apply_provider_patch(&mut baseline, &patch);
        let after = baseline
            .iter()
            .find(|p| p.id == "deepseek")
            .and_then(|p| p.endpoints.iter().find(|e| e.id == "openai"))
            .map(|e| e.base_url.clone())
            .expect("endpoint");
        assert_eq!(before, after, "不安全 http 覆盖应被拒绝");
    }

    #[test]
    fn override_remove_endpoint() {
        let mut baseline = builtin_providers();
        let patch: ProviderPatch = toml::from_str(
            r#"
            id = "deepseek"
            remove = ["responses"]
            "#,
        )
        .expect("parse patch");
        apply_provider_patch(&mut baseline, &patch);
        let p = baseline
            .iter()
            .find(|p| p.id == "deepseek")
            .expect("provider");
        assert!(!p.endpoints.iter().any(|e| e.id == "responses"));
        assert!(p.endpoints.iter().any(|e| e.id == "openai"));
    }

    #[test]
    fn extract_providers_section_finds_segment() {
        let text = r#"
[profile.default]
model = "x"

[[providers]]
id = "deepseek"
[[providers.endpoints]]
id = "openai"
base_url = "https://api.deepseek.com"

[mcp]
foo = 1
"#;
        let seg = extract_providers_section(text).expect("segment");
        assert!(seg.contains("[[providers]]"));
        assert!(seg.contains("base_url"));
        assert!(!seg.contains("[mcp]"));
        assert!(!seg.contains("[profile"));
    }

    #[test]
    fn parse_override_rejects_missing_id() {
        let raw = r#"
[[providers]]
display = "no id"
"#;
        let patches = parse_override_toml(raw).expect("parse ok");
        assert!(patches.is_empty(), "缺 id 的 patch 应被过滤");
    }

    #[test]
    fn openrouter_text_endpoint_has_router_safe_capabilities() {
        let endpoint = find_endpoint("openrouter", "openai").expect("OpenRouter endpoint");
        assert_eq!(endpoint.base_url, "https://openrouter.ai/api/v1");
        assert_eq!(
            models_url_for("openrouter", "openai").as_deref(),
            Some(
                "https://openrouter.ai/api/v1/models?output_modalities=text&supported_parameters=tools&sort=pricing-low-to-high"
            )
        );
        assert!(!endpoint.has_balance);
        assert!(!endpoint.supports_thinking);
        // reasoning_effort 简写 + 稀疏档位钳制(ox-alpha: max/high/low)。
        assert!(endpoint.supports_reasoning_effort);
        assert_eq!(
            endpoint.effort_allowlist.as_deref(),
            Some(&["max".to_string(), "high".to_string(), "low".to_string()][..])
        );
        assert!(endpoint.tool_call_content_null);
        assert!(!endpoint.supports_reasoning_content);
        assert!(endpoint.require_provider_parameters);
    }

    #[test]
    fn existing_openai_preset_keeps_legacy_capabilities() {
        let endpoint = find_endpoint("openai", "openai").expect("OpenAI endpoint");
        assert!(endpoint.supports_thinking);
        assert!(endpoint.supports_reasoning_effort);
        assert!(!endpoint.tool_call_content_null);
        assert!(endpoint.supports_reasoning_content);
        assert!(!endpoint.require_provider_parameters);
    }

    #[test]
    fn openai_responses_endpoint_exists() {
        let endpoint = find_endpoint("openai", "responses").expect("OpenAI Responses endpoint");
        assert_eq!(endpoint.protocol, "responses");
        assert_eq!(endpoint.base_url, "https://api.openai.com/v1");
        assert!(!endpoint.supports_thinking);
        assert!(endpoint.supports_reasoning_effort);
        assert!(!endpoint.supports_reasoning_content);
        assert!(endpoint.responses_search_function_alias.is_none());
    }

    #[test]
    fn protocol_for_responses_endpoint() {
        let proto = protocol_for("openai", "responses");
        assert_eq!(proto, "responses");
    }

    #[test]
    fn image_model_support_layers_endpoint_flag_and_allowlist() {
        // deepseek 已开图但限 vision 模型：非 vision 仍 false
        assert!(!image_model_supported(
            "deepseek",
            "openai",
            "google/gemini-2.0"
        ));
        assert!(!image_model_supported(
            "deepseek",
            "openai",
            "deepseek-v4-flash"
        ));
        assert!(image_model_supported(
            "deepseek",
            "openai",
            "deepseek-v4-flash-vision-exp"
        ));
        assert!(image_model_supported(
            "deepseek",
            "responses",
            "deepseek-v4-flash-vision-exp"
        ));
        assert!(!image_model_supported(
            "deepseek",
            "responses",
            "deepseek-v4-flash"
        ));
        // opencode-go:端点开图且无 allowlist → 所有模型放行。
        assert!(image_model_supported("opencode-go", "openai", "任意-模型"));
        // openrouter:allowlist 生效 —— 大小写不敏感、精确与前缀通配。
        assert!(image_model_supported(
            "openrouter",
            "openai",
            "stealth/ox-alpha"
        ));
        assert!(image_model_supported(
            "openrouter",
            "openai",
            "Stealth/OX-ALPHA"
        ));
        assert!(image_model_supported(
            "openrouter",
            "openai",
            "google/gemini-3-pro"
        ));
        assert!(!image_model_supported(
            "openrouter",
            "openai",
            "deepseek/deepseek-v4-pro"
        ));
        assert!(!image_model_supported(
            "openrouter",
            "openai",
            "meta-llama/llama-3.3-70b"
        ));
    }

    #[test]
    fn glm_vision_allowlist_matches_bigmodel_support_matrix() {
        // glm-5.3-flash 是 VLM → 放行（大小写不敏感）。
        assert!(image_model_supported("glm", "openai", "glm-5.3-flash"));
        assert!(image_model_supported("glm", "openai", "GLM-5.3-Flash"));
        // 5V / 4.5V / 4.6V / 4V-Plus 系列前缀放行。
        assert!(image_model_supported("glm", "openai", "glm-5v-turbo"));
        assert!(image_model_supported("glm", "openai", "glm-4.6v"));
        assert!(image_model_supported("glm", "openai", "glm-4v-plus-0111"));
        // 文本模型必须拒绝：glm-5.3 官方仅支持文本模态。
        assert!(!image_model_supported("glm", "openai", "glm-5.3"));
        assert!(!image_model_supported("glm", "openai", "glm-5.2"));
        assert!(!image_model_supported("glm", "openai", "glm-4.7"));
        // glm-4v-flash 官方不支持 Base64 编码（harness 只发 base64）→ 拒绝。
        assert!(!image_model_supported("glm", "openai", "glm-4v-flash"));
    }

    #[test]
    fn chat_endpoint_still_works() {
        let proto = protocol_for("openai", "openai");
        assert_eq!(proto, "openai");
        let url = base_url_for("openai", "openai");
        assert_eq!(url, "https://api.openai.com/v1");
    }

    #[test]
    fn deepseek_responses_endpoint_exists() {
        let endpoint = find_endpoint("deepseek", "responses").expect("DeepSeek Responses endpoint");
        assert_eq!(endpoint.protocol, "responses");
        assert_eq!(endpoint.base_url, "https://api.deepseek.com");
        assert_eq!(endpoint.responses_path.as_deref(), Some("/responses"));
        assert_eq!(endpoint.default_model, "deepseek-v4-flash");
        assert_eq!(
            endpoint.models,
            vec![
                "deepseek-v4-flash".to_string(),
                "deepseek-v4-flash-vision-exp".to_string()
            ]
        );
        assert!(endpoint.beta);
        assert!(!endpoint.supports_thinking);
        assert!(endpoint.supports_reasoning_effort);
        assert!(!endpoint.supports_reasoning_content);
        assert!(endpoint.supports_image_tool);
        assert_eq!(
            endpoint.image_models.as_deref(),
            Some(&["deepseek-v4-flash-vision-exp".to_string()][..])
        );
        assert_eq!(
            endpoint.responses_search_function_alias.as_deref(),
            Some("qaqh_search")
        );
    }

    #[test]
    fn deepseek_responses_protocol_flows_through() {
        assert_eq!(protocol_for("deepseek", "responses"), "responses");
        assert_eq!(protocol_for("deepseek", "openai"), "openai");
        // Unknown endpoint falls back to the openai protocol (backward compat).
        assert_eq!(protocol_for("deepseek", "unknown"), "openai");
    }

    #[test]
    fn deepseek_openai_supports_vision_only_for_vision_model() {
        let endpoint = find_endpoint("deepseek", "openai").expect("DeepSeek openai endpoint");
        assert!(endpoint.supports_image_tool);
        assert_eq!(
            endpoint.image_models.as_deref(),
            Some(&["deepseek-v4-flash-vision-exp".to_string()][..])
        );
        // 非 vision 模型被拒绝，vision 模型放行（大小写不敏感）
        assert!(image_tool_enabled("deepseek", "openai"));
        assert!(image_tool_enabled("deepseek", "responses"));
        assert!(image_model_supported(
            "deepseek",
            "openai",
            "deepseek-v4-flash-vision-exp"
        ));
        assert!(image_model_supported(
            "deepseek",
            "openai",
            "DEEPSEEK-V4-FLASH-VISION-EXP"
        ));
        assert!(!image_model_supported(
            "deepseek",
            "openai",
            "deepseek-v4-flash"
        ));
        assert!(!image_model_supported(
            "deepseek",
            "openai",
            "deepseek-v4-pro"
        ));
    }

    #[test]
    fn qwen_responses_endpoint_exists() {
        let endpoint = find_endpoint("qwen", "responses").expect("Qwen Responses endpoint");
        assert_eq!(endpoint.protocol, "responses");
        assert_eq!(endpoint.base_url, "https://dashscope.aliyuncs.com");
        assert_eq!(
            endpoint.responses_path.as_deref(),
            Some("/compatible-mode/v1/responses")
        );
        assert_eq!(
            models_url_for("qwen", "responses").as_deref(),
            Some("https://dashscope.aliyuncs.com/compatible-mode/v1/models")
        );
        assert!(endpoint.beta);
        assert!(!endpoint.supports_thinking);
        assert!(endpoint.supports_reasoning_effort);
        assert!(!endpoint.supports_reasoning_content);
        // Bridge must not disturb the default chat endpoint.
        assert_eq!(protocol_for("qwen", "openai"), "openai");
    }

    #[test]
    fn doubao_responses_endpoint_exists() {
        let endpoint = find_endpoint("doubao", "responses").expect("Doubao Responses endpoint");
        assert_eq!(endpoint.protocol, "responses");
        assert_eq!(endpoint.base_url, "https://ark.cn-beijing.volces.com");
        assert_eq!(
            endpoint.responses_path.as_deref(),
            Some("/api/v3/responses")
        );
        assert_eq!(
            models_url_for("doubao", "responses").as_deref(),
            Some("https://ark.cn-beijing.volces.com/api/v3/models")
        );
        assert!(endpoint.beta);
        assert!(!endpoint.supports_thinking);
        assert!(endpoint.supports_reasoning_effort);
        assert!(!endpoint.supports_reasoning_content);
        assert_eq!(protocol_for("doubao", "openai"), "openai");
    }

    #[test]
    fn mimo_responses_endpoint_exists() {
        let endpoint = find_endpoint("mimo", "responses").expect("MiMo Responses endpoint");
        assert_eq!(endpoint.protocol, "responses");
        assert_eq!(endpoint.base_url, "https://api.xiaomimimo.com/v1");
        assert_eq!(endpoint.responses_path.as_deref(), Some("/responses"));
        assert_eq!(
            models_url_for("mimo", "responses").as_deref(),
            Some("https://api.xiaomimimo.com/v1/models")
        );
        assert!(endpoint.beta);
        assert!(!endpoint.supports_thinking);
        assert!(endpoint.supports_reasoning_effort);
        assert_eq!(endpoint.responses_effort_max, "high");
        assert!(!endpoint.supports_reasoning_content);
        assert_eq!(protocol_for("mimo", "openai"), "openai");
    }

    #[test]
    fn opencode_go_chat_endpoint_exists() {
        let endpoint = find_endpoint("opencode-go", "openai").expect("opencode-go endpoint");
        assert_eq!(endpoint.protocol, "openai");
        assert_eq!(endpoint.base_url, "https://opencode.ai/zen/go/v1");
        assert_eq!(endpoint.default_model, "deepseek-v4-flash");
        assert_eq!(
            endpoint.models.len(),
            15,
            "official Go model list (chat channel)"
        );
        assert!(endpoint.models.contains(&"deepseek-v4-flash".to_string()));
        assert!(endpoint.models.contains(&"kimi-k3".to_string()));
        assert!(!endpoint.models.contains(&"grok-4.5".to_string()));
        assert!(!endpoint.models.contains(&"minimax-m3".to_string()));
        // 本家不发 thinking 参数（推理默认开），只发 reasoning_effort。
        assert!(!endpoint.supports_thinking);
        assert!(endpoint.supports_reasoning_effort);
        assert!(matches!(endpoint.cache_field, CacheTokenField::None));
        assert!(!endpoint.has_balance);
        assert_eq!(
            models_url_for("opencode-go", "openai").as_deref(),
            Some("https://opencode.ai/zen/go/v1/models")
        );
    }

    #[test]
    fn opencode_go_responses_endpoint_exists() {
        let endpoint = find_endpoint("opencode-go", "responses").expect("opencode-go Responses");
        assert_eq!(endpoint.protocol, "responses");
        assert_eq!(endpoint.base_url, "https://opencode.ai/zen/go/v1");
        assert_eq!(endpoint.responses_path.as_deref(), Some("/responses"));
        assert_eq!(
            endpoint.models,
            vec!["grok-4.5".to_string(), "gpt-5.6-luna".to_string()]
        );
        assert_eq!(endpoint.default_model, "grok-4.5");
        assert!(endpoint.beta);
        assert!(!endpoint.supports_thinking);
        assert!(endpoint.supports_reasoning_effort);
        // grok-4.5 最高档 high（超档 400）；luna 的 xhigh/max 待验证后放开。
        assert_eq!(endpoint.responses_effort_max, "high");
        assert!(!endpoint.supports_reasoning_content);
        // minimax 走 anthropic messages 协议（未实现）→ 不进任何端点。
        assert!(!endpoint.models.contains(&"minimax-m3".to_string()));
    }

    #[test]
    fn workbuddy_proxy_endpoint_exists() {
        let endpoint = find_endpoint("workbuddy", "openai").expect("workbuddy endpoint");
        assert_eq!(endpoint.protocol, "openai");
        assert_eq!(endpoint.base_url, "http://127.0.0.1:8787/v1");
        assert_eq!(endpoint.default_model, "glm-5.2");
        // 静态表与反代 /v1/models 动态表（及 staticModels() 兑底）对齐。
        assert_eq!(endpoint.models.len(), 15);
        assert!(endpoint.models.contains(&"glm-5.2".to_string()));
        assert!(endpoint.models.contains(&"hy3".to_string()));
        assert!(endpoint.models.contains(&"kimi-k3-1".to_string()));
        // 上游不收 thinking/enable_thinking；只透传 reasoning_effort，
        // 降级交给反代（按模型 supportedEfforts 转译）→ 端点无白名单。
        assert!(!endpoint.supports_thinking);
        assert!(endpoint.supports_reasoning_effort);
        assert!(endpoint.effort_allowlist.is_none());
        // 思考内容：流式 delta.reasoning_content（反代 keep_reasoning 默认开）。
        assert!(endpoint.supports_reasoning_content);
        // 上游在 finish 帧总带 usage，不发 stream_options.include_usage。
        assert!(!endpoint.include_stream_usage);
        // usage 顶层 prompt_cache_hit_tokens/miss 与 DeepSeek 同形。
        assert!(matches!(
            endpoint.cache_field,
            CacheTokenField::PromptCacheHitTokens
        ));
        assert!(!endpoint.has_balance);
        // models_url 显式含 /models 路径时直接返回，不重复追加。
        assert_eq!(
            models_url_for("workbuddy", "openai").as_deref(),
            Some("http://127.0.0.1:8787/v1/models")
        );
    }
}
