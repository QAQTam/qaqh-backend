//! PTC 模式的模型面：把工具 schema 投影成一段 TypeScript 声明。
//!
//! 这是**另一种呈现**，不是另一个执行面：同一份注册表，`native` 发 JSON Schema，
//! `ptc` 发 `declare const tools {...}`。因此本 crate 只做纯映射，没有执行逻辑，
//! 也不认识工具语义。
//!
//! qaqh 的 schema 生成约定（`qaqh_tool_core::schema`）决定了这里的输入形状：
//! draft 2020-12、无 `$schema`、无 `$defs`/`$ref`、可选字段只体现为「不在
//! `required` 里」、`#[serde(deny_unknown_fields)]` 生成 `additionalProperties:
//! false`。渲染器仍然对 `oneOf`/`anyOf`/`$ref` 做防御性降级，避免将来 schema
//! 变体直接产出非法 TS。
//!
//! 输出必须**字节稳定**：工具按名字字典序排列，描述折叠成单行 JSDoc，缩进固定
//! 两空格。这样系统提示词前缀才能被 provider 缓存复用。

#![deny(missing_docs)]

use qaqh_ptc_core::PtcProgramFormat;
use serde_json::Value;

/// 渲染需要的一个工具：模型面 schema + 规范输出 schema。
#[derive(Debug, Clone)]
pub struct ToolSchema {
    /// 工具名（任意字符串；渲染时按需加引号）。
    pub name: String,
    /// 模型可见描述。
    pub description: String,
    /// 参数 JSON Schema（object）。
    pub parameters: Value,
    /// 输出 JSON Schema（object）。
    pub output: Value,
}

impl ToolSchema {
    /// 以名字、描述与两份 schema 构造。
    pub fn new(
        name: impl Into<String>,
        description: impl Into<String>,
        parameters: Value,
        output: Value,
    ) -> Self {
        Self {
            name: name.into(),
            description: description.into(),
            parameters,
            output,
        }
    }
}

const INDENT: &str = "  ";

/// 渲染完整的 `tools:sdk` 段：使用说明 + 类型声明 + `declare const tools`。
///
/// 入参里的 `run_code` 自身应已被调用方排除。空工具集也会产出合法的空 map。
pub fn render_tools_sdk(schemas: &[ToolSchema], format: PtcProgramFormat) -> String {
    let mut sorted: Vec<&ToolSchema> = schemas.iter().collect();
    sorted.sort_by(|left, right| left.name.cmp(&right.name));

    let mut instructions = String::new();
    instructions.push_str(&render_usage(format));
    instructions.push_str("\n\nProgram-only SDK bindings:\n\n```ts\n");
    instructions.push_str("type JsonValue = null | boolean | number | string | JsonValue[] | { [key: string]: JsonValue }\n");

    instructions.push_str("\ninterface ToolArgsMap {\n");
    for schema in &sorted {
        push_member(&mut instructions, schema, &schema.parameters);
    }
    instructions.push_str("}\n");

    instructions.push_str("\ninterface ToolOutputMap {\n");
    for schema in &sorted {
        push_member(&mut instructions, schema, &schema.output);
    }
    instructions.push_str("}\n");

    instructions.push_str(
        "\ntype ToolName = keyof ToolOutputMap\n\n\
         declare class ToolCallError extends Error {\n  \
         readonly name: \"ToolCallError\"\n  \
         readonly toolName: ToolName\n\
         }\n\n\
         declare const tools: {\n  \
         [K in ToolName]: (args: ToolArgsMap[K]) => Promise<ToolOutputMap[K]>\n\
         }\n",
    );
    instructions.push_str("```\n");
    instructions
}

