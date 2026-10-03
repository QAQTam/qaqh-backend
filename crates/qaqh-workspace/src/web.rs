//! Web tool — fetch URL content.
//!
//! Web *search* is no longer a local tool: DeepSeek / OpenAI Responses APIs
//! ship a built-in `web_search` tool executed server-side (see
//! `qaqh-gate/src/responses.rs`). The model triggers it on its own, so the
//! local Bing-RSS parser was removed — this tool only fetches URLs the model
//! (or user) explicitly wants to read.

use std::path::{Path, PathBuf};
use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ToolRisk;
use crate::tool_api::{
    OutputBudget, ToolBody, ToolCallContext, ToolContentBlock, ToolDescriptor, ToolDisplay,
    ToolError, ToolErrorCode, ToolErrorKind, ToolExecutionError, ToolExposure, ToolHeader,
    ToolName, ToolProjection, ToolSource, TypedTool,
};

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WebFetchArgs {
    #[serde(default)]
    pub url: String,
    #[serde(default)]
    pub output: Option<String>,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct WebFetchOutput {
    #[serde(skip)]
    #[schemars(skip)]
    content: String,
}

impl ToolProjection for WebFetchOutput {
    fn model_blocks(&self) -> Vec<ToolContentBlock> {
        vec![ToolContentBlock::Text {
            text: self.content.clone(),
        }]
    }

    fn display(&self, args: &serde_json::Value) -> ToolDisplay {
        let query = args
            .get("url")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let scope = args
            .get("output")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        let (body_text, body_truncated) =
            crate::tool_api::display::clamp_display_body(&self.content);
        let body = ToolBody::Text {
            text: body_text,
            truncated: body_truncated,
        };
        match query {
            Some(query) => ToolDisplay::new(
                ToolHeader::Query {
                    query: query.to_string(),
                    scope,
                },
                body,
            ),
            None => ToolDisplay::new(
                ToolHeader::Other {
                    label: "web_fetch".to_string(),
                },
                body,
            ),
        }
    }
}

pub struct WebFetchTool;

impl TypedTool for WebFetchTool {
    type Args = WebFetchArgs;
    type Output = WebFetchOutput;

    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: ToolName::new("web_fetch").expect("valid web_fetch tool name"),
            display_name: None,
            description: "Fetch URL (http). Plain HTTP; web_search is server-side built-in."
                .to_string(),
            input_schema: web_fetch_schema(),
            output_schema: serde_json::to_value(schemars::schema_for!(WebFetchOutput))
                .expect("web_fetch output schema"),
            category: crate::permission::ToolCategory::Net,
            risk: ToolRisk::ReadOnly,
            default_timeout: Duration::from_secs(30),
            exposure: ToolExposure::Direct,
            source: ToolSource::Builtin,
            output_budget: OutputBudget::default(),
            capabilities: crate::tool_capabilities::builtin_capabilities("web_fetch")
                .unwrap_or_default(),
        }
    }

    fn run(
        &self,
        ctx: &ToolCallContext,
        args: Self::Args,
    ) -> Result<Self::Output, ToolExecutionError> {
        let url = args.url.as_str();
        if !url.starts_with("http") {
            return Err(web_error(
                ToolErrorKind::InvalidArguments,
                "missing_url",
                "web_fetch: 'url' (starting with http) is required; web search is handled by the model's built-in web_search tool",
                Some("Pass a URL to fetch, or rely on the model's server-side web_search."),
            ));
        }
        let content = fetch_content(ctx, url, args.output.as_deref(), ctx.timeout.as_secs())?;
        Ok(WebFetchOutput { content })
    }
}

fn http_agent(timeout_secs: u64) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(timeout_secs)))
        .build()
        .into()
}

#[allow(clippy::result_large_err)] // ToolExecutionError is the frozen typed boundary.
fn fetch_content(
    ctx: &ToolCallContext,
    url: &str,
    output: Option<&str>,
    timeout_secs: u64,
) -> Result<String, ToolExecutionError> {
    const MAX_WEB_BODY_BYTES: u64 = 512 * 1024;
    let resp = http_agent(timeout_secs)
        .get(url)
        .header(
            "User-Agent",
            "Mozilla/5.0 (Windows NT 10.0; Win64; x64) AppleWebKit/537.36 (KHTML, like Gecko) Chrome/151.0.0.0 Safari/537.36",
        )
        .call()
        .map_err(|error| web_fetch_payload_error("fetch_error", format!("{error}"), ""))?;
    if resp
        .body()
        .content_length()
        .is_some_and(|len| len > MAX_WEB_BODY_BYTES)
    {
        return Err(web_fetch_payload_error(
            "response_too_large",
            format!("Response exceeds the {MAX_WEB_BODY_BYTES} byte limit"),
            "Fetch a narrower URL or use a source with a paginated API.",
        ));
    }
    let is_html = resp
        .headers()
        .get("Content-Type")
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| value.contains("html"));
    let body = resp
        .into_body()
        .with_config()
        .limit(MAX_WEB_BODY_BYTES)
        .read_to_string()
        .map_err(|_| {
            web_fetch_payload_error(
                "read_error",
                "Response could not be read within the body limit",
                "Fetch a narrower URL or use a source with a paginated API.",
            )
        })?;
    let readable = if is_html || body.trim_start().starts_with('<') {
        html2text::from_read(body.as_bytes(), body.len().min(120_000)).unwrap_or(body)
    } else {
        body
    };
    if let Some(output) = output {
        let target = resolve_output_path(ctx, output);
        let before = std::fs::read_to_string(&target).ok();
        let _ = std::fs::write(&target, &readable);
        crate::journal::record_change(
            &ctx.session_id,
            "",
            "web_fetch",
            output,
            "overwrite",
            before.as_deref(),
            Some(&readable),
            "ok",
        );
        // Lead with a save marker: plain-text folding keeps the first line,
        // so a folded historical result still tells the model where the
        // content lives — it can `read` the file instead of re-fetching.
        return Ok(format!(
            "[saved to {}]\n{}",
            display_workspace_path(ctx, &target),
            readable
        ));
    }
    Ok(readable)
}

