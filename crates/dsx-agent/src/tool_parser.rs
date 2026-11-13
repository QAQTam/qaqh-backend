use dsx_types::{ToolCall, FunctionCall, ToolDef};

/// Parse DeepSeek v4 XML-style tool calls from text content.
pub fn parse_xml_tool_calls(content: &str, tool_names: &[String]) -> (String, Vec<ToolCall>) {
    let mut cleaned = String::with_capacity(content.len());
    let mut tool_calls = Vec::new();
    let mut tc_index = 0u32;

    let mut tag_map: Vec<(String, String)> = tool_names.iter()
        .map(|n| (n.clone(), n.clone()))
        .collect();
    for (alias, name) in &[("read", "read_file"), ("write", "write_file"), ("edit", "edit_file")] {
        if tool_names.iter().any(|n| n == name) {
            tag_map.push((alias.to_string(), name.to_string()));
        }
    }

    let mut remaining = content;

    'outer: while let Some(tc_start) = remaining.find('<') {
        if remaining[tc_start..].starts_with("<tool_use>") {
            if let Some((name, args_text, rest)) = parse_tool_use_block(&remaining[tc_start..]) {
                cleaned.push_str(&remaining[..tc_start]);
                let arguments = normalize_args(&args_text);
                tool_calls.push(ToolCall {
                    id: format!("xml_tc_{tc_index}"),
                    call_type: "function".to_string(),
                    function: FunctionCall { name, arguments },
                });
                tc_index += 1;
                remaining = rest;
                continue 'outer;
            }
        }

        let after_lt = &remaining[tc_start..];
        if let Some(end) = after_lt.find('>') {
            let tag_name = &after_lt[1..end];
            let closing = format!("</{tag_name}>");

            if let Some((_, tool_name)) = tag_map.iter().find(|(tag, _)| *tag == tag_name) {
                if let Some(close_pos) = after_lt[end..].find(&closing) {
                    let block_content = &after_lt[end + 1..end + close_pos];
                    let full_xml_len = end + close_pos + closing.len();

                    cleaned.push_str(&remaining[..tc_start]);

                    let args = extract_child_args(block_content);
                    let arguments = normalize_args(&args);

                    tool_calls.push(ToolCall {
                        id: format!("xml_tc_{tc_index}"),
                        call_type: "function".to_string(),
                        function: FunctionCall {
                            name: tool_name.to_string(),
                            arguments,
                        },
                    });
                    tc_index += 1;
                    remaining = &after_lt[full_xml_len..];
                    continue 'outer;
                }
            }
        }

        cleaned.push_str(&remaining[..tc_start + 1]);
        remaining = &remaining[tc_start + 1..];
    }

    cleaned.push_str(remaining);
    (cleaned.trim().to_string(), tool_calls)
}

fn parse_tool_use_block(s: &str) -> Option<(String, String, &str)> {
    let prefix = "<tool_use>";
    let suffix = "</tool_use>";
    let s = s.strip_prefix(prefix)?;
    let end = s.find(suffix)?;
    let block = &s[..end];
    let rest = &s[end + suffix.len()..];

    let n_open = "<tool_name>";
    let n_close = "</tool_name>";
    let ns = block.find(n_open)?;
    let after_ns = &block[ns + n_open.len()..];
    let ne = after_ns.find(n_close)?;
    let name = after_ns[..ne].trim().to_string();
    let args_text = after_ns[ne + n_close.len()..].trim().to_string();
    Some((name, args_text, rest))
}

fn extract_child_args(xml: &str) -> String {
    let mut map = serde_json::Map::new();
    let mut s = xml.trim();
    while let Some(lt) = s.find('<') {
        if s[lt..].starts_with("</") || s[lt..].starts_with("<![") {
            s = &s[lt + 1..];
            continue;
        }
        if let Some(gt) = s[lt..].find('>') {
            let tag = &s[lt + 1..lt + gt];
            let closing = format!("</{tag}>");
            if let Some(close) = s[lt + gt + 1..].find(&closing) {
                let value = s[lt + gt + 1..lt + gt + 1 + close].trim();
                map.insert(tag.to_string(), serde_json::json!(value));
                s = &s[lt + gt + 1 + close + closing.len()..];
            } else {
                s = &s[lt + 1..];
            }
        } else {
            break;
        }
    }
    serde_json::to_string(&map).unwrap_or_else(|_| "{}".into())
}

