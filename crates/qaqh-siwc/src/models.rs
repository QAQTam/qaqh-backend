//! 模型发现：`GET {api_base}/models`（用同一 access token）。
//!
//! 文档要点：
//! - 只保留 `visibility == "list"` 的项，并**保持服务端顺序**；
//! - `display_name` 给 UI 显示，`slug` 才是请求里用的 `model`；
//! - 用户切 ChatGPT 账户后必须重新取（额度与可用模型跟着账户走）。

use serde_json::Value;

use crate::error::{SiwcCode, SiwcError};
use crate::http::Http;

/// 模型选择器的一项。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ModelInfo {
    pub slug: String,
    pub display_name: String,
}

/// 拉取可用模型目录。
pub fn list_models(
    http: &Http,
    api_base: &str,
    access_token: &str,
) -> Result<Vec<ModelInfo>, SiwcError> {
    let url = format!("{}/models", api_base.trim_end_matches('/'));
    let response = http.get(&url, Some(access_token))?;
    if !response.is_success() {
        let code = crate::error::classify_provider_error(
            response.status,
            response.error_code().as_deref(),
        );
        return Err(SiwcError::new(
            code,
            format!(
                "模型目录 HTTP {}: {}",
                response.status,
                response.error_message()
            ),
        )
        .with_request_id(response.request_id.clone()));
    }
    let value = response.json()?;
    parse_models(&value)
}

/// 解析 `{"models":[{slug, display_name, visibility}]}`。
///
/// 缺 `visibility` 的项**保留**：文档只要求「只展示 `visibility == "list"`」，
/// 而把字段缺失一律当隐藏会让一次上游字段调整变成空选择器。
pub fn parse_models(value: &Value) -> Result<Vec<ModelInfo>, SiwcError> {
    let items = value
        .get("models")
        .and_then(Value::as_array)
        .ok_or_else(|| {
            SiwcError::new(
                SiwcCode::ProviderError,
                "模型目录响应缺 models 数组".to_string(),
            )
        })?;
    let mut models = Vec::new();
    for item in items {
        if let Some(visibility) = item.get("visibility").and_then(Value::as_str)
            && visibility != "list"
        {
            continue;
        }
        let Some(slug) = item.get("slug").and_then(Value::as_str) else {
            continue;
        };
        let display_name = item
            .get("display_name")
            .and_then(Value::as_str)
            .unwrap_or(slug);
        models.push(ModelInfo {
            slug: slug.to_owned(),
            display_name: display_name.to_owned(),
        });
    }
    Ok(models)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keeps_list_visible_models_in_server_order() {
        let value = serde_json::json!({
            "models": [
                {"slug": "gpt-6.1-sol", "display_name": "GPT-6.1 Sol", "visibility": "list"},
                {"slug": "hidden-internal", "display_name": "Internal", "visibility": "hidden"},
                {"slug": "gpt-6.1-mini", "display_name": "GPT-6.1 mini", "visibility": "list"},
                {"slug": "no-visibility-field"}
            ]
        });
        let models = parse_models(&value).expect("models");
        let slugs: Vec<&str> = models.iter().map(|m| m.slug.as_str()).collect();
        assert_eq!(slugs, vec!["gpt-6.1-sol", "gpt-6.1-mini", "no-visibility-field"]);
        assert_eq!(models[0].display_name, "GPT-6.1 Sol");
        // display_name 缺失时回退 slug（UI 至少能显示点东西）。
        assert_eq!(models[2].display_name, "no-visibility-field");
    }

    #[test]
    fn missing_models_array_is_an_error_not_an_empty_picker() {
        assert_eq!(
            parse_models(&serde_json::json!({"data": []}))
                .expect_err("shape")
                .code(),
            SiwcCode::ProviderError
        );
    }
}
