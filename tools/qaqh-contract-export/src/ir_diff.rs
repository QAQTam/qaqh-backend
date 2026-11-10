//! ADR-0008 第 2 步的**诊断**：把 IR（JSON Schema，直接读自 Rust DTO）说过的话，
//! 与 `cs_backend` 已经发出去的 `cs/*.cs` 逐条对账。
//!
//! 为什么不是"两个渲染器比字节"：ts-rs 12 的公开 API 只有渲染后的 TS 字符串
//! （`name`/`inline`/`decl`/`docs` + 依赖遍历器），**没有任何结构化类型树**（实测
//! `ts-rs-12.0.1/src/lib.rs:366-470`，全 crate 无 `pub enum Type`/`Typ`）。所以真 IR
//! 只能是 schemars 那一侧；要"双跑"就得拿 IR 的事实去核对已成品的 C#，而不是再写
//! 第二个解析器（那正是本 ADR 要退役的东西）。
//!
//! C# 侧的读取是**属性级断言**，不是语法分析：`cs_backend` 自己的发射格式把 JSON 原名
//! 放在 attribute 里（`emit_properties`/`Decl::Enum`/`Decl::Union` 的 format! 字面量），
//! 因此这里只按那几行字面量取事实。
//!
//! 这个命令**不写任何文件**，也不参与 `generate()`：它是量尺，不是产物。
//! 退出码只在"对账本身发现丢失/错类"时为 1；已知降级类（整数宽度、strict-input 丢标志）
//! 只报数，不判红 —— 那些是 ADR-0008 要消灭的对象，不是这个工具的 bug。

use std::collections::BTreeMap;
use std::path::Path;

use serde_json::Value;

use super::schema_backend::surface;

/// 2^53-1：TS `number`（以及 `.with_large_int("number")`）能精确表示的整数上界。
const MAX_SAFE_INTEGER: i128 = 9_007_199_254_740_991;

#[derive(Default)]
struct Report {
    /// IR 与 C# 都覆盖到的属性总数。
    compared: usize,
    covered_types: Vec<String>,
    /// C# 侧实际发出的类型文件数（扣掉两个基建文件）—— IR 覆盖率的 denominator。
    /// 这个数字就是 ADR-0008 第 1 步"扩面"还剩多少工作量。
    cs_total_types: usize,
    /// 全表面统计：C# 里所有整数属性数 / 其中无符号 CLR 类型（ulong·uint·byte…）的数量。
    /// Rust 侧的 DTO 全是 u64/u32/u8，而无符号数为 0 就是"符号位在窄腰上被丢掉"的总量证据
    /// ——它不依赖 IR 覆盖率，所以是当下唯一能对整个 C# 表面下的结论。
    cs_integer_props: usize,
    cs_unsigned_props: usize,
    /// 整数宽度/符号位降级（例：schema `uint8` → C# `long`）。
    int_width: Vec<String>,
    /// 同类降级的**计数**（`期望 → 实际`），报告用它说总量，明细列表只用来举例。
    width_pairs: BTreeMap<String, usize>,
    /// 真实丢失或错类的属性（必须修）。
    lost: Vec<String>,
    /// `float` 被判成整数（`FLOAT_FIELDS` 那一类，数据会被截）。
    float_as_integer: Vec<String>,
    /// 可空性/可选性约定差异（只报不判红：schemars 与 ts-rs 对 serde 属性的处理不必相同）。
    optionality: Vec<String>,
    /// `deny_unknown_fields` 在 C# 投影里没有对应标志（strict-input 类丢失，ADR-0004 决策 5）。
    strict_input_lost: Vec<String>,
    /// IR 侧尚未结构化（union/别名等）——明说，不假装比过。
    not_compared: Vec<String>,
    /// 夹具里超出 2^53-1 的整数（对 TS/JS 消费者是精度损失，不是类型错误）。
    unsafe_integers: Vec<String>,
}

