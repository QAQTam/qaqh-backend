//! Arg parsers: extract action, path, command from tool call JSON.

/// Extract the "action" field from tool args JSON.
pub fn tool_action(args: &str) -> String {
    serde_json::from_str::<serde_json::Value>(args)
        .ok()
        .and_then(|v| v.get("action").and_then(|a| a.as_str().map(|s| s.to_string())))
        .unwrap_or_default()
}

/// Parse file path from tool args JSON.
pub fn parse_file_arg(args: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(args).ok()
        .and_then(|v| v.get("path").and_then(|p| p.as_str()).map(|s| s.to_string()))
}

/// Parse command string from exec tool args JSON.
pub fn parse_cmd_arg(args: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(args).ok()
        .and_then(|v| v.get("command").and_then(|c| c.as_str()).map(|s| s.to_string()))
}
