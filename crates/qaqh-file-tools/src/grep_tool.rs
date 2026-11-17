//! `grep`：内容搜索工具——直接使用 ripgrep 的核心库
//! （`grep-searcher` + `grep-regex` + `ignore` **并行 walker**，与 rg 二进制
//! 同一实现姿态），**零外部二进制依赖**：用户机器上没有 rg 也能用。
//!
//! 与 `glob`（文件名列举）分工：grep 搜**文件内容**，输出 `path:line:content`
//! （上下文行用 rg 的 `path-line-content` 格式）。与 `exec` 分工：grep 是
//! 模型友好的封装——workspace 边界内、结果截断、结构化输出。
//!
//! - 正则语法 = rg（Rust regex 引擎；`(?i)`、`\b`、`|` 等均可用）。
//! - 默认大小写不敏感（对齐 Claude Code Grep）；`case_sensitive=true` 关闭。
//! - 默认跳过 hidden/gitignored/binary 文件（rg 原生行为）。
//! - `max_results` 截断 + `truncated` 标记，防上下文爆炸。
//! - `ctx.timeout`/cancellation 是每个 walk 条目都消费的硬护栏；非法 `-g`
//!   显式拒绝而非静默吞掉（2026-11-17 假阴性复盘）。

use grep_regex::RegexMatcherBuilder;
use grep_searcher::{BinaryDetection, Searcher, SearcherBuilder, Sink, SinkContext, SinkMatch};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeSet;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::ToolRisk;
use crate::tool_api::{
    ToolBody, ToolCallContext, ToolContentBlock, ToolDisplay, ToolError, ToolErrorKind,
    ToolExecutionError, ToolHeader, ToolMeta, ToolProjection, TypedTool,
};
use crate::walk_limit::{WalkLimit, WalkStop};

const DEFAULT_MAX_RESULTS: usize = 200;
const MAX_RESULTS_CAP: usize = 2_000;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GrepArgs {
    /// ripgrep regex.
    #[serde(default)]
    pub pattern: String,
    /// Dirs to search (`path` / `file` / `dir` are accepted as aliases; a bare
    /// string means a single dir).
    #[serde(
        default,
        alias = "path",
        alias = "file",
        alias = "dir",
        deserialize_with = "crate::tool_api::lenient::opt_one_or_many_strings"
    )]
    pub paths: Option<Vec<String>>,
    /// ripgrep --glob filters (a bare string means a single filter).
    #[serde(
        default,
        deserialize_with = "crate::tool_api::lenient::opt_one_or_many_strings"
    )]
    pub glob: Option<Vec<String>>,
    /// Default false.
    #[serde(default)]
    pub case_sensitive: Option<bool>,
    /// Context lines before each match.
    #[serde(default)]
    pub context_before: Option<u64>,
    /// Context lines after each match.
    #[serde(default)]
    pub context_after: Option<u64>,
    /// Default 200.
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

#[derive(Debug, Default, Serialize, Deserialize, JsonSchema)]
pub struct GrepOutput {
    pub status: String,
    pub matches: Vec<GrepMatch>,
    pub truncated: bool,
    pub count: usize,
    #[serde(skip)]
    #[schemars(skip)]
    lines: Vec<MatchLine>,
    /// walk 因 tool deadline 提前停走——结果不完整。非 wire（skip）。
    #[serde(skip)]
    #[schemars(skip)]
    pub deadline_hit: bool,
    /// walk 因调用被取消而提前停走。非 wire（skip）。
    #[serde(skip)]
    #[schemars(skip)]
    pub cancelled_hit: bool,
    /// walk 扫过的可见文件数（skip；区分"无内容匹配"与"被 -g 筛空"）。
    #[serde(skip)]
    #[schemars(skip)]
    pub files_scanned: usize,
    /// 通过过滤器、实际进入内容搜索的文件数。
    #[serde(skip)]
    #[schemars(skip)]
    pub files_searched: usize,
}

