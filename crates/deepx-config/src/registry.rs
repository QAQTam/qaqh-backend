//! Provider registry — known providers and their endpoints.
//!
//! Architecture:
//!   Provider (e.g. DeepSeek) has 1..N Endpoints (all OpenAI-compatible for now).
//!   User selects (provider_id, endpoint_id) → protocol + base_url auto-fill.
//!   Model list is fetched from endpoint's /models URL at runtime.
//!
//! Backward compat: old provider_id "deepseek-openai"/"deepseek-anthropic" are
//! auto-migrated to provider_id="deepseek" + endpoint="openai".

use deepx_types::{CacheTokenField, EndpointSpec, ProviderSpec, ThinkingParamMode, UserSendMode};

fn deepseek() -> ProviderSpec {
    ProviderSpec {
        id: "deepseek".into(),
        display: "DeepSeek".into(),
        endpoints: vec![EndpointSpec {
            id: "openai".into(),
            display: "OpenAI-compatible".into(),
            protocol: "openai".into(),
            base_url: "https://api.deepseek.com".into(),
            default_model: String::new(),
            models: vec![],
            models_url: Some("https://api.deepseek.com".into()),
            user_id_mode: Some(UserSendMode::Body),
            // chat_path: None → "/chat/completions" (default)
            // thinking_mode: OpenAi (default)
            // cache_field: PromptCacheHitTokens (default)
            ..Default::default()
        }],
    }
}

fn qwen() -> ProviderSpec {
    ProviderSpec {
        id: "qwen".into(),
        display: "Qwen (阿里百炼)".into(),
        endpoints: vec![EndpointSpec {
            id: "openai".into(),
            display: "OpenAI-compatible".into(),
            protocol: "openai".into(),
            base_url: "https://dashscope.aliyuncs.com".into(),
            default_model: String::new(),
            models: vec![],
            models_url: Some("https://dashscope.aliyuncs.com/compatible-mode/v1".into()),
            chat_path: Some("/compatible-mode/v1/chat/completions".into()),
            thinking_mode: ThinkingParamMode::QwenEnableThinking,
            cache_field: CacheTokenField::PromptDetailsCached,
            has_balance: false,
            ..Default::default()
        }],
    }
}

fn glm() -> ProviderSpec {
    ProviderSpec {
        id: "glm".into(),
        display: "GLM (智谱AI)".into(),
        endpoints: vec![EndpointSpec {
            id: "openai".into(),
            display: "OpenAI-compatible".into(),
            protocol: "openai".into(),
            base_url: "https://open.bigmodel.cn".into(),
            default_model: String::new(),
            models: vec![],
            models_url: Some("https://open.bigmodel.cn/api/paas/v4".into()),
            chat_path: Some("/api/paas/v4/chat/completions".into()),
            cache_field: CacheTokenField::PromptDetailsCached,
            do_sample: Some(false),
            has_balance: false,
            ..Default::default()
        }],
    }
}

fn kimi() -> ProviderSpec {
    ProviderSpec {
        id: "kimi".into(),
        display: "Kimi (月之暗面)".into(),
        endpoints: vec![EndpointSpec {
            id: "openai".into(),
            display: "OpenAI-compatible".into(),
            protocol: "openai".into(),
            base_url: "https://api.moonshot.cn/v1".into(),
            default_model: String::new(),
            models: vec![],
            models_url: Some("https://api.moonshot.cn/v1".into()),
            balance_path: Some("/users/me/balance".into()),
            cache_field: CacheTokenField::UsageCachedTokens,
            ..Default::default()
        }],
    }
}

fn mimo() -> ProviderSpec {
    ProviderSpec {
        id: "mimo".into(),
        display: "MiMo (小米)".into(),
        endpoints: vec![EndpointSpec {
            id: "openai".into(),
            display: "OpenAI-compatible".into(),
            protocol: "openai".into(),
            base_url: "https://api.xiaomimimo.com/v1".into(),
            default_model: String::new(),
            models: vec![],
            models_url: Some("https://api.xiaomimimo.com/v1".into()),
            cache_field: CacheTokenField::None,
            has_balance: false,
            ..Default::default()
        }],
    }
}