pub(super) fn run(stage: &Path) -> Result<(), Box<dyn std::error::Error>> {
    let mut report = Report::default();

    for item in surface() {
        let serialize = (item.serialize)()?;
        let deserialize = (item.deserialize)()?;
        let cs_path = stage.join("cs").join(format!("{}.cs", item.name));
        let Some(cs) = read_cs(&cs_path)? else {
            // cs_backend 刻意不为 `export type X = string` 这种纯别名发文件；
            // 只有 IR 侧也确实是纯标量时才允许缺席，否则就是丢失。
            if is_scalar_alias(&serialize) {
                report.not_compared.push(format!(
                    "{}: IR 是标量别名（{}），C# 无独立文件 —— 与 cs_backend 的 ALIAS 约定一致",
                    item.name,
                    scalar_kind(&serialize)
                ));
                continue;
            }
            report.lost.push(format!(
                "{}: IR 有结构，但 {} 不存在",
                item.name,
                cs_path.display()
            ));
            continue;
        };

        compare_type(&item.name, &serialize, &cs, &mut report);

        // strict-input 方向：deserialize 视图的 additionalProperties:false 在 C# 侧
        // 必须有个等价标志（System.Text.Json 默认忽略未知键 = 静默宽松）。
        if strict_input_required(&deserialize) && !cs.contains("JsonUnmappedMemberHandling") {
            report.strict_input_lost.push(format!(
                "{}: deny_unknown_fields 派生出 additionalProperties:false，C# 投影无任何未映射成员标志 ⇒ 消费端默认宽松",
                item.name
            ));
        }
    }

    report.unsafe_integers = unsafe_integer_fixture_values(stage);
    // 覆盖率的分母 + 全表面整数统计（不依赖 IR 覆盖，见 Report 字段注释）。
    let (types, integers, unsigned) = cs_surface_counts(stage);
    report.cs_total_types = types;
    report.cs_integer_props = integers;
    report.cs_unsigned_props = unsigned;

    print_report(&report);
    if report.lost.is_empty() && report.float_as_integer.is_empty() {
        Ok(())
    } else {
        Err("ir-diff 发现 IR 与 C# 投影不一致（见上）".into())
    }
}

fn compare_type(name: &str, schema: &Value, cs: &str, report: &mut Report) {
    let Some(members) = string_enum_members(schema) else {
        let Some(properties) = object_properties(schema) else {
            report.not_compared.push(format!(
                "{}: IR 形状是 {}，本版 ir-diff 只结构化 object/string-enum",
                name,
                ir_kind(schema)
            ));
            return;
        };
        let emitted = cs_properties(cs);
        if emitted.is_empty() {
            report
                .lost
                .push(format!("{name}: C# 文件里一个 [JsonPropertyName] 都没找到"));
            return;
        }
        report.covered_types.push(name.to_owned());

        for (key, schema_property) in &properties {
            let Some((actual_ty, optional)) = emitted.get(key) else {
                report.lost.push(format!("{name}.{key}: IR 有，C# 无"));
                continue;
            };
            for (extra_key, _) in emitted.iter() {
                if !properties.iter().any(|(k, _)| k == extra_key) {
                    report
                        .lost
                        .push(format!("{name}.{extra_key}: C# 有，IR 无"));
                }
            }
            report.compared += 1;
            let expected = expected_clr(schema_property);
            let actual = actual_ty.trim_end_matches('?').to_owned();
            if expected == actual {
                continue;
            }
            // 只有"IR 说可选、C# 却没标 WhenWritingNull"才是额外事实，值得写进这行；
            // 必填属性一律不缀，否则 38 行里全是同一段废话，真正差异反而看不见。
            let optional_drift =
                schema_property.get("__required") == Some(&Value::Bool(false)) && !*optional;
            let note = format!(
                "{name}.{key}: schema={}{} ⇒ 期望 {expected}，实际 {actual}{}",
                ir_kind(schema_property),
                format_hint(schema_property),
                if optional_drift {
                    "（且 IR 可选、C# 未标 WhenWritingNull）"
                } else {
                    ""
                }
            );
            if is_integerish(&expected) && is_integerish(&actual) {
                report.int_width.push(note);
                *report
                    .width_pairs
                    .entry(format!("期望 {expected} → 实际 {actual}"))
                    .or_insert(0) += 1;
            } else if expected == "double" && is_integerish(&actual) {
                report.float_as_integer.push(note);
            } else if nullable_drift(schema_property, *optional) {
                report.optionality.push(note);
            } else {
                report.lost.push(note);
            }
        }
        return;
    };

    // string enum：成员集合必须一一对应（判别式集合是闭合的，见 ADR-0004 closed-set）。
    let emitted = cs_enum_members(cs);
    report.covered_types.push(name.to_owned());
    for literal in &members {
        report.compared += 1;
        if !emitted.iter().any(|(_, value)| value == literal) {
            report
                .lost
                .push(format!("{name}: 判别值 {literal:?} 在 C# enum 里不存在"));
        }
    }
    if emitted.len() != members.len() {
        report.lost.push(format!(
            "{name}: IR 有 {} 个判别值，C# enum 有 {} 个成员",
            members.len(),
            emitted.len()
        ));
    }
}

