//! Gate lap 阶段：provider 构建与模型请求前置准备。
//!
//! 当前承载 `run_lap` 的 provider 构建（纯只读、行为不变）。gate 请求
//! 主体（`chat_stream` + 流式事件聚合 + 错误归一）随后续拆分逐步迁入。

use crate::ringing_v1::types::RingContext;

/// 依当前 config / endpoint 重建 provider（Responses 或 OpenAI 形态）。
///
/// 从 `run_lap` 原样抽出的 provider 构建逻辑；`ep`/`is_responses` 仅在
/// 此处使用，迁移后 `run_lap` 不再持有它们。
pub(crate) fn provider_for(ctx: &RingContext) -> qaqh_gate::ProviderConfig {
    let ep = qaqh_config::registry::find_endpoint(
        &ctx.agent.config.provider_id,
        &ctx.agent.config.endpoint,
    );
    let is_responses = ep.as_ref().map(|e| e.protocol.as_str()) == Some("responses");
    if is_responses {
        let mut p = qaqh_gate::ProviderConfig::responses(
            &ctx.agent.config.base_url,
            &ctx.agent.config.api_key,
            &ctx.agent.config.model,
            ep.as_ref().and_then(|e| e.responses_path.clone()),
        );
        if let Some(endpoint) = ep.as_ref() {
            p.responses_compat = qaqh_gate::ResponsesCompat {
                web_search: endpoint.responses_web_search,
                echo_web_search_call: endpoint.responses_echo_web_search_call,
                send_include: endpoint.responses_send_include,
                effort_max: endpoint.responses_effort_max.clone(),
                supports_user: endpoint.responses_supports_user,
                search_function_alias: endpoint.responses_search_function_alias.clone(),
                echo_reasoning_content: endpoint.responses_echo_reasoning_content,
            };
        }
        // Muse Spark 专项：物理前缀缓存 + 关明文回放 + 放宽档位至 xhigh
        if p.model.contains("muse-spark") {
            p.prompt_cache_key = Some(ctx.agent.session.seed.clone());
            p.responses_compat.echo_reasoning_content = false;
            p.responses_compat.effort_max = "xhigh".into();
            p.responses_compat.web_search = false;
            p.responses_compat.echo_web_search_call = false;
        }
        p
    } else {
        let mut p = qaqh_gate::ProviderConfig::openai(
            &ctx.agent.config.base_url,
            &ctx.agent.config.api_key,
            &ctx.agent.config.model,
            ep.as_ref().and_then(|e| e.user_id_mode.clone()),
            ep.as_ref().and_then(|e| e.chat_path.clone()),
            ep.as_ref()
                .map(|e| e.thinking_mode.clone())
                .unwrap_or_default(),
            ep.as_ref()
                .map(|e| e.cache_field.clone())
                .unwrap_or_default(),
            ep.as_ref().map(|e| e.supports_thinking).unwrap_or(true),
            ep.as_ref().and_then(|e| e.do_sample),
        )
        .with_stateful(ep.as_ref().map(|e| e.stateful).unwrap_or(false))
        .with_stream_usage(ep.as_ref().map(|e| e.include_stream_usage).unwrap_or(false));
        if let Some(endpoint) = ep.as_ref() {
            p.supports_reasoning_effort = endpoint.supports_reasoning_effort;
            p.tool_call_content_null = endpoint.tool_call_content_null;
            p.supports_reasoning_content = endpoint.supports_reasoning_content;
            p.require_provider_parameters = endpoint.require_provider_parameters;
        }
        p
    }
}
