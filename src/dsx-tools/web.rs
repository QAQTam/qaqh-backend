//! Web tools: search, fetch, Context7 documentation queries.


use crate::types::{SafetyLevel, ToolDef};
use super::{parse_arg, parse_opt};
const CONTEXT7_KEY: &str = "ctx7sk-91c0b5c4-6ca0-4d01-857a-50edbb0d4a33";
const CONTEXT7_ENDPOINT: &str = "https://mcp.context7.com/mcp";
// ── Context7 tools (live documentation) ──

#[allow(dead_code)]
pub(super) fn context7_resolve_def() -> ToolDef {
    ToolDef {
        call_type: "function".into(),
        function: crate::types::ToolFunction {
            name: "context7_resolve".into(),
            description: "Resolve library name to Context7 ID for doc queries.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "name": {"type": "string", "description": "Library name (e.g. Next.js, Prisma, React)"},
                    "query": {"type": "string", "description": "What you need to do with this library"}
                },
                "required": ["name", "query"],
                "additionalProperties": false
            }),
        },
    }
}

#[allow(dead_code)]
pub(super) fn context7_query_def() -> ToolDef {
    ToolDef {
        call_type: "function".into(),
        function: crate::types::ToolFunction {
            name: "context7_query".into(),
            description: "Get current docs and code examples for a library by Context7 ID.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "library_id": {"type": "string", "description": "Context7 library ID (e.g. /vercel/next.js) from context7_resolve"},
                    "query": {"type": "string", "description": "What you need to learn or implement"}
                },
                "required": ["library_id", "query"],
                "additionalProperties": false
            }),
        },
    }
}

pub(super) fn exec_context7_rpc(method: &str, args: &serde_json::Value) -> String {
    let client = match reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(15))
        .user_agent("dsx/3.2")
        .build()
    {
        Ok(c) => c,
        Err(e) => return format!("[ERROR] HTTP client: {}", e),
    };

    let body = serde_json::json!({
        "jsonrpc": "2.0",
        "method": "tools/call",
        "params": {
            "name": method,
            "arguments": args,
        },
        "id": 1,
    });

    let resp = match client
        .post(CONTEXT7_ENDPOINT)
        .header("Content-Type", "application/json")
        .header("Accept", "application/json, text/event-stream")
        .header("CONTEXT7_API_KEY", CONTEXT7_KEY)
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

    // Parse SSE response: extract data: {...} line
    for line in text.lines() {
        if let Some(json_str) = line.strip_prefix("data: ") {
            if let Ok(data) = serde_json::from_str::<serde_json::Value>(json_str) {
                if let Some(content) = data.get("result").and_then(|r| r.get("content")) {
                    if let Some(arr) = content.as_array() {
                        let mut out = String::from("[OK] Context7:\n");
                        for item in arr {
                            if let Some(text_val) = item.get("text").and_then(|t| t.as_str()) {
                                // Truncate very long responses
                                if text_val.len() > 15000 {
                                    let cut = text_val.char_indices().nth(15000).map(|(i, _)| i).unwrap_or(text_val.len()); out.push_str(&text_val[..cut]);
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
                // Check for errors
                if let Some(err) = data.get("error") {
                    let msg = err.get("message").and_then(|m| m.as_str()).unwrap_or("unknown");
                    return format!("[ERROR] Context7: {}", msg);
                }
            }
        }
    }

    format!("[ERROR] No result from Context7\n[HINT] The library may not exist. Try context7_resolve with a different name.")
}

pub(super) fn exec_context7_resolve(args: &str) -> String {
    let v = match serde_json::from_str::<serde_json::Value>(args) {
        Ok(v) => v,
        Err(_) => return "[ERROR] Invalid arguments. Expected: {name, query}".to_string(),
    };
    // Map name → libraryName (MCP API expects libraryName)
    let mut mapped = serde_json::Map::new();
    if let Some(name) = v.get("name").and_then(|v| v.as_str()) {
        mapped.insert("libraryName".to_string(), serde_json::json!(name));
    }
    if let Some(q) = v.get("query").and_then(|v| v.as_str()) {
        mapped.insert("query".to_string(), serde_json::json!(q));
    }
    exec_context7_rpc("resolve-library-id", &serde_json::Value::Object(mapped))
}

pub(super) fn exec_context7_query(args: &str) -> String {
    let v = match serde_json::from_str::<serde_json::Value>(args) {
        Ok(v) => v,
        Err(_) => return "[ERROR] Invalid arguments. Expected: {library_id, query}".to_string(),
    };
    // Map library_id → libraryId for Context7 API
    let mut mapped = serde_json::Map::new();
    if let Some(lid) = v.get("library_id").and_then(|v| v.as_str()) {
        mapped.insert("libraryId".to_string(), serde_json::json!(lid));
    } else {
        return "[ERROR] Missing required field: library_id".to_string();
    }
    if let Some(q) = v.get("query").and_then(|v| v.as_str()) {
        mapped.insert("query".to_string(), serde_json::json!(q));
    }
    exec_context7_rpc("query-docs", &serde_json::Value::Object(mapped))
}
#[allow(dead_code)]
pub(super) fn web_fetch_def() -> ToolDef {
    ToolDef {
        call_type: "function".into(),
        function: crate::types::ToolFunction {
            name: "web_fetch".into(),
            description: "Fetch a URL and return readable text directly in the result. The content is already formatted — no need to save to a file or call read_file afterwards. Max 100KB.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "url": {"type": "string", "description": "The URL to fetch"},
                    "output": {"type": "string", "description": "Optional: save the extracted text to this file path for later reference"}
                },
                "required": ["url"],
                "additionalProperties": false
            }),
        },
    }
}

pub(super) fn exec_web_fetch(args: &str) -> String {
    let url = parse_arg(args, "url");
    let client = match reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(30))
        .user_agent("dsx/3.2 (DeepSeek Coder)")
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
                    // Convert HTML to readable plain text (html2text 0.17 returns Result)
                    let readable = match html2text::from_read(body.as_bytes(), body.len().min(120_000)) {
                        Ok(t) => t,
                        Err(e) => return format!("[ERROR] html2text: {}", e),
                    };
                    let truncated = readable.len() > 100_000;
                    let display = if truncated {
                        let end = find_char_boundary(&readable, 100_000);
                        format!("{}... [truncated: {} total chars]", &readable[..end], readable.len())
                    } else { readable.clone() };
                    // Optional: save to local file
                    let output_path = parse_opt(args, "output");
                    let saved = if let Some(ref path) = output_path {
                        if let Some(parent) = std::path::Path::new(path).parent() {
                            let _ = std::fs::create_dir_all(parent);
                        }
                        match std::fs::write(path, &readable) {
                            Ok(_) => format!("\n📁 Saved to {}", path),
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

pub(super) fn find_char_boundary(s: &str, max: usize) -> usize {
    if max >= s.len() { return s.len(); }
    let mut end = max;
    while end > 0 && !s.is_char_boundary(end) { end -= 1; }
    end
}

#[allow(dead_code)]
pub(super) fn web_search_def() -> ToolDef {
    ToolDef {
        call_type: "function".into(),
        function: crate::types::ToolFunction {
            name: "web_search".into(),
            description: "Web search. Returns result titles and snippets.".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {"type": "string", "description": "Search query"}
                },
                "required": ["query"],
                "additionalProperties": false
            }),
        },
    }
}

