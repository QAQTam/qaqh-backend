//! `qaqh-daemon --acp`:把 QAQHarness 注册进 Zed 的 `agent_servers`。
//!
//! 这是用户在本机敲的**本地安装动作**(等价 `code --install-extension`),
//! 与"远端 daemon 不得碰本机文件"的边界不冲突:远端场景走不到这个 flag。
//!
//! Zed 的 settings.json 是 JSONC(允许注释/尾逗号)。这里做**注释保留的
//! 文本级拼接**,绝不整文件重排:语义判断用注释剥离后的解析结果,修改只
//! 落在定位到的插入/替换点;写回前留一份 `.qaqh-bak`。

use std::path::{Path, PathBuf};

/// `agent_servers` 里我们的条目键(Zed 新线程菜单里的显示名)。
const AGENT_KEY: &str = "QAQHarness";

/// 注册动作(报告用)。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Action {
    /// 条目已在且指向本二进制,不写盘。
    Already,
    /// 顶层新增 `agent_servers`(或文件为空/新建)。
    Inserted,
    /// 在既有 `agent_servers` 对象内新增条目。
    InsertedNested,
    /// 既有条目指向别处,替换为本二进制。
    Updated,
}

/// Zed 用户设置文件路径;`None` = 该平台配置目录不存在(Zed 未安装过)。
/// `QAQH_ZED_SETTINGS` 显式覆盖(测试/非标准安装)。
pub fn settings_path() -> Option<PathBuf> {
    if let Ok(path) = std::env::var("QAQH_ZED_SETTINGS")
        && !path.trim().is_empty()
    {
        return Some(PathBuf::from(path));
    }
    let dir = if cfg!(windows) {
        std::env::var("APPDATA").ok().map(|root| PathBuf::from(root).join("Zed"))
    } else if cfg!(target_os = "macos") {
        std::env::var("HOME")
            .ok()
            .map(|root| PathBuf::from(root).join("Library/Application Support/Zed"))
    } else {
        std::env::var("HOME").ok().map(|root| PathBuf::from(root).join(".config/zed"))
    }?;
    if !dir.is_dir() {
        return None;
    }
    Some(dir.join("settings.json"))
}

/// 期望的条目 wire 形态。`command` 用正斜杠(Zed Windows 接受,免去转义)。
fn desired_entry(command: &str) -> serde_json::Value {
    serde_json::json!({
        "type": "custom",
        "command": command,
        "args": ["acp"],
    })
}

fn normalize(path: &str) -> String {
    path.replace('\\', "/")
}

/// 手动接入提示(找不到 Zed 配置目录时的出路)。
fn manual_snippet(command: &str) -> String {
    let entry = serde_json::to_string_pretty(&desired_entry(command)).expect("entry json");
    format!(
        "{{\n  \"agent_servers\": {{\n    \"{AGENT_KEY}\": {}\n  }}\n}}",
        indent(&entry, 6)
    )
}

fn indent(text: &str, spaces: usize) -> String {
    let pad = " ".repeat(spaces);
    text.lines()
        .enumerate()
        .map(|(index, line)| {
            if index == 0 {
                line.to_owned()
            } else {
                format!("{pad}{line}")
            }
        })
        .collect::<Vec<_>>()
        .join("\n")
}

/// 把本二进制注册进 Zed 设置;返回给用户的报告。失败(找不到目录/文件病态)
/// 只报告不改盘,daemon 启动继续。
pub fn install(command_exe: &Path) -> Result<String, String> {
    let command = normalize(&command_exe.to_string_lossy());
    let Some(path) = settings_path() else {
        return Err(format!(
            "未找到 Zed 设置目录(Windows: %APPDATA%\\Zed)。手动接入:在 settings.json 顶层加\n{}",
            manual_snippet(&command)
        ));
    };
    let text = std::fs::read_to_string(&path).unwrap_or_default();
    let (new_text, action) = wire_entry(&text, &command)?;
    if action == Action::Already {
        return Ok(format!("Zed 已指向本二进制({})", path.display()));
    }
    let backup = {
        let mut name = path.file_name().expect("settings file name").to_owned();
        name.push(".qaqh-bak");
        path.with_file_name(name)
    };
    std::fs::write(&backup, &text)
        .map_err(|error| format!("备份 {} 失败: {error}", backup.display()))?;
    std::fs::write(&path, &new_text)
        .map_err(|error| format!("写入 {} 失败: {error}", path.display()))?;
    Ok(match action {
        Action::Already => unreachable!("Already 已提前返回"),
        Action::Inserted => format!("已创建 {}", path.display()),
        Action::InsertedNested | Action::Updated => format!("已更新 {}(备份 {})", path.display(), backup.display()),
    })
}

