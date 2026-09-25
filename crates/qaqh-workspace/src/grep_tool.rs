//! `grep`：内容搜索工具——直接使用 ripgrep 的核心库
//! （`grep-searcher` + `grep-regex` + `ignore`，与 rg 二进制同一实现），
//! **零外部二进制依赖**：用户机器上没有 rg 也能用。
//!
//! 与 `glob`（文件名列举）分工：grep 搜**文件内容**，输出 `path:line:content`
//! （上下文行用 rg 的 `path-line-content` 格式）。与 `exec` 分工：grep 是
//! 模型友好的封装——workspace 边界内、结果截断、结构化输出。
//!
//! - 正则语法 = rg（Rust regex 引擎；`(?i)`、`\b`、`|` 等均可用）。
//! - 默认大小写不敏感（对齐 Claude Code Grep）；`case_sensitive=true` 关闭。
//! - 默认跳过 hidden/gitignored/binary 文件（rg 原生行为）。
//! - `max_results` 截断 + `truncated` 标记，防上下文爆炸。

use grep_regex::RegexMatcherBuilder;
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkContext, SinkMatch};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::time::Duration;

use crate::ToolRisk;
use crate::tool_api::{
    OutputBudget, ToolBody, ToolCallContext, ToolContentBlock, ToolDescriptor, ToolDisplay,
    ToolError, ToolErrorCode, ToolErrorKind, ToolExecutionError, ToolExposure, ToolHeader,
    ToolName, ToolProjection, ToolSource, TypedTool,
};