fn normalize_args(args_text: &str) -> String {
    let trimmed = args_text.trim();
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(trimmed) {
        if v.is_object() {
            return trimmed.to_string();
        }
    }
    serde_json::json!({"query": trimmed}).to_string()
}

/// Parse DeepSeek DSML tool calls from content.
/// Schema-aware: uses tool_defs for type-safe parameter conversion.
/// Format: <\u{ff5c}DSML\u{ff5c}tool_calls>...<｜DSML｜invoke name="fn">...</｜DSML｜invoke>...
pub fn parse_dsml_tool_calls(content: &str, tool_defs: &[ToolDef]) -> (String, Vec<ToolCall>) {
    let dsml = "\u{ff5c}DSML\u{ff5c}";
    let tc_start = format!("<{dsml}tool_calls>");
    let tc_end = format!("</{dsml}tool_calls>");
    let invoke_tag = format!("<{dsml}invoke ");
    let invoke_close = format!("</{dsml}invoke>");
    let param_tag = format!("<{dsml}parameter ");
    let param_close = format!("</{dsml}parameter>");

    // Build schema lookup: function_name → property_name → {type}
    let mut schema_map: std::collections::HashMap<&str, &serde_json::Map<String, serde_json::Value>> = std::collections::HashMap::new();
    for td in tool_defs {
        if let Some(props) = td.function.parameters.get("properties").and_then(|v| v.as_object()) {
            schema_map.insert(td.function.name.as_str(), props);
        }
    }

    let mut cleaned = String::new();
    let mut tool_calls = Vec::new();
    let mut idx = 0u32;

    let mut remaining = content;
    loop {
        let Some(tc_pos) = remaining.find(&tc_start) else {
            cleaned.push_str(remaining);
            break;
        };
        cleaned.push_str(&remaining[..tc_pos]);
        let after_tc = &remaining[tc_pos + tc_start.len()..];

        let Some(tc_end_pos) = after_tc.find(&tc_end) else {
            cleaned.push_str(&remaining[tc_pos..]);
            break;
        };
        let block = &after_tc[..tc_end_pos];
        remaining = &after_tc[tc_end_pos + tc_end.len()..];

        let mut bq = block;
        loop {
            let Some(inv_pos) = bq.find(&invoke_tag) else { break };
            let after_inv = &bq[inv_pos + invoke_tag.len()..];
            let name = extract_attr_value(after_inv, "name").unwrap_or_default();

            let Some(inv_end) = after_inv.find(&invoke_close) else { break };
            let body = &after_inv[..inv_end];
            bq = &after_inv[inv_end + invoke_close.len()..];

            let param_types = schema_map.get(name.as_str()).copied();
            let args = extract_dsml_params_typed(body, &param_tag, &param_close, param_types);

            tool_calls.push(ToolCall {
                id: format!("dsml_tc_{idx}"),
                call_type: "function".to_string(),
                function: FunctionCall { name, arguments: args },
            });
            idx += 1;
        }
    }

    (cleaned.trim().to_string(), tool_calls)
}

fn extract_attr_value(s: &str, attr: &str) -> Option<String> {
    let pattern = format!("{attr}=\"");
    let pos = s.find(&pattern)?;
    let after = &s[pos + pattern.len()..];
    let end = after.find('"')?;
    Some(after[..end].to_string())
}

