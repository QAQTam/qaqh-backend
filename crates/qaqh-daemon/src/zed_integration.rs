//! `qaqh-daemon --acp`:把 QAQHarness 注册进 Zed 的 `agent_servers`。
//!
//! 这是用户在本机敲的**本地安装动作**(等价 `code --install-extension`),
//! 与"远端 daemon 不得碰本机文件"的边界不冲突:远端场景走不到这个 flag。
//!
//! Zed 的 settings.json 是 JSONC(允许注释/尾逗号)。这里做**注释保留的
//! 增量编辑**,绝不整文件重排:v4.5 P2-2b 起解析与拼接交给
//! `jsonc-parser` 的 CST(基线 480 行手写状态机物理删除),改动只落在
//! `append`/`set_value` 命中的节点;写回前留一份 `.qaqh-bak`。

use std::path::{Path, PathBuf};

use jsonc_parser::cst::{CstInputValue, CstObjectProp, CstRootNode};
use jsonc_parser::ParseOptions;

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
        std::env::var("APPDATA")
            .ok()
            .map(|root| PathBuf::from(root).join("Zed"))
    } else if cfg!(target_os = "macos") {
        std::env::var("HOME")
            .ok()
            .map(|root| PathBuf::from(root).join("Library/Application Support/Zed"))
    } else {
        std::env::var("HOME")
            .ok()
            .map(|root| PathBuf::from(root).join(".config/zed"))
    }?;
    if !dir.is_dir() {
        return None;
    }
    Some(dir.join("settings.json"))
}

/// 期望的条目 wire 形态。`command` 用正斜杠(Zed Windows 接受,免去转义)。
fn desired_entry(command: &str) -> CstInputValue {
    CstInputValue::Object(vec![
        ("type".to_owned(), CstInputValue::String("custom".to_owned())),
        ("command".to_owned(), CstInputValue::String(normalize(command))),
        (
            "args".to_owned(),
            CstInputValue::Array(vec![CstInputValue::String("acp".to_owned())]),
        ),
    ])
}

fn normalize(path: &str) -> String {
    path.replace('\\', "/")
}

/// 手动接入提示(找不到 Zed 配置目录时的出路)。
fn manual_snippet(command: &str) -> String {
    // 序列化同样交给 CST(空文档 set_value 后的 pretty 输出),
    // `desired_entry` 保持全文件唯一形状源。
    let scratch = CstRootNode::parse("", &jsonc_options()).expect("空文本必可解析");
    scratch.set_value(desired_entry(command));
    let entry = scratch.to_string();
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
        Action::InsertedNested | Action::Updated => {
            format!("已更新 {}(备份 {})", path.display(), backup.display())
        }
    })
}

// ──────────────────── JSONC 编辑（jsonc-parser CST） ────────────────────
//
// 基线在此手写的词法分类(Class/classify)、注释剥离(strip_comments)、
// 字节区间定位(top_level_close/member_value_span)、逗号试探(needs_comma)
// 与拼接全部物理删除:注释/空白的**逐字节保留**、尾逗号风格跟随、增量
// 写回改由 `jsonc-parser` 的 CST(一等编辑 API)承担。

/// Zed 设置 = 标准 JSONC:允许注释与尾逗号,其余严格 JSON(fail-closed)。
/// dprint jsonc-parser 的默认 `ParseOptions` 极宽松(单引号、十六进制、
/// 缺逗号等 JSON5 行为全开),逐项收紧到与旧实现剥离注释后交给
/// `serde_json` 的严格 JSON 面一致。
fn jsonc_options() -> ParseOptions {
    ParseOptions {
        allow_comments: true,
        allow_trailing_commas: true,
        allow_loose_object_property_names: false,
        allow_missing_commas: false,
        allow_single_quoted_strings: false,
        allow_hexadecimal_numbers: false,
        allow_unary_plus_numbers: false,
        allow_bare_decimal_point_numbers: false,
        allow_non_finite_numbers: false,
        allow_extended_string_escapes: false,
    }
}

