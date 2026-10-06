use super::core::{ReplaceError, find_occurrences, nearest_match, run_replace};
use super::exec_edit;
use crate::ToolResult;
use serde_json::json;

fn call(path: &str, old: &str, new: &str) -> ToolResult {
    exec_edit(&json!({"path": path, "old_str": old, "new_str": new}))
}

fn call_all(path: &str, old: &str, new: &str, replace_all: bool) -> ToolResult {
    exec_edit(&json!({
        "path": path,
        "old_str": old,
        "new_str": new,
        "replace_all": replace_all
    }))
}

fn write_file(dir: &tempfile::TempDir, name: &str, content: &str) -> String {
    let path = dir.path().join(name);
    std::fs::write(&path, content).unwrap();
    path.to_string_lossy().to_string()
}

fn read(path: &str) -> String {
    std::fs::read_to_string(path).unwrap()
}

fn code(result: &ToolResult) -> &str {
    result.error.as_ref().map(|e| e.code.as_str()).unwrap_or("")
}

// ─────────────────────────────────────────────────────────────
// core：精确匹配与唯一性
// ─────────────────────────────────────────────────────────────

#[test]
fn inline_substring_replaces_without_touching_rest() {
    let out = run_replace("prefix: aaa bbb\nother\n", "aaa", "AAA", false).unwrap();
    assert_eq!(out.edited, "prefix: AAA bbb\nother\n");
    assert_eq!(out.lines, vec![1]);
}

#[test]
fn multi_line_substring_replaces() {
    let out = run_replace("a\nb\nc\nd\n", "b\nc", "B\nC", false).unwrap();
    assert_eq!(out.edited, "a\nB\nC\nd\n");
    assert_eq!(out.lines, vec![2]);
}

#[test]
fn new_str_empty_deletes_the_match() {
    let out = run_replace("a\nb\nc\n", "b\n", "", false).unwrap();
    assert_eq!(out.edited, "a\nc\n");
}

#[test]
fn not_found_carries_nearest_match() {
    match run_replace("alpha\nlet x = 1;\nbeta\n", "let x = 2;", "y", false) {
        Err(ReplaceError::NotFound { nearest }) => {
            let nearest = nearest.expect("nearest match");
            assert_eq!(nearest.start_line, 2);
            assert!(nearest.text.contains("let x = 1;"));
            assert!(nearest.score > 0.5, "score {}", nearest.score);
        }
        other => panic!("expected NotFound, got {}", describe(other)),
    }
}

#[test]
fn nearest_match_prefers_the_closest_line() {
    let nearest = nearest_match("alpha\nlet x = 1;\nbeta\n", "let x = 2;").unwrap();
    assert_eq!(nearest.start_line, 2);
}

#[test]
fn ambiguous_counts_overlapping_occurrences() {
    assert_eq!(find_occurrences("aaa", "aa"), vec![0, 1]);
}

#[test]
fn ambiguous_returns_total_and_occurrences() {
    match run_replace("x\nx\nx\n", "x", "y", false) {
        Err(ReplaceError::Ambiguous { total, occurrences }) => {
            assert_eq!(total, 3);
            assert_eq!(occurrences.len(), 3);
            assert_eq!(occurrences[0].line, 1);
            assert!(occurrences[0].context.contains("L1: x"));
        }
        other => panic!("expected Ambiguous, got {}", describe(other)),
    }
}

#[test]
fn replace_all_replaces_every_occurrence() {
    let out = run_replace("x\na\nx\n", "x", "y", true).unwrap();
    assert_eq!(out.edited, "y\na\ny\n");
    assert_eq!(out.replaced, 2);
    assert_eq!(out.lines, vec![1, 3]);
}

#[test]
fn replace_all_without_match_is_not_found() {
    assert!(matches!(
        run_replace("a\n", "z", "y", true),
        Err(ReplaceError::NotFound { .. })
    ));
}

#[test]
fn replace_all_uses_non_overlapping_matches() {
    // "aa" 在 "aaa" 里有重叠命中（0/1），replace_all 按 str::replace 语义取非重叠。
    let out = run_replace("aaa", "aa", "X", true).unwrap();
    assert_eq!(out.edited, "Xa");
    assert_eq!(out.replaced, 1);
}

