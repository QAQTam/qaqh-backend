//! Web tools — fetch a URL, and (on official-tool profiles) search the web.
//!
//! `web_fetch` has two execution paths, chosen per call by
//! [`ToolCallContext::official_web`]:
//!
//! - **官方工具面**（`EndpointCompat::official_web_tools`）→ `POST /v1/tools/fetch`，
//!   平台直接返回 Markdown，跳过 `html2text`；
//! - **其余 profile** → 本模块自实现的 `ureq` + `html2text` 抓取。
//!
//! `search` **没有**自实现版本（本地 Bing-RSS 解析器已退役）：它只在官方工具面
//! 的 profile 上被 runtime 放进模型工具清单（`AgentState::refresh_tool_defs`）。
//! 工具名必须是 canonical `search`：Responses 端点若保留该名，由 gate 的
//! `search_function_alias` 出站改名（`qaqh-gate/src/responses_sdk.rs`）。

use std::path::{Path, PathBuf};
use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ToolRisk;
use crate::tool_api::{
    ToolBody, ToolCallContext, ToolContentBlock, ToolDisplay, ToolError, ToolErrorCode,
    ToolErrorKind, ToolExecutionError, ToolHeader, ToolMeta, ToolProjection, TypedTool,
};

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WebFetchArgs {
    /// URL
    #[serde(default)]
    pub url: String,
    /// Save to file (optional)
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

    fn meta(&self) -> ToolMeta {
        ToolMeta::new(
            "web_fetch",
            "Fetch URL (http). Plain HTTP; web_search is server-side built-in.",
            crate::permission::ToolCategory::Net,
            ToolRisk::ReadOnly,
            Duration::from_secs(30),
        )
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
        let content = match &ctx.official_web {
            Some(backend) => fetch_official(ctx, backend, url, args.output.as_deref())?,
            None => fetch_content(ctx, url, args.output.as_deref(), ctx.timeout.as_secs())?,
        };
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
    Ok(persist_output(ctx, output, readable))
}

/// 官方抓取路径的正文上限（字符）：官方 `content` 是整页 Markdown，不做上限
/// 会把一次抓取变成几十万 token。与自实现路径的 120k 读取上限同口径。
const MAX_OFFICIAL_BODY_CHARS: usize = 120_000;

/// 单条搜索结果的正文上限（字符）。四条一页时总量仍可控。
const MAX_SEARCH_HIT_CHARS: usize = 4_000;

/// 官方抓取：正文已是 Markdown，跳过 `html2text`；其余（落盘/journal）与自实现共用。
#[allow(clippy::result_large_err)] // ToolExecutionError is the frozen typed boundary.
fn fetch_official(
    ctx: &ToolCallContext,
    backend: &crate::tool_api::OfficialWebBackend,
    url: &str,
    output: Option<&str>,
) -> Result<String, ToolExecutionError> {
    let page = backend.inner().fetch(url, ctx.timeout).map_err(|message| {
        web_error(
            ToolErrorKind::Unavailable,
            "official_fetch_error",
            format!("web_fetch: 官方抓取失败（{message}）"),
            Some("换一个 URL 重试；若该 profile 不需要官方面，把 official_web_tools 关掉即回自实现抓取。"),
        )
    })?;
    let title = page.title.trim();
    let mut readable = if title.is_empty() {
        page.content
    } else {
        format!("# {title}\n\n{}", page.content)
    };
    if readable.chars().count() > MAX_OFFICIAL_BODY_CHARS {
        readable = readable.chars().take(MAX_OFFICIAL_BODY_CHARS).collect();
        readable.push_str("\n\n…[truncated by web_fetch]");
    }
    Ok(persist_output(ctx, output, readable))
}

/// 落盘 + journal 记录 + `[saved to …]` 前缀（两条抓取路径共用一份实现）。
fn persist_output(ctx: &ToolCallContext, output: Option<&str>, readable: String) -> String {
    let Some(output) = output else {
        return readable;
    };
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
    format!(
        "[saved to {}]\n{}",
        display_workspace_path(ctx, &target),
        readable
    )
}

/// 官方搜索工具（canonical 名 `search`）。
///
/// 只有在 `ToolCallContext::official_web` 非空时才有意义；runtime 会按配置把它
/// 从其它 profile 的工具清单里摘掉，`run` 里的 `None` 分支只是防御。
pub struct SearchTool;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct WebSearchArgs {
    /// Search query (1–128 characters; supports the official `site:` / `lang:` / `after:` syntax)
    pub query: String,
    /// Number of results (1–50, endpoint default 10)
    #[serde(default)]
    pub count: Option<u32>,
    /// Also return each result's page body (default false; bodies cost many tokens)
    #[serde(default)]
    pub need_content: bool,
}

#[derive(Debug, Serialize, JsonSchema)]
pub struct WebSearchOutput {
    #[serde(skip)]
    #[schemars(skip)]
    text: String,
}

impl ToolProjection for WebSearchOutput {
    fn model_blocks(&self) -> Vec<ToolContentBlock> {
        vec![ToolContentBlock::Text {
            text: self.text.clone(),
        }]
    }

    fn display(&self, args: &serde_json::Value) -> ToolDisplay {
        let query = args
            .get("query")
            .and_then(serde_json::Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        let (body_text, body_truncated) = crate::tool_api::display::clamp_display_body(&self.text);
        let body = ToolBody::Text {
            text: body_text,
            truncated: body_truncated,
        };
        match query {
            Some(query) => ToolDisplay::new(ToolHeader::Query { query, scope: None }, body),
            None => ToolDisplay::new(
                ToolHeader::Other {
                    label: "search".to_string(),
                },
                body,
            ),
        }
    }
}

impl TypedTool for SearchTool {
    type Args = WebSearchArgs;
    type Output = WebSearchOutput;

    fn meta(&self) -> ToolMeta {
        ToolMeta::new(
            "search",
            "Web search (official Search API). Only exposed on profiles that declare official web tools.",
            crate::permission::ToolCategory::Net,
            ToolRisk::ReadOnly,
            Duration::from_secs(30),
        )
    }

    fn run(
        &self,
        ctx: &ToolCallContext,
        args: Self::Args,
    ) -> Result<Self::Output, ToolExecutionError> {
        let Some(backend) = &ctx.official_web else {
            return Err(web_error(
                ToolErrorKind::Unavailable,
                "official_search_unavailable",
                "search: 当前 profile 没有启用官方搜索（official_web_tools）",
                Some(
                    "这个工具只在声明了官方工具面的端点上出现；其它端点用模型内置的 web_search 或 web_fetch。",
                ),
            ));
        };
        let hits = backend
            .inner()
            .search(&args.query, args.count, args.need_content)
            .map_err(|message| {
                web_error(
                    ToolErrorKind::Unavailable,
                    "official_search_error",
                    format!("search: 官方搜索失败（{message}）"),
                    Some("稍后重试；持续失败时检查 DeepSeek 账号登录态与额度。"),
                )
            })?;
        Ok(WebSearchOutput {
            text: render_hits(&args.query, &hits),
        })
    }
}

/// 把官方结果渲染成模型可读的纯文本（标题 / URL / 摘要 / 可选正文）。
fn render_hits(query: &str, hits: &[crate::tool_api::OfficialHit]) -> String {
    if hits.is_empty() {
        return format!("No results for {query:?}.");
    }
    let mut out = String::new();
    for (idx, hit) in hits.iter().enumerate() {
        out.push_str(&format!(
            "{}. {}\n   {}\n",
            idx + 1,
            hit.title.trim(),
            hit.url.trim()
        ));
        let summary = hit.summary.trim();
        if !summary.is_empty() {
            out.push_str(&format!("   {summary}\n"));
        }
        if let Some(content) = &hit.content {
            let body: String = content.trim().chars().take(MAX_SEARCH_HIT_CHARS).collect();
            if !body.is_empty() {
                out.push_str(&format!("   ---\n   {}\n", body.replace('\n', "\n   ")));
            }
        }
    }
    out
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

pub fn register(mgr: &mut crate::ToolManager) {
    mgr.register_typed(WebFetchTool);
    // 暴露面由 runtime 按配置收窄（`official_web_tools`），不是在这里判断：
    // 工具清单是每条会话重建的，全局注册只决定"存在"。
    mgr.register_typed(SearchTool);
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
            exec_progress: None,
            official_web: None,
            source: crate::tool_api::ToolCallSource::Model,
        }
    }

    #[derive(Default)]
    struct StubBackend {
        page: Option<crate::tool_api::OfficialPage>,
        hits: Vec<crate::tool_api::OfficialHit>,
        calls: std::sync::Mutex<Vec<(String, Option<u32>, bool)>>,
    }

    impl crate::tool_api::OfficialWebTools for StubBackend {
        fn fetch(
            &self,
            url: &str,
            _timeout: Duration,
        ) -> Result<crate::tool_api::OfficialPage, String> {
            Ok(self.page.clone().unwrap_or(crate::tool_api::OfficialPage {
                title: "Stub Title".to_string(),
                content: format!("body of {url}"),
            }))
        }

        fn search(
            &self,
            query: &str,
            count: Option<u32>,
            need_content: bool,
        ) -> Result<Vec<crate::tool_api::OfficialHit>, String> {
            self.calls.lock().expect("stub calls lock").push((
                query.to_string(),
                count,
                need_content,
            ));
            Ok(self.hits.clone())
        }
    }

    fn ctx_with_official(root: &Path, backend: std::sync::Arc<StubBackend>) -> ToolCallContext {
        ToolCallContext {
            official_web: Some(crate::tool_api::OfficialWebBackend::new(backend)),
            ..ctx(root)
        }
    }

    #[test]
    fn official_fetch_skips_the_network_and_still_persists_output() {
        let dir = tempfile::tempdir().expect("tempdir");
        let backend = std::sync::Arc::new(StubBackend::default());
        let ctx = ctx_with_official(dir.path(), backend);

        let out = WebFetchTool
            .run(
                &ctx,
                WebFetchArgs {
                    url: "https://example.test/a".to_string(),
                    output: Some("page.md".to_string()),
                },
            )
            .expect("official fetch");
        let content = out
            .model_blocks()
            .into_iter()
            .find_map(|block| match block {
                ToolContentBlock::Text { text } => Some(text),
                _ => None,
            })
            .expect("text block");

        assert!(content.starts_with("[saved to page.md]"), "{content}");
        assert!(content.contains("# Stub Title"), "{content}");
        assert!(content.contains("body of https://example.test/a"));
        let saved = std::fs::read_to_string(dir.path().join("page.md")).expect("saved file");
        assert!(saved.contains("body of https://example.test/a"));
    }

    #[test]
    fn search_without_a_backend_is_an_unavailable_error() {
        let dir = tempfile::tempdir().expect("tempdir");
        let error = SearchTool
            .run(
                &ctx(dir.path()),
                WebSearchArgs {
                    query: "rust".to_string(),
                    count: None,
                    need_content: false,
                },
            )
            .expect_err("no official backend");
        match error {
            ToolExecutionError::Recoverable(error) => {
                assert_eq!(error.code.as_str(), "official_search_unavailable");
            }
            ToolExecutionError::Fatal(error) => panic!("unexpected fatal: {}", error.code),
        }
    }

    #[test]
    fn search_renders_hits_and_forwards_arguments() {
        let dir = tempfile::tempdir().expect("tempdir");
        let backend = std::sync::Arc::new(StubBackend {
            page: None,
            hits: vec![
                crate::tool_api::OfficialHit {
                    title: "First".to_string(),
                    url: "https://a.test".to_string(),
                    summary: "短摘要".to_string(),
                    content: None,
                },
                crate::tool_api::OfficialHit {
                    title: "Second".to_string(),
                    url: "https://b.test".to_string(),
                    summary: String::new(),
                    content: Some("line1\nline2".to_string()),
                },
            ],
            calls: std::sync::Mutex::new(Vec::new()),
        });
        let ctx = ctx_with_official(dir.path(), backend.clone());

        let out = SearchTool
            .run(
                &ctx,
                WebSearchArgs {
                    query: "rust".to_string(),
                    count: Some(3),
                    need_content: true,
                },
            )
            .expect("search");
        let text = out
            .model_blocks()
            .into_iter()
            .find_map(|block| match block {
                ToolContentBlock::Text { text } => Some(text),
                _ => None,
            })
            .expect("text block");

        assert!(text.contains("1. First"), "{text}");
        assert!(text.contains("https://a.test"));
        assert!(text.contains("短摘要"));
        assert!(text.contains("2. Second"));
        assert!(text.contains("line1"), "{text}");
        assert_eq!(
            backend.calls.lock().expect("calls").as_slice(),
            &[("rust".to_string(), Some(3), true)]
        );
    }

    #[test]
    fn search_with_no_hits_says_so() {
        let dir = tempfile::tempdir().expect("tempdir");
        let backend = std::sync::Arc::new(StubBackend::default());
        let ctx = ctx_with_official(dir.path(), backend);

        let out = SearchTool
            .run(
                &ctx,
                WebSearchArgs {
                    query: "nothing".to_string(),
                    count: None,
                    need_content: false,
                },
            )
            .expect("search");
        let text = out
            .model_blocks()
            .into_iter()
            .find_map(|block| match block {
                ToolContentBlock::Text { text } => Some(text),
                _ => None,
            })
            .expect("text block");
        assert!(text.contains("No results"), "{text}");
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
    fn registration_is_typed_and_schema_is_type_generated() {
        let mut manager = crate::ToolManager::new();
        register(&mut manager);
        let registered = manager.builtins.get("web_fetch").expect("registered");
        assert_eq!(registered.descriptor.name.as_str(), "web_fetch");
        // v2：schema 由 WebFetchArgs 类型生成——url 仍出现在 properties 中
        // （required 约束由 run() 的 missing_url 校验兜底）。
        assert!(
            registered.descriptor.input_schema["properties"]["url"].is_object(),
            "generated schema must expose url"
        );
        assert!(
            registered.descriptor.input_schema["properties"]["output"].is_object(),
            "generated schema must expose output"
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
