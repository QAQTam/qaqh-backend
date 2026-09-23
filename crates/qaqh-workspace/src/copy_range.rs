//! `copy_range`：按**内容锚定范围**在文件间拷贝代码/文本。
//!
//! 解决"模型拷贝代码必须重新输出正文"的痛点：给出源范围的两行锚
//! （起始行 + 结束行）和目标插入锚，正文由引擎从文件读取原样搬运——
//! 模型只需输出几个锚字符串（几个 token），不需要重打正文。
//!
//! - **定位是内容匹配，不是行号**：锚 = 行级精确匹配（尾部空白容差）；
//!   多处命中 → 拒绝并附候选（拷贝错误会静默损坏目标，不降级模糊匹配）。
//! - 源范围 `[start_anchor, end_anchor]` 含两端行；`end_anchor` 缺省 = 单行。
//! - 目标插入：`insert_after`/`insert_before`（锚行）/ `append`（文件尾）/
//!   `prepend`（文件头）。
//! - LF 规范视图匹配（与 read 一致）；插入行使用目标文件的行尾风格
//!   （CRLF 文件插入 CRLF），未改动行保持原字节。
//! - 同文件拷贝：插入点落在被拷贝区间内 → 拒绝（区间随插入位移会乱）。

use std::path::Path;
use std::time::Duration;

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

use crate::ToolRisk;
use crate::file_mutate::{mutation_display, mutation_error, resolve_mutation_path};
use crate::tool_api::{
    OutputBudget, ToolCallContext, ToolContentBlock, ToolDescriptor, ToolDisplay,
    ToolExecutionError, ToolExposure, ToolName, ToolProjection, ToolSource, TypedTool,
};

const MODES: &[&str] = &["insert_after", "insert_before", "append", "prepend"];

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Mode {
    InsertAfter,
    InsertBefore,
    Append,
    Prepend,
}

impl Mode {
    fn parse(s: &str) -> Option<Mode> {
        match s {
            "insert_after" => Some(Mode::InsertAfter),
            "insert_before" => Some(Mode::InsertBefore),
            "append" => Some(Mode::Append),
            "prepend" => Some(Mode::Prepend),
            _ => None,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Mode::InsertAfter => "insert_after",
            Mode::InsertBefore => "insert_before",
            Mode::Append => "append",
            Mode::Prepend => "prepend",
        }
    }
}

struct RangeResult {
    copied: Vec<String>,
    /// 源区间（1-based 展示行号，含两端）
    range: (usize, usize),
    /// 目标插入位置（0-based 行索引）
    insert_at: usize,
    mode: Mode,
    target_before: String,
    target_after: String,
}

/// 行级精确匹配（尾部空白容差）。返回所有命中位置（0-based）。
fn locate_exact(lines: &[String], anchor: &str) -> Vec<usize> {
    let anchor = anchor.trim_end();
    lines
        .iter()
        .enumerate()
        .filter(|(_, line)| line.trim_end() == anchor)
        .map(|(index, _)| index)
        .collect()
}

fn ambiguous_error(kind: &str, anchor: &str, hits: &[usize]) -> ToolExecutionError {
    let locations: Vec<String> = hits
        .iter()
        .take(5)
        .map(|&index| format!("L{}", index + 1))
        .collect();
    mutation_error(
        kind,
        format!(
            "{kind}: anchor {anchor:?} matches {} locations: {}",
            hits.len(),
            locations.join(", ")
        ),
        Some(
            "Make the anchor a longer/more unique line fragment, or add neighboring context in the anchor text (anchors match a WHOLE line).",
        ),
        json!({
            "timeis": crate::now_utc8(),
            "status": "error",
            "code": kind,
            "candidates": hits.iter().take(5).map(|&index| index + 1).collect::<Vec<usize>>(),
        }),
    )
}

fn not_found_error(kind: &str, anchor: &str) -> ToolExecutionError {
    mutation_error(
        kind,
        format!(
            "{kind}: anchor {anchor:?} not found — check the exact line content (whitespace at line ends is tolerated; leading whitespace is significant)"
        ),
        None,
        json!({
            "timeis": crate::now_utc8(),
            "status": "error",
            "code": kind,
            "anchor": anchor,
        }),
    )
}