fn render_usage(format: PtcProgramFormat) -> String {
    let program = match format {
        PtcProgramFormat::FunctionBody => {
            "the body of an async TypeScript function (erasable syntax only — no `enum` or \
             namespaces; type annotations are advisory, the code runs type-stripped)"
        }
        PtcProgramFormat::Repl | PtcProgramFormat::Script => "the program to execute",
    };
    let output = match format {
        PtcProgramFormat::FunctionBody => {
            "- Emit results with `return` and/or `console.log(...)`. Only what you print or return \
             is program output."
        }
        PtcProgramFormat::Repl | PtcProgramFormat::Script => {
            "- Only values emitted as program output enter the conversation."
        }
    };
    format!(
        "## Writing code for run_code\n\n\
         `run_code` takes two required arguments: `code`, {program}, and `description`, a short \
         summary of what the program does. The declarations below are SDK bindings for this \
         program; a declaration does not make its name a directly callable tool.\n\n\
         Inside the program:\n\n\
         - Call tools as `await tools.name(args)` — quoted access for exotic names: \
         `tools[\"my-tool\"](args)`. Every call resolves to the tool's canonical JSON value; \
         arguments must be lossless JSON.\n\
         - A FAILED tool call rejects with `ToolCallError`, whose `toolName` identifies the failed \
         tool and whose `message` is human-readable — `try/catch` it to handle and continue.\n\
         - Independent read-only calls MAY overlap under `Promise.all`; mutating calls run alone, \
         in submission order. Sequence dependent work with `await`.\n\
         {output}\n\
         - Intermediate results stay out of the conversation: return or print only what the next \
         step needs."
    )
}

fn push_member(out: &mut String, schema: &ToolSchema, node: &Value) {
    for line in doc_lines(&schema.description) {
        out.push_str(INDENT);
        out.push_str(&line);
        out.push('\n');
    }
    out.push_str(INDENT);
    out.push_str(&render_key(&schema.name));
    out.push_str(": ");
    out.push_str(&render_type(node, 1));
    out.push_str(";\n");
}

/// 把描述压成单行 JSDoc，并转义会提前闭合注释的序列。
fn doc_lines(description: &str) -> Vec<String> {
    let collapsed = description.split_whitespace().collect::<Vec<_>>().join(" ");
    if collapsed.is_empty() {
        return Vec::new();
    }
    vec![format!("/** {} */", collapsed.replace("*/", "*\\/"))]
}

/// 合法 TS 标识符按裸名渲染，其余加引号——任意工具名都可达，无需别名。
fn render_key(name: &str) -> String {
    if is_typescript_identifier(name) {
        name.to_owned()
    } else {
        serde_json::to_string(name).unwrap_or_else(|_| "\"<unprintable>\"".to_owned())
    }
}

fn is_typescript_identifier(name: &str) -> bool {
    let mut chars = name.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !(first.is_ascii_alphabetic() || first == '_' || first == '$') {
        return false;
    }
    chars.all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '$')
}

/// 渲染一个类型表达式；`depth` 是当前缩进层级（对象成员换行时使用）。
fn render_type(node: &Value, depth: usize) -> String {
    let Some(object) = node.as_object() else {
        return "unknown".to_owned();
    };

    if let Some(constant) = object.get("const") {
        return render_literal(constant);
    }
    if let Some(values) = object.get("enum").and_then(Value::as_array) {
        if values.is_empty() {
            return "never".to_owned();
        }
        let parts: Vec<String> = values.iter().map(render_literal).collect();
        return parts.join(" | ");
    }
    if let Some(variants) = object
        .get("oneOf")
        .or_else(|| object.get("anyOf"))
        .and_then(Value::as_array)
        .filter(|variants| !variants.is_empty())
    {
        let parts: Vec<String> = variants
            .iter()
            .map(|variant| render_type(variant, depth))
            .collect();
        return parts.join(" | ");
    }
    if let Some(variants) = object
        .get("allOf")
        .and_then(Value::as_array)
        .filter(|variants| !variants.is_empty())
    {
        let parts: Vec<String> = variants
            .iter()
            .map(|variant| render_type(variant, depth))
            .collect();
        return parts.join(" & ");
    }
    if object.contains_key("$ref") || object.contains_key("$dynamicRef") {
        return "unknown".to_owned();
    }

    match object.get("type") {
        Some(Value::String(kind)) => render_kind(kind, object, depth),
        Some(Value::Array(kinds)) => {
            let mut parts: Vec<String> = kinds
                .iter()
                .filter_map(Value::as_str)
                .map(|kind| render_kind(kind, object, depth))
                .collect();
            parts.sort();
            parts.dedup();
            match parts.len() {
                0 => "unknown".to_owned(),
                1 => parts.remove(0),
                _ => parts.join(" | "),
            }
        }
        _ => "unknown".to_owned(),
    }
}