fn repair_param_dict(
    map: &serde_json::Map<String, serde_json::Value>,
    param_types: &serde_json::Map<String, serde_json::Value>,
) -> serde_json::Map<String, serde_json::Value> {
    let allowed: std::collections::HashSet<&str> = param_types.keys().map(|k| k.as_str()).collect();
    for wrapper in &["arguments", "input"] {
        if map.len() == 1 && !allowed.contains(wrapper) {
            if let Some(inner) = map.get(*wrapper) {
                if let Some(obj) = inner.as_object() {
                    let obj_keys: std::collections::HashSet<&str> = obj.keys().map(|k| k.as_str()).collect();
                    if obj_keys.is_subset(&allowed) {
                        return obj.clone();
                    }
                }
            }
        }
    }
    map.clone()
}

fn convert_param_value(value: &str, param_type: Option<&str>) -> serde_json::Value {
    if value.eq_ignore_ascii_case("null") {
        return serde_json::Value::Null;
    }
    match param_type {
        Some("integer") | Some("int") => {
            match value.parse::<i64>() {
                Ok(n) => serde_json::json!(n),
                Err(_) => serde_json::json!(value),
            }
        }
        Some("number") | Some("float") => {
            value.parse::<f64>().map(|f| {
                if f == f.trunc() && f.is_finite() {
                    serde_json::json!(f as i64)
                } else {
                    serde_json::json!(f)
                }
            }).unwrap_or_else(|_| serde_json::json!(value))
        }
        Some("boolean") | Some("bool") => {
            let v = value.trim().to_lowercase();
            serde_json::json!(v == "true" || v == "1")
        }
        Some("array") | Some("object") => {
            serde_json::from_str(value).unwrap_or_else(|_| serde_json::json!(value))
        }
        _ => {
            // unknown type → try JSON, fallback to string
            serde_json::from_str(value).unwrap_or_else(|_| serde_json::json!(value))
        }
    }
}

fn extract_dsml_params_typed(
    body: &str,
    param_tag: &str,
    param_close: &str,
    param_types: Option<&serde_json::Map<String, serde_json::Value>>,
) -> String {
    let mut map = serde_json::Map::new();
    let mut rem = body;
    loop {
        let Some(p) = rem.find(param_tag) else { break };
        let after = &rem[p + param_tag.len()..];
        let name = extract_attr_value(after, "name").unwrap_or_default();
        let str_attr = extract_attr_value(after, "string").unwrap_or_default();

        let Some(end) = after.find(param_close) else { break };
        let value_text = after[..end].trim();
        rem = &after[end + param_close.len()..];

        let value = if str_attr == "true" {
            serde_json::json!(value_text)
        } else {
            let param_type = param_types.and_then(|pt| pt.get(&name)).and_then(|s| s.get("type")).and_then(|t| t.as_str());
            convert_param_value(value_text, param_type)
        };
        map.insert(name, value);
    }

    if let Some(pt) = param_types {
        map = repair_param_dict(&map, pt);
    }

    serde_json::to_string(&map).unwrap_or_else(|_| "{}".into())
}

/// Convert HP tool_calls JSON (flat or nested) to ToolCall vec.
pub fn parse_tool_calls(tcs: &serde_json::Value) -> Vec<ToolCall> {
    let arr = match tcs.as_array() { Some(a) => a, None => return vec![] };
    arr.iter().filter_map(|tc| {
        let id = tc.get("id").and_then(|i| i.as_str()).unwrap_or("");
        let (name, arguments) = if let Some(func) = tc.get("function") {
            let n = func.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let a = func.get("arguments").and_then(|v| v.as_str()).unwrap_or("{}");
            (n, a)
        } else {
            let n = tc.get("name").and_then(|v| v.as_str()).unwrap_or("");
            let a = tc.get("arguments").and_then(|v| v.as_str()).unwrap_or("{}");
            (n, a)
        };
        if id.is_empty() || name.is_empty() { return None; }
        Some(ToolCall {
            id: id.to_string(),
            call_type: "function".to_string(),
            function: FunctionCall { name: name.to_string(), arguments: arguments.to_string() },
        })
    }).collect()
}
