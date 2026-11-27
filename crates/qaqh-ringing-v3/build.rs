//! 协议指纹生成器（`ADR-0042` 铁律 2："机器生成协议指纹，严禁手改字符串版本"）。
//!
//! ## 算法
//!
//! 1. 读 `protocol_rev.txt` → `PROTOCOL_REV`（唯一人工维护的版本位，一个整数字，
//!    diff 里看得见、review 得动）；
//! 2. 把 `qaqh-core-vocab/src/**/*.rs` 与 `qaqh-ringing-v3/src/**/*.rs`（排除测试文件）
//!    的**归一化字节流**（剥注释 + 折叠空白）按 crate 与文件名排序拼接，
//!    sha256 得 `PROTOCOL_FINGERPRINT: [u8; 32]`。
//!
//! ## 为什么纳入 `qaqh-core-vocab`
//!
//! `qaqh-ringing-v3` 线缆 DTO 直接内嵌了 `qaqh-core-vocab` 的类型（`ActivityState`、
//! `TurnTerminal`、`UsageInfo`、`ContentRef`、`ToolMode`、`ToolCatalog` 等）。
//! 若只哈希本 crate 源码，改动 `qaqh-core-vocab` 字段顺序时指纹不会变化，跨端握手
//! 会放行并触发位置编码静默错位。
//!
//! ## 为什么"剥注释 + 折叠空白"
//!
//! 直接把源文件字节哈希会让**改一行注释**就换指纹——那等于每次文档修订都强制
//! bump 协议版本，指纹会退化成噪声，最后没人再信它。剥掉注释与空白之后，
//! 指纹只对**结构变动**敏感（字段增删、变体增删、类型改名、顺序调整），
//! 这才是"线缆形状变了"的机器可判据。

use std::env;
use std::fs;
use std::path::{Path, PathBuf};

use sha2::{Digest, Sha256};

fn main() {
    let manifest_dir = PathBuf::from(env::var("CARGO_MANIFEST_DIR").expect("manifest dir"));
    let rev_path = manifest_dir.join("protocol_rev.txt");
    let rev_text = fs::read_to_string(&rev_path).expect("read protocol_rev.txt");
    let rev: u32 = rev_text
        .trim()
        .parse()
        .expect("protocol_rev.txt 必须是单个十进制整数");
    println!("cargo:rerun-if-changed=protocol_rev.txt");

    let vocab_src = manifest_dir
        .parent()
        .expect("crates dir")
        .join("qaqh-core-vocab")
        .join("src");
    let v3_src = manifest_dir.join("src");

    let mut sources = Vec::new();
    collect_sources("qaqh-core-vocab", &vocab_src, &mut sources);
    collect_sources("qaqh-ringing-v3", &v3_src, &mut sources);
    sources.sort_by(|left, right| left.0.cmp(&right.0));

    let mut hasher = Sha256::new();
    // REV 一起进哈希：同一份形状配不同 REV 必须是不同指纹。
    hasher.update(rev.to_le_bytes());
    for (name, text) in &sources {
        hasher.update(name.as_bytes());
        hasher.update([0u8]);
        hasher.update(normalize(text));
        hasher.update([0u8]);
    }
    let digest = hasher.finalize();

    let mut fingerprint = String::from("[");
    for (index, byte) in digest.iter().enumerate() {
        if index > 0 {
            fingerprint.push_str(", ");
        }
        fingerprint.push_str(&format!("{byte}u8"));
    }
    fingerprint.push(']');

    let out_dir = PathBuf::from(env::var("OUT_DIR").expect("out dir"));
    let generated = format!(
        "// 本文件由 build.rs 生成，**不要手改**（ADR-0042 铁律 2）。\n\
         /// 协议修订号（唯一人工维护的版本位，来源 `protocol_rev.txt`）。\n\
         pub const PROTOCOL_REV: u32 = {rev};\n\
         /// 由类型定义源码机器计算的协议指纹（sha256，32 字节）。\n\
         pub const PROTOCOL_FINGERPRINT: [u8; 32] = {fingerprint};\n"
    );
    fs::write(out_dir.join("contract.rs"), generated).expect("write contract.rs");
}

/// 收集 `root` 下所有 `.rs`（递归），返回 `(prefix/相对文件名, 全文)`。
fn collect_sources(prefix: &str, root: &Path, collected: &mut Vec<(String, String)>) {
    println!("cargo:rerun-if-changed={}", root.display());
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                println!("cargo:rerun-if-changed={}", path.display());
                stack.push(path);
                continue;
            }
            // 测试文件不进指纹：改一条断言不该等于改协议形状。
            let is_test_file = path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name == "tests.rs" || name.ends_with("_tests.rs"));
            if path.extension().and_then(|ext| ext.to_str()) != Some("rs") || is_test_file {
                continue;
            }
            let text = fs::read_to_string(&path).expect("read source");
            println!("cargo:rerun-if-changed={}", path.display());
            let rel = path
                .strip_prefix(root)
                .expect("under src")
                .to_string_lossy()
                .replace('\\', "/");
            collected.push((format!("{prefix}/{rel}"), text));
        }
    }
}