#[test]
fn exec_replace_all_reports_count_and_shifts() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_file(&dir, "all.txt", "x\nmid\nx\nmid\nx\n");
    let result = call_all(&path, "x", "x\nY", true);
    assert!(result.is_success(), "{}", result.model_text());
    assert_eq!(read(&path), "x\nY\nmid\nx\nY\nmid\nx\nY\n");
    assert!(result.model_text().contains("replaced 3 occurrences"));
    // 三处起始行 1/3/5，各 +1 行。
    assert_eq!(crate::file_state::correct_line(&path, 2), Some((3, 1)));
    assert_eq!(crate::file_state::correct_line(&path, 4), Some((6, 2)));
    assert_eq!(crate::file_state::correct_line(&path, 6), Some((9, 3)));
}

#[test]
fn extending_context_makes_a_repeat_unique() {
    let content = "fn a() {\n    return Ok(());\n}\nfn b() {\n    return Ok(());\n}\n";
    let out = run_replace(
        content,
        "fn b() {\n    return Ok(());\n}",
        "fn b() {\n    return Err(());\n}",
        false,
    )
    .unwrap();
    assert!(out.edited.contains("return Err(())"));
    assert!(out.edited.contains("fn a() {\n    return Ok(());"));
}

#[test]
fn fragment_old_matches_inside_an_indented_line() {
    // 子串语义：不带缩进的片段也能命中（这正是 v2 整行语义做不到的）。
    let out = run_replace("    let x = 1;\n", "let x = 1;", "let x = 2;", false).unwrap();
    assert_eq!(out.edited, "    let x = 2;\n");
}

#[test]
fn wrong_whitespace_inside_old_does_not_match() {
    assert!(matches!(
        run_replace("    let x = 1;\n", "let x =  1;", "let x = 2;", false),
        Err(ReplaceError::NotFound { .. })
    ));
}

fn describe(r: Result<super::core::ReplaceOutcome, ReplaceError>) -> &'static str {
    match r {
        Ok(_) => "Ok",
        Err(ReplaceError::NotFound { .. }) => "NotFound",
        Err(ReplaceError::Ambiguous { .. }) => "Ambiguous",
    }
}

// ─────────────────────────────────────────────────────────────
// exec：成功路径（pass + 展示面 diff）
// ─────────────────────────────────────────────────────────────

#[test]
fn exec_replaces_and_returns_compact_pass() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_file(&dir, "a.txt", "prefix: aaa\nother\n");
    let result = call(&path, "aaa", "AAA");
    assert!(result.is_success(), "{}", result.model_text());
    assert_eq!(read(&path), "prefix: AAA\nother\n");
    let text = result.model_text();
    assert!(text.starts_with("[OK] edit "), "{text}");
    assert!(text.contains("replaced 1 occurrence"), "{text}");
    assert!(
        !text.contains("diff"),
        "model text must stay compact: {text}"
    );
    assert!(
        !text.contains("hash"),
        "model text must stay compact: {text}"
    );
    // diff 只走展示面。
    assert!(
        result
            .diff
            .as_deref()
            .is_some_and(|d| d.contains("+prefix: AAA"))
    );
}

#[test]
fn exec_multibyte_inline_replace() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_file(&dir, "cjk.txt", "第一行\n第二行\n第三行\n");
    let result = call(&path, "第二行", "第二行（改）");
    assert!(result.is_success(), "{}", result.model_text());
    assert_eq!(read(&path), "第一行\n第二行（改）\n第三行\n");
}

#[test]
fn exec_crlf_file_roundtrip() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_file(&dir, "crlf.txt", "a\r\nb\r\n");
    let result = call(&path, "b", "B");
    assert!(result.is_success(), "{}", result.model_text());
    assert_eq!(read(&path), "a\r\nB\r\n");
    // 请求侧 CRLF 同样归一化。
    let result = call(&path, "B\r\n", "C\r\n");
    assert!(result.is_success(), "{}", result.model_text());
    assert_eq!(read(&path), "a\r\nC\r\n");
}

#[test]
fn exec_shift_ledger_records_line_growth() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_file(&dir, "shift.txt", "a\nb\nc\n");
    let result = call(&path, "b", "b1\nb2");
    assert!(result.is_success(), "{}", result.model_text());
    // 旧 L3（c）在编辑后 = L4。
    assert_eq!(crate::file_state::correct_line(&path, 3), Some((4, 1)));
}

#[test]
fn exec_inline_replace_records_zero_shift() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_file(&dir, "inline.txt", "a\nb\nc\n");
    let result = call(&path, "b", "B");
    assert!(result.is_success(), "{}", result.model_text());
    assert_eq!(crate::file_state::correct_line(&path, 3), Some((3, 0)));
}