pub(super) fn exec_web_search(args: &str) -> String {
    let query = parse_arg(args, "query");
    // Bing (cn.bing.com — accessible in China, no API key)
    let url = format!("https://cn.bing.com/search?q={}&setlang=zh-cn", urlencoding(&query));
    let client = match reqwest::blocking::Client::builder()
        .timeout(std::time::Duration::from_secs(10))
        .user_agent("dsx/3.2")
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
                    // Extract text from Bing result page (between <a> and <p> tags)
                    let mut results = Vec::new();
                    // Capture <a href="...">title</a>
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
                    // Also capture <p> snippets
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
                        format!("[OK] Bing: {}\n\n(no results — Bing may have blocked the request)", query)
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

pub(super) fn strip_html(s: &str) -> String {
    let mut result = String::new();
    let mut in_tag = false;
    for c in s.chars() {
        if c == '<' { in_tag = true; continue; }
        if c == '>' { in_tag = false; continue; }
        if !in_tag { result.push(c); }
    }
    result.trim().to_string()
}

pub(super) fn urlencoding(s: &str) -> String {
    s.chars().map(|c| {
        if c.is_ascii_alphanumeric() || c == '-' || c == '_' || c == '.' || c == '~' || c == ' ' {
            if c == ' ' { '+'.to_string() } else { c.to_string() }
        } else {
            format!("%{:02X}", c as u8)
        }
    }).collect()
}

// ── Consolidated web tool definition ──

pub fn web_def() -> ToolDef {
    ToolDef {
        call_type: "function".into(),
        function: crate::types::ToolFunction {
            name: "web".into(),
            description: "Web operations. Actions: fetch (get URL content as readable text), search (web search via Bing), context7_resolve (resolve library name to doc ID), context7_query (query library docs by ID).".into(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "action": {
                        "type": "string",
                        "enum": ["fetch", "search", "context7_resolve", "context7_query"],
                        "description": "Action to perform"
                    },
                    "url": {"type": "string", "description": "The URL to fetch (for fetch)"},
                    "query": {"type": "string", "description": "Search query (for search) or what you need to do (for context7)"},
                    "name": {"type": "string", "description": "Library name, e.g. Next.js, Prisma (for context7_resolve)"},
                    "library_id": {"type": "string", "description": "Context7 library ID, e.g. /vercel/next.js (for context7_query)"},
                    "output": {"type": "string", "description": "Optional: save extracted text to this file path (for fetch)"}
                },
                "required": ["action"],
                "additionalProperties": false
            }),
        },
    }
}

pub fn exec_web(args: &str) -> String {
    let action = parse_arg(args, "action");
    match action.as_str() {
        "fetch" => exec_web_fetch(args),
        "search" => exec_web_search(args),
        "context7_resolve" => exec_context7_resolve(args),
        "context7_query" => exec_context7_query(args),
        _ => format!("[ERROR] Unknown web action: {}. Valid: fetch, search, context7_resolve, context7_query", action),
    }
}

pub fn classify_web(args: &str) -> SafetyLevel {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(args) {
        match v.get("action").and_then(|a| a.as_str()) {
            Some("search" | "context7_resolve" | "context7_query") => SafetyLevel::Safe,
            Some("fetch") => SafetyLevel::Confirm,
            _ => SafetyLevel::Confirm,
        }
    } else {
        SafetyLevel::Confirm
    }
}
