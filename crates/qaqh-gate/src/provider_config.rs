//! Construction of a provider from a frozen endpoint profile.

use qaqh_types::{EndpointCompat, Wire};

/// All inputs are explicit; credential acquisition is performed by the host.
pub struct EndpointProfile<'a> {
    pub wire: Wire,
    pub base_url: &'a str,
    pub api_key: &'a str,
    pub model: &'a str,
    pub compat: &'a EndpointCompat,
    pub session_id: &'a str,
    pub request_tag: &'a str,
}

/// Build the wire-specific provider without accessing runtime state or credentials.
pub fn build_provider(profile: EndpointProfile<'_>) -> crate::ProviderConfig {
    let EndpointProfile {
        wire,
        base_url,
        api_key,
        model,
        compat,
        session_id,
        request_tag,
    } = profile;
    let mut provider = match wire {
        Wire::Messages => {
            let mut p =
                crate::ProviderConfig::messages(base_url, api_key, model, compat.path.clone());
            p.supports_thinking = compat.supports_thinking;
            p.supports_reasoning_effort = compat.supports_reasoning_effort;
            p.supports_reasoning_content = compat.supports_reasoning_content;
            p.thinking_budget_large = compat.thinking_budget_large;
            p
        }
        Wire::Responses => {
            let mut p =
                crate::ProviderConfig::responses(base_url, api_key, model, compat.path.clone());
            p.responses_compat = crate::ResponsesCompat {
                web_search: compat.responses_web_search,
                echo_web_search_call: compat.responses_echo_web_search_call,
                send_include: compat.responses_send_include,
                effort_max: compat.responses_effort_max.clone(),
                supports_user: compat.responses_supports_user,
                search_function_alias: compat.responses_search_function_alias.clone(),
                echo_reasoning_content: compat.responses_echo_reasoning_content,
                siwc_plan: compat.responses_siwc_plan,
            };
            p
        }
        Wire::Chat => {
            let mut p = crate::ProviderConfig::chat(
                base_url,
                api_key,
                model,
                compat.user_id_mode.clone(),
                compat.path.clone(),
                compat.thinking_mode.clone(),
                compat.cache_field.clone(),
                compat.supports_thinking,
                compat.do_sample,
            )
            .with_stream_usage(compat.include_stream_usage);
            p.supports_reasoning_effort = compat.supports_reasoning_effort;
            p.effort_allowlist = compat.effort_allowlist.clone();
            p.tool_call_content_null = compat.tool_call_content_null;
            p.supports_reasoning_content = compat.supports_reasoning_content;
            p.require_provider_parameters = compat.require_provider_parameters;
            p
        }
        Wire::Gemini => {
            let mut p =
                crate::ProviderConfig::gemini(base_url, api_key, model, compat.path.clone());
            p.supports_thinking = compat.supports_thinking;
            p.supports_reasoning_effort = compat.supports_reasoning_effort;
            p.effort_allowlist = compat.effort_allowlist.clone();
            p
        }
    };
    provider.send_request_context = compat.send_request_context;
    provider.cache_usage_semantics = compat.cache_usage_semantics;
    provider
        .with_opencode_headers(session_id, request_tag)
        .with_retry(compat.retry.clone())
}
