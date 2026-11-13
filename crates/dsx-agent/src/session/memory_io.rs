//! Memory tier and pitfall persistence.

use crate::memory::{FileSemanticEntry, PitfallGuide, SemanticMemory};

// ── Pitfall guide ──

pub fn load_pitfalls() -> PitfallGuide {
    let Some(path) = super::pitfalls_path() else { return PitfallGuide::default(); };
    if !path.exists() { return PitfallGuide::default(); }
    let Ok(data) = std::fs::read_to_string(&path) else { return PitfallGuide::default(); };
    serde_json::from_str(&data).unwrap_or_default()
}

pub fn save_pitfalls(guide: &PitfallGuide) {
    let Some(path) = super::pitfalls_path() else { return };
    let _ = std::fs::create_dir_all(path.parent().unwrap());
    if let Ok(json) = serde_json::to_string(guide) {
        let _ = std::fs::write(&path, json);
    }
}

// ── Memory I/O ──

/// Read a memory file, truncating to ~4K tokens (~15K chars) for context injection.
pub fn read_memory(seed: &str, tier: &str) -> String {
    let Some(path) = super::memory_path(seed, tier) else { return String::new(); };
    if !path.exists() { return String::new(); }
    let Ok(content) = std::fs::read_to_string(&path) else { return String::new(); };
    // Truncate to ~4K tokens (roughly 12K chars for CJK, 16K for ASCII).
    // Trim from the beginning to keep newest content.
    if content.len() > 16000 {
        let start = content.len() - 16000;
        let mut s = start;
        while s < content.len() && !content.is_char_boundary(s) { s += 1; }
        if let Some(nl) = content[s..].find('\n') {
            content[s + nl + 1..].to_string()
        } else {
            content[s..].to_string()
        }
    } else {
        content
    }
}

/// Overwrite a memory file with new content.
pub fn write_memory(seed: &str, tier: &str, content: &str) {
    let Some(path) = super::memory_path(seed, tier) else { return };
    let _ = std::fs::create_dir_all(path.parent().unwrap());
    let _ = std::fs::write(&path, content);
}

/// Append a learning entry to learning.md (cross-session self-improvement log).
pub fn append_learning(seed: &str, entry: &str) {
    let Some(path) = super::memory_path(seed, "learning") else { return };
    let _ = std::fs::create_dir_all(path.parent().unwrap());

    let mut existing = if path.exists() {
        std::fs::read_to_string(&path).unwrap_or_default()
    } else {
        String::new()
    };

    // Keep under ~12K chars (trim from beginning, keep newest)
    if existing.len() > 12000 {
        let cut = existing.len().saturating_sub(8000);
        let mut s = cut;
        while s < existing.len() && !existing.is_char_boundary(s) { s += 1; }
        if let Some(nl) = existing[s..].find('\n') {
            existing = existing[s + nl + 1..].to_string();
        } else {
            existing = existing[s..].to_string();
        }
    }

    let ts = super::now_epoch();
    let dt = super::chrono_date();
    let header = format!("# {} (epoch:{})\n", dt, ts);
    // Only add header if file is new or day changed
    let out = if existing.is_empty() {
        format!("{}{}\n", header, entry)
    } else {
        // Prepend header if not already present for today
        if !existing.contains(&dt) {
            format!("{}\n{}{}", header, existing, entry)
        } else {
            format!("{}{}", existing, entry)
        }
    };
    let _ = std::fs::write(&path, &out);
}

/// Append a line to a memory file, respecting max file size (~8K tokens).
pub fn append_memory(seed: &str, tier: &str, line: &str) {
    let Some(path) = super::memory_path(seed, tier) else { return };
    let _ = std::fs::create_dir_all(path.parent().unwrap());

    let mut existing = if path.exists() {
        std::fs::read_to_string(&path).unwrap_or_default()
    } else {
        String::new()
    };

    // Keep file under ~8K tokens
    const MAX_CHARS: usize = 32000;
    if existing.len() > MAX_CHARS {
        let cut = existing.len().saturating_sub(MAX_CHARS / 2);
        existing = existing[cut..].to_string();
        // Skip to next newline for clean truncation
        if let Some(nl) = existing.find('\n') {
            existing = existing[nl + 1..].to_string();
        }
    }

    existing.push_str(line);
    existing.push('\n');
    let _ = std::fs::write(&path, &existing);
}

/// Write memory file, preserving AI-authored notes (lines starting with "- key: content").
/// Used for "short" tier where both the system (round entries) and AI (mem_save) write.
pub fn write_memory_preserving_notes(seed: &str, tier: &str, new_content: &str) {
    let Some(path) = super::memory_path(seed, tier) else { return };
    let _ = std::fs::create_dir_all(path.parent().unwrap());

    // Extract AI-authored notes from existing file (lines like "- key: content")
    let mut notes: Vec<String> = Vec::new();
    if path.exists() {
        if let Ok(existing) = std::fs::read_to_string(&path) {
            for line in existing.lines() {
                let trimmed = line.trim();
                if trimmed.starts_with("- ") && trimmed.contains(": ") {
                    notes.push(trimmed.to_string());
                }
            }
        }
    }

    let mut out = new_content.to_string();
    if !notes.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    for note in &notes {
        out.push_str(note);
        out.push('\n');
    }
    let _ = std::fs::write(&path, &out);
}

/// Write SemanticMemory to disk as JSON for glance tool queries.
pub fn write_semantic_memory(seed: &str, sem: &SemanticMemory) {
    let Some(dir) = super::session_dir(seed) else { return };
    let _ = std::fs::create_dir_all(&dir);
    let path = dir.join("semantic-mem.json");
    if let Ok(json) = serde_json::to_string(sem) {
        let _ = std::fs::write(&path, json);
    }
}

/// Look up a single file entry from persisted SemanticMemory.
pub fn read_semantic_entry(seed: &str, file_path: &str) -> Option<FileSemanticEntry> {
    let Some(dir) = super::session_dir(seed) else { return None };
    let path = dir.join("semantic-mem.json");
    let Ok(data) = std::fs::read_to_string(&path) else { return None };
    let Ok(sem) = serde_json::from_str::<SemanticMemory>(&data) else { return None };
    sem.entries.get(file_path).cloned()
}

/// Delete a key from memory files.
pub fn forget_memory_key(seed: &str, key: &str) {
    for tier in ["long", "short", "tasks"] {
        let Some(path) = super::memory_path(seed, tier) else { continue };
        if !path.exists() { continue; }
        if let Ok(content) = std::fs::read_to_string(&path) {
            let filtered: String = content
                .lines()
                .filter(|l| !l.contains(key))
                .collect::<Vec<_>>()
                .join("\n");
            let out = if filtered.is_empty() { filtered } else { filtered + "\n" };
            let _ = std::fs::write(&path, out);
        }
    }
}