// ────────────────────────── JSONC 扫描与拼接 ──────────────────────────

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Class {
    Code,
    Str,
    LineComment,
    BlockComment,
}

/// 逐**字节**分类 JSONC 文本(字符串/行注释/块注释)。全部扫描器都以字节
/// 偏移工作:结构字符全是 ASCII,多字节内容只出现在字符串/注释体内,逐字
/// 节标注即可,注释里的非 ASCII 不会错位。
fn classify(text: &str) -> Vec<Class> {
    let bytes = text.as_bytes();
    let mut classes = Vec::with_capacity(bytes.len());
    let mut in_string = false;
    let mut escaped = false;
    let mut index = 0;
    while index < bytes.len() {
        let byte = bytes[index];
        if in_string {
            if escaped {
                escaped = false;
                classes.push(Class::Str);
            } else {
                match byte {
                    b'\\' => {
                        escaped = true;
                        classes.push(Class::Str);
                    }
                    // 收尾引号标 Code(与开引号对称):扫描器才能确定字面量
                    // 边界,content 切片 = (开引号, 收尾引号) 半开区间。
                    b'"' => {
                        in_string = false;
                        classes.push(Class::Code);
                    }
                    _ => classes.push(Class::Str),
                }
            }
            index += 1;
            continue;
        }
        match byte {
            b'"' => {
                in_string = true;
                classes.push(Class::Code);
                index += 1;
            }
            b'/' if index + 1 < bytes.len() && bytes[index + 1] == b'/' => {
                classes.push(Class::LineComment);
                classes.push(Class::LineComment);
                index += 2;
                while index < bytes.len() {
                    let line_end = bytes[index] == b'\n';
                    classes.push(if line_end {
                        Class::Code
                    } else {
                        Class::LineComment
                    });
                    index += 1;
                    if line_end {
                        break;
                    }
                }
            }
            b'/' if index + 1 < bytes.len() && bytes[index + 1] == b'*' => {
                classes.push(Class::BlockComment);
                classes.push(Class::BlockComment);
                index += 2;
                while index < bytes.len() {
                    if bytes[index] == b'*'
                        && index + 1 < bytes.len()
                        && bytes[index + 1] == b'/'
                    {
                        classes.push(Class::BlockComment);
                        classes.push(Class::BlockComment);
                        index += 2;
                        break;
                    }
                    classes.push(Class::BlockComment);
                    index += 1;
                }
            }
            _ => {
                classes.push(Class::Code);
                index += 1;
            }
        }
    }
    classes
}

/// 注释整体替换为空格(换行保留),字符串原样;顺带**去掉尾逗号**(JSONC
/// 合法、严格 JSON 非法):得到可被 serde_json 解析的纯 JSON 分析视图。
/// 字节数不变,类数组与输出仍一一对应。
fn strip_comments(text: &str, classes: &[Class]) -> String {
    let bytes = text.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    for (index, &byte) in bytes.iter().enumerate() {
        out.push(match classes.get(index).copied().unwrap_or(Class::Code) {
            Class::LineComment | Class::BlockComment => {
                if byte == b'\n' {
                    b'\n'
                } else {
                    b' '
                }
            }
            _ => byte,
        });
    }
    // 尾逗号:`,` 后跳过空白,紧跟 `}`/`]` → 替换为空格。
    for index in 0..out.len() {
        if out[index] == b','
            && classes.get(index).copied().unwrap_or(Class::Code) == Class::Code
        {
            let mut probe = index + 1;
            while probe < out.len() && out[probe].is_ascii_whitespace() {
                probe += 1;
            }
            if probe < out.len() && (out[probe] == b'}' || out[probe] == b']') {
                out[index] = b' ';
            }
        }
    }
    String::from_utf8(out).expect("只做逐字节替换,UTF-8 边界不变")
}

/// 顶层对象的闭合 `}` 字节偏移(深度 1→0 的位置)。未找到返回 `None`。
fn top_level_close(text: &str, classes: &[Class]) -> Option<usize> {
    let bytes = text.as_bytes();
    let mut depth: usize = 0;
    for (index, class) in classes.iter().enumerate() {
        if *class != Class::Code {
            continue;
        }
        match bytes[index] {
            b'{' => depth += 1,
            b'}' => {
                depth = depth.saturating_sub(1);
                if depth == 0 {
                    return Some(index);
                }
            }
            _ => {}
        }
    }
    None
}

