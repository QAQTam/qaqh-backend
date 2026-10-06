//! Native glob tool: workspace-rooted path listing with gitignore-style patterns.
//!
//! Pure in-process implementation (`globset` matcher + `ignore::WalkBuilder`
//! traversal) — **no shell, no external binaries**. Sandbox-safe: `ReadOnly`
//! risk, workspace-rooted, hidden/gitignored paths skipped by default.
//!
//! Syntax is identical to `rg -g` / gitignore / VS Code file search:
//! - `*` matches within one path segment (does NOT cross `/`)
//! - `**` matches across any number of directories
//! - `?` matches a single character
//! - `[a-z]` / `[abc]` character classes
//! - `{a,b}` alternation

use std::path::{Path, PathBuf};
use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::ToolRisk;
use crate::tool_api::{
    ToolBody, ToolCallContext, ToolContentBlock, ToolDisplay, ToolError, ToolErrorKind,
    ToolExecutionError, ToolHeader, ToolMeta, ToolProjection, TypedTool,
};

/// 默认返回上限：防超大仓库结果爆炸（`rg --files` 语义下的熔断）。
const DEFAULT_MAX_RESULTS: usize = 500;
const MAX_RESULTS_CAP: usize = 10_000;

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct GlobArgs {
    /// Glob pattern
    #[serde(default)]
    pub pattern: String,
    /// Search root
    #[serde(default)]
    pub path: Option<String>,
    /// Max results (default 500)
    #[serde(default)]
    pub max_results: Option<u64>,
}

#[derive(Debug, Serialize, Deserialize, JsonSchema)]
pub struct GlobOutput {
    pub matches: Vec<String>,
    pub truncated: bool,
    pub count: usize,
}

impl GlobOutput {
    fn model_text(&self) -> String {
        let mut text = self.matches.join("\n");
        if self.truncated {
            text.push_str(&format!(
                "\n... truncated at {} matches",
                self.matches.len()
            ));
        }
        if text.is_empty() {
            text = "(no files match the pattern)".to_string();
        }
        text
    }

    fn display_summary(&self) -> String {
        let mut summary = format!(
            "{} match{}",
            self.count,
            if self.count == 1 { "" } else { "es" }
        );
        if self.truncated {
            summary.push_str(" · truncated");
        }
        summary
    }
}

impl ToolProjection for GlobOutput {
    fn model_blocks(&self) -> Vec<ToolContentBlock> {
        vec![ToolContentBlock::Text {
            text: self.model_text(),
        }]
    }

    fn display(&self, args: &Value) -> ToolDisplay {
        glob_display(args, self)
    }
}

pub struct GlobTool;

impl TypedTool for GlobTool {
    type Args = GlobArgs;
    type Output = GlobOutput;

    fn meta(&self) -> ToolMeta {
        ToolMeta::new(
            "glob",
            "List files by glob (gitignore-aware, native). Pattern vs rg -g.",
            crate::permission::ToolCategory::Read,
            ToolRisk::ReadOnly,
            Duration::from_secs(30),
        )
    }

    fn run(
        &self,
        ctx: &ToolCallContext,
        args: Self::Args,
    ) -> Result<Self::Output, ToolExecutionError> {
        let pattern = args.pattern.trim();
        if pattern.is_empty() {
            return Err(glob_error(
                "glob: pattern is required (e.g. \"src/**/*.rs\")",
            ));
        }

        let max_results = args
            .max_results
            .unwrap_or(DEFAULT_MAX_RESULTS as u64)
            .clamp(1, MAX_RESULTS_CAP as u64) as usize;

        // gitignore 风格：`*` 不跨 `/`（与 rg -g / VS Code 一致）。
        let glob = globset::GlobBuilder::new(pattern)
            .literal_separator(true)
            .build()
            .map_err(|error| glob_error(format!("glob: invalid pattern: {error}")))?;
        let matcher = glob.compile_matcher();

        let root_path = glob_root(ctx, args.path.as_deref());
        let walker = ignore::WalkBuilder::new(&root_path)
            // rg --files 默认：跳过 hidden + gitignored + parent 忽略规则；
            // require_git(false)：非 git 仓库目录同样应用 .gitignore（对齐 rg）。
            .standard_filters(true)
            .require_git(false)
            .build();

        let mut matches: Vec<String> = Vec::new();
        let mut truncated = false;
        for entry in walker.flatten() {
            if !entry
                .file_type()
                .is_some_and(|file_type| file_type.is_file())
            {
                continue;
            }
            let Ok(rel) = entry.path().strip_prefix(&root_path) else {
                continue;
            };
            // 统一 `/` 分隔（globset 按 `/` 匹配；Windows 下 Path 是 `\`）。
            let rel_str = rel.to_string_lossy().replace('\\', "/");
            if matcher.is_match(&rel_str) {
                matches.push(rel_str);
                if matches.len() >= max_results {
                    truncated = true;
                    break;
                }
            }
        }

        matches.sort();
        Ok(GlobOutput {
            count: matches.len(),
            matches,
            truncated,
        })
    }
}

