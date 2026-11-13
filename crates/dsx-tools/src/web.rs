//! Web tools: search, fetch, Context7 documentation queries.

use crate::{ToolCallCtx, ToolResult};

// Context7 API key from environment variable (not hardcoded).
// Fallback for backward compatibility during transition.
const CONTEXT7_ENDPOINT: &str = "https://mcp.context7.com/mcp";

fn context7_key() -> String {
    std::env::var("CONTEXT7_API_KEY").unwrap_or_else(|_| {
        "ctx7sk-91c0b5c4-6ca0-4d01-857a-50edbb0d4a33".to_string()
    })
}

// ── Handler 函数（新 IPC 框架）──

pub fn handle_fetch(ctx: ToolCallCtx) -> ToolResult {
    let args = build_args_json(&ctx);
    ToolResult::ok(exec_web_fetch(&args))
}

pub fn handle_search(ctx: ToolCallCtx) -> ToolResult {
    let args = build_args_json(&ctx);
    ToolResult::ok(exec_web_search(&args))
}

pub fn handle_c7_resolve(ctx: ToolCallCtx) -> ToolResult {
    let args = build_args_json(&ctx);
    ToolResult::ok(exec_context7_resolve(&args))
}

pub fn handle_c7_query(ctx: ToolCallCtx) -> ToolResult {
    let args = build_args_json(&ctx);
    ToolResult::ok(exec_context7_query(&args))
}

fn build_args_json(ctx: &ToolCallCtx) -> String {
    serde_json::to_string(&ctx.args).unwrap_or_default()
}

// ── Context7 tools (live documentation) ──

fn exec_context7_rpc(method: &str, args: &serde_json::Value) -> String {
    let client = match reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .user_agent("dsx/4.0")
        .build()
    {
        Ok(c) => c,
        Err(e) => return format!("[ERROR] HTTP client: {}", e),
    };

    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "tools/call",
        "params": { "name": method, "arguments": args },
        "id": 1,
    });

    let resp = match client
        .post(CONTEXT7_ENDPOINT)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("CONTEXT7_API_KEY", context7_key())
        .json(&body)
        .send()
    {
        Ok(r) => r,
        Err(e) => return format!("[ERROR] Context7 request failed: {}", e),
    };

    let text = match resp.text() {
        Ok(t) => t,
        Err(e) => return format!("[ERROR] Read response: {}", e),
    };

    for line in text.lines() {
        if let Some(json_str) = line.strip_prefix("data: ") {
            if let Ok(data) = serde_json::from_str::<serde_json::Value>(json_str) {
                if let Some(content) = data.get("result").and_then(|r| r.get("content")) {
                    if let Some(arr) = content.as_array() {
                        let mut out = String::from("[OK] Context7:\n");
                        for item in arr {
                            if let Some(text_val) = item.get("text").and_then(|t| t.as_str()) {
                                if text_val.len() > 15000 {
                                    let cut = text_val.char_indices().nth(15000).map(|(i, _)| i).unwrap_or(text_val.len());
                                    out.push_str(&text_val[..cut]);
                                    out.push_str(&format!("\n... [truncated: {} total chars]", text_val.len()));
                                } else {
                                    out.push_str(text_val);
                                }
                                out.push('\n');
                            }
                        }
                        return out;
                    }
                }
                if let Some(err) = data.get("error") {
                    let msg = err.get("message").and_then(|m| m.as_str()).unwrap_or("unknown");
                    return format!("[ERROR] Context7: {}", msg);
                }
            }
        }
    }
    format!("[ERROR] No result from Context7\n[HINT] The library may not exist. Try context7_resolve with a different name.")
}

fn exec_context7_resolve(args: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(args).unwrap_or_default();
    let mut mapped = serde_json::Map::new();
    if let Some(name) = v.get("name").and_then(|v| v.as_str()) {
        mapped.insert("libraryName".to_string(), serde_json::json!(name));
    }
    if let Some(q) = v.get("query").and_then(|v| v.as_str()) {
        mapped.insert("query".to_string(), serde_json::json!(q));
    }
    // Also check top-level key directly (IPC ctx passes the original args)
    if !mapped.contains_key("libraryName") {
        if let Some(name) = v.get("libraryName").and_then(|v| v.as_str()) {
            mapped.insert("libraryName".to_string(), serde_json::json!(name));
        }
    }
    exec_context7_rpc("resolve-library-id", &serde_json::Value::Object(mapped))
}

