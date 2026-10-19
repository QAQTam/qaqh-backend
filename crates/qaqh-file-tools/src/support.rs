//! 工具结果信封助手（自门面 lib.rs 收敛而来，qaqh-workspace 转为 re-export）。

use qaqh_types::ToolResult;
use qaqh_types::platform::now_utc8;

/// 原始 args 里的文件路径：canonical `path`，兼收 Claude 标准拼写 `file_path`。
///
/// 供**读原始 JSON 的投影层**使用（display / code_delta）。typed 反序列化那一侧
/// 由各 Args 的 `#[serde(alias = "file_path")]` 承担，两处必须同源，否则会出现
/// 「工具执行成功但 UI 头部的路径是空的」。
pub fn arg_path(args: &serde_json::Value) -> Option<&str> {
    args.get("path")
        .or_else(|| args.get("file_path"))
        .and_then(serde_json::Value::as_str)
}

/// Build a JSON success response for tools that only need a status message.
/// Extra fields can be added via `extra`.
pub fn json_ok(extra: serde_json::Value) -> String {
    let mut v = serde_json::json!({"timeis": now_utc8(), "status": "ok"});
    if let Some(obj) = v.as_object_mut() {
        if let Some(ext) = extra.as_object() {
            for (k, val) in ext {
                obj.insert(k.clone(), val.clone());
            }
        } else if !extra.is_null() {
            obj.insert("content".to_string(), extra);
        }
    }
    v.to_string()
}

/// Build a structured error [`ToolResult`] (canonical `ToolError` fields:
/// code / message / retryable / hint). Historic name retained; the legacy
/// JSON-envelope string form only survives in `todo.rs` (its `Err(String)`
/// channel crosses into `qaqh-runtime::service` — see `todo_err` there).
pub fn json_err(
    code: impl Into<String>,
    message: impl Into<String>,
    hint: impl Into<String>,
) -> ToolResult {
    let hint = hint.into();
    ToolResult::error_with(code, message, false, Some(hint).filter(|h| !h.is_empty()))
}

/// Legacy string-JSON error envelope — ONLY for functions whose error channel
/// is `String` (todo.rs `Err(String)` crosses into `qaqh-runtime::service`;
/// web.rs `web_fetch` returns `String`). Do not use in new code: prefer
/// [`json_err`] which returns a structured [`ToolResult`].
pub fn json_err_string(
    code: impl Into<String>,
    message: impl Into<String>,
    hint: impl Into<String>,
) -> String {
    serde_json::json!({
        "timeis": now_utc8(),
        "status": "error",
        "code": code.into(),
        "message": message.into(),
        "hint": hint.into(),
    })
    .to_string()
}