const DEFAULT_MAX_RESULTS: usize = 200;
const MAX_RESULTS_CAP: usize = 2_000;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GrepArgs {
    #[serde(default)]
    pub pattern: String,
    #[serde(default)]
    pub paths: Option<Vec<String>>,
    #[serde(default)]
    pub glob: Option<Vec<String>>,
    #[serde(default)]
    pub case_sensitive: Option<bool>,
    #[serde(default)]
    pub context_before: Option<u64>,
    #[serde(default)]
    pub context_after: Option<u64>,
    #[serde(default)]
    pub max_results: Option<u64>,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct GrepMatch {
    pub path: String,
    pub line: u64,
    pub content: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
struct MatchLine {
    path: String,
    line: u64,
    content: String,
    is_context: bool,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct GrepOutput {
    pub status: String,
    pub matches: Vec<GrepMatch>,
    pub truncated: bool,
    pub count: usize,
    #[serde(skip)]
    #[schemars(skip)]
    lines: Vec<MatchLine>,
}

impl GrepOutput {
    fn model_text(&self) -> String {
        let mut text = String::new();
        if self.lines.is_empty() {
            for matched in &self.matches {
                text.push_str(&format!(
                    "{}:{}:{}\n",
                    matched.path, matched.line, matched.content
                ));
            }
        } else {
            for matched in &self.lines {
                if matched.is_context {
                    text.push_str(&format!(
                        "{}-{}-{}\n",
                        matched.path, matched.line, matched.content
                    ));
                } else {
                    text.push_str(&format!(
                        "{}:{}:{}\n",
                        matched.path, matched.line, matched.content
                    ));
                }
            }
        }
        if self.truncated {
            text.push_str(&format!(
                "... truncated at {} matches (narrow the pattern, add glob filters, or set a higher max_results)\n",
                self.count
            ));
        }
        if text.is_empty() {
            text = "(no matches)\n".to_string();
        }
        text
    }
    fn display_summary(&self) -> String {
        let files = self
            .matches
            .iter()
            .map(|matched| matched.path.as_str())
            .collect::<BTreeSet<_>>()
            .len();
        let mut summary = format!(
            "{} match{} · {} file{}",
            self.count,
            if self.count == 1 { "" } else { "es" },
            files,
            if files == 1 { "" } else { "s" }
        );
        if self.truncated {
            summary.push_str(" · truncated");
        }
        summary
    }
}

impl ToolProjection for GrepOutput {
    fn model_blocks(&self) -> Vec<ToolContentBlock> {
        vec![ToolContentBlock::Text {
            text: self.model_text(),
        }]
    }

    fn summary(&self) -> Option<String> {
        Some(self.display_summary())
    }

    fn display(&self, args: &Value) -> ToolDisplay {
        let query = args
            .get("pattern")
            .and_then(Value::as_str)
            .map(str::trim)
            .filter(|value| !value.is_empty());
        let scope = args
            .get("path")
            .and_then(Value::as_str)
            .or_else(|| {
                args.get("paths")
                    .and_then(Value::as_array)
                    .and_then(|paths| paths.first())
                    .and_then(Value::as_str)
            })
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_string);
        let text = self.model_text();
        let (body_text, body_truncated) = crate::tool_api::display::clamp_display_body(&text);
        match query {
            Some(query) => ToolDisplay::new(
                ToolHeader::Query {
                    query: query.to_string(),
                    scope,
                },
                ToolBody::Text {
                    text: body_text.clone(),
                    truncated: body_truncated,
                },
            )
            .with_summary(self.display_summary()),
            None => ToolDisplay::new(
                ToolHeader::Other {
                    label: "grep".to_string(),
                },
                ToolBody::Text {
                    text: body_text,
                    truncated: body_truncated,
                },
            ),
        }
    }
}

pub struct GrepTool;

impl TypedTool for GrepTool {
    type Args = GrepArgs;
    type Output = GrepOutput;

    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: ToolName::new("grep").expect("valid grep tool name"),
            display_name: None,
            description: "Search file contents with ripgrep regex. Returns path:line:content; filter files with glob."
                .to_string(),
            input_schema: grep_schema(),
            output_schema: serde_json::to_value(schemars::schema_for!(GrepOutput))
                .expect("grep output schema"),
            category: crate::permission::ToolCategory::Read,
            risk: ToolRisk::ReadOnly,
            default_timeout: Duration::from_secs(60),
            exposure: ToolExposure::Direct,
            source: ToolSource::Builtin,
            output_budget: OutputBudget::default(),
            capabilities: crate::tool_capabilities::builtin_capabilities("grep")
                .unwrap_or_default(),
        }
    }

    fn run(
        &self,
        ctx: &ToolCallContext,
        args: Self::Args,
    ) -> Result<Self::Output, ToolExecutionError> {
        let pattern = args.pattern.trim();
        if pattern.is_empty() {
            return Err(grep_error(
                "grep: 'pattern' is required (regex, e.g. \"fn main\" or \"TODO|FIXME\")",
            ));
        }
        let max_results = args
            .max_results
            .unwrap_or(DEFAULT_MAX_RESULTS as u64)
            .clamp(1, MAX_RESULTS_CAP as u64) as usize;

        let matcher = RegexMatcherBuilder::new()
            .case_insensitive(!args.case_sensitive.unwrap_or(false))
            .build(pattern)
            .map_err(|error| grep_error(format!("grep: invalid regex {pattern:?}: {error}")))?;

        let context_before = args.context_before.unwrap_or(0) as usize;
        let context_after = args.context_after.unwrap_or(0) as usize;

        let globs = args.glob.unwrap_or_default();
        let (neg_globs, pos_globs): (Vec<&str>, Vec<&str>) = globs
            .iter()
            .map(String::as_str)
            .partition(|glob| glob.starts_with('!'));
        let pos_matchers: Vec<globset::GlobMatcher> = pos_globs
            .iter()
            .filter_map(|glob| {
                globset::GlobBuilder::new(glob)
                    .literal_separator(true)
                    .build()
                    .ok()
            })
            .map(|glob| glob.compile_matcher())
            .collect();
        let neg_matchers: Vec<globset::GlobMatcher> = neg_globs
            .iter()
            .filter_map(|glob| {
                globset::GlobBuilder::new(glob.strip_prefix('!').unwrap_or(glob))
                    .literal_separator(true)
                    .build()
                    .ok()
            })
            .map(|glob| glob.compile_matcher())
            .collect();
        let glob_filter = |rel: &str| -> bool {
            // rg -g 是 gitignore 语义：不含 `/` 的模式（如 "*.rs"）匹配任意层级
            // 的 basename；含 `/` 的模式匹配相对路径。
            let basename = rel.rsplit('/').next().unwrap_or(rel);
            let pos_hit = pos_matchers.is_empty()
                || pos_matchers
                    .iter()
                    .any(|matcher| matcher.is_match(rel) || matcher.is_match(basename));
            if !pos_hit {
                return false;
            }
            !neg_matchers
                .iter()
                .any(|matcher| matcher.is_match(rel) || matcher.is_match(basename))
        };

        let ws_path = if ctx.workspace_root.as_os_str().is_empty() {
            PathBuf::from(".")
        } else {
            ctx.workspace_root.clone()
        };
        let strip_verbatim = |path: &std::path::Path| -> PathBuf {
            let string = path.to_string_lossy();
            let string = string.strip_prefix(r"\\?\").unwrap_or(&string);
            PathBuf::from(string)
        };
        let ws_abs = ws_path.canonicalize().unwrap_or_else(|_| ws_path.clone());
        let ws_abs = strip_verbatim(&ws_abs);

        let raw_paths = args
            .paths
            .unwrap_or_default()
            .into_iter()
            .filter(|path| !path.is_empty())
            .collect::<Vec<_>>();
        let mut roots: Vec<PathBuf> = Vec::new();
        if raw_paths.is_empty() {
            roots.push(ws_abs.clone());
        } else {
            for raw_path in &raw_paths {
                let path = std::path::Path::new(raw_path);
                let resolved = if path.is_absolute() {
                    crate::permission::normalize_lexically(path)
                } else {
                    crate::permission::normalize_lexically(&ws_path.join(path))
                };
                let abs = std::path::absolute(&resolved).unwrap_or(resolved);
                if !path_within_workspace(&abs, &ws_abs) {
                    return Err(grep_error(format!(
                        "grep: path {raw_path:?} resolves outside the workspace — search is workspace-bounded"
                    )));
                }
                roots.push(abs);
            }
        }

        let mut searcher = SearcherBuilder::new()
            .line_number(true)
            .after_context(context_after)
            .before_context(context_before)
            .binary_detection(BinaryDetection::quit(b'\x00'))
            .build();

        let mut all_matches: Vec<MatchLine> = Vec::new();
        let mut truncated = false;

        'roots: for root in &roots {
            let walker = ignore::WalkBuilder::new(root)
                .standard_filters(true)
                .require_git(false)
                .build();
            for entry in walker.flatten() {
                if !entry
                    .file_type()
                    .is_some_and(|file_type| file_type.is_file())
                {
                    continue;
                }
                let path = entry.path();
                let rel = path
                    .strip_prefix(&ws_abs)
                    .unwrap_or(path)
                    .to_string_lossy()
                    .replace('\\', "/");
                if !glob_filter(&rel) {
                    continue;
                }
                // max_results 是**全局**预算：先看已收集数，再给当前文件剩余配额。
                let collected = all_matches.iter().filter(|m| !m.is_context).count();
                if collected >= max_results {
                    truncated = true;
                    break 'roots;
                }
                let mut sink = CollectSink {
                    path: rel.clone(),
                    abs_path: path.to_string_lossy().into_owned(),
                    matches: Vec::new(),
                    max_matches: max_results - collected,
                    truncated: false,
                };
                // 忽略单个文件的 IO 错误（rg 行为：不可读文件跳过）。
                let _ = searcher.search_path(&matcher, path, &mut sink);
                if sink.truncated {
                    truncated = true;
                }
                all_matches.extend(sink.matches);
                if truncated {
                    break 'roots;
                }
            }
        }

        let match_count = all_matches.iter().filter(|m| !m.is_context).count();
        let matches = all_matches
            .iter()
            .filter(|m| !m.is_context)
            .map(|m| GrepMatch {
                path: m.path.clone(),
                line: m.line,
                content: m.content.clone(),
            })
            .collect();
        Ok(GrepOutput {
            status: "ok".to_string(),
            matches,
            truncated,
            count: match_count,
            lines: all_matches,
        })
    }
}