/// 在 `[start..end]` 范围(一段 JSON 对象体内)定位键为 `key` 的成员,
/// 返回其 value 的 `(start, end_inclusive)` 字节偏移。只接受对象的**直接**
/// 成员(相对深度 0,前置码位为 `{` 或 `,`),避免误命中嵌套/值字符串。
fn member_value_span(
    text: &str,
    classes: &[Class],
    range: std::ops::Range<usize>,
    key: &str,
) -> Option<(usize, usize)> {
    let bytes = text.as_bytes();
    let mut depth: usize = 0;
    let mut index = range.start;
    while index < range.end {
        let class = classes.get(index).copied().unwrap_or(Class::Code);
        if class != Class::Code {
            index += 1;
            continue;
        }
        match bytes[index] {
            b'{' | b'[' => {
                depth += 1;
                index += 1;
            }
            b'}' | b']' => {
                if depth == 0 {
                    return None;
                }
                depth -= 1;
                index += 1;
            }
            b'"' => {
                // 读字符串字面量(classify 已把整段标为 Str)。
                let start = index;
                index += 1;
                while index < range.end && classes.get(index) == Some(&Class::Str) {
                    index += 1;
                }
                // 此刻 index 指向收尾引号。
                let content = std::str::from_utf8(&bytes[start + 1..index])
                    .expect("字符串字面量边界必是 UTF-8 边界");
                index += 1;
                if content != key || depth != 1 {
                    continue;
                }
                // 前置码位必须是对象成员位置(`{` 或 `,`)。首成员时回探越不
                // 过 range.start(那里是开括号),直接落入后续冒号查找接受——
                // 相对深度 0 的守卫已保证这是键而非值字符串。
                let mut probe = start;
                while probe > range.start {
                    probe -= 1;
                    match classes.get(probe).copied().unwrap_or(Class::Code) {
                        Class::Code if bytes[probe].is_ascii_whitespace() => continue,
                        Class::Code => {
                            if bytes[probe] != b'{' && bytes[probe] != b',' {
                                return None; // 是值字符串,不是成员键
                            }
                            break;
                        }
                        _ => continue, // 跳过注释
                    }
                }
                // 跳过 ws/注释找 `:`
                while index < range.end {
                    match classes.get(index).copied().unwrap_or(Class::Code) {
                        Class::Code if bytes[index].is_ascii_whitespace() => index += 1,
                        Class::LineComment | Class::BlockComment => index += 1,
                        Class::Code if bytes[index] == b':' => break,
                        _ => return None, // 键后没有冒号,不是成员
                    }
                }
                if index >= range.end {
                    return None;
                }
                index += 1; // 越过 `:`
                while index < range.end {
                    match classes.get(index).copied().unwrap_or(Class::Code) {
                        Class::Code if bytes[index].is_ascii_whitespace() => index += 1,
                        Class::LineComment | Class::BlockComment => index += 1,
                        _ => break,
                    }
                }
                let value_start = index;
                let value_end = match bytes[index] {
                    b'"' => {
                        index += 1;
                        while index < range.end && classes.get(index) == Some(&Class::Str) {
                            index += 1;
                        }
                        index
                    }
                    b'{' | b'[' => {
                        let opener = bytes[index];
                        let closer = if opener == b'{' { b'}' } else { b']' };
                        let mut inner: usize = 0;
                        let mut end = None;
                        while index < range.end {
                            if classes.get(index) != Some(&Class::Code) {
                                index += 1;
                                continue;
                            }
                            if bytes[index] == opener {
                                inner += 1;
                            } else if bytes[index] == closer {
                                inner -= 1;
                                if inner == 0 {
                                    end = Some(index);
                                    break;
                                }
                            }
                            index += 1;
                        }
                        end?
                    }
                    _ => {
                        // 原始字面量:number/true/false/null,读到结构边界前。
                        while index < range.end {
                            let class = classes.get(index).copied().unwrap_or(Class::Code);
                            if class == Class::Code
                                && (bytes[index] == b','
                                    || bytes[index] == b'}'
                                    || bytes[index] == b']')
                            {
                                break;
                            }
                            index += 1;
                        }
                        // index 停在边界符上(或范围尾);回退尾随 ws。
                        while index > value_start
                            && text.as_bytes()[index - 1].is_ascii_whitespace()
                        {
                            index -= 1;
                        }
                        index.checked_sub(1)?
                    }
                };
                return Some((value_start, value_end));
            }
            _ => index += 1,
        }
    }
    None
}