fn exec_context7_query(args: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(args).unwrap_or_default();
    let mut mapped = serde_json::Map::new();
    if let Some(lid) = v.get("library_id").and_then(|v| v.as_str()) {
        mapped.insert("libraryId".to_string(), serde_json::json!(lid));
    } else if let Some(lid) = v.get("libraryId").and_then(|v| v.as_str()) {
        mapped.insert("libraryId".to_string(), serde_json::json!(lid));
    } else {
        return "[ERROR] Missing required field: library_id".to_string();
    }
    if let Some(q) = v.get("query").and_then(|v| v.as_str()) {
        mapped.insert("query".to_string(), serde_json::json!(q));
    }
    exec_context7_rpc("query-docs", &serde_json::Value::Object(mapped))
}

// ── Web fetch ──

fn exec_web_fetch(args: &str) -> String {
    let url = parse_arg(args, "url");
    let client = match reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .user_agent("dsx/4.0")
        .build()
    {
        Ok(c) => c,
        Err(e) => return format!("[ERROR] Cannot create HTTP client: {}\n[HINT] Internal error.", e),
    };
    match client.get(&url).send() {
        Ok(resp) => {
            let status = resp.status();
            match resp.text() {
                Ok(body) => {
                    let readable = match html2text::from_read(body.as_bytes(), body.len().min(120_000)) {
                        Ok(t) => t,
                        Err(e) => return format!("[ERROR] html2text: {}", e),
                    };
                    let truncated = readable.len() > 100_000;
                    let display = if truncated {
                        let end = find_char_boundary(&readable, 100_000);
                        format!("{}... [truncated: {} total chars]", &readable[..end], readable.len())
                    } else { readable.clone() };
                    let output_path = parse_opt(args, "output");
                    let saved = if let Some(ref path) = output_path {
                        if let Some(parent) = std::path::Path::new(path).parent() {
                            let _ = std::fs::create_dir_all(parent);
                        }
                        match std::fs::write(path, &readable) {
                            Ok(_) => format!("\nSaved to {}", path),
                            Err(e) => format!("\n[HINT] Could not save to {}: {}", path, e),
                        }
                    } else { String::new() };
                    if status.is_success() {
                        format!("[OK] {} ({} chars)\n\n{}{}", status, display.len(), display, saved)
                    } else {
                        format!("[PARTIAL] HTTP {}\n\n{}{}", status, display, saved)
                    }
                }
                Err(e) => format!("[ERROR] Cannot read response body: {}\n[HINT] The URL may not return text.", e),
            }
        }
        Err(e) => format!("[ERROR] Cannot fetch {}: {}\n[HINT] Check the URL or network.", url, e),
    }
}

fn find_char_boundary(s: &str, max: usize) -> usize {
    if max >= s.len() { return s.len(); }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) { end -= 1; }
    end
}

// ── Web search ──