// ─────────────────────────────────────────────────────────────
// exec：失败路径（NOT_FOUND / AMBIGUOUS + diff）
// ─────────────────────────────────────────────────────────────

#[test]
fn exec_not_found_returns_diff_in_model_text() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_file(&dir, "nf.txt", "alpha\nlet x = 1;\nbeta\n");
    let result = call(&path, "let x = 2;", "let x = 3;");
    assert!(!result.is_success());
    assert_eq!(code(&result), "not_found");
    let text = result.model_text();
    assert!(text.contains("not_found"), "{text}");
    assert!(text.contains("diff (your old_str"), "{text}");
    assert!(text.contains("-let x = 2;"), "{text}");
    assert!(text.contains("+let x = 1;"), "{text}");
    // 文件与账本都不动。
    assert_eq!(read(&path), "alpha\nlet x = 1;\nbeta\n");
    assert_eq!(crate::file_state::correct_line(&path, 3), None);
}

#[test]
fn exec_ambiguous_lists_occurrences_and_leaves_file_untouched() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_file(&dir, "amb.txt", "x\nx\n");
    let result = call(&path, "x", "y");
    assert!(!result.is_success());
    assert_eq!(code(&result), "ambiguous_match");
    assert_eq!(result.data["match_count"], 2);
    let text = result.model_text();
    assert!(text.contains("matches 2 locations"), "{text}");
    assert!(text.contains("occurrence #1 at L1"), "{text}");
    assert!(text.contains("occurrence #2 at L2"), "{text}");
    assert_eq!(read(&path), "x\nx\n");
}

#[test]
fn exec_read_prefix_is_not_stripped() {
    // 模型把 read 的 "L1: " 前缀抄进 old_str 时，diff 会指出真实行内容。
    let dir = tempfile::tempdir().unwrap();
    let path = write_file(&dir, "prefix.txt", "foo\n");
    let result = call(&path, "L1: foo", "bar");
    assert!(!result.is_success());
    assert_eq!(code(&result), "not_found");
    let text = result.model_text();
    assert!(text.contains("-L1: foo"), "{text}");
    assert!(text.contains("+foo"), "{text}");
}

#[test]
fn exec_empty_file_is_not_found() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_file(&dir, "empty.txt", "");
    let result = call(&path, "x", "y");
    assert!(!result.is_success());
    assert_eq!(code(&result), "not_found");
    assert_eq!(read(&path), "");
}

// ─────────────────────────────────────────────────────────────
// exec：参数校验与文件错误
// ─────────────────────────────────────────────────────────────

#[test]
fn exec_missing_path_is_parse_error() {
    let result = exec_edit(&json!({"old_str": "a", "new_str": "b"}));
    assert_eq!(code(&result), "parse_error");
    assert!(result.model_text().contains("missing 'path'"));
}

#[test]
fn exec_missing_old_or_new_is_parse_error() {
    let missing_old = exec_edit(&json!({"path": "a.txt", "new_str": "b"}));
    assert!(missing_old.model_text().contains("missing 'old_str'"));
    let missing_new = exec_edit(&json!({"path": "a.txt", "old_str": "a"}));
    assert!(missing_new.model_text().contains("missing 'new_str'"));
}

#[test]
fn exec_empty_old_str_is_rejected() {
    let result = exec_edit(&json!({"path": "a.txt", "old_str": "", "new_str": "b"}));
    assert_eq!(code(&result), "parse_error");
    assert!(result.model_text().contains("must be non-empty"));
}

#[test]
fn exec_identical_old_and_new_is_rejected() {
    let result = exec_edit(&json!({"path": "a.txt", "old_str": "a", "new_str": "a"}));
    assert_eq!(code(&result), "parse_error");
    assert!(result.model_text().contains("identical"));
}

#[test]
fn exec_missing_file_does_not_create() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("ghost.txt").to_string_lossy().to_string();
    let result = call(&path, "a", "b");
    assert_eq!(code(&result), "file_not_found");
    assert!(!std::path::Path::new(&path).exists());
}

#[test]
fn exec_binary_file_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let path = dir.path().join("bin.dat");
    std::fs::write(&path, [0u8, 159, 146, 150]).unwrap();
    let result = call(&path.to_string_lossy(), "a", "b");
    assert_eq!(code(&result), "not_utf8_text");
}