fn minimax() -> ProviderSpec {
    ProviderSpec {
        id: "minimax".into(),
        display: "MiniMax (稀宇)".into(),
        endpoints: vec![EndpointSpec {
            id: "openai".into(),
            display: "OpenAI-compatible".into(),
            protocol: "openai".into(),
            base_url: "https://api.minimaxi.com/v1".into(),
            default_model: String::new(),
            models: vec![],
            models_url: Some("https://api.minimaxi.com/v1".into()),
            thinking_mode: ThinkingParamMode::MiniMaxAdaptive,
            cache_field: CacheTokenField::None,
            has_balance: false,
            ..Default::default()
        }],
    }
}

fn doubao() -> ProviderSpec {
    ProviderSpec {
        id: "doubao".into(),
        display: "Doubao (火山方舟)".into(),
        endpoints: vec![EndpointSpec {
            id: "openai".into(),
            display: "OpenAI-compatible".into(),
            protocol: "openai".into(),
            base_url: "https://ark.cn-beijing.volces.com".into(),
            default_model: String::new(),
            models: vec![],
            models_url: Some("https://ark.cn-beijing.volces.com/api/v3".into()),
            chat_path: Some("/api/v3/chat/completions".into()),
            ..Default::default()
        }],
    }
}

fn openai() -> ProviderSpec {
    ProviderSpec {
        id: "openai".into(),
        display: "OpenAI".into(),
        endpoints: vec![EndpointSpec {
            id: "openai".into(),
            display: "Chat Completions".into(),
            protocol: "openai".into(),
            base_url: "https://api.openai.com/v1".into(),
            default_model: String::new(),
            models: vec![],
            models_url: Some("https://api.openai.com/v1".into()),
            ..Default::default()
        }],
    }
}

fn deepseek_web() -> ProviderSpec {
    ProviderSpec {
        id: "deepseek-web".into(),
        display: "DeepSeek Web (CDP Proxy)".into(),
        endpoints: vec![EndpointSpec {
            id: "cdp".into(),
            display: "CDP Proxy (localhost:8080)".into(),
            protocol: "openai".into(),
            base_url: "http://localhost:8080/v1".into(),
            default_model: "deepseek-v4-pro".into(),
            models: vec!["deepseek-v4-flash".into(), "deepseek-v4-pro".into()],
            models_url: Some("http://localhost:8080/v1".into()),
            user_id_mode: Some(UserSendMode::Body),
            has_balance: false,
            supports_thinking: true,
            stateful: true,
            ..Default::default()
        }],
    }
}

fn providers() -> Vec<ProviderSpec> {
    vec![
        deepseek(),
        qwen(),
        glm(),
        kimi(),
        mimo(),
        minimax(),
        doubao(),
        openai(),
        deepseek_web(),
    ]
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
    let stripped = base.trim_end_matches('/');
    Some(format!("{}/models", stripped))
}

pub fn fetch_models(provider_id: &str, endpoint_id: &str, api_key: &str) -> Vec<String> {
    if find_endpoint(provider_id, endpoint_id).is_none() {
        return vec![];
    };

    let url = match models_url_for(provider_id, endpoint_id) {
        Some(u) => u,
        None => return vec![],
    };

    let agent: ureq::Agent = ureq::Agent::config_builder()
        .timeout_global(Some(std::time::Duration::from_secs(10)))
        .build()
        .into();

    match agent
        .get(&url)
        .header("Authorization", &format!("Bearer {}", api_key))
        .call()
    {
        Ok(resp) => {
            let body: Result<serde_json::Value, _> = resp.into_body().read_json();
            match body {
                Ok(v) => {
                    let models: Vec<String> = v["data"]
                        .as_array()
                        .map(|arr| {
                            arr.iter()
                                .filter_map(|m| m["id"].as_str().map(String::from))
                                .filter(|id| !id.starts_with("deepseek-re"))
                                .collect()
                        })
                        .unwrap_or_default();
                    if models.is_empty() { vec![] } else { models }
                }
                Err(_) => vec![],
            }
        }
        Err(_) => vec![],
    }
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