fn glob_root(ctx: &ToolCallContext, raw_path: Option<&str>) -> PathBuf {
    let raw_path = raw_path.unwrap_or_default();
    let root = if raw_path.is_empty() {
        ctx.workspace_root.clone()
    } else {
        let path = Path::new(raw_path);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            ctx.workspace_root.join(path)
        }
    };
    let normalized = crate::permission::normalize_lexically(&root);
    if normalized.as_os_str().is_empty() {
        PathBuf::from(".")
    } else {
        normalized
    }
}

fn glob_display(args: &Value, output: &GlobOutput) -> ToolDisplay {
    let (glob_body, glob_body_truncated) =
        crate::tool_api::display::clamp_display_body(&output.model_text());
    let pattern = args
        .get("pattern")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let root = args
        .get("path")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty());
    let path = match (root, pattern) {
        (Some(root), Some(pattern)) => format!("{root}:{pattern}"),
        (Some(root), None) => root.to_string(),
        (None, Some(pattern)) => pattern.to_string(),
        (None, None) => {
            return ToolDisplay::new(
                ToolHeader::Other {
                    label: "glob".to_string(),
                },
                ToolBody::Text {
                    text: glob_body.clone(),
                    truncated: glob_body_truncated,
                },
            );
        }
    };
    ToolDisplay::new(
        ToolHeader::Path {
            path,
            op: crate::tool_api::PathOp::List,
        },
        ToolBody::Text {
            text: glob_body,
            truncated: glob_body_truncated,
        },
    )
    .with_summary(output.display_summary())
}

fn glob_error(message: impl Into<String>) -> ToolExecutionError {
    ToolExecutionError::Recoverable(ToolError::new(ToolErrorKind::Execution, message))
}

// ── Registration ──