impl GrepOutput {
    fn stop_note(&self) -> Option<&'static str> {
        if self.cancelled_hit {
            Some("... stopped: call cancelled — results are INCOMPLETE")
        } else if self.deadline_hit {
            Some(
                "... stopped: walk deadline exceeded — results are INCOMPLETE (narrow `paths`/`glob`, or use exec rg on huge unignored trees)",
            )
        } else {
            None
        }
    }

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
        if let Some(note) = self.stop_note() {
            if !text.is_empty() {
                text.push('\n');
            }
            text.push_str(note);
            text.push('\n');
        } else if text.is_empty() {
            // 2026-11-17 假阴性复盘：必须区分"真的无匹配"与"-g 过滤器把可见
            // 文件全筛没了"——后者谎报 no matches 会把模型引向"代码不存在"。
            text = if self.files_searched == 0 && self.files_scanned > 0 {
                format!(
                    "(no matches — the glob filter(s) excluded all {} files scanned; drop or widen `glob`)\n",
                    self.files_scanned
                )
            } else {
                "(no matches)\n".to_string()
            };
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
        if self.stop_note().is_some() {
            summary.push_str(" · incomplete");
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

    fn meta(&self) -> ToolMeta {
        ToolMeta::new(
            "grep",
            "Search file contents with ripgrep regex. Returns path:line:content; filter files with glob.",
            crate::permission::ToolCategory::Read,
            ToolRisk::ReadOnly,
            Duration::from_secs(60),
        )
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

        let globs: Vec<String> = args
            .glob
            .unwrap_or_default()
            .into_iter()
            .map(|glob| glob.trim().to_string())
            .filter(|glob| !glob.is_empty())
            .collect();
        // 2026-11-17 复盘：内嵌换行的 `-g` pattern 若放行编译，会静默把整个
        // 搜索空间筛空并谎报 "(no matches)"——假阴性。显式拒绝；纯外围空白
        // 由上面的 trim 无害修复。
        for glob in &globs {
            let bare = glob.strip_prefix('!').unwrap_or(glob);
            if bare.is_empty() {
                return Err(grep_error(format!("grep: empty glob filter {glob:?}")));
            }
            if bare.contains(['\n', '\r']) {
                return Err(grep_error(format!(
                    "grep: invalid glob {glob:?}: embedded newline; pass filters like \"*.rs\""
                )));
            }
        }
        let (neg_globs, pos_globs): (Vec<&str>, Vec<&str>) = globs
            .iter()
            .map(String::as_str)
            .partition(|glob| glob.starts_with('!'));
        let compile_glob = |glob: &str| -> Result<globset::GlobMatcher, ToolExecutionError> {
            let bare = glob.strip_prefix('!').unwrap_or(glob);
            globset::GlobBuilder::new(bare)
                .literal_separator(true)
                .build()
                .map_err(|error| grep_error(format!("grep: invalid glob {glob:?}: {error}")))
                .map(|compiled| compiled.compile_matcher())
        };
        let mut pos_matchers: Vec<globset::GlobMatcher> = Vec::new();
        for glob in &pos_globs {
            pos_matchers.push(compile_glob(glob)?);
        }
        let mut neg_matchers: Vec<globset::GlobMatcher> = Vec::new();
        for glob in &neg_globs {
            neg_matchers.push(compile_glob(glob)?);
        }
        let glob_filter: Arc<dyn Fn(&str) -> bool + Send + Sync> = Arc::new(
            move |rel: &str| -> bool {
                // rg -g 是 gitignore 语义：不含 `/` 的模式（如 "*.rs"）匹配任意
                // 层级的 basename；含 `/` 的模式匹配相对路径。
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
            },
        );

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
                // 2026-10-05 读自由规则：grep 是读取工具，路径完全不受工作区
                // 限制（与 read/glob 同一姿态）——工作区内路径默认根语义保留，
                // 工作区外绝对/相对路径直接搜索，不再硬拒。
                roots.push(abs);
            }
        }

        let matcher = Arc::new(matcher);
        let ws_abs = Arc::new(ws_abs);
        let limit = WalkLimit::from_ctx(ctx);
        let state = Arc::new(SearchState::new(max_results));

        'roots: for root in &roots {
            // rg 默认并行遍历；单线程在 16k 条目的 target/ 上要 10s+。
            let walker = ignore::WalkBuilder::new(root.clone())
                .standard_filters(true)
                .require_git(false)
                .build_parallel();
            walker.run(|| {
                let matcher = Arc::clone(&matcher);
                let glob_filter = Arc::clone(&glob_filter);
                let ws_abs = Arc::clone(&ws_abs);
                let state = Arc::clone(&state);
                let limit = limit.clone();
                let mut searcher = SearcherBuilder::new()
                    .line_number(true)
                    .after_context(context_after)
                    .before_context(context_before)
                    .binary_detection(BinaryDetection::quit(b'\x00'))
                    .build();
                Box::new(move |entry| {
                    match limit.stopped() {
                        Some(WalkStop::Cancelled) => {
                            state.cancelled_hit.store(true, Ordering::Relaxed);
                            return ignore::WalkState::Quit;
                        }
                        Some(WalkStop::Deadline) => {
                            state.deadline_hit.store(true, Ordering::Relaxed);
                            return ignore::WalkState::Quit;
                        }
                        None => {}
                    }
                    let Ok(entry) = entry else {
                        return ignore::WalkState::Continue;
                    };
                    if !entry
                        .file_type()
                        .is_some_and(|file_type| file_type.is_file())
                    {
                        return ignore::WalkState::Continue;
                    }
                    state.scanned.fetch_add(1, Ordering::Relaxed);
                    let path = entry.path();
                    let rel = path
                        .strip_prefix(ws_abs.as_path())
                        .unwrap_or(path)
                        .to_string_lossy()
                        .replace('\\', "/");
                    if !glob_filter(&rel) {
                        return ignore::WalkState::Continue;
                    }
                    // max_results 是**全局**预算：先看已收集数，再给当前文件剩余配额。
                    if state.counted.load(Ordering::Acquire) >= state.max {
                        state.truncated.store(true, Ordering::Relaxed);
                        return ignore::WalkState::Quit;
                    }
                    state.searched.fetch_add(1, Ordering::Relaxed);
                    let mut sink = CollectSink {
                        path: rel,
                        abs_path: path.to_string_lossy().into_owned(),
                        matches: Vec::new(),
                        max_matches: state
                            .max
                            .saturating_sub(state.counted.load(Ordering::Relaxed)),
                        truncated: false,
                    };
                    // 忽略单个文件的 IO 错误（rg 行为：不可读文件跳过）。
                    let _ = searcher.search_path(matcher.as_ref(), path, &mut sink);
                    if sink.truncated {
                        state.truncated.store(true, Ordering::Relaxed);
                    }
                    if !sink.matches.is_empty() {
                        let fresh = sink.matches.iter().filter(|m| !m.is_context).count();
                        let total = {
                            let mut lines =
                                state.lines.lock().unwrap_or_else(|e| e.into_inner());
                            lines.extend(sink.matches.drain(..));
                            state.counted.fetch_add(fresh, Ordering::AcqRel) + fresh
                        };
                        if total >= state.max {
                            state.truncated.store(true, Ordering::Relaxed);
                            return ignore::WalkState::Quit;
                        }
                    }
                    ignore::WalkState::Continue
                })
            });
            if state.truncated.load(Ordering::Acquire)
                || state.deadline_hit.load(Ordering::Relaxed)
                || state.cancelled_hit.load(Ordering::Relaxed)
            {
                break 'roots;
            }
        }

        // 并行收集没有确定遍历序：全局按 (path, line) 排序（比 rg 的并发序
        // 更确定），再按全局预算裁剪——多 worker 竞态下可能轻微超收。
        let mut all_matches =
            std::mem::take(&mut *state.lines.lock().unwrap_or_else(|e| e.into_inner()));
        all_matches.sort_by(|a, b| {
            (&a.path, a.line, a.is_context).cmp(&(&b.path, b.line, b.is_context))
        });
        let mut kept: Vec<MatchLine> = Vec::new();
        let mut match_count = 0usize;
        let mut trimmed = false;
        for line in all_matches {
            if !line.is_context {
                if match_count >= max_results {
                    trimmed = true;
                    break;
                }
                match_count += 1;
            }
            kept.push(line);
        }
        let truncated = trimmed || state.truncated.load(Ordering::Relaxed);
        let matches = kept
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
            lines: kept,
            deadline_hit: state.deadline_hit.load(Ordering::Relaxed),
            cancelled_hit: state.cancelled_hit.load(Ordering::Relaxed),
            files_scanned: state.scanned.load(Ordering::Relaxed),
            files_searched: state.searched.load(Ordering::Relaxed),
        })
    }
}