// ── IR 侧读取（JSON Schema 子集）──────────────────────────────────────────────

fn ir_kind(schema: &Value) -> String {
    if schema.get("oneOf").is_some() || schema.get("anyOf").is_some() {
        return "tagged-union".to_owned();
    }
    if schema.get("$ref").is_some() {
        return format!("$ref:{}", tail(schema["$ref"].as_str().unwrap_or_default()));
    }
    match schema.get("type") {
        Some(Value::String(kind)) => kind.clone(),
        Some(Value::Array(kinds)) => kinds
            .iter()
            .filter_map(Value::as_str)
            .collect::<Vec<_>>()
            .join("|"),
        _ => "unknown".to_owned(),
    }
}

fn tail(reference: &str) -> String {
    reference.rsplit('/').next().unwrap_or(reference).to_owned()
}

fn is_scalar_alias(schema: &Value) -> bool {
    matches!(schema.get("type"), Some(Value::String(_)))
        && schema.get("properties").is_none()
        && schema.get("enum").is_none()
}

fn scalar_kind(schema: &Value) -> String {
    ir_kind(schema)
}

fn string_enum_members(schema: &Value) -> Option<Vec<String>> {
    let values = schema.get("enum")?.as_array()?;
    if schema.get("type").and_then(Value::as_str) == Some("string") {
        return Some(
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect(),
        );
    }
    None
}

fn object_properties(schema: &Value) -> Option<Vec<(String, Value)>> {
    let map = schema.get("properties")?.as_object()?;
    let required: Vec<String> = schema
        .get("required")
        .and_then(Value::as_array)
        .map(|values| {
            values
                .iter()
                .filter_map(Value::as_str)
                .map(str::to_owned)
                .collect()
        })
        .unwrap_or_default();
    Some(
        map.iter()
            .map(|(key, value)| {
                let mut node = value.clone();
                node.as_object_mut().expect("property schema").insert(
                    "__required".to_owned(),
                    Value::Bool(required.iter().any(|item| item == key)),
                );
                (key.clone(), node)
            })
            .collect(),
    )
}

fn strict_input_required(schema: &Value) -> bool {
    schema
        .get("additionalProperties")
        .is_some_and(|value| value == &Value::Bool(false))
}

/// IR 事实 → CLR 类型的期望表。注意方向：这里的每一项都能从 schema 的
/// `type`/`format`/`minimum` 推出来，不是把 C# 侧的约定抄回来（那正是 FLOAT_FIELDS 的病）。
fn expected_clr(schema: &Value) -> String {
    if let Some(reference) = schema.get("$ref").and_then(Value::as_str) {
        return tail(reference);
    }
    if let Some(items) = schema.get("items") {
        return format!("{}[]", expected_clr(items));
    }
    if let Some(values) = schema.get("additionalProperties") {
        if values.is_object() {
            return format!("Dictionary<string, {}>", expected_clr(values));
        }
    }
    match schema.get("type") {
        Some(Value::Array(kinds)) => {
            // ["integer","null"] 之类：取非 null 的那一支。
            for kind in kinds.iter().filter_map(Value::as_str) {
                if kind != "null" {
                    let mut inner = Value::Object(serde_json::Map::new());
                    inner
                        .as_object_mut()
                        .unwrap()
                        .insert("type".into(), kind.into());
                    for key in ["format", "minimum", "maximum", "items", "$ref"] {
                        if let Some(value) = schema.get(key) {
                            inner
                                .as_object_mut()
                                .unwrap()
                                .insert(key.into(), value.clone());
                        }
                    }
                    return expected_clr(&inner);
                }
            }
            "object".to_owned()
        }
        Some(Value::String(kind)) => match kind.as_str() {
            "string" => "string".to_owned(),
            "boolean" => "bool".to_owned(),
            "number" => "double".to_owned(),
            "integer" => integer_clr(schema),
            "object" => "object".to_owned(),
            other => other.to_owned(),
        },
        _ => "object".to_owned(),
    }
}

fn integer_clr(schema: &Value) -> String {
    let format = schema
        .get("format")
        .and_then(Value::as_str)
        .unwrap_or_default();
    let unsigned_by_format = matches!(
        format,
        "uint8" | "uint16" | "uint32" | "uint64" | "uint" | "usize"
    );
    let non_negative = schema
        .get("minimum")
        .and_then(Value::as_i64)
        .is_some_and(|min| min >= 0);
    let unsigned = unsigned_by_format || (format.is_empty() && non_negative);
    match format {
        "uint8" => "byte",
        "uint16" => "ushort",
        "uint32" => "uint",
        "uint64" | "uint" | "usize" => "ulong",
        "int8" => "sbyte",
        "int16" => "short",
        "int32" => "int",
        "int64" => "long",
        _ if unsigned => "ulong",
        _ => "long",
    }
    .to_owned()
}