pub fn register(mgr: &mut impl qaqh_tool_core::tool_api::RegistersTyped) {
    mgr.register_typed_tool(GlobTool);
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        fs::create_dir_all(dir.path().join("src/sub")).expect("mkdir src/sub");
        fs::create_dir_all(dir.path().join("crates/qaqh-a/src")).expect("mkdir crates");
        fs::write(dir.path().join("src/a.rs"), "fn a() {}\n").expect("write a.rs");
        fs::write(dir.path().join("src/sub/b.rs"), "fn b() {}\n").expect("write b.rs");
        fs::write(dir.path().join("src/data.txt"), "x\n").expect("write txt");
        fs::write(dir.path().join("src/.hidden.rs"), "fn h() {}\n").expect("write hidden");
        fs::write(
            dir.path().join("crates/qaqh-a/src/lib.rs"),
            "pub fn x() {}\n",
        )
        .expect("write lib.rs");
        fs::write(dir.path().join("README.md"), "# hi\n").expect("write readme");
        dir
    }

    fn ctx(root: &Path) -> ToolCallContext {
        ToolCallContext {
            call_id: "glob-test".to_string(),
            session_id: "glob-test-session".to_string(),
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

    fn parse_args(args: serde_json::Value) -> GlobArgs {
        serde_json::from_value(args).expect("valid glob args")
    }

    #[allow(clippy::result_large_err)] // ToolExecutionError is the frozen typed boundary.
    fn run(root: &Path, args: serde_json::Value) -> Result<GlobOutput, ToolExecutionError> {
        let mut args = args;
        if let Some(object) = args.as_object_mut() {
            object.insert(
                "path".to_string(),
                serde_json::json!(root.to_string_lossy().to_string()),
            );
        }
        GlobTool.run(&ctx(root), parse_args(args))
    }

    fn lines(output: &GlobOutput) -> Vec<String> {
        output.model_text().lines().map(String::from).collect()
    }

    fn error_code(error: ToolExecutionError) -> String {
        match error {
            ToolExecutionError::Recoverable(error) => error.code.as_str().to_string(),
            ToolExecutionError::Fatal(error) => error.code,
        }
    }

    #[test]
    fn glob_star_star_matches_recursively() {
        let dir = fixture();
        let output = run(dir.path(), serde_json::json!({ "pattern": "src/**/*.rs" }))
            .expect("glob succeeds");
        let list = lines(&output);
        assert!(list.contains(&"src/a.rs".to_string()));
        assert!(list.contains(&"src/sub/b.rs".to_string()));
        // `*` 不跨 `/`：单星不匹配子目录。
        let single =
            run(dir.path(), serde_json::json!({ "pattern": "src/*.rs" })).expect("glob succeeds");
        let single_list = lines(&single);
        assert!(single_list.contains(&"src/a.rs".to_string()));
        assert!(!single_list.contains(&"src/sub/b.rs".to_string()));
    }

    #[test]
    fn glob_skips_hidden_and_gitignored() {
        let dir = fixture();
        fs::write(dir.path().join(".gitignore"), "data.txt\n").expect("gitignore");
        let output =
            run(dir.path(), serde_json::json!({ "pattern": "**/*" })).expect("glob succeeds");
        let list = lines(&output);
        assert!(list.contains(&"src/a.rs".to_string()));
        assert!(
            !list.contains(&"src/.hidden.rs".to_string()),
            "hidden must be skipped"
        );
        assert!(
            !list.contains(&"src/data.txt".to_string()),
            "gitignored must be skipped"
        );
    }

    #[test]
    fn glob_alternation_and_root_limiting() {
        let dir = fixture();
        let output = run(
            dir.path(),
            serde_json::json!({ "pattern": "crates/{qaqh-a,qaqh-b}/src/lib.rs" }),
        )
        .expect("glob succeeds");
        let list = lines(&output);
        assert!(list.contains(&"crates/qaqh-a/src/lib.rs".to_string()));
        assert!(!list.contains(&"README.md".to_string()));
    }

    #[test]
    fn glob_empty_path_anchors_to_session_workspace() {
        let dir = fixture();
        let tool_ctx = ctx(dir.path());

        // No `path` argument at all → search the explicit session workspace.
        let output = GlobTool
            .run(
                &tool_ctx,
                parse_args(serde_json::json!({ "pattern": "src/a.rs" })),
            )
            .expect("glob succeeds");
        assert!(
            lines(&output).contains(&"src/a.rs".to_string()),
            "empty path must search the session workspace"
        );

        // Explicit relative path still resolves against the workspace;
        // results are printed relative to the search root.
        let relative = GlobTool
            .run(
                &tool_ctx,
                parse_args(serde_json::json!({ "pattern": "a.rs", "path": "src" })),
            )
            .expect("glob succeeds");
        assert!(lines(&relative).contains(&"a.rs".to_string()));
    }

    #[test]
    fn glob_errors_preserve_legacy_code() {
        let dir = fixture();
        let invalid = run(
            dir.path(),
            serde_json::json!({ "pattern": "src/[unclosed" }),
        )
        .expect_err("invalid glob must error");
        assert_eq!(error_code(invalid), "execution");

        let missing = GlobTool
            .run(&ctx(dir.path()), parse_args(serde_json::json!({})))
            .expect_err("missing pattern must error");
        assert_eq!(error_code(missing), "execution");

        let none = run(dir.path(), serde_json::json!({ "pattern": "*.toml" }))
            .expect("empty match is success");
        assert!(
            lines(&none)
                .iter()
                .any(|line| line.contains("no files match"))
        );
    }

    #[test]
    fn glob_is_read_only_under_permission_engine() {
        // 权限引擎必须把 glob（descriptor 声明 ReadOnly）视为只读：Level 2 自动批准。
        use crate::permission::{PermissionDecision, PermissionLevel, needs_permission};
        let ws = std::env::temp_dir().join("qaqh-glob-perm");
        let decision = needs_permission(
            PermissionLevel::ReadOnly,
            "glob",
            &serde_json::json!({ "pattern": "**/*.rs" }),
            &ws,
            &Default::default(),
            crate::permission::ToolCategory::Read,
        );
        assert!(matches!(decision, PermissionDecision::AutoApprove));
    }

    #[test]
    fn glob_respects_max_results() {
        let dir = fixture();
        let output = run(
            dir.path(),
            serde_json::json!({ "pattern": "**/*", "max_results": 2 }),
        )
        .expect("glob succeeds");
        let text = output.model_text();
        assert!(text.contains("truncated"));
        let path_lines = text.lines().filter(|line| !line.starts_with("...")).count();
        assert_eq!(path_lines, 2);
        assert_eq!(output.count, 2);
        assert!(output.truncated);
    }

    #[test]
    fn glob_registration_is_typed_and_schema_is_type_generated() {
        let mut manager = qaqh_workspace::ToolManager::new();
        register(&mut manager);
        let registered = manager.builtin("glob").expect("glob registered");
        assert_eq!(
            registered.descriptor.name.as_str(),
            "glob",
            "glob must be on the typed execution surface"
        );
        // v2：schema 由 GlobArgs 类型生成——pattern 带 serde(default)，不再是 required。
        assert!(
            registered.descriptor.input_schema["properties"]["pattern"].is_object(),
            "generated input schema must carry pattern"
        );
        assert!(
            registered.descriptor.input_schema["properties"]["max_results"].is_object(),
            "generated input schema must carry max_results"
        );
        assert_eq!(
            registered.descriptor.input_schema["additionalProperties"],
            serde_json::json!(false)
        );
        assert_eq!(registered.descriptor.output_schema["type"], "object");
    }

    #[test]
    fn glob_typed_output_and_display_share_the_same_text() {
        let output = GlobOutput {
            matches: vec!["src/a.rs".to_string()],
            truncated: false,
            count: 1,
        };
        let model = match output.model_blocks().into_iter().next() {
            Some(ToolContentBlock::Text { text }) => text,
            _ => panic!("glob output must have a text model block"),
        };
        assert_eq!(model, "src/a.rs");
        let display = output.display(&serde_json::json!({"pattern": "**/*.rs"}));
        assert_eq!(display.summary.as_deref(), Some("1 match"));
        assert_ne!(
            display.summary.as_deref(),
            Some(model.lines().next().unwrap_or_default()),
            "glob summary must be metadata, not the body's first line"
        );
        assert_eq!(
            display.header,
            ToolHeader::Path {
                path: "**/*.rs".to_string(),
                op: crate::tool_api::PathOp::List,
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