fn render_kind(kind: &str, object: &serde_json::Map<String, Value>, depth: usize) -> String {
    match kind {
        "string" => "string".to_owned(),
        "integer" | "number" => "number".to_owned(),
        "boolean" => "boolean".to_owned(),
        "null" => "null".to_owned(),
        "array" => {
            let items = object.get("items");
            let item = items
                .map(|items| render_type(items, depth))
                .unwrap_or_else(|| "unknown".to_owned());
            // 联合/交叉作为元素类型必须加括号；对象字面量不加，保持多行可读性。
            if items.is_some_and(is_union_like) {
                format!("({item})[]")
            } else {
                format!("{item}[]")
            }
        }
        "object" => render_object(object, depth),
        _ => "unknown".to_owned(),
    }
}

fn render_object(object: &serde_json::Map<String, Value>, depth: usize) -> String {
    let Some(properties) = object.get("properties").and_then(Value::as_object) else {
        return match object.get("additionalProperties") {
            Some(Value::Bool(false)) => "Record<string, never>".to_owned(),
            Some(schema @ Value::Object(_)) => {
                format!("Record<string, {}>", render_type(schema, depth))
            }
            _ => "Record<string, JsonValue>".to_owned(),
        };
    };
    if properties.is_empty() {
        return "Record<string, JsonValue>".to_owned();
    }

    let required: Vec<&str> = object
        .get("required")
        .and_then(Value::as_array)
        .map(|names| names.iter().filter_map(Value::as_str).collect())
        .unwrap_or_default();

    let mut out = String::from("{\n");
    for (name, schema) in properties {
        if let Some(description) = schema.get("description").and_then(Value::as_str) {
            for line in doc_lines(description) {
                out.push_str(&INDENT.repeat(depth + 1));
                out.push_str(&line);
                out.push('\n');
            }
        }
        out.push_str(&INDENT.repeat(depth + 1));
        out.push_str(&render_key(name));
        if !required.contains(&name.as_str()) {
            out.push('?');
        }
        out.push_str(": ");
        out.push_str(&render_type(schema, depth + 1));
        out.push_str(";\n");
    }
    out.push_str(&INDENT.repeat(depth));
    out.push('}');
    out
}

fn render_literal(value: &Value) -> String {
    match value {
        Value::String(text) => {
            serde_json::to_string(text).unwrap_or_else(|_| "\"<unprintable>\"".to_owned())
        }
        Value::Null => "null".to_owned(),
        Value::Bool(flag) => flag.to_string(),
        Value::Number(number) => number.to_string(),
        other => serde_json::to_string(other).unwrap_or_else(|_| "unknown".to_owned()),
    }
}

/// 该 schema 是否渲染成顶层联合/交叉（因此作为元素类型时必须加括号）。
fn is_union_like(node: &Value) -> bool {
    let Some(object) = node.as_object() else {
        return false;
    };
    if object.contains_key("oneOf") || object.contains_key("anyOf") || object.contains_key("allOf")
    {
        return true;
    }
    if let Some(values) = object.get("enum").and_then(Value::as_array) {
        return values.len() > 1;
    }
    matches!(object.get("type"), Some(Value::Array(kinds)) if kinds.len() > 1)
}