/// 在 `at` 之前插入新成员时,是否需要前置逗号(回看跳过 ws/注释)。
fn needs_comma(text: &str, classes: &[Class], at: usize) -> bool {
    let bytes = text.as_bytes();
    let mut probe = at;
    while probe > 0 {
        probe -= 1;
        match classes.get(probe).copied().unwrap_or(Class::Code) {
            Class::Code if bytes[probe].is_ascii_whitespace() => continue,
            Class::LineComment | Class::BlockComment => continue,
            Class::Code => {
                return bytes[probe] != b',' && bytes[probe] != b'{' && bytes[probe] != b'[';
            }
            Class::Str => return true, // 字符串值结尾 → 需要逗号
        }
    }
    false
}

/// 把 `command` 指向的本二进制拼进设置文本。纯函数:输入原文,输出新文
/// 与动作,便于单测覆盖全部拼接分支。
fn wire_entry(text: &str, command: &str) -> Result<(String, Action), String> {
    if text.trim().is_empty() {
        let entry = serde_json::to_string(&desired_entry(command)).expect("entry json");
        return Ok((
            format!("{{\n  \"agent_servers\": {{\n    \"{AGENT_KEY}\": {entry}\n  }}\n}}\n"),
            Action::Inserted,
        ));
    }
    let classes = classify(text);
    let stripped = strip_comments(text, &classes);
    let parsed: serde_json::Value = serde_json::from_str(&stripped)
        .map_err(|error| format!("settings.json 无法解析(Zed 设置已是病态): {error}"))?;
    let Some(object) = parsed.as_object() else {
        return Err("settings.json 顶层不是 JSON 对象".into());
    };
    let wired = object
        .get("agent_servers")
        .and_then(|servers| servers.get(AGENT_KEY))
        .and_then(|entry| {
            let command_value = entry.get("command")?.as_str()?;
            let args = entry.get("args")?.as_array()?;
            let kind = entry.get("type")?.as_str()?;
            Some(
                kind == "custom"
                    && normalize(command_value) == normalize(command)
                    && args.len() == 1
                    && args[0].as_str() == Some("acp"),
            )
        })
        .unwrap_or(false);
    if wired {
        return Ok((text.to_owned(), Action::Already));
    }
    let entry = serde_json::to_string(&desired_entry(command)).expect("entry json");
    // 拼接一律走字节:所有边界都是扫描器标出的 ASCII 结构位,`&str` 切片
    // 形式会被 `-D clippy::string-slice` 拒绝,字节版还顺带免除边界疑虑。
    let bytes = text.as_bytes();
    let from_utf8 = |out: Vec<u8>| {
        String::from_utf8(out).expect("只在结构符边界拼接,UTF-8 边界不变")
    };
    match member_value_span(text, &classes, 0..text.len(), "agent_servers") {
        Some((value_start, value_end)) => {
            match member_value_span(
                text,
                &classes,
                value_start..value_end + 1,
                AGENT_KEY,
            ) {
                Some((existing_start, existing_end)) => {
                    let mut out = Vec::with_capacity(text.len() + entry.len());
                    out.extend_from_slice(&bytes[..existing_start]);
                    out.extend_from_slice(entry.as_bytes());
                    out.extend_from_slice(&bytes[existing_end + 1..]);
                    Ok((from_utf8(out), Action::Updated))
                }
                None => {
                    // 插在 agent_servers 对象闭合 `}` 之前(value_end 即该括号)。
                    let at = value_end;
                    let comma = if needs_comma(text, &classes, at) { ", " } else { "" };
                    let mut out = Vec::with_capacity(text.len() + entry.len() + 4);
                    out.extend_from_slice(&bytes[..at]);
                    out.extend_from_slice(comma.as_bytes());
                    out.extend_from_slice(snippet(&entry).as_bytes());
                    out.extend_from_slice(&bytes[at..]);
                    Ok((from_utf8(out), Action::InsertedNested))
                }
            }
        }
        None => {
            let close = top_level_close(text, &classes)
                .ok_or_else(|| "settings.json 找不到顶层对象闭合".to_owned())?;
            // 插入点顶在最后一个非空白码位之后:逗号贴着前一个成员,
            // 避免出现"逗号独占一行"的难看结果。
            let mut insert_at = close;
            while insert_at > 0 && bytes[insert_at - 1].is_ascii_whitespace() {
                insert_at -= 1;
            }
            let comma = if needs_comma(text, &classes, insert_at) { "," } else { "" };
            let mut out = Vec::with_capacity(text.len() + entry.len() + 32);
            out.extend_from_slice(&bytes[..insert_at]);
            out.extend_from_slice(comma.as_bytes());
            out.extend_from_slice(b"\n  \"agent_servers\": {\n    ");
            out.extend_from_slice(snippet(&entry).as_bytes());
            out.extend_from_slice(b"\n  }");
            out.extend_from_slice(&bytes[insert_at..]);
            Ok((from_utf8(out), Action::Inserted))
        }
    }
}