fn grep_error(message: impl Into<String>) -> ToolExecutionError {
    let mut error = ToolError::new(ToolErrorKind::Execution, message);
    error.code = ToolErrorCode::from_legacy("TOOL_ERROR");
    ToolExecutionError::Recoverable(error)
}

fn grep_schema() -> Value {
    serde_json::json!({
        "type": "object",
        "properties": {
            "pattern": {"type": "string", "description": "Regex (rg syntax)"},
            "paths": {"type": "array", "items": {"type": "string"}, "description": "Dirs to search"},
            "glob": {"type": "array", "items": {"type": "string"}, "description": "File filters (rg -g)"},
            "case_sensitive": {"type": "boolean", "default": false, "description": "Case-sensitive (default false)"},
            "context_before": {"type": "integer", "minimum": 0, "description": "Context before"},
            "context_after": {"type": "integer", "minimum": 0, "description": "Context after"},
            "max_results": {"type": "integer", "minimum": 1, "maximum": 2000, "description": "Max results (default 200)"}
        },
        "required": ["pattern"],
        "additionalProperties": false
    })
}

/// Sink：按 searcher 的回调顺序收集匹配行与上下文行（顺序天然正确）。
/// 达到 max_results 条匹配后返回 `Ok(false)` 停止当前文件。
struct CollectSink {
    path: String,
    /// 绝对路径（与 read 的账本 key 同形态，record_grep 清链用）。
    abs_path: String,
    matches: Vec<MatchLine>,
    max_matches: usize,
    truncated: bool,
}