fn resolve_output_path(ctx: &ToolCallContext, raw_path: &str) -> PathBuf {
    let path = Path::new(raw_path);
    let joined = if path.is_absolute() {
        path.to_path_buf()
    } else if ctx.workspace_root.as_os_str().is_empty() {
        PathBuf::from(raw_path)
    } else {
        ctx.workspace_root.join(path)
    };
    crate::permission::normalize_lexically(&joined)
}

fn display_workspace_path(ctx: &ToolCallContext, target: &Path) -> String {
    target
        .strip_prefix(&ctx.workspace_root)
        .unwrap_or(target)
        .to_string_lossy()
        .replace('\\', "/")
}

fn web_fetch_payload_error(
    code: &str,
    message: impl Into<String>,
    hint: &str,
) -> ToolExecutionError {
    // detail 是人读一句话（模型面与 error 槽共用）；hint 走结构化槽。
    // 此前把 §7 JSON 信封塞进 detail——模型与展示面拿到的都是一坨 JSON
    // （2026-10-03 拆考古层时一并清除，无兼容包袱）。
    web_error(
        ToolErrorKind::Execution,
        code,
        message,
        Some(hint).filter(|hint| !hint.is_empty()),
    )
}

fn web_error(
    kind: ToolErrorKind,
    code: &str,
    message: impl Into<String>,
    hint: Option<&str>,
) -> ToolExecutionError {
    let mut error = ToolError::new(kind, message);
    error.code = ToolErrorCode::parse_or_builtin(code, kind);
    if let Some(hint) = hint {
        error = error.with_hint(hint);
    }
    ToolExecutionError::Recoverable(error)
}

fn web_fetch_schema() -> serde_json::Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "url": {"type": "string", "description": "URL"},
            "output": {"type": "string", "description": "Save to file (optional)"}
        },
        "required": ["url"],
        "additionalProperties": false
    })
}

pub fn register(mgr: &mut crate::ToolManager) {
    mgr.register_typed(WebFetchTool);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn ctx(root: &Path) -> ToolCallContext {
        ToolCallContext {
            call_id: "web-fetch-test".to_string(),
            session_id: "web-fetch-test-session".to_string(),
            workspace_root: root.to_path_buf(),
            mode: crate::tool_api::AgentMode::Code,
            permission_level: crate::permission::PermissionLevel::ReadOnly,
            sandbox: crate::tool_api::SandboxMode::Main,
            sandbox_spec: crate::tool_api::SandboxSpec::workspace_write(root.to_path_buf()),
            exec_default_shell: None,
            timeout: Duration::from_secs(30),
            cancellation: crate::tool_api::CancellationToken::new(),
            progress: None,
            source: crate::tool_api::ToolCallSource::Model,
        }
    }

    #[test]
    fn missing_url_preserves_legacy_code_and_hint() {
        let dir = tempfile::tempdir().expect("tempdir");
        let error = WebFetchTool
            .run(
                &ctx(dir.path()),
                WebFetchArgs {
                    url: String::new(),
                    output: None,
                },
            )
            .expect_err("missing url");
        match error {
            ToolExecutionError::Recoverable(error) => {
                assert_eq!(error.code.as_str(), "missing_url");
                assert!(error.hint.is_some());
            }
            ToolExecutionError::Fatal(error) => panic!("unexpected fatal: {}", error.code),
        }
    }

    #[test]
    fn success_output_keeps_legacy_empty_canonical_data() {
        let output = WebFetchOutput {
            content: "page text\n".to_string(),
        };
        assert_eq!(
            serde_json::to_value(&output).expect("serialize"),
            serde_json::json!({})
        );
        let model = match output.model_blocks().into_iter().next() {
            Some(ToolContentBlock::Text { text }) => text,
            _ => panic!("web_fetch output must have a text model block"),
        };
        assert_eq!(model, "page text\n");
        let display = output.display(&serde_json::json!({
            "url": "https://example.test",
            "output": "page.md"
        }));
        assert_eq!(
            display.header,
            ToolHeader::Query {
                query: "https://example.test".to_string(),
                scope: Some("page.md".to_string()),
            }
        );
        assert_eq!(display.summary, None);
    }

    #[test]
    fn registration_is_typed_and_schema_keeps_required_url() {
        let mut manager = crate::ToolManager::new();
        register(&mut manager);
        let registered = manager.builtins.get("web_fetch").expect("registered");
        assert!(registered.legacy.is_none());
        assert_eq!(
            registered.descriptor.input_schema["required"],
            serde_json::json!(["url"])
        );
        assert_eq!(
            registered.descriptor.input_schema["additionalProperties"],
            serde_json::json!(false)
        );
    }

    #[test]
    fn output_path_resolves_against_explicit_workspace() {
        let dir = tempfile::tempdir().expect("tempdir");
        let resolved = resolve_output_path(&ctx(dir.path()), "nested/page.md");
        assert_eq!(resolved, dir.path().join("nested/page.md"));
        assert_eq!(
            display_workspace_path(&ctx(dir.path()), &resolved),
            "nested/page.md"
        );
    }
}