/// 核心逻辑：读源 → 定位区间 → 读目标 → 定位插入点 → 生成并写入新目标内容。
/// `src`/`tgt` 为已解析的绝对路径。
#[allow(clippy::too_many_arguments, clippy::result_large_err)] // 迁移期保留原工具参数面与冻结错误边界。
fn run_copy_range(
    src: &Path,
    tgt: &Path,
    target_display: &str,
    session_id: &str,
    start_anchor: &str,
    end_anchor: Option<&str>,
    target_anchor: Option<&str>,
    mode: Mode,
) -> Result<RangeResult, ToolExecutionError> {
    let src_content = std::fs::read_to_string(src).map_err(|error| {
        mutation_error(
            "SOURCE_READ_ERROR",
            format!(
                "SOURCE_READ_ERROR: failed to read source {}: {error}",
                src.to_string_lossy()
            ),
            None,
            json!({
                "timeis": crate::now_utc8(),
                "status": "error",
                "code": "SOURCE_READ_ERROR",
                "path": src.to_string_lossy(),
            }),
        )
    })?;
    let (src_lf, _) = crate::file_shared::normalize_newlines(&src_content);
    let mut src_lines: Vec<String> = src_lf.split('\n').map(String::from).collect();
    if src_lines.last().is_some_and(|line| line.is_empty()) {
        src_lines.pop();
    }

    // ── 源区间定位 ──────────────────────────────────────────────
    let start_hits = locate_exact(&src_lines, start_anchor);
    if start_hits.is_empty() {
        return Err(not_found_error("SOURCE_START_NOT_FOUND", start_anchor));
    }
    if start_hits.len() > 1 {
        return Err(ambiguous_error(
            "SOURCE_START_AMBIGUOUS",
            start_anchor,
            &start_hits,
        ));
    }
    let start_idx = start_hits[0];

    let end_idx = match end_anchor {
        Some(end_anchor) => {
            // 结束锚：从起始行之后（含）顺序找——首个命中即结束行。
            let end_hits: Vec<usize> = src_lines
                .iter()
                .enumerate()
                .skip(start_idx)
                .filter(|(_, line)| line.trim_end() == end_anchor.trim_end())
                .map(|(index, _)| index)
                .collect();
            match end_hits.first() {
                Some(&index) => index,
                None => return Err(not_found_error("SOURCE_END_NOT_FOUND", end_anchor)),
            }
        }
        None => start_idx,
    };

    let copied: Vec<String> = src_lines[start_idx..=end_idx].to_vec();
    let range = (start_idx + 1, end_idx + 1);

    // ── 目标读取与插入点定位 ────────────────────────────────────
    let target_before = std::fs::read_to_string(tgt).map_err(|error| {
        mutation_error(
            "TARGET_READ_ERROR",
            format!(
                "TARGET_READ_ERROR: failed to read target {}: {error}",
                tgt.to_string_lossy()
            ),
            None,
            json!({
                "timeis": crate::now_utc8(),
                "status": "error",
                "code": "TARGET_READ_ERROR",
                "path": tgt.to_string_lossy(),
            }),
        )
    })?;
    let (tgt_lf, endings) = crate::file_shared::normalize_newlines(&target_before);
    let mut tgt_lines: Vec<String> = tgt_lf.split('\n').map(String::from).collect();
    if tgt_lines.last().is_some_and(|line| line.is_empty()) {
        tgt_lines.pop();
    }

    let insert_at = match mode {
        Mode::Append => tgt_lines.len(),
        Mode::Prepend => 0,
        Mode::InsertAfter | Mode::InsertBefore => {
            let anchor = target_anchor.ok_or_else(|| {
                mutation_error(
                    "MISSING_TARGET_ANCHOR",
                    format!(
                        "MISSING_TARGET_ANCHOR: mode={:?} requires 'target_anchor'",
                        mode.name()
                    ),
                    None,
                    json!({
                        "timeis": crate::now_utc8(),
                        "status": "error",
                        "code": "MISSING_TARGET_ANCHOR",
                    }),
                )
            })?;
            let hits = locate_exact(&tgt_lines, anchor);
            if hits.is_empty() {
                return Err(not_found_error("TARGET_ANCHOR_NOT_FOUND", anchor));
            }
            if hits.len() > 1 {
                return Err(ambiguous_error("TARGET_ANCHOR_AMBIGUOUS", anchor, &hits));
            }
            match mode {
                Mode::InsertAfter => hits[0] + 1,
                _ => hits[0],
            }
        }
    };

    // ── 同文件区间冲突 ──────────────────────────────────────────
    let same_file = src.canonicalize().ok() == tgt.canonicalize().ok();
    if same_file && insert_at > start_idx && insert_at <= end_idx {
        // 插入点落在 [start..=end] 区间内 → 位移后区间漂移。
        // （insert_at == end_idx + 1，即区间正后方插入，是安全的。）
        return Err(mutation_error(
            "INSERT_INSIDE_RANGE",
            format!(
                "INSERT_INSIDE_RANGE: source and target are the same file and the insertion point (after line {}) lies inside the copied range L{}-L{} — copy would shift the range",
                insert_at, range.0, range.1
            ),
            Some(
                "Insert before the range start, after the range end, or use a different target file.",
            ),
            json!({
                "timeis": crate::now_utc8(),
                "status": "error",
                "code": "INSERT_INSIDE_RANGE",
            }),
        ));
    }

    // ── 组装并写回 ──────────────────────────────────────────────
    let eol = endings.preferred.as_str();
    tgt_lines.splice(insert_at..insert_at, copied.iter().cloned());
    let mut target_after = tgt_lines.join(eol);
    // 历史行为：目标文件以换行结尾（无尾换行时补上；空文件不加）。
    if !target_after.is_empty() && !target_after.ends_with('\n') {
        target_after.push_str(eol);
    }
    // 写策略：拒绝符号链接（不替换链接、不穿透写）与设备/FIFO/目录。
    let tgt_str = tgt.to_string_lossy();
    if let Err(guard) = crate::file_shared::ensure_writable_regular_target(&tgt_str) {
        let code = guard.code();
        return Err(mutation_error(
            code,
            format!("{code}: {}", guard.message()),
            guard.hint().as_deref(),
            json!({
                "timeis": crate::now_utc8(),
                "status": "error",
                "code": code,
            }),
        ));
    }
    std::fs::write(tgt, &target_after).map_err(|error| {
        mutation_error(
            "TARGET_WRITE_ERROR",
            format!(
                "TARGET_WRITE_ERROR: failed to write target {}: {error}",
                tgt.to_string_lossy()
            ),
            None,
            json!({
                "timeis": crate::now_utc8(),
                "status": "error",
                "code": "TARGET_WRITE_ERROR",
                "path": tgt.to_string_lossy(),
            }),
        )
    })?;

    crate::journal::record_change(
        session_id,
        "",
        "copy_range",
        target_display,
        mode.name(),
        Some(&target_before),
        Some(&target_after),
        "ok",
    );

    Ok(RangeResult {
        copied,
        range,
        insert_at,
        mode,
        target_before,
        target_after,
    })
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct CopyRangeArgs {
    pub source_path: String,
    pub source_start: String,
    #[serde(default)]
    pub source_end: Option<String>,
    pub target_path: String,
    #[serde(default)]
    pub target_anchor: Option<String>,
    #[serde(default = "default_mode")]
    pub mode: String,
}

fn default_mode() -> String {
    "append".to_string()
}

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
pub struct CopyRangeOutput {
    pub timeis: String,
    pub status: String,
    pub copied_lines: usize,
    pub source_range: [usize; 2],
    pub source_path: String,
    pub target_path: String,
    pub mode: String,
    pub insert_at: usize,
    pub lines_added: u32,
    pub lines_removed: u32,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub first_changed_line: Option<u32>,
    #[serde(skip)]
    #[schemars(skip)]
    model_text: String,
    #[serde(skip)]
    #[schemars(skip)]
    diff: Option<String>,
}

impl ToolProjection for CopyRangeOutput {
    fn model_blocks(&self) -> Vec<ToolContentBlock> {
        vec![ToolContentBlock::Text {
            text: self.model_text.clone(),
        }]
    }

    fn summary(&self) -> Option<String> {
        self.model_text
            .lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .map(|line| line.chars().take(160).collect())
    }

    fn display(&self, args: &Value) -> ToolDisplay {
        mutation_display(
            args.get("target_path").and_then(Value::as_str),
            &self.target_path,
            crate::tool_api::PathOp::Write,
            "copy_range",
            &self.model_text,
            self.diff.clone(),
        )
    }
}

pub struct CopyRangeTool;

impl TypedTool for CopyRangeTool {
    type Args = CopyRangeArgs;
    type Output = CopyRangeOutput;

    fn descriptor(&self) -> ToolDescriptor {
        ToolDescriptor {
            name: ToolName::new("copy_range").expect("valid copy_range tool name"),
            display_name: None,
            description: "Copy a line range by exact line anchors: source_start/source_end; mode=insert_after|insert_before|append|prepend."
                .to_string(),
            input_schema: copy_range_schema(),
            output_schema: serde_json::to_value(schemars::schema_for!(CopyRangeOutput))
                .expect("copy_range output schema"),
            category: crate::permission::ToolCategory::Write,
            risk: ToolRisk::Write,
            default_timeout: Duration::from_secs(60),
            exposure: ToolExposure::Direct,
            source: ToolSource::Builtin,
            output_budget: OutputBudget::default(),
            capabilities: crate::tool_capabilities::builtin_capabilities("copy_range")
                .unwrap_or_default(),
        }
    }

    #[allow(clippy::result_large_err)] // ToolExecutionError is the frozen typed boundary.
    fn run(
        &self,
        ctx: &ToolCallContext,
        args: Self::Args,
    ) -> Result<Self::Output, ToolExecutionError> {
        let mode = Mode::parse(&args.mode).ok_or_else(|| {
            mutation_error(
                "INVALID_MODE",
                format!(
                    "INVALID_MODE: invalid mode — use one of: {}",
                    MODES.join(" | ")
                ),
                None,
                json!({}),
            )
        })?;
        let source_path = args.source_path;
        let target_path = args.target_path;
        let workspace = ctx.workspace_root.clone();
        let src =
            crate::apply_patch_engine::resolve_workspace_path(&workspace, Path::new(&source_path))
                .map_err(|error| {
                    mutation_error(
                        "PATH_OUTSIDE_WORKSPACE",
                        format!("PATH_OUTSIDE_WORKSPACE: {error}"),
                        None,
                        json!({}),
                    )
                })?;
        let tgt =
            crate::apply_patch_engine::resolve_workspace_path(&workspace, Path::new(&target_path))
                .map_err(|error| {
                    mutation_error(
                        "PATH_OUTSIDE_WORKSPACE",
                        format!("PATH_OUTSIDE_WORKSPACE: {error}"),
                        None,
                        json!({}),
                    )
                })?;

        let result = run_copy_range(
            &src,
            &tgt,
            &target_path,
            &ctx.session_id,
            &args.source_start,
            args.source_end.as_deref(),
            args.target_anchor.as_deref(),
            mode,
        )?;

        // 账本同步：目标已写盘，登记最新内容供 edit 防漂移。
        // 键形态必须与 read/write 的账本键完全一致（lib 版本，不 canonicalize）。
        let ledger_key = resolve_mutation_path(ctx, &target_path);
        if let Ok(content) = std::fs::read_to_string(&tgt) {
            crate::file_state::record_write(&ledger_key, &content);
        }

        let copied_lines = result.copied.len();
        let mut model_text = format!(
            "[OK] copy_range — {copied_lines} line(s) L{}-L{} copied: {source_path} → {target_path} ({})\n",
            result.range.0,
            result.range.1,
            match result.mode {
                Mode::InsertAfter => format!("after line {}", result.insert_at),
                Mode::InsertBefore => format!("before line {}", result.insert_at + 1),
                Mode::Append => "append".to_string(),
                Mode::Prepend => "prepend".to_string(),
            }
        );
        if let Some(anchor) = &args.target_anchor {
            model_text = format!(
                "[OK] copy_range — {copied_lines} line(s) L{}-L{} copied: {source_path} → {target_path} ({} {anchor:?})\n",
                result.range.0,
                result.range.1,
                result.mode.name(),
            );
        }

        let (before_lf, _) = crate::file_shared::normalize_newlines(&result.target_before);
        let (after_lf, _) = crate::file_shared::normalize_newlines(&result.target_after);
        let diff_text = crate::file_shared::unified_diff(&before_lf, &after_lf, &target_path);
        let has_diff = !diff_text.is_empty();
        let (lines_added, lines_removed, first_line) =
            crate::file_shared::diff_stats_between(&before_lf, &after_lf);

        Ok(CopyRangeOutput {
            timeis: crate::now_utc8(),
            status: "ok".to_string(),
            copied_lines,
            source_range: [result.range.0, result.range.1],
            source_path,
            target_path,
            mode: result.mode.name().to_string(),
            insert_at: result.insert_at,
            lines_added,
            lines_removed,
            first_changed_line: has_diff.then_some(first_line),
            model_text,
            diff: has_diff.then_some(diff_text),
        })
    }
}

fn copy_range_schema() -> Value {
    json!({
        "type": "object",
        "properties": {
            "source_path": {"type": "string", "description": "Source file"},
            "source_start": {"type": "string", "description": "Start anchor line (exact)"},
            "source_end": {"type": "string", "description": "End anchor inclusive; omit=single line", "default": null},
            "target_path": {"type": "string", "description": "Target file"},
            "target_anchor": {"type": "string", "description": "Anchor for insert_after/before"},
            "mode": {"type": "string", "enum": ["insert_after", "insert_before", "append", "prepend"], "description": "Insert position (default append)", "default": "append"}
        },
        "required": ["source_path", "source_start", "target_path"],
        "additionalProperties": false
    })
}

pub fn register(mgr: &mut crate::ToolManager) {
    mgr.register_typed(CopyRangeTool);
}

/// Compatibility entry retained for existing in-process callers/tests.
///
/// Production registration uses [`CopyRangeTool`] directly; the pre-check keeps
/// the legacy `MISSING_ARGUMENT` shape for malformed calls.
#[cfg(test)]
fn exec_copy_range(args: &Value) -> crate::ToolResult {
    use crate::tool_api::ErasedTool;

    let get = |key: &str| args.get(key).and_then(Value::as_str);
    if get("source_path").is_none() || get("source_start").is_none() || get("target_path").is_none()
    {
        return crate::json_err(
            "MISSING_ARGUMENT",
            "copy_range requires 'source_path', 'source_start' and 'target_path'",
            "",
        );
    }
    let ctx =
        crate::file_mutate::ambient_tool_context("copy-range-compat", Duration::from_secs(60));
    crate::tool_api::TypedToolAdapter::new(CopyRangeTool)
        .execute(ctx, args.clone())
        .unwrap_or_else(|fatal| panic!("copy_range tool fatal: {}", fatal.message))
        .to_tool_result()
}

// ─────────────────────────────────────────────────────────────
// Tests
// ─────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    /// 写 CURRENT_WORKSPACE 的测试必须串行（全局静态，并行测试会互相踩踏）。
    static WS_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    fn setup(files: &[(&str, &str)]) -> (tempfile::TempDir, std::path::PathBuf) {
        let dir = tempfile::tempdir().expect("tempdir");
        for (name, content) in files {
            let p = dir.path().join(name);
            if let Some(parent) = p.parent() {
                std::fs::create_dir_all(parent).unwrap();
            }
            std::fs::write(p, content).unwrap();
        }
        let path = dir.path().to_path_buf();
        (dir, path)
    }

    fn run(
        dir: &std::path::Path,
        src: &str,
        start: &str,
        end: Option<&str>,
        tgt: &str,
        anchor: Option<&str>,
        mode: &str,
    ) -> Result<String, String> {
        let _guard = WS_LOCK.lock().unwrap();
        let mut args = serde_json::json!({
            "source_path": src,
            "source_start": start,
            "target_path": tgt,
            "mode": mode,
        });
        if let Some(e) = end {
            args["source_end"] = serde_json::json!(e);
        }
        if let Some(a) = anchor {
            args["target_anchor"] = serde_json::json!(a);
        }
        // 注入 workspace（避免依赖全局 CURRENT_WORKSPACE）
        crate::CURRENT_WORKSPACE
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clone_from(&dir.to_string_lossy().to_string());
        let result = exec_copy_range(&args);
        if result.is_success() {
            Ok(result.model_text().to_string())
        } else {
            Err(result.model_text().to_string())
        }
    }

    #[test]
    fn copies_range_to_append() {
        let (dir, ws) = setup(&[
            (
                "src.rs",
                "fn a() {}\nfn target() {\n    body\n}\nfn c() {}\n",
            ),
            ("dst.rs", "// header\n"),
        ]);
        let out = run(
            &ws,
            "src.rs",
            "fn target() {",
            Some("}"),
            "dst.rs",
            None,
            "append",
        )
        .unwrap();
        assert!(out.starts_with("[OK] copy_range"));
        assert!(out.contains("3 line(s)"));
        assert_eq!(
            std::fs::read_to_string(dir.path().join("dst.rs")).unwrap(),
            "// header\nfn target() {\n    body\n}\n"
        );
    }

    #[test]
    fn copies_single_line_without_source_end() {
        let (dir, ws) = setup(&[
            ("src.rs", "keep\nfn marker() {}\nkeep2\n"),
            ("dst.rs", "a\nb\n"),
        ]);
        run(
            &ws,
            "src.rs",
            "fn marker() {}",
            None,
            "dst.rs",
            None,
            "append",
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("dst.rs")).unwrap(),
            "a\nb\nfn marker() {}\n"
        );
    }

    #[test]
    fn insert_after_anchor() {
        let (dir, ws) = setup(&[
            ("src.rs", "line1\nline2\nline3\n"),
            ("dst.rs", "head\nmid\ntail\n"),
        ]);
        run(
            &ws,
            "src.rs",
            "line2",
            None,
            "dst.rs",
            Some("mid"),
            "insert_after",
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("dst.rs")).unwrap(),
            "head\nmid\nline2\ntail\n"
        );
    }

    #[test]
    fn insert_before_anchor() {
        let (dir, ws) = setup(&[("src.rs", "X1\nX2\n"), ("dst.rs", "a\nb\nc\n")]);
        run(
            &ws,
            "src.rs",
            "X1",
            Some("X2"),
            "dst.rs",
            Some("b"),
            "insert_before",
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("dst.rs")).unwrap(),
            "a\nX1\nX2\nb\nc\n"
        );
    }

    #[test]
    fn prepend_to_file() {
        let (dir, ws) = setup(&[("src.rs", "HEADER\n"), ("dst.rs", "body\n")]);
        run(&ws, "src.rs", "HEADER", None, "dst.rs", None, "prepend").unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("dst.rs")).unwrap(),
            "HEADER\nbody\n"
        );
    }

    #[test]
    fn ambiguous_source_start_rejected_with_candidates() {
        let (_dir, ws) = setup(&[("src.rs", "dup\nx\ndup\ny\n"), ("dst.rs", "out\n")]);
        let err = run(&ws, "src.rs", "dup", None, "dst.rs", None, "append").unwrap_err();
        assert!(err.contains("SOURCE_START_AMBIGUOUS"), "got: {err}");
        assert!(err.contains("L1"), "candidates expected, got: {err}");
        assert!(err.contains("L3"), "candidates expected, got: {err}");
    }

    #[test]
    fn end_anchor_missing_rejected() {
        let (_dir, ws) = setup(&[("src.rs", "start\nbody\n"), ("dst.rs", "out\n")]);
        let err = run(
            &ws,
            "src.rs",
            "start",
            Some("nope"),
            "dst.rs",
            None,
            "append",
        )
        .unwrap_err();
        assert!(err.contains("SOURCE_END_NOT_FOUND"), "got: {err}");
    }

    #[test]
    fn same_file_insert_inside_range_rejected() {
        let (_dir, ws) = setup(&[("f.rs", "a\nb\nc\nd\n")]);
        let err = run(
            &ws,
            "f.rs",
            "b",
            Some("c"),
            "f.rs",
            Some("b"),
            "insert_after",
        )
        .unwrap_err();
        assert!(err.contains("INSERT_INSIDE_RANGE"), "got: {err}");
    }

    #[test]
    fn same_file_insert_after_range_allowed() {
        let (dir, ws) = setup(&[("f.rs", "a\nb\nc\nd\n")]);
        run(
            &ws,
            "f.rs",
            "b",
            Some("c"),
            "f.rs",
            Some("d"),
            "insert_before",
        )
        .unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("f.rs")).unwrap(),
            "a\nb\nc\nb\nc\nd\n"
        );
    }

    #[test]
    fn crlf_target_uses_crlf_for_inserted_lines() {
        let (dir, ws) = setup(&[("src.rs", "s1\ns2\n"), ("dst.rs", "h1\r\nh2\r\n")]);
        run(&ws, "src.rs", "s1", Some("s2"), "dst.rs", None, "append").unwrap();
        assert_eq!(
            std::fs::read_to_string(dir.path().join("dst.rs")).unwrap(),
            "h1\r\nh2\r\ns1\r\ns2\r\n"
        );
    }

    #[test]
    fn copy_range_ledger_key_matches_resolved_absolute_path() {
        // BUG-2026-09-13-16 回归：copy_range 必须用与 read/write 同形的账本键
        // （lib::resolve_workspace_path，无 \\?\ verbatim 前缀），否则
        // STALE_FILE 校验看不到 copy_range 的写。
        let (dir, ws) = setup(&[("src.rs", "copied\n"), ("dst.rs", "head\n")]);
        let raw_src = "src.rs";
        let raw_tgt = "dst.rs";
        run(&ws, raw_src, "copied", None, raw_tgt, None, "append").unwrap();

        // 键形态 = lib::resolve_workspace_path(CURRENT_WORKSPACE + 相对路径)，
        // 与 read（file_query）/write（file_mutate）记账键同源同形。
        let expected_key = crate::resolve_workspace_path(raw_tgt);
        assert!(
            std::path::Path::new(&expected_key).is_absolute(),
            "resolve_workspace_path must yield an absolute key, got {expected_key}"
        );
        assert!(
            !expected_key.contains(r"\\\\?\\"),
            "ledger key must not carry a verbatim \\\\?\\ prefix: {expected_key}"
        );
        let wrote = std::fs::read_to_string(dir.path().join(raw_tgt)).unwrap();
        let expected_hash =
            crate::file_shared::content_hash(&crate::file_shared::normalize_newlines(&wrote).0);
        assert_eq!(
            crate::file_state::last_hash(&expected_key),
            Some(expected_hash.clone()),
            "ledger key must be the resolved absolute path"
        );
        // 相对路径原样记账 = 缺陷形态：同一文件两套键
        assert_eq!(
            crate::file_state::last_hash(raw_tgt),
            None,
            "raw relative path must NOT be used as the ledger key"
        );
    }

    #[test]
    fn stale_file_check_sees_copy_range_write() {
        // 缺陷前的行为：copy_range 用相对路径记账 → 绝对路径键无账本记录 →
        // 后续 write 的 STALE_FILE 校验被绕过（写方沿用编辑前的指纹也能通过）。
        let (dir, ws) = setup(&[("src.rs", "copied\n"), ("dst.rs", "head\n")]);
        let raw_src = "src.rs";
        let raw_tgt = "dst.rs";
        // 键形态与 read/write 同源：lib::resolve_workspace_path 产出的形态
        // （CURRENT_WORKSPACE + 相对路径，Windows 分隔符不强制转斜杠——
        // lib 版本用 Path::join，键里是反斜杠）。
        let abs_tgt = std::path::Path::new(ws.to_string_lossy().as_ref())
            .join(raw_tgt)
            .to_string_lossy()
            .to_string();

        // 全程用绝对路径（resolve_workspace_path 对绝对路径直接返回），
        // 不依赖全局 CURRENT_WORKSPACE，可与其它并行测试共存。
        let _serial = crate::TEST_RUNTIME_SERIAL
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        crate::CURRENT_WORKSPACE
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clone_from(&ws.to_string_lossy().to_string());

        // 先 read 一次，让账本持有「copy 之前」的基线指纹（键 = 绝对路径）。
        let read_result = crate::file_query::exec_read(&serde_json::json!({"path": abs_tgt}));
        assert!(
            read_result.is_success(),
            "read failed: {}",
            read_result.model_text()
        );
        let baseline =
            crate::file_shared::content_hash(&crate::file_shared::normalize_newlines("head\n").0);
        assert_eq!(
            crate::file_state::last_hash(&abs_tgt),
            Some(baseline.clone()),
            "read establishes the ledger baseline on the absolute key"
        );
        // copy_range 的目标路径用相对形态：缺陷正是「相对路径与绝对路径两套键」，
        // 绝对参数路径在 resolve 后与账本键同形，无法暴露该缺陷。
        crate::CURRENT_WORKSPACE
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .clone_from(&ws.to_string_lossy().to_string());

        // copy_range 追加一行（写工具改动了文件）。
        let result = exec_copy_range(&serde_json::json!({
            "source_path": raw_src,
            "source_start": "copied",
            "target_path": raw_tgt,
            "mode": "append",
        }));
        assert!(
            result.is_success(),
            "copy_range failed: {}",
            result.model_text()
        );
        let after_copy = std::fs::read_to_string(dir.path().join(raw_tgt)).unwrap();
        assert_eq!(after_copy.replace("\r\n", "\n"), "head\ncopied\n");

        // 修复重点：copy_range 的写必须落到账本的绝对路径键上（与 read/edit
        // 同键），否则下面这条「copy 后的真实指纹」断言会拿到 copy 前的旧值。
        let disk_lf = crate::file_shared::normalize_newlines(&after_copy).0;
        assert_eq!(
            crate::file_state::last_hash(&abs_tgt),
            Some(crate::file_shared::content_hash(&disk_lf)),
            "copy_range must refresh the ledger so STALE_FILE sees its write"
        );
        assert_ne!(
            crate::file_state::last_hash(&abs_tgt),
            Some(baseline.clone()),
            "ledger key still holds the pre-copy fingerprint"
        );

        // 端到端复核：copy_range 之后的 write 校验应看到 copy_range 的写，
        // 传入 copy_range 之前的指纹必须被判定为 STALE_FILE。
        let stale = crate::file_mutate::exec_write_file(&serde_json::json!({
            "path": abs_tgt,
            "content": "clobbered\n",
            "expected_hash": baseline.clone(),
        }));
        assert!(
            !stale.is_success(),
            "write with the pre-copy fingerprint must be rejected as STALE_FILE"
        );
        assert!(
            stale.model_text().contains("STALE_FILE"),
            "expected STALE_FILE, got: {}",
            stale.model_text()
        );
    }

    #[test]
    fn typed_copy_range_registration_and_display_are_same_source() {
        let mut manager = crate::ToolManager::new();
        register(&mut manager);
        assert!(
            manager.builtins["copy_range"].legacy.is_none(),
            "copy_range still has legacy executor"
        );

        let (_dir, workspace) = setup(&[("src.rs", "copied\n"), ("dst.rs", "head\n")]);
        let _guard = WS_LOCK.lock().unwrap();
        crate::CURRENT_WORKSPACE
            .write()
            .unwrap_or_else(|error| error.into_inner())
            .clone_from(&workspace.to_string_lossy().to_string());
        let result = exec_copy_range(&serde_json::json!({
            "source_path": "src.rs",
            "source_start": "copied",
            "target_path": "dst.rs",
            "mode": "append",
        }));
        assert!(result.is_success(), "{}", result.model_text());
        assert_eq!(result.data["copied_lines"], serde_json::json!(1));
        let display = result.display().expect("typed display");
        let display_text = match &display.body {
            Some(qaqh_types::ToolResultDisplayBody::Text { text, .. }) => text,
            other => panic!("unexpected copy_range display body: {other:?}"),
        };
        assert_eq!(display_text, result.model_text());
    }

    #[test]
    fn missing_anchor_when_mode_requires_it() {
        let (_dir, ws) = setup(&[("src.rs", "s\n"), ("dst.rs", "t\n")]);
        let err = run(&ws, "src.rs", "s", None, "dst.rs", None, "insert_after").unwrap_err();
        assert!(err.contains("MISSING_TARGET_ANCHOR"), "got: {err}");
    }
}