/// 成员片段(嵌套插入用,两格缩进的对齐交给上下文)。
fn snippet(entry: &str) -> String {
    format!("\"{AGENT_KEY}\": {entry}")
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMMAND: &str = "C:/qaqh/target/release/qaqh-daemon.exe";

    fn parse(text: &str) -> serde_json::Value {
        let classes = classify(text);
        serde_json::from_str(&strip_comments(text, &classes))
            .expect("拼接结果剥离注释后必须仍是合法 JSON")
    }

    #[test]
    fn empty_text_creates_object() {
        let (out, action) = wire_entry("", COMMAND).expect("wire");
        assert_eq!(action, Action::Inserted);
        let value = parse(&out);
        assert_eq!(
            value["agent_servers"]["QAQHarness"]["command"].as_str(),
            Some(COMMAND)
        );
    }

    #[test]
    fn top_level_insert_adds_missing_comma() {
        let (out, action) =
            wire_entry("{\n  \"theme\": \"one-dark\"\n}", COMMAND).expect("wire");
        assert_eq!(action, Action::Inserted);
        let value = parse(&out);
        assert_eq!(value["theme"].as_str(), Some("one-dark"));
        assert_eq!(value["agent_servers"]["QAQHarness"]["args"][0], "acp");
    }

    #[test]
    fn trailing_comma_is_not_doubled() {
        let (out, action) =
            wire_entry("{\n  \"theme\": \"one-dark\",\n}", COMMAND).expect("wire");
        assert_eq!(action, Action::Inserted);
        parse(&out);
    }

    #[test]
    fn nested_insert_goes_inside_existing_agent_servers() {
        let text = "{\n  \"agent_servers\": {\n    \"Other\": {\"type\": \"custom\", \"command\": \"x\"}\n  },\n  \"theme\": \"x\"\n}";
        let (out, action) = wire_entry(text, COMMAND).expect("wire");
        assert_eq!(action, Action::InsertedNested);
        let value = parse(&out);
        assert!(value["agent_servers"]["QAQHarness"]["command"].is_string());
        assert!(value["agent_servers"]["Other"].is_object());
    }

    #[test]
    fn existing_entry_with_wrong_command_is_replaced() {
        let text = format!(
            "{{\n  \"agent_servers\": {{\n    \"{AGENT_KEY}\": {{\"type\": \"custom\", \"command\": \"old.exe\", \"args\": []}}\n  }}\n}}"
        );
        let (out, action) = wire_entry(&text, COMMAND).expect("wire");
        assert_eq!(action, Action::Updated);
        let value = parse(&out);
        assert_eq!(
            value["agent_servers"]["QAQHarness"]["command"].as_str(),
            Some(COMMAND)
        );
        assert_eq!(value["agent_servers"]["QAQHarness"]["args"][0], "acp");
    }

    #[test]
    fn already_wired_is_a_noop() {
        let (once, _) = wire_entry("", COMMAND).expect("wire");
        let (twice, action) = wire_entry(&once, COMMAND).expect("wire");
        assert_eq!(action, Action::Already);
        assert_eq!(twice, once);
    }

    #[test]
    fn comments_survive_the_splice() {
        let text = "{\n  // 界面主题\n  \"theme\": \"one-dark\", /* 内联 */\n}";
        let (out, _) = wire_entry(text, COMMAND).expect("wire");
        assert!(out.contains("// 界面主题"), "行注释丢失");
        assert!(out.contains("/* 内联 */"), "块注释丢失");
        parse(&out);
    }
}