fn format_hint(schema: &Value) -> String {
    let mut hint = String::new();
    if let Some(format) = schema.get("format").and_then(Value::as_str) {
        hint.push_str(&format!(" format={format}"));
    }
    if let Some(min) = schema.get("minimum") {
        hint.push_str(&format!(" min={min}"));
    }
    if let Some(max) = schema.get("maximum") {
        hint.push_str(&format!(" max={max}"));
    }
    if schema.get("__required") == Some(&Value::Bool(false)) {
        hint.push_str(" optional");
    }
    hint
}

fn is_integerish(clr: &str) -> bool {
    matches!(
        clr.trim_end_matches('?'),
        "byte" | "sbyte" | "short" | "ushort" | "int" | "uint" | "long" | "ulong"
    )
}

/// 可空性差异单独归类：schemars 与 ts-rs 对 `Option`/`skip_serializing_if` 的表达
/// 不必同形，把它算成"丢失"会是假警报。
fn nullable_drift(schema: &Value, cs_optional: bool) -> bool {
    let nullable_in_ir = schema
        .get("type")
        .and_then(Value::as_array)
        .is_some_and(|kinds| kinds.iter().any(|kind| kind == "null"));
    let optional_in_ir = schema.get("__required") == Some(&Value::Bool(false));
    (nullable_in_ir || optional_in_ir) != cs_optional
}

// ── C# 侧读取：只认 cs_backend 自己发射的那几行字面量 ──────────────────────────

/// `{ jsonKey -> (clrType, WhenWritingNull) }`，保持发射顺序。
fn cs_properties(content: &str) -> BTreeMap<String, (String, bool)> {
    let mut out = BTreeMap::new();
    let lines: Vec<&str> = content.lines().map(str::trim).collect();
    let mut index = 0;
    while index < lines.len() {
        let Some(key) = attribute_value(lines[index], "JsonPropertyName") else {
            index += 1;
            continue;
        };
        let mut optional = false;
        let mut cursor = index + 1;
        while cursor < lines.len() {
            let line = lines[cursor];
            if line.starts_with("[JsonIgnore(Condition = JsonIgnoreCondition.WhenWritingNull)]") {
                optional = true;
                cursor += 1;
                continue;
            }
            if let Some(rest) = line.strip_prefix("public ") {
                if let Some(brace) = rest.find(" { get; init; }") {
                    let declaration = rest[..brace].trim();
                    let mut pieces: Vec<&str> = declaration.split_whitespace().collect();
                    // 最后一件是属性名（pascal 化），其余拼成 CLR 类型（含泛型空格）。
                    if pieces.len() > 1 {
                        pieces.pop();
                        out.insert(key, (pieces.join(" "), optional));
                    }
                }
            }
            break;
        }
        index = cursor.max(index + 1);
    }
    out
}

fn cs_enum_members(content: &str) -> Vec<(String, String)> {
    let mut out = Vec::new();
    let lines: Vec<&str> = content.lines().map(str::trim).collect();
    for (position, line) in lines.iter().enumerate() {
        if let Some(literal) = attribute_value(line, "JsonStringEnumMemberName") {
            let member = lines
                .get(position + 1)
                .map(|next| next.trim_end_matches(',').trim().to_owned())
                .unwrap_or_default();
            out.push((member, literal));
        }
    }
    out
}

fn attribute_value(line: &str, attribute: &str) -> Option<String> {
    let prefix = format!("[{attribute}(\"");
    let rest = line.strip_prefix(&prefix)?;
    Some(rest.split('"').next()?.to_owned())
}

fn read_cs(path: &Path) -> Result<Option<String>, Box<dyn std::error::Error>> {
    if !path.is_file() {
        return Ok(None);
    }
    Ok(Some(std::fs::read_to_string(path)?))
}