/// 条目是否已指向本二进制。与旧实现逐字段一致:type=`custom`、
/// command 斜杠归一后相等、args 恰为 `["acp"]`。
fn entry_is_wired(entry: &CstObjectProp, command: &str) -> bool {
    let Some(value) = entry.value().and_then(|node| node.to_serde_value()) else {
        return false;
    };
    let Some(args) = value.get("args").and_then(serde_json::Value::as_array) else {
        return false;
    };
    value.get("type").and_then(serde_json::Value::as_str) == Some("custom")
        && value
            .get("command")
            .and_then(serde_json::Value::as_str)
            .is_some_and(|existing| normalize(existing) == normalize(command))
        && args.len() == 1
        && args.first().and_then(serde_json::Value::as_str) == Some("acp")
}

/// 把 `command` 指向的本二进制拼进设置文本。纯函数:输入原文,输出新文
/// 与动作,便于单测覆盖全部拼接分支。语义判定与定位走 CST 树
/// (`get`/`object_value`),改动走 `append`/`set_value`,除被编辑节点外
/// 原文逐字节保留(含注释/空白/尾逗号风格)。
fn wire_entry(text: &str, command: &str) -> Result<(String, Action), String> {
    let root = CstRootNode::parse(text, &jsonc_options())
        .map_err(|error| format!("settings.json 无法解析(Zed 设置已是病态): {error}"))?;
    let is_new_file = text.trim().is_empty();
    // 空文件/新建:根无值 → 造对象;非空:根值必须是对象(否则病态,不碰盘)。
    let Some(root_object) = (if is_new_file {
        root.object_value_or_create()
    } else {
        root.object_value()
    }) else {
        return Err("settings.json 顶层不是 JSON 对象".to_owned());
    };
    let action = match root_object.get("agent_servers") {
        Some(existing) => {
            let Some(servers) = existing.object_value() else {
                // 相对基线的**显式行为修正**(ADR-0044 登记):旧扫描器对
                // 非对象 `agent_servers` 也会拼出坏 JSON 写盘,现 fail-closed。
                return Err("settings.json 的 agent_servers 不是 JSON 对象(Zed 设置已是病态)".to_owned());
            };
            match servers.get(AGENT_KEY) {
                Some(entry_prop) if entry_is_wired(&entry_prop, command) => {
                    return Ok((text.to_owned(), Action::Already));
                }
                Some(entry_prop) => {
                    entry_prop.set_value(desired_entry(command));
                    Action::Updated
                }
                None => {
                    servers.append(AGENT_KEY, desired_entry(command));
                    Action::InsertedNested
                }
            }
        }
        None => {
            let servers = root_object
                .object_value_or_create("agent_servers")
                .expect("agent_servers 成员刚确认不存在,append 建对象必成");
            servers.append(AGENT_KEY, desired_entry(command));
            Action::Inserted
        }
    };
    Ok((root.to_string(), action))
}

#[cfg(test)]
mod tests {
    use super::*;

    const COMMAND: &str = "C:/qaqh/target/release/qaqh-daemon.exe";

    fn parse(text: &str) -> serde_json::Value {
        CstRootNode::parse(text, &jsonc_options())
            .expect("拼接结果必须仍是合法 JSONC")
            .to_serde_value()
            .expect("拼接结果顶层必有值")
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
        let (out, action) = wire_entry("{\n  \"theme\": \"one-dark\"\n}", COMMAND).expect("wire");
        assert_eq!(action, Action::Inserted);
        let value = parse(&out);
        assert_eq!(value["theme"].as_str(), Some("one-dark"));
        assert_eq!(value["agent_servers"]["QAQHarness"]["args"][0], "acp");
    }

    #[test]
    fn trailing_comma_is_not_doubled() {
        let (out, action) = wire_entry("{\n  \"theme\": \"one-dark\",\n}", COMMAND).expect("wire");
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