fn grep_error(message: impl Into<String>) -> ToolExecutionError {
    ToolExecutionError::Recoverable(ToolError::new(ToolErrorKind::Execution, message))
}

/// 并行 walker 的共享状态：全局预算、收集、过滤器命中计数与停走标记。
struct SearchState {
    lines: Mutex<Vec<MatchLine>>,
    /// 已收集的非上下文匹配行数（全局预算）。
    counted: AtomicUsize,
    truncated: AtomicBool,
    /// 扫过的可见文件数（过滤前）。
    scanned: AtomicUsize,
    /// 实际进入 searcher 的文件数（过滤后）——用于区分 `(no matches)`
    /// 与 `glob` 筛空全部文件。
    searched: AtomicUsize,
    deadline_hit: AtomicBool,
    cancelled_hit: AtomicBool,
    max: usize,
}

impl SearchState {
    fn new(max: usize) -> Self {
        Self {
            lines: Mutex::new(Vec::new()),
            counted: AtomicUsize::new(0),
            truncated: AtomicBool::new(false),
            scanned: AtomicUsize::new(0),
            searched: AtomicUsize::new(0),
            deadline_hit: AtomicBool::new(false),
            cancelled_hit: AtomicBool::new(false),
            max,
        }
    }
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

pub fn register(mgr: &mut impl qaqh_tool_core::tool_api::RegistersTyped) {
    mgr.register_typed_tool(GrepTool);
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
            permission_level: crate::permission::PermissionLevel::ReadOnly,
            sandbox: crate::tool_api::SandboxMode::Main,
            sandbox_spec: crate::tool_api::SandboxSpec::workspace_write(root.to_path_buf()),
            exec_default_shell: None,
            timeout: Duration::from_secs(60),
            cancellation: crate::tool_api::CancellationToken::new(),
            progress: None,
            exec_progress: None,
            exec_host: None,
            official_web: None,
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

    /// 模型先验里的签名是 `grep(pattern, path)`（Claude 标准 schema），而我们
    /// 的正式名是 `paths`。别名必须让这类调用**一次成功**，否则每次都要多一轮。
    #[test]
    fn claude_style_path_argument_is_accepted() {
        let (_dir, root) = setup(&[("src/a.rs", "fn alpha() {}\n"), ("b.rs", "fn beta() {}\n")]);
        let single = run(
            &root,
            serde_json::json!({ "pattern": "fn ", "path": "src" }),
        )
        .expect("path 别名");
        assert_eq!(single.count, 1, "裸串 path 应视作单元素 paths");
        assert!(single.matches[0].path.ends_with("a.rs"));

        let many = run(
            &root,
            serde_json::json!({ "pattern": "fn ", "paths": ["src", "b.rs"] }),
        )
        .expect("paths 数组");
        assert_eq!(many.count, 2);
    }

    /// `glob` 同样吃裸串（`"*.rs"` ≡ `["*.rs"]`）；逗号分隔不展平。
    #[test]
    fn bare_glob_string_is_a_single_filter() {
        let (_dir, root) = setup(&[("a.rs", "target\n"), ("a.md", "target\n")]);
        let output = run(
            &root,
            serde_json::json!({ "pattern": "target", "glob": "*.rs" }),
        )
        .expect("glob 裸串");
        assert_eq!(output.count, 1);
        assert!(output.matches[0].path.ends_with("a.rs"));
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
        assert_eq!(error_code(error), "execution");

        let missing = run(&root, serde_json::json!({})).expect_err("missing pattern");
        assert_eq!(error_code(missing), "execution");
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
    fn path_outside_workspace_is_searched_read_free() {
        // 2026-10-05 读自由规则：grep 路径完全不受工作区限制——工作区外的
        // 绝对路径照常搜索并命中（与 read/glob 同一姿态）。
        let dir = tempfile::tempdir().expect("tempdir");
        let ws = dir.path().join("ws");
        let outside = dir.path().join("outside");
        std::fs::create_dir_all(&ws).unwrap();
        std::fs::create_dir_all(&outside).unwrap();
        std::fs::write(outside.join("needle.txt"), "abc\n").unwrap();

        let output = run(
            &ws,
            serde_json::json!({
                "pattern": "abc",
                "paths": [outside.join("needle.txt").to_string_lossy()],
            }),
        )
        .expect("outside-path grep must succeed (reads are workspace-free)");
        assert_eq!(output.count, 1);
        assert!(output.matches[0].path.contains("needle.txt"));
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
    fn grep_registration_is_typed_and_schema_is_type_generated() {
        let mut manager = qaqh_workspace::ToolManager::new();
        register(&mut manager);
        let registered = manager.builtin("grep").expect("grep registered");
        assert_eq!(
            registered.descriptor.name.as_str(),
            "grep",
            "grep must be on the typed execution surface"
        );
        // v2：schema 由 GrepArgs 类型生成——pattern 带 serde(default)，不再是 required。
        assert!(
            registered.descriptor.input_schema["properties"]["pattern"].is_object(),
            "generated input schema must carry pattern"
        );
        assert!(
            registered.descriptor.input_schema["properties"]["paths"].is_object(),
            "generated input schema must carry paths"
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
            ..Default::default()
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

    /// 内嵌换行/未闭合的 `-g` pattern 必须显式拒绝——静默吞掉会把整个搜索
    /// 空间筛空并谎报 no matches（2026-11-17 假阴性复盘）。外围空白 trim 修复。
    #[test]
    fn invalid_glob_filter_is_rejected_not_silently_swallowed() {
        let (_dir, root) = setup(&[("a.rs", "target\n")]);
        let error = run(
            &root,
            serde_json::json!({ "pattern": "target", "glob": "src\n*.rs" }),
        )
        .expect_err("embedded-newline glob must error");
        assert_eq!(error_code(error), "execution");

        let error = run(
            &root,
            serde_json::json!({ "pattern": "target", "glob": ["["] }),
        )
        .expect_err("unclosed glob must error");
        assert_eq!(error_code(error), "execution");

        let output = run(
            &root,
            serde_json::json!({ "pattern": "target", "glob": "\n\n*.rs\n\n" }),
        )
        .expect("outer whitespace is trimmed, glob stays valid");
        assert_eq!(output.count, 1);
    }

    /// "过滤器筛空一切" 与 "真的无匹配" 必须可区分。
    #[test]
    fn glob_excluding_every_file_says_so_instead_of_no_matches() {
        let (_dir, root) = setup(&[("a.rs", "target\n"), ("b.md", "target\n")]);
        let output = run(
            &root,
            serde_json::json!({ "pattern": "target", "glob": ["nope/*.rs"] }),
        )
        .expect("grep");
        assert_eq!(output.count, 0);
        let text = output.model_text();
        assert!(text.contains("glob filter(s) excluded"), "got: {text}");
    }

    /// deadline/取消必须掐停并行 walker，返回部分结果 + INCOMPLETE 标记。
    #[test]
    fn grep_deadline_and_cancel_stop_walk_with_marker() {
        let (_dir, root) = setup(&[("a.rs", "target\n"), ("b.rs", "target\n")]);

        let mut tool_ctx = ctx(root.as_path());
        tool_ctx.timeout = Duration::from_nanos(1);
        let output = GrepTool
            .run(
                &tool_ctx,
                parse_args(serde_json::json!({ "pattern": "target" })),
            )
            .expect("deadline stop returns partial results, not an error");
        assert!(output.deadline_hit, "1ns deadline must trip");
        assert!(output.model_text().contains("deadline"));

        let tool_ctx = ctx(root.as_path());
        tool_ctx.cancellation.cancel();
        let output = GrepTool
            .run(
                &tool_ctx,
                parse_args(serde_json::json!({ "pattern": "target" })),
            )
            .expect("cancelled stop returns partial results, not an error");
        assert!(output.cancelled_hit);
        assert!(output.model_text().contains("cancelled"));
    }
}
