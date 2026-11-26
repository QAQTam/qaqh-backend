//! 协议指纹生成器（`ADR-0042` 铁律 2："机器生成协议指纹，严禁手改字符串版本"）。
//!
//! ## 算法
//!
//! 1. 读 `protocol_rev.txt` → `PROTOCOL_REV`（唯一人工维护的版本位，一个整数字，
//!    diff 里看得见、review 得动）；
//! 2. 把 `src/**/*.rs` 的**归一化文本**（剥注释 + 折叠空白）按文件名排序拼接，
//!    sha256 得 `PROTOCOL_FINGERPRINT: [u8; 32]`。
//!
//! ## 为什么"剥注释 + 折叠空白"
//!
//! 直接把源文件字节哈希会让**改一行注释**就换指纹——那等于每次文档修订都强制
//! bump 协议版本，指纹会退化成噪声，最后没人再信它。剥掉注释与空白之后，
//! 指纹只对**结构变动**敏感（字段增删、变体增删、类型改名、顺序调整），
//! 这才是"线缆形状变了"的机器可判据。
//!
//! 指纹是**产物**不是**输入**：源码里没有可以手改的 `[u8; 32]` 字面量，
//! 想伪造就得伪造到编译期生成物上——那会在 review 里一眼看见 build.rs 被改。

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

    let mut sources = collect_sources(&manifest_dir.join("src"));
    sources.sort_by(|left, right| left.0.cmp(&right.0));

    let mut hasher = Sha256::new();
    // REV 一起进哈希：同一份形状配不同 REV 必须是不同指纹。
    hasher.update(rev.to_le_bytes());
    for (name, text) in &sources {
        hasher.update(name.as_bytes());
        hasher.update([0u8]);
        hasher.update(normalize(text).as_bytes());
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

/// 收集 `src/` 下所有 `.rs`（递归），返回 (相对文件名, 全文)。
fn collect_sources(root: &Path) -> Vec<(String, String)> {
    let mut collected = Vec::new();
    let mut stack = vec![root.to_path_buf()];
    while let Some(dir) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
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
            let name = path
                .strip_prefix(root)
                .expect("under src")
                .to_string_lossy()
                .replace('\\', "/");
            collected.push((name, text));
        }
    }
    collected
}

/// 归一化：剥 `//` 与 `/* */` 注释，字符串字面量原样保留，折叠空白。
///
/// 手写而不是引正则：注释剥离必须**区分字符串字面量里的 `//`**（`"qaqh-ringing/3"`
/// 里的斜杠不能被当成注释起点），正则做不到这件事，裸索引推进可以。
fn normalize(source: &str) -> String {
    let bytes = source.as_bytes();
    let mut out = String::with_capacity(source.len());
    let mut index = 0usize;
    let mut pending_space = false;
    while index < bytes.len() {
        let byte = bytes[index];
        if byte == b'/' && index + 1 < bytes.len() && bytes[index + 1] == b'/' {
            while index < bytes.len() && bytes[index] != b'\n' {
                index += 1;
            }
            continue;
        }
        if byte == b'/' && index + 1 < bytes.len() && bytes[index + 1] == b'*' {
            index += 2;
            while index + 1 < bytes.len() && !(bytes[index] == b'*' && bytes[index + 1] == b'/') {
                index += 1;
            }
            index = (index + 2).min(bytes.len());
            continue;
        }
        if byte == b'"' {
            // 字符串字面量：原样搬运（含转义），不做空白折叠。
            if pending_space && !out.is_empty() {
                out.push(' ');
            }
            pending_space = false;
            out.push('"');
            index += 1;
            while index < bytes.len() {
                let inner = bytes[index];
                out.push(inner as char);
                index += 1;
                if inner == b'\\' && index < bytes.len() {
                    out.push(bytes[index] as char);
                    index += 1;
                    continue;
                }
                if inner == b'"' {
                    break;
                }
            }
            continue;
        }
        if byte.is_ascii_whitespace() {
            pending_space = true;
            index += 1;
            continue;
        }
        if pending_space && !out.is_empty() {
            out.push(' ');
        }
        pending_space = false;
        out.push(byte as char);
        index += 1;
    }
    out
}