impl Sink for CollectSink {
    type Error = std::io::Error;

    fn matched(&mut self, _searcher: &Searcher, mat: &SinkMatch<'_>) -> Result<bool, Self::Error> {
        if self.matches.iter().filter(|m| !m.is_context).count() >= self.max_matches {
            self.truncated = true;
            return Ok(false);
        }
        // grep 输出的是实时行号：清空该文件的偏移链，防止后续 read 用
        // grep 行号时被账本修正误伤（read→edit→grep→read 链）。
        crate::file_state::record_grep(&self.abs_path);
        let content = mat
            .lines()
            .next()
            .map(|l| String::from_utf8_lossy(l).into_owned())
            .unwrap_or_default();
        let content = content.trim_end_matches(['\n', '\r']).to_string();
        self.matches.push(MatchLine {
            path: self.path.clone(),
            line: mat.line_number().unwrap_or(0),
            content,
            is_context: false,
        });
        Ok(true)
    }

    fn context(
        &mut self,
        _searcher: &Searcher,
        context: &SinkContext<'_>,
    ) -> Result<bool, Self::Error> {
        // 上下文行跟随在匹配行之后输出（rg 的 -A/-B 语义）。
        if self.truncated {
            return Ok(false);
        }
        let content = String::from_utf8_lossy(context.bytes())
            .trim_end_matches(['\n', '\r'])
            .to_string();
        self.matches.push(MatchLine {
            path: self.path.clone(),
            line: context.line_number().unwrap_or(0),
            content,
            is_context: true,
        });
        Ok(true)
    }
}