/// 扫一遍 `cs/`：类型文件数、整数属性数、其中无符号 CLR 的属性数。
/// 口径与 `cs_properties` 完全一致（只数带 `[JsonPropertyName]` 的发射属性；
/// `[JsonExtensionData]` 没有该 attribute，因此不计入 —— 报数时不假装比 IR 多看了东西）。
fn cs_surface_counts(stage: &Path) -> (usize, usize, usize) {
    let Ok(dir) = std::fs::read_dir(stage.join("cs")) else {
        return (0, 0, 0);
    };
    let (mut types, mut integers, mut unsigned) = (0usize, 0usize, 0usize);
    for entry in dir.filter_map(|entry| entry.ok()) {
        let path = entry.path();
        if path.extension().and_then(|ext| ext.to_str()) != Some("cs") {
            continue;
        }
        let stem = path
            .file_stem()
            .and_then(|stem| stem.to_str())
            .unwrap_or_default();
        if stem.starts_with("Contract") {
            continue;
        }
        types += 1;
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        for (clr, _) in cs_properties(&text).into_values() {
            let bare = clr.trim_end_matches('?');
            if is_integerish(bare) {
                integers += 1;
            }
            if matches!(bare, "ulong" | "uint" | "ushort" | "byte") {
                unsigned += 1;
            }
        }
    }
    (types, integers, unsigned)
}

// ── 全表面的一条免费信号：夹具里的超大整数 ─────────────────────────────────────

fn unsafe_integer_fixture_values(stage: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let fixtures = stage.join("fixtures");
    let Ok(dir) = std::fs::read_dir(&fixtures) else {
        return out;
    };
    let entries: Vec<_> = dir
        .filter_map(|entry| entry.ok())
        .map(|entry| entry.path())
        .collect();
    for path in entries {
        if path.extension().and_then(|ext| ext.to_str()) != Some("json") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(parsed) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        let name = path
            .file_name()
            .unwrap_or_default()
            .to_string_lossy()
            .into_owned();
        walk_numbers(&parsed, &name, &mut Vec::new(), &mut out);
    }
    out.sort();
    out
}

fn walk_numbers(value: &Value, fixture: &str, trail: &mut Vec<String>, out: &mut Vec<String>) {
    match value {
        Value::Object(map) => {
            for (key, child) in map {
                trail.push(key.clone());
                walk_numbers(child, fixture, trail, out);
                trail.pop();
            }
        }
        Value::Array(items) => {
            for (index, child) in items.iter().enumerate() {
                trail.push(format!("[{index}]"));
                walk_numbers(child, fixture, trail, out);
                trail.pop();
            }
        }
        Value::Number(number) => {
            if let Some(text) = number
                .as_i64()
                .and_then(|int| (int.abs() as i128 > MAX_SAFE_INTEGER).then(|| format!("{int}")))
            {
                out.push(format!("{fixture} {} = {text}", trail.join(".")));
            }
        }
        _ => {}
    }
}

// ── 报告 ──────────────────────────────────────────────────────────────────────

fn print_report(report: &Report) {
    println!(
        "ir-diff: IR 覆盖 {} 型 / C# 共发 {} 型文件（{:.1}%），逐属性对账 {} 项；不落任何文件",
        report.covered_types.len(),
        report.cs_total_types,
        if report.cs_total_types == 0 {
            0.0
        } else {
            100.0 * report.covered_types.len() as f64 / report.cs_total_types as f64
        },
        report.compared,
    );
    println!("  已对账：{}", report.covered_types.join(", "));
    section(
        "整数宽度/符号位降级（已知，ADR-0008 要消灭的类）",
        &report.int_width,
    );
    if !report.width_pairs.is_empty() {
        let mut ranked: Vec<_> = report.width_pairs.iter().collect();
        ranked.sort_by(|a, b| b.1.cmp(a.1).then_with(|| a.0.cmp(b.0)));
        println!("  降级形态汇总（这些就是 IR 直出 C# 要修的全部差异）：");
        for (pair, count) in ranked {
            println!("    {count:4}x {pair}");
        }
        println!(
            "  C# 全表面整数属性 {} 个，其中无符号 CLR（ulong/uint/byte/…）{} 个",
            report.cs_integer_props, report.cs_unsigned_props
        );
    }
    section(
        "float 被判成整数（会截数据，必须修）",
        &report.float_as_integer,
    );
    section("属性/判别式丢失或错类（必须修）", &report.lost);
    section(
        "strict-input 标志在 C# 投影里丢失",
        &report.strict_input_lost,
    );
    section("可空性/可选性约定差异（只报）", &report.optionality);
    section("IR 侧未结构化（明说不假装）", &report.not_compared);
    section("夹具整数超出 2^53-1（JS 侧精度）", &report.unsafe_integers);
}

fn section(title: &str, items: &[String]) {
    if items.is_empty() {
        println!("  {title}: 0");
        return;
    }
    println!("  {title}: {}", items.len());
    for item in items.iter().take(12) {
        println!("    - {item}");
    }
    if items.len() > 12 {
        println!("    …（另 {} 项）", items.len() - 12);
    }
}