fn exec_web_search(args: &str) -> String {
    let query = parse_arg(args, "query");
    let url = format!("https://cn.bing.com/search?q={}&setlang=zh-cn", urlencoding(&query));
    let client = match reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .user_agent("dsx/4.0")
        .build()
    {
        Ok(c) => c,
        Err(e) => return format!("[ERROR] Cannot create HTTP client: {}", e),
    };
    match client.get(&url).send() {
        Ok(resp) => {
            let _status = resp.status();
            match resp.text() {
                Ok(body) => {
                    let mut results = Vec::new();
                    for cap in body.match_indices("<a href=\"http") {
                        let rest = &body[cap.0..];
                        if let Some(end) = rest.find("</a>") {
                            let tag = &rest[..end + 4];
                            let text = strip_html(tag);
                            if !text.is_empty() && text.len() > 5 {
                                results.push(text);
                            }
                        }
                        if results.len() >= 15 { break; }
                    }
                    for cap in body.match_indices("<p>") {
                        let rest = &body[cap.0..];
                        if let Some(end) = rest.find("</p>") {
                            let text = strip_html(&rest[..end + 4]);
                            let t = text.trim();
                            if t.len() > 10 && !results.contains(&t.to_string()) {
                                results.push(format!("  {}", t));
                            }
                        }
                        if results.len() >= 25 { break; }
                    }
                    if results.is_empty() {
                        format!("[OK] Bing: {}\n\n(no results)", query)
                    } else {
                        format!("[OK] Bing search: {}\n\n{}", query, results.join("\n"))
                    }
                }
                Err(e) => format!("[ERROR] Cannot read search results: {}", e),
            }
        }
        Err(e) => format!("[ERROR] Search failed: {}\n[HINT] Check network connection.", e),
    }
}

fn strip_html(s: &str) -> String {
    let mut result = String::new();
    let mut in_tag = false;
    for c in s.chars() {
        if c == '<' { in_tag = true; continue; }
        if c == '>' { in_tag = false; continue; }
        if !in_tag { result.push(c); }
    }
    result.trim().to_string()
}

fn urlencoding(s: &str) -> String {
    s.chars().map(|c| {
        if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' || c == '~' || c == ' ' {
            if c == ' ' { '+'.to_string() } else { c.to_string() }
        } else {
            format!("%{:02X}", c as u8)
        }
    }).collect()
}

// ── 注册入口 ──

use crate::{ToolHandler, ToolKey, SafetyVerdict};
use std::time::Duration;

pub fn register(mgr: &mut crate::ToolManager) {
    mgr.register(ToolHandler {
        key: ToolKey::new("web_fetch", ""),
        description: "Fetch a URL and return readable text content.",
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "url": {"type": "string"},
                "output": {"type": "string"}
            },
            "required": ["url"],
            "additionalProperties": false
        }),
        handler: handle_fetch,
        safety: |_| SafetyVerdict::allowed(),
        default_timeout: Duration::from_secs(30),
    });
    mgr.register(ToolHandler {
        key: ToolKey::new("web_search", ""),
        description: "Search the web (Bing).",
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "query": {"type": "string"}
            },
            "required": ["query"],
            "additionalProperties": false
        }),
        handler: handle_search,
        safety: |_| SafetyVerdict::allowed(),
        default_timeout: Duration::from_secs(15),
    });
    mgr.register(ToolHandler {
        key: ToolKey::new("context7_resolve", ""),
        description: "Resolve a library name to a Context7 library ID.",
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "name": {"type": "string", "description": "Library name"},
                "query": {"type": "string"}
            },
            "required": ["name"],
            "additionalProperties": false
        }),
        handler: handle_c7_resolve,
        safety: |_| SafetyVerdict::allowed(),
        default_timeout: Duration::from_secs(15),
    });
    mgr.register(ToolHandler {
        key: ToolKey::new("context7_query", ""),
        description: "Query Context7 documentation for a library.",
        input_schema: serde_json::json!({
            "type": "object",
            "properties": {
                "library_id": {"type": "string"},
                "query": {"type": "string"}
            },
            "required": ["library_id"],
            "additionalProperties": false
        }),
        handler: handle_c7_query,
        safety: |_| SafetyVerdict::allowed(),
        default_timeout: Duration::from_secs(15),
    });
}

// ── 参数解析（兼容垫片）──

fn parse_arg(args: &str, key: &str) -> String {
    serde_json::from_str::<serde_json::Value>(args)
        .ok()
        .and_then(|v| v.get(key).and_then(|v| v.as_str()).map(String::from))
        .unwrap_or_default()
}

fn parse_opt(args: &str, key: &str) -> Option<String> {
    serde_json::from_str::<serde_json::Value>(args)
        .ok()
        .and_then(|v| v.get(key).and_then(|v| v.as_str()).map(String::from))
}