/// Lexically resolve `.`/`..` components without touching the filesystem.
fn lexically_normalize(p: &std::path::Path) -> std::path::PathBuf {
    use std::path::Component;
    let mut out = std::path::PathBuf::new();
    for comp in p.components() {
        match comp {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    out.push(Component::ParentDir.as_os_str());
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    out
}

/// Component-wise prefix comparison (case-insensitive on Windows), with `..`
/// lexically resolved first — prevents `ws/../../..` from slipping past a
/// plain `Path::starts_with` (which compares component prefixes only).
fn path_within_workspace(abs: &std::path::Path, ws: &std::path::Path) -> bool {
    let a_norm = lexically_normalize(abs);
    let a: Vec<_> = a_norm.components().collect();
    let w_norm = lexically_normalize(ws);
    let w: Vec<_> = w_norm.components().collect();
    if a.len() < w.len() {
        return false;
    }
    a[..w.len()].iter().zip(w.iter()).all(|(x, y)| {
        #[cfg(windows)]
        {
            x.as_os_str().to_string_lossy().to_lowercase()
                == y.as_os_str().to_string_lossy().to_lowercase()
        }
        #[cfg(not(windows))]
        {
            x == y
        }
    })
}

pub fn register(mgr: &mut crate::ToolManager) {
    mgr.register_typed(GrepTool);
}

// ── Tests ──

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn setup(files: &[(&str, &str)]) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        for (name, content) in files {
            let path = dir.path().join(name);
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(path, content).unwrap();
        }
        let root = dir.path().to_path_buf();
        (dir, root)
    }

    fn ctx(root: &Path) -> ToolCallContext {
        ToolCallContext {
            call_id: "grep-test".to_string(),
            session_id: "grep-test-session".to_string(),
            workspace_root: root.to_path_buf(),
            mode: crate::tool_api::AgentMode::Code,
            permission_level: crate::permission::PermissionLevel::ReadFree,
            sandbox: crate::tool_api::SandboxMode::Main,
            sandbox_spec: crate::tool_api::SandboxSpec::workspace_write(root.to_path_buf()),
            exec_default_shell: None,
            timeout: Duration::from_secs(60),
            cancellation: crate::tool_api::CancellationToken::new(),
            progress: None,
            source: crate::tool_api::ToolCallSource::Model,
        }
    }

    fn parse_args(args: serde_json::Value) -> GrepArgs {
        serde_json::from_value(args).expect("valid grep args")
    }

    #[allow(clippy::result_large_err)] // ToolExecutionError is the frozen typed boundary.
    fn run(root: &Path, args: serde_json::Value) -> Result<GrepOutput, ToolExecutionError> {
        GrepTool.run(&ctx(root), parse_args(args))
    }

    fn error_code(error: ToolExecutionError) -> String {
        match error {
            ToolExecutionError::Recoverable(error) => error.code.as_str().to_string(),
            ToolExecutionError::Fatal(error) => error.code,
        }
    }

    #[test]
    fn basic_search_returns_path_line_content() {
        let (_dir, root) = setup(&[
            ("src/a.rs", "fn alpha() {}\nlet x = 1;\n"),
            ("src/b.rs", "fn beta() {}\n"),
            ("README.md", "no code here\n"),
        ]);
        let output = run(&root, serde_json::json!({ "pattern": "fn \\w+" })).expect("grep");
        assert_eq!(output.status, "ok");
        assert_eq!(output.count, 2);
        assert_eq!(output.matches[0].line, 1);
        assert!(output.matches[0].content.contains("fn "));
    }

    #[test]
    fn case_insensitive_by_default_sensitive_when_requested() {
        let (_dir, root) = setup(&[("f.txt", "Hello\nworld\n")]);
        let insensitive = run(&root, serde_json::json!({ "pattern": "hello" })).expect("grep");
        assert_eq!(insensitive.count, 1);
        let sensitive = run(
            &root,
            serde_json::json!({ "pattern": "hello", "case_sensitive": true }),
        )
        .expect("grep");
        assert_eq!(sensitive.count, 0);
    }

    #[test]
    fn glob_filters_files() {
        let (_dir, root) = setup(&[("src/a.rs", "target\n"), ("src/a.md", "target\n")]);
        let output = run(
            &root,
            serde_json::json!({ "pattern": "target", "glob": ["*.rs"] }),
        )
        .expect("grep");
        assert_eq!(output.count, 1);
        assert!(output.matches[0].path.ends_with("a.rs"));
    }

    #[test]
    fn max_results_truncates_with_marker() {
        let files: Vec<(String, String)> = (0..20)
            .map(|i| (format!("f{i:02}.txt"), format!("hit line {i}\n")))
            .collect();
        let file_refs: Vec<(&str, &str)> = files
            .iter()
            .map(|(name, content)| (name.as_str(), content.as_str()))
            .collect();
        let (_dir, root) = setup(&file_refs);
        let output = run(
            &root,
            serde_json::json!({ "pattern": "hit", "max_results": 5 }),
        )
        .expect("grep");
        assert_eq!(output.count, 5);
        assert!(output.truncated);
        assert!(output.model_text().contains("truncated at 5 matches"));
    }

    #[test]
    fn no_match_is_ok_with_zero_count() {
        let (_dir, root) = setup(&[("f.txt", "abc\n")]);
        let output = run(&root, serde_json::json!({ "pattern": "zzz" })).expect("grep");
        assert_eq!(output.status, "ok");
        assert_eq!(output.count, 0);
        assert_eq!(output.model_text(), "(no matches)\n");
    }

    #[test]
    fn invalid_regex_preserves_legacy_error_code() {
        let (_dir, root) = setup(&[("f.txt", "abc\n")]);
        let error = run(&root, serde_json::json!({ "pattern": "(" })).expect_err("invalid regex");
        assert_eq!(error_code(error), "TOOL_ERROR");

        let missing = run(&root, serde_json::json!({})).expect_err("missing pattern");
        assert_eq!(error_code(missing), "TOOL_ERROR");
    }

    #[test]
    fn context_lines_are_in_model_text_but_not_canonical_matches() {
        let (_dir, root) = setup(&[("f.txt", "a\nTARGET\nc\n")]);
        let output = run(
            &root,
            serde_json::json!({ "pattern": "TARGET", "context_before": 1, "context_after": 1 }),
        )
        .expect("grep");
        assert_eq!(output.count, 1);
        assert_eq!(output.matches.len(), 1);
        assert_eq!(output.matches[0].line, 2);
        let text = output.model_text();
        assert!(text.contains("f.txt-1-a\n"), "got: {text}");
        assert!(text.contains("f.txt:2:TARGET\n"), "got: {text}");
        assert!(text.contains("f.txt-3-c\n"), "got: {text}");
    }

    #[test]
    fn path_outside_workspace_rejected_with_legacy_code() {
        let (_dir, root) = setup(&[("f.txt", "abc\n")]);
        let error = run(
            &root,
            serde_json::json!({ "pattern": "abc", "paths": ["../../.."] }),
        )
        .expect_err("outside path rejected");
        assert_eq!(error_code(error), "TOOL_ERROR");
    }

    #[test]
    fn gitignored_files_are_skipped() {
        let (_dir, root) = setup(&[
            ("keep.txt", "needle\n"),
            (".gitignore", "skip.txt\n"),
            ("skip.txt", "needle\n"),
        ]);
        let output = run(&root, serde_json::json!({ "pattern": "needle" })).expect("grep");
        assert_eq!(output.count, 1);
        assert!(output.matches[0].path.contains("keep.txt"));
    }

    #[test]
    fn grep_registration_is_typed_and_descriptor_keeps_legacy_schema() {
        let mut manager = crate::ToolManager::new();
        register(&mut manager);
        let registered = manager.builtins.get("grep").expect("grep registered");
        assert!(
            registered.legacy.is_none(),
            "grep must not use legacy executor"
        );
        assert_eq!(
            registered.descriptor.input_schema["required"],
            serde_json::json!(["pattern"])
        );
        assert_eq!(
            registered.descriptor.input_schema["additionalProperties"],
            serde_json::json!(false)
        );
        assert_eq!(registered.descriptor.output_schema["type"], "object");
    }

    #[test]
    fn grep_typed_output_and_display_share_the_same_text() {
        let output = GrepOutput {
            status: "ok".to_string(),
            matches: vec![GrepMatch {
                path: "src/a.rs".to_string(),
                line: 7,
                content: "TODO".to_string(),
            }],
            truncated: false,
            count: 1,
            lines: vec![MatchLine {
                path: "src/a.rs".to_string(),
                line: 7,
                content: "TODO".to_string(),
                is_context: false,
            }],
        };
        let model = match output.model_blocks().into_iter().next() {
            Some(ToolContentBlock::Text { text }) => text,
            _ => panic!("grep output must have a text model block"),
        };
        assert_eq!(model, "src/a.rs:7:TODO\n");
        let display = output.display(&serde_json::json!({
            "pattern": "TODO",
            "paths": ["src"]
        }));
        assert_eq!(display.summary.as_deref(), Some("1 match · 1 file"));
        assert_ne!(
            display.summary.as_deref(),
            Some(model.lines().next().unwrap_or_default()),
            "grep summary must be metadata, not the body's first line"
        );
        assert_eq!(
            display.header,
            ToolHeader::Query {
                query: "TODO".to_string(),
                scope: Some("src".to_string()),
            }
        );
        assert_eq!(
            display.body,
            ToolBody::Text {
                text: model,
                truncated: false,
            }
        );
    }
}