/// `run_code` 的参数 schema；`timeoutMs` 只在运行时支持逐次预算时出现。
///
/// 描述同时点名 `code` 与 `description`：只发 `{code}` 的模型会因缺参被拒，
/// 连同已写好的整个程序一起丢失。
pub fn run_code_parameters(description_code: &str, timeout: Option<(u64, u64)>) -> Value {
    let mut properties = serde_json::Map::new();
    properties.insert(
        "code".to_owned(),
        serde_json::json!({
            "type": "string",
            "description": description_code,
        }),
    );
    properties.insert(
        "description".to_owned(),
        serde_json::json!({
            "type": "string",
            "description": "Clear, concise description of what this program does in active voice, 5-10 words (shown in the UI).",
        }),
    );
    if let Some((default_ms, max_ms)) = timeout {
        properties.insert(
            "timeoutMs".to_owned(),
            serde_json::json!({
                "type": "number",
                "description": format!(
                    "Positive elapsed-time budget in milliseconds, including nested tool and approval waits. Default {default_ms}; capped at {max_ms}. Zero does not disable the deadline."
                ),
            }),
        );
    }
    serde_json::json!({
        "type": "object",
        "additionalProperties": false,
        "required": ["code", "description"],
        "properties": properties,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema(name: &str) -> ToolSchema {
        ToolSchema::new(
            name,
            "Read a file",
            json!({
                "type": "object",
                "additionalProperties": false,
                "required": ["path"],
                "properties": {
                    "path": { "type": "string", "description": "File path" },
                    "start_line": { "type": "integer" },
                }
            }),
            json!({
                "type": "object",
                "additionalProperties": false,
                "properties": { "content": { "type": "string" } }
            }),
        )
    }

    #[test]
    fn renders_stable_sorted_declarations() {
        let rendered = render_tools_sdk(
            &[schema("read"), schema("grep")],
            PtcProgramFormat::FunctionBody,
        );
        let grep_at = rendered.find("grep: ").expect("grep member");
        let read_at = rendered.find("read: ").expect("read member");
        assert!(grep_at < read_at, "工具必须按名字字典序渲染:\n{rendered}");
        assert!(rendered.contains("declare const tools"));
        assert!(rendered.contains("path: string;"));
        assert!(rendered.contains("start_line?: number;"));
        // 同一输入重复渲染必须字节一致。
        let again = render_tools_sdk(
            &[schema("grep"), schema("read")],
            PtcProgramFormat::FunctionBody,
        );
        assert_eq!(rendered, again);
    }

    #[test]
    fn exotic_names_are_quoted_and_stay_reachable() {
        let rendered = render_tools_sdk(
            &[schema("mcp__fs__read_file"), schema("weird name")],
            PtcProgramFormat::FunctionBody,
        );
        assert!(rendered.contains("mcp__fs__read_file:"));
        assert!(rendered.contains("\"weird name\":"));
    }

    #[test]
    fn enums_and_nested_arrays_lower_to_unions() {
        let rendered = render_tools_sdk(
            &[ToolSchema::new(
                "todo_write",
                "Write the todo list",
                json!({
                    "type": "object",
                    "required": ["items"],
                    "properties": {
                        "items": {
                            "type": "array",
                            "items": {
                                "type": "object",
                                "required": ["title", "status"],
                                "properties": {
                                    "title": { "type": "string" },
                                    "status": { "enum": ["pending", "done"] }
                                }
                            }
                        }
                    }
                }),
                json!({ "type": "object", "properties": {} }),
            )],
            PtcProgramFormat::FunctionBody,
        );
        assert!(rendered.contains("items: {"));
        assert!(rendered.contains("status: \"pending\" | \"done\";"));
        assert!(rendered.contains("}[];"));
    }

    #[test]
    fn unsupported_constructs_degrade_to_unknown() {
        let rendered = render_tools_sdk(
            &[ToolSchema::new(
                "x",
                "",
                json!({ "type": "object", "properties": { "a": { "$ref": "#/$defs/A" } } }),
                json!({ "type": "object" }),
            )],
            PtcProgramFormat::FunctionBody,
        );
        assert!(rendered.contains("a?: unknown;"));
    }

    #[test]
    fn descriptions_are_single_line_and_cannot_close_the_comment() {
        let rendered = render_tools_sdk(
            &[ToolSchema::new(
                "x",
                "first line\nsecond */ line",
                json!({ "type": "object" }),
                json!({ "type": "object" }),
            )],
            PtcProgramFormat::FunctionBody,
        );
        assert!(rendered.contains("/** first line second *\\/ line */"));
    }

    #[test]
    fn empty_tool_set_still_declares_the_call_surface() {
        let rendered = render_tools_sdk(&[], PtcProgramFormat::FunctionBody);
        assert!(rendered.contains("interface ToolArgsMap {\n}"));
        assert!(rendered.contains("declare const tools"));
        assert!(rendered.contains("run_code"));
    }

    #[test]
    fn run_code_parameters_point_at_both_required_arguments() {
        let schema = run_code_parameters("The program", Some((120_000, 600_000)));
        assert_eq!(schema["required"], json!(["code", "description"]));
        assert_eq!(schema["additionalProperties"], json!(false));
        assert!(
            schema["properties"]["timeoutMs"]["description"]
                .as_str()
                .unwrap_or_default()
                .contains("Default 120000")
        );
        let without = run_code_parameters("The program", None);
        assert!(without["properties"].get("timeoutMs").is_none());
    }
}
