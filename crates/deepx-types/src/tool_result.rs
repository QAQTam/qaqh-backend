//! Canonical tool execution result shared by the tool runtime and Ringing.
//!
//! A tool has one authoritative status. Human summaries, compact metadata and
//! the bounded model projection are separate fields so transport and UI code
//! never have to infer failure from the shape of textual output.

use serde::{Deserialize, Serialize};
use ts_rs::TS;

pub const TOOL_SUMMARY_MAX_CHARS: usize = 512;
// Keep the model projection near the planned six-thousand-token budget.
// The limit is expressed in Unicode characters because provider tokenizers
// are not available at this shared contract boundary.
pub const TOOL_MODEL_MAX_CHARS: usize = 24_000;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize, TS)]
#[serde(rename_all = "snake_case")]
pub enum ToolStatus {
    Ok,
    Error,
    Partial,
    Backgrounded,
    Cancelled,
}

impl ToolStatus {
    pub fn is_success(self) -> bool {
        matches!(self, Self::Ok | Self::Backgrounded)
    }

    pub fn is_failure(self) -> bool {
        matches!(self, Self::Error | Self::Partial | Self::Cancelled)
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ContentRef {
    pub content_id: String,
    pub media_type: String,
    pub sha256: String,
    pub truncated: bool,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct ToolContinuation {
    pub tool: String,
    #[ts(type = "JsonValue")]
    pub args: serde_json::Value,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct ToolModelPayload {
    pub text: String,
    pub truncated: bool,
    pub total_tokens: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub continuation: Option<ToolContinuation>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, TS)]
pub struct ToolError {
    pub code: String,
    pub message: String,
    pub retryable: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hint: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, TS)]
pub struct ToolResult {
    pub status: ToolStatus,
    pub summary: String,
    #[ts(type = "JsonValue")]
    pub data: serde_json::Value,
    pub model: ToolModelPayload,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub output_ref: Option<ContentRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<ToolError>,
}

impl ToolResult {
    pub fn ok(text: impl Into<String>) -> Self {
        Self::text(ToolStatus::Ok, text.into())
    }

    pub fn partial(text: impl Into<String>) -> Self {
        let text = text.into();
        Self::with_error(ToolStatus::Partial, text.clone(), "PARTIAL", text, false, None)
    }

    pub fn cancelled(text: impl Into<String>) -> Self {
        let text = text.into();
        Self::with_error(ToolStatus::Cancelled, text.clone(), "CANCELLED", text, false, None)
    }

    pub fn backgrounded(text: impl Into<String>) -> Self {
        Self::text(ToolStatus::Backgrounded, text.into())
    }

    pub fn error(message: impl Into<String>) -> Self {
        let message = message.into();
        Self::error_with("TOOL_ERROR", message, false, None)
    }

    pub fn error_with(
        code: impl Into<String>,
        message: impl Into<String>,
        retryable: bool,
        hint: Option<String>,
    ) -> Self {
        let code = code.into();
        let message = message.into();
        Self::with_error(
            ToolStatus::Error,
            message.clone(),
            code,
            message,
            retryable,
            hint,
        )
    }

    pub fn ok_data(data: serde_json::Value, text: impl Into<String>) -> Self {
        let text = text.into();
        let mut result = Self::text(ToolStatus::Ok, text);
        result.data = compact_data(data);
        result
    }

    pub fn error_data(
        code: impl Into<String>,
        message: impl Into<String>,
        retryable: bool,
        hint: Option<String>,
        data: serde_json::Value,
    ) -> Self {
        let mut result = Self::error_with(code, message, retryable, hint);
        result.data = compact_data(data);
        result
    }

    pub fn text(status: ToolStatus, text: String) -> Self {
        let text = text;
        let model_text = bounded_text(&text, TOOL_MODEL_MAX_CHARS);
        Self {
            status,
            summary: bounded_text(&text, TOOL_SUMMARY_MAX_CHARS).0,
            data: serde_json::Value::Object(Default::default()),
            model: ToolModelPayload {
                text: model_text.0,
                truncated: model_text.1,
                total_tokens: estimate_tokens(&text),
                continuation: None,
            },
            output_ref: None,
            error: None,
        }
    }

    pub fn with_error(
        status: ToolStatus,
        summary: impl Into<String>,
        code: impl Into<String>,
        message: impl Into<String>,
        retryable: bool,
        hint: Option<String>,
    ) -> Self {
        let mut result = Self::text(status, summary.into());
        result.error = Some(ToolError {
            code: code.into(),
            message: bounded_text(&message.into(), TOOL_SUMMARY_MAX_CHARS).0,
            retryable,
            hint: hint.map(|value| bounded_text(&value, TOOL_SUMMARY_MAX_CHARS).0),
        });
        result
    }

    pub fn is_success(&self) -> bool {
        self.status.is_success()
    }

    pub fn model_text(&self) -> &str {
        &self.model.text
    }

    pub fn validate(&self) -> Result<(), &'static str> {
        if self.summary.chars().count() > TOOL_SUMMARY_MAX_CHARS {
            return Err("summary exceeds the Unicode character budget");
        }
        if self.status.is_failure() && self.error.is_none() {
            return Err("failure result must include error");
        }
        if self.status.is_success() && self.error.is_some() {
            return Err("successful result must not include error");
        }
        if self.model.text.chars().count() > TOOL_MODEL_MAX_CHARS {
            return Err("model text exceeds the default model budget");
        }
        Ok(())
    }

    /// Stable payload used by provider adapters and context accounting.
    pub fn project_for_model(&self) -> serde_json::Value {
        serde_json::json!({
            "status": self.status,
            "summary": self.summary,
            "data": self.data,
            "text": self.model.text,
            "truncated": self.model.truncated,
            "continuation": self.model.continuation,
        })
    }
}

fn compact_data(data: serde_json::Value) -> serde_json::Value {
    match data {
        serde_json::Value::Object(mut object) => {
            object.remove("stdout");
            object.remove("stderr");
            object.remove("output");
            object.remove("content");
            serde_json::Value::Object(object)
        }
        serde_json::Value::Null => serde_json::Value::Object(Default::default()),
        other => other,
    }
}

fn bounded_text(text: &str, max_chars: usize) -> (String, bool) {
    let mut chars = text.chars();
    let bounded: String = chars.by_ref().take(max_chars).collect();
    let truncated = chars.next().is_some();
    (bounded, truncated)
}

fn estimate_tokens(text: &str) -> u64 {
    ((text.chars().count() as u64) + 3) / 4
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_status_is_the_only_failure_authority() {
        let result = ToolResult::error_with(
            "NOT_FOUND",
            "missing",
            false,
            Some("retry read".into()),
        );
        assert_eq!(result.status, ToolStatus::Error);
        assert!(result.error.is_some());
        assert!(!result.is_success());
        result.validate().unwrap();
    }

    #[test]
    fn summary_budget_is_unicode_safe_and_model_projection_is_stable() {
        let result = ToolResult::ok("界".repeat(TOOL_MODEL_MAX_CHARS + 100));
        assert_eq!(result.summary.chars().count(), TOOL_SUMMARY_MAX_CHARS);
        assert!(result.model.truncated);
        assert!(result.project_for_model().get("success").is_none());
        result.validate().unwrap();
    }

    #[test]
    fn compact_data_drops_large_inline_output_fields() {
        let result = ToolResult::ok_data(
            serde_json::json!({"path":"a.rs", "stdout":"large", "output":"large"}),
            "done",
        );
        assert_eq!(result.data["path"], "a.rs");
        assert!(result.data.get("stdout").is_none());
        assert!(result.data.get("output").is_none());
    }
}
