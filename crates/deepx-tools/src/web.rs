//! Web tool — fetch URLs and search the web (Bing RSS).

use crate::{JsonArgs, ToolCallCtx, ToolHandler, ToolResult, ToolRisk};
use std::time::Duration;

fn http_agent(timeout_secs: u64) -> ureq::Agent {
    ureq::Agent::config_builder()
        .timeout_global(Some(Duration::from_secs(timeout_secs)))
        .build()
        .into()
}

pub(super) fn handle_web(ctx: ToolCallCtx) -> ToolResult {
    let timeout_secs = ctx.timeout_secs.unwrap_or(30);
    if ctx.args.s("url").starts_with("http") {
        ToolResult::ok(&web_fetch(&ctx.args, timeout_secs))
    } else {
        ToolResult::ok(&web_search(&ctx.args, timeout_secs))
    }
}

fn web_fetch(args: &serde_json::Value, timeout_secs: u64) -> String {
    const MAX_WEB_BODY_BYTES: u64 = 512 * 1024;
    let url = args.s("url");
    if url.is_empty() || !url.starts_with("http") {
        return crate::json_err("INVALID_URL", "web: url must start with http", "");
    }
    let resp = match http_agent(timeout_secs)
        .get(&url)
        .header("User-Agent", "Mozilla/5.0 (compatible; DeepX/0.7)")
        .call()
    {
        Ok(r) => r,
        Err(e) => return crate::json_err("FETCH_ERROR", &format!("{e}"), ""),
    };
    if resp
        .body()
        .content_length()
        .is_some_and(|len| len > MAX_WEB_BODY_BYTES)
    {
        return crate::json_err(
            "RESPONSE_TOO_LARGE",
            &format!("Response exceeds the {} byte limit", MAX_WEB_BODY_BYTES),
            "Fetch a narrower URL or use a source with a paginated API.",
        );
    }
    let is_html = resp
        .headers()
        .get("Content-Type")
        .and_then(|v| v.to_str().ok())
        .map(|s| s.contains("html"))
        .unwrap_or(false);
    let body = match resp
        .into_body()
        .with_config()
        .limit(MAX_WEB_BODY_BYTES)
        .read_to_string()
    {
        Ok(b) => b,
        Err(_) => {
            return crate::json_err(
                "READ_ERROR",
                "Response could not be read within the body limit",
                "Fetch a narrower URL or use a source with a paginated API.",
            );
        }
    };
    let readable = if is_html || body.trim_start().starts_with("<") {
        html2text::from_read(body.as_bytes(), body.len().min(120_000)).unwrap_or(body)
    } else {
        body
    };
    if let Some(out) = args.get("output").and_then(|v| v.as_str()) {
        let _ = std::fs::write(crate::resolve_workspace_path(out), &readable);
    }
    crate::json_ok(serde_json::json!({"content": readable}))
}

const BING: &str = "https://cn.bing.com/search?format=rss&q=";

fn web_search(args: &serde_json::Value, timeout_secs: u64) -> String {
    let q = args.s("query");
    if q.is_empty() {
        return crate::json_err("MISSING_QUERY", "web: 'query' or 'url' required", "");
    }
    let resp = match http_agent(timeout_secs)
        .get(&format!("{BING}{}", urlenc(&q)))
        .header("User-Agent", "Mozilla/5.0 (compatible; DeepX/0.7)")
        .call()
    {
        Ok(r) => r,
        Err(e) => return crate::json_err("BING_ERROR", &format!("{e}"), ""),
    };
    let body = match resp.into_body().read_to_string() {
        Ok(b) => b,
        Err(_) => return crate::json_err("BING_ERROR", "read failed", ""),
    };
    let mut results: Vec<serde_json::Value> = Vec::new();
    let mut pos = 0;
    while let Some(s) = body[pos..].find("<item>") {
        pos += s;
        let start = pos;
        if let Some(e) = body[pos..].find("</item>") {
            pos += e + 7;
        } else {
            break;
        }
        let xml = &body[start..pos];
        let t = xml_tag(xml, "title");
        let l = xml_tag(xml, "link");
        let sn = strip_html(&xml_tag(xml, "description"));
        if !t.is_empty() && !l.is_empty() {
            results.push(serde_json::json!({"title":t,"url":l,"snippet":sn}));
            if results.len() >= 10 {
                break;
            }
        }
    }
    if results.is_empty() {
        return crate::json_ok(serde_json::json!({"query":q,"results":[],"source":"bing"}));
    }
    crate::json_ok(serde_json::json!({"query":q,"results":results,"source":"bing"}))
}

fn urlenc(s: &str) -> String {
    let mut o = String::new();
    for b in s.bytes() {
        match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => {
                o.push(b as char)
            }
            b' ' => o.push_str("%20"),
            _ => {
                o.push('%');
                o.push_str(&format!("{:02X}", b));
            }
        }
    }
    o
}
fn xml_tag(xml: &str, tag: &str) -> String {
    let o = format!("<{}>", tag);
    let c = format!("</{}>", tag);
    if let (Some(s), Some(e)) = (xml.find(&o), xml.find(&c)) {
        xml[s + o.len()..e].to_string()
    } else {
        String::new()
    }
}
/// Lightweight XML entity decoder and HTML stripper for RSS snippets.
///
/// Bing RSS is the primary consumer.  Its format is stable and predictable,
/// so a full XML parser (`quick-xml`) is not warranted — but document the
/// assumptions so a future reader can judge the trade-off.
///
/// **Limitations:**
/// - No CDATA section support (Bing RSS does not use them).
/// - Tag matching is case-sensitive (correct for XML).
/// - Entity decoding covers the five XML pre-defined entities plus
///   numeric `&#NNN;` / `&#xNNN;` references.
fn strip_html(s: &str) -> String {
    let mut o = String::new();
    let mut t = false;
    for c in s.chars() {
        match c {
            '<' => t = true,
            '>' => t = false,
            _ if !t => o.push(c),
            _ => {}
        }
    }
    // Pre-defined XML entities
    let s = o
        .replace("&amp;", "&")
        .replace("&lt;", "<")
        .replace("&gt;", ">")
        .replace("&quot;", "\"")
        .replace("&apos;", "'")
        .replace("&#39;", "'");
    // Numeric character references (&#NNN; and &#xNNN;)
    // Use a simple regex; the `regex` crate is already a dependency.
    regex::Regex::new(r"&#x?([0-9a-fA-F]+);")
        .ok()
        .map(|re| {
            re.replace_all(&s, |caps: &regex::Captures| {
                let num = &caps[1];
                let radix = if caps[0].contains('x') { 16 } else { 10 };
                u32::from_str_radix(num, radix)
                    .ok()
                    .and_then(char::from_u32)
                    .map(|c| c.to_string())
                    .unwrap_or_else(|| caps[0].to_string())
            })
            .to_string()
        })
        .unwrap_or(s)
}

pub fn register(mgr: &mut crate::ToolManager) {
    mgr.register(ToolHandler { key: "web".to_string(),
        description: "Web operations: fetch URL content (pass 'url') or search the web via Bing RSS (pass 'query').",
        input_schema: serde_json::json!({"type":"object","properties":{"url":{"type":"string","description":"URL to fetch"},"query":{"type":"string","description":"Search query"},"output":{"type":"string","description":"Optional file path"}},"required":[],"additionalProperties":false}),
        handler: handle_web, risk: ToolRisk::ReadOnly, default_timeout: std::time::Duration::from_secs(30),
    });
}