/// 归一化：剥 `//` 与嵌套 `/* */` 注释，原样保留普通/原始字符串及字符字面量，折叠空白。
fn normalize(source: &str) -> Vec<u8> {
    let bytes = source.as_bytes();
    let mut out = Vec::with_capacity(bytes.len());
    let mut index = 0usize;
    let mut pending_space = false;

    while index < bytes.len() {
        let byte = bytes[index];

        // 行注释
        if byte == b'/' && index + 1 < bytes.len() && bytes[index + 1] == b'/' {
            while index < bytes.len() && bytes[index] != b'\n' {
                index += 1;
            }
            continue;
        }

        // 嵌套块注释 /* ... /* ... */ ... */
        if byte == b'/' && index + 1 < bytes.len() && bytes[index + 1] == b'*' {
            index += 2;
            let mut depth = 1usize;
            while index < bytes.len() && depth > 0 {
                if index + 1 < bytes.len() && bytes[index] == b'/' && bytes[index + 1] == b'*' {
                    depth += 1;
                    index += 2;
                } else if index + 1 < bytes.len() && bytes[index] == b'*' && bytes[index + 1] == b'/' {
                    depth -= 1;
                    index += 2;
                } else {
                    index += 1;
                }
            }
            continue;
        }

        // 原始字符串 r"..." / r#"..."# / br"..." / br#"..."#
        if let Some((prefix_len, hashes)) = match_raw_string_start(bytes, index) {
            flush_space(&mut out, &mut pending_space);
            out.extend_from_slice(&bytes[index..index + prefix_len]);
            index += prefix_len;
            while index < bytes.len() {
                let ch = bytes[index];
                out.push(ch);
                index += 1;
                if ch == b'"' && bytes[index..].iter().take(hashes).all(|&b| b == b'#')
                    && bytes.len() - index >= hashes
                {
                    out.extend_from_slice(&bytes[index..index + hashes]);
                    index += hashes;
                    break;
                }
            }
            continue;
        }

        // 普通字符串（含 b"..." 时 `b` 当普通标识符字节先吐出，紧接着命中此分支）
        if byte == b'"' {
            flush_space(&mut out, &mut pending_space);
            out.push(b'"');
            index += 1;
            while index < bytes.len() {
                let inner = bytes[index];
                out.push(inner);
                index += 1;
                if inner == b'\\' && index < bytes.len() {
                    out.push(bytes[index]);
                    index += 1;
                    continue;
                }
                if inner == b'"' {
                    break;
                }
            }
            continue;
        }

        // 字符字面量（如 '"' / '\'' / '\x7f' / '\u{...}'）——不把生命期 `'a` 误判成字面量
        if byte == b'\'' && let Some(lit_len) = match_char_literal_len(bytes, index) {
            flush_space(&mut out, &mut pending_space);
            out.extend_from_slice(&bytes[index..index + lit_len]);
            index += lit_len;
            continue;
        }

        if byte.is_ascii_whitespace() {
            pending_space = true;
            index += 1;
            continue;
        }

        flush_space(&mut out, &mut pending_space);
        out.push(byte);
        index += 1;
    }
    out
}

fn flush_space(out: &mut Vec<u8>, pending_space: &mut bool) {
    if *pending_space && !out.is_empty() {
        out.push(b' ');
    }
    *pending_space = false;
}

fn match_raw_string_start(bytes: &[u8], index: usize) -> Option<(usize, usize)> {
    // 若前面紧挨着标识符字节（如 `str"..."`），那不是 `r"..."`
    if index > 0 && is_ident_continue(bytes[index - 1]) {
        return None;
    }
    let mut cursor = index;
    if bytes.get(cursor) == Some(&b'b') {
        cursor += 1;
    }
    if bytes.get(cursor) != Some(&b'r') {
        return None;
    }
    cursor += 1;
    let mut hashes = 0usize;
    while bytes.get(cursor) == Some(&b'#') {
        hashes += 1;
        cursor += 1;
    }
    if bytes.get(cursor) == Some(&b'"') {
        Some((cursor + 1 - index, hashes))
    } else {
        None
    }
}

fn match_char_literal_len(bytes: &[u8], index: usize) -> Option<usize> {
    let mut cursor = index + 1;
    if cursor >= bytes.len() {
        return None;
    }
    if bytes[cursor] == b'\\' {
        cursor += 1;
        if cursor >= bytes.len() {
            return None;
        }
        cursor += 1;
        while cursor < bytes.len() && cursor - index <= 16 {
            if bytes[cursor] == b'\'' {
                return Some(cursor + 1 - index);
            }
            if bytes[cursor] == b'\n' {
                return None;
            }
            cursor += 1;
        }
        return None;
    }
    let rest = std::str::from_utf8(&bytes[cursor..]).ok()?;
    let ch = rest.chars().next()?;
    let end = cursor + ch.len_utf8();
    if bytes.get(end) == Some(&b'\'') {
        Some(end + 1 - index)
    } else {
        None
    }
}

fn is_ident_continue(byte: u8) -> bool {
    byte.is_ascii_alphanumeric() || byte == b'_'
}
