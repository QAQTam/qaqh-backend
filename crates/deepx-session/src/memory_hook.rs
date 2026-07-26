//! Cross-session memory archiving hook.
//!
//! Extracts noteworthy exchanges from completed sessions and persists
//! them to the `MemoryStore` for future `memory_search` retrieval.
//!
//! ## Usage
//!
//! ```ignore
//! #[cfg(feature = "rag")]
//! deepx_session::memory_hook::archive_session_memories(
//!     &session_id,
//!     &messages,
//!     data_dir,
//! ).ok();
//! ```

use std::path::Path;

use deepx_types::Message;
use deepx_vector::memory::{MemoryEntry, MemoryStore};
use deepx_vector::VectorResult;

/// Archive noteworthy exchanges from a session into persistent memory.
///
/// Returns the number of memories archived.
pub fn archive_session_memories(
    session_id: &str,
    messages: &[Message],
    data_dir: &Path,
) -> VectorResult<usize> {
    let store = MemoryStore::open(data_dir)?;
    let entries = extract_entries(session_id, messages);
    let count = entries.len();

    for entry in entries {
        store.remember(entry)?;
    }

    log::info!("archived {} memories from session {}", count, session_id);
    Ok(count)
}

/// Extract text content from a Message's content blocks.
fn message_text(msg: &Message) -> String {
    msg.content
        .iter()
        .filter_map(|block| match block {
            deepx_types::ContentBlock::Text { text } => Some(text.as_str()),
            deepx_types::ContentBlock::Reasoning { reasoning } => Some(reasoning.as_str()),
            _ => None,
        })
        .collect::<Vec<&str>>()
        .join("\n")
}

/// Scan messages for patterns indicating decisions, fixes, or findings.
fn extract_entries(session_id: &str, messages: &[Message]) -> Vec<MemoryEntry> {
    let mut entries = Vec::new();
    let timestamp = chrono_now();

    for (i, msg) in messages.iter().enumerate() {
        let content = message_text(msg);
        if content.is_empty() {
            continue;
        }
        let lower = content.to_lowercase();

        let mem_type = if lower.contains("决定")
            || lower.contains("选择")
            || lower.contains("方案")
        {
            "decision"
        } else if lower.contains("修复")
            || lower.contains("fix")
            || lower.contains("解决")
            || lower.contains("bug")
        {
            "fix"
        } else if lower.contains("发现")
            || lower.contains("找到")
            || lower.contains("原来")
            || lower.contains("根因")
        {
            "finding"
        } else {
            continue;
        };

        entries.push(MemoryEntry {
            id: format!("{session_id}-{i}"),
            memory_type: mem_type.into(),
            session_id: session_id.into(),
            timestamp: timestamp.clone(),
            content: extract_excerpt(&content, 500),
            metadata: String::new(),
        });
    }

    // Fallback: last meaningful message as summary
    if entries.is_empty() {
        for msg in messages.iter().rev() {
            let content = message_text(msg);
            if !content.is_empty() {
                entries.push(MemoryEntry {
                    id: format!("{session_id}-summary"),
                    memory_type: "summary".into(),
                    session_id: session_id.into(),
                    timestamp,
                    content: extract_excerpt(&content, 600),
                    metadata: String::new(),
                });
                break;
            }
        }
    }

    entries
}

fn extract_excerpt(content: &str, max_len: usize) -> String {
    if content.len() <= max_len {
        content.to_string()
    } else if let Some(cut) = content[..max_len].rfind("\n\n") {
        content[..cut].to_string()
    } else if let Some(cut) = content[..max_len].rfind('\n') {
        content[..cut].to_string()
    } else if let Some(cut) = content[..max_len].rfind(". ") {
        content[..=cut].to_string()
    } else {
        content[..max_len].to_string()
    }
}

// ─── Timestamp helpers ──────────────────────────────��────────────────

fn chrono_now() -> String {
    use std::time::{SystemTime, UNIX_EPOCH};
    let dur = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let secs = dur.as_secs();
    let days = secs / 86400;
    let tod = secs % 86400;
    let (y, m, d) = days_to_ymd(days as i64);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        tod / 3600,
        (tod % 3600) / 60,
        tod % 60,
    )
}

fn days_to_ymd(days: i64) -> (i64, u32, u32) {
    let mut y = 1970i64;
    let mut d = days;
    loop {
        let diy = if is_leap(y) { 366 } else { 365 };
        if d < diy {
            break;
        }
        d -= diy;
        y += 1;
    }
    let md = if is_leap(y) {
        [31, 29, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    } else {
        [31, 28, 31, 30, 31, 30, 31, 31, 30, 31, 30, 31]
    };
    let mut m = 0u32;
    for &days_in_m in &md {
        if d < days_in_m as i64 {
            break;
        }
        d -= days_in_m as i64;
        m += 1;
    }
    (y, m + 1, (d + 1) as u32)
}

fn is_leap(y: i64) -> bool {
    (y % 4 == 0 && y % 100 != 0) || y % 400 == 0
}

// ─── Tests ───────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn make_msg(content: &str) -> Message {
        Message {
            msg_id: None,
            role: "user".into(),
            content: vec![deepx_types::ContentBlock::Text {
                text: content.into(),
            }],
            name: None,
        }
    }

    #[test]
    fn extract_decision() {
        let msgs = vec![
            make_msg("hello"),
            make_msg("我决定使用 SQLite 作为本地存储方案"),
        ];
        let entries = extract_entries("s1", &msgs);
        assert!(!entries.is_empty());
        assert_eq!(entries[0].memory_type, "decision");
    }

    #[test]
    fn extract_fix() {
        let msgs = vec![make_msg("修复了 Tauri 构建问题：需要安装 Windows SDK")];
        let entries = extract_entries("s2", &msgs);
        assert!(!entries.is_empty());
        assert_eq!(entries[0].memory_type, "fix");
    }

    #[test]
    fn extract_finding() {
        let msgs = vec![make_msg("发现根因是 borrow checker 的生命周期推断有误")];
        let entries = extract_entries("s3", &msgs);
        assert!(!entries.is_empty());
        assert_eq!(entries[0].memory_type, "finding");
    }

    #[test]
    fn fallback_summary() {
        let msgs = vec![make_msg("some random text without keywords")];
        let entries = extract_entries("s4", &msgs);
        assert!(!entries.is_empty());
        assert_eq!(entries[0].memory_type, "summary");
    }

    #[test]
    fn empty_messages() {
        let entries = extract_entries("s5", &[]);
        assert!(entries.is_empty());
    }
}