// ─────────────────────────────────────────────────────────────
// 工具契约：只暴露三个字段
// ─────────────────────────────────────────────────────────────

#[test]
fn tool_contract_exposes_str_replace_fields() {
    let mut mgr = qaqh_workspace::ToolManager::new();
    super::register(&mut mgr);
    let handler = mgr.lookup("edit").expect("edit tool registered");
    let schema = &handler.input_schema;
    let required: Vec<&str> = schema["required"]
        .as_array()
        .unwrap()
        .iter()
        .map(|v| v.as_str().unwrap())
        .collect();
    assert_eq!(required, vec!["path", "old_str", "new_str"]);
    let props = schema["properties"].as_object().unwrap();
    assert_eq!(props.len(), 4, "schema: {schema}");
    assert_eq!(props["replace_all"]["type"], "boolean");
    assert_eq!(props["replace_all"]["default"], false);
    let all = format!("{} {}", handler.description, schema);
    for legacy in [
        "hunks",
        "kind",
        "context_before",
        "context_after",
        "hint_line",
        "expected_hash",
        "dry_run",
    ] {
        assert!(
            !all.contains(legacy),
            "legacy field '{legacy}' still exposed: {all}"
        );
    }
    assert!(handler.description.contains("exactly once"));
    assert!(handler.description.contains("replace_all"));
}

#[test]
fn exec_mixed_endings_preserve_surrounding_bytes() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_file(&dir, "mixed.txt", "a\r\nb\nc\r\nd\r\n");
    // 命中 LF 行 → 插入文本用 LF；文件其余字节原样保留。
    let result = call(&path, "b", "B1\nB2");
    assert!(result.is_success(), "{}", result.model_text());
    assert_eq!(read(&path), "a\r\nB1\nB2\nc\r\nd\r\n");
    // 命中 CRLF 行 → 插入文本用 CRLF；其余不变。
    let result = call(&path, "d", "D1\nD2");
    assert!(result.is_success(), "{}", result.model_text());
    assert_eq!(read(&path), "a\r\nB1\nB2\nc\r\nD1\r\nD2\r\n");
}

#[test]
fn exec_lone_cr_file_is_normalized_to_lf_on_edit() {
    let dir = tempfile::tempdir().unwrap();
    let path = write_file(&dir, "cr.txt", "a\rb\rc\r");
    let result = call(&path, "b", "B");
    assert!(result.is_success(), "{}", result.model_text());
    // 孤立 CR 的命中行按 LF 还原；其余孤立 CR 字节原样保留。
    assert_eq!(read(&path), "a\rB\rc\r");
}

#[cfg(unix)]
#[test]
fn exec_symlink_target_is_rejected() {
    let dir = tempfile::tempdir().unwrap();
    let target = dir.path().join("target.txt");
    std::fs::write(&target, "hello\n").unwrap();
    let link = dir.path().join("link.txt");
    std::os::unix::fs::symlink(&target, &link).unwrap();
    let result = call(&link.to_string_lossy(), "hello", "HELLO");
    assert_eq!(code(&result), "symlink_target");
    assert!(
        result.model_text().contains("symbolic link"),
        "{}",
        result.model_text()
    );
    assert_eq!(read(&target.to_string_lossy()), "hello\n");
    assert!(
        std::fs::symlink_metadata(&link)
            .unwrap()
            .file_type()
            .is_symlink()
    );
}

#[test]
fn typed_edit_registration_and_display_are_same_source() {
    let mut manager = qaqh_workspace::ToolManager::new();
    super::register(&mut manager);
    assert!(
        manager.builtin("edit").is_some(),
        "edit must be on the typed execution surface"
    );

    let dir = tempfile::tempdir().unwrap();
    let path = write_file(&dir, "typed.txt", "old\n");
    let result = call(&path, "old", "new");
    assert!(result.is_success(), "{}", result.model_text());
    assert_eq!(result.data["replaced"], json!(1));
    let display = result.display().expect("typed display");
    let display_text = match &display.body {
        Some(qaqh_types::ToolResultDisplayBody::Text { text, .. }) => text,
        other => panic!("unexpected edit display body: {other:?}"),
    };
    assert_eq!(display_text, result.model_text());
    let expected_summary = format!("{path} · +1 -1");
    assert_eq!(
        display.summary.as_deref(),
        Some(expected_summary.as_str()),
        "edit display summary must be path metadata, not the [OK] output first line"
    );
}
