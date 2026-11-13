//! Real implementations for shared types and session I/O.
//!
//! dsx-tools runs as a subprocess. For memory/plan/pitfall persistence, it accesses
//! the session filesystem directly (same paths as dsx-agent), avoiding IPC round-trips
//! for simple read/write operations.

use dsx_types;
use std::path::PathBuf;

use serde::{Deserialize, Serialize};

// ── StreamEvent (used by exec.rs async spawners) ──

#[derive(Debug, Clone)]
pub enum StreamEvent {
    ExecDone(String, String),
    ExecProgress(String),
    ExecStarted(String, u32),
    Error(String),
}

// ── AgentEmotion (used by pitfall_save tool) ──

#[derive(Debug, Clone)]
pub enum AgentEmotion {
    Flow, Calm, Anxious, Frustrated, Confused, Panic,
}

// ── ErrorKind (used by pitfall_save tool) ──

#[derive(Debug, Clone)]
pub enum ErrorKind {
    ToolParameter, FileAccess, ExecFailure, NetworkFailure, ApiError,
    SudoFailure, Panic, SessionMissing, ToolNotFound,
    Unknown, Permission, Config, Io, Timeout,
}

// ── Pitfall types ──

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PitfallEntry {
    pub emotion: String,
    pub error_kind: String,
    pub tool: String,
    pub description: String,
    pub lesson: String,
    #[serde(default)]
    pub files: Vec<String>,
    #[serde(default)]
    pub frequency: u32,
    #[serde(default)]
    pub first_seen: u64,
    #[serde(default)]
    pub last_seen: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PitfallGuide {
    pub entries: Vec<PitfallEntry>,
}

impl Default for PitfallGuide {
    fn default() -> Self {
        Self { entries: Vec::new() }
    }
}

impl PitfallGuide {
    pub fn upsert(&mut self, emotion: AgentEmotion, kind: ErrorKind, tool: &str,
                  description: &str, lesson: &str, files: &[String]) {
        let kind_name = format!("{:?}", kind);
        let emotion_label = format!("{:?}", emotion);
        let now = now_epoch();
        let desc_prefix: String = description.chars().take(40).collect();

        for entry in &mut self.entries {
            if entry.error_kind == kind_name && entry.tool == tool
                && entry.description.starts_with(&desc_prefix)
            {
                entry.frequency += 1;
                entry.last_seen = now;
                if !files.is_empty() {
                    for f in files {
                        if !entry.files.contains(f) {
                            entry.files.push(f.clone());
                        }
                    }
                }
                entry.lesson = format!("{}\n{}", entry.lesson, lesson);
                if entry.lesson.len() > 500 {
                    entry.lesson = entry.lesson[..500].to_string();
                }
                return;
            }
        }

        self.entries.push(PitfallEntry {
            emotion: emotion_label,
            error_kind: kind_name,
            tool: tool.to_string(),
            description: description.chars().take(300).collect(),
            lesson: lesson.chars().take(200).collect(),
            files: files.to_vec(),
            frequency: 1,
            first_seen: now,
            last_seen: now,
        });

        if self.entries.len() > 50 {
            self.entries.sort_by(|a, b| {
                a.frequency.cmp(&b.frequency)
                    .then_with(|| a.last_seen.cmp(&b.last_seen))
            });
            self.entries.remove(0);
        }
    }
}

// ── Path resolution (same as dsx-agent::session) ──

fn now_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn chrono_date() -> String {
    let secs = now_epoch();
    let days = (secs / 86400) as u32;
    let y = 1970 + days / 365;
    let rem = days % 365;
    let m = (rem / 30).min(11) + 1;
    let d = (rem % 30).min(30) + 1;
    format!("{:04}-{:02}-{:02}", y, m, d)
}

fn sessions_dir() -> Option<PathBuf> {
    Some(dsx_types::platform::sessions_dir())
}

fn session_dir(seed: &str) -> Option<PathBuf> {
    sessions_dir().map(|d| {
        let date = chrono_date();
        d.join(format!("{}-{}", seed, date))
    })
}

fn memory_path(seed: &str, tier: &str) -> Option<PathBuf> {
    session_dir(seed).map(|d| d.join(format!("{}-mem.md", tier)))
}

fn pitfalls_path() -> Option<PathBuf> {
    sessions_dir().map(|d| d.join("pitfalls.json"))
}

fn plans_dir() -> Option<PathBuf> {
    Some(dsx_types::platform::plans_dir())
}

fn plan_path(seed: &str, name: &str) -> Option<PathBuf> {
    let slug = slugify(name);
    let date = chrono_date();
    plans_dir().map(|d| d.join(format!("{}-{}-{}.md", seed, slug, date)))
}

fn slugify(name: &str) -> String {
    let slug: String = name
        .chars()
        .filter_map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' { Some(c.to_ascii_lowercase()) }
            else if c.is_alphanumeric() { Some(c) }
            else { Some('-') }
        })
        .collect();
    let trimmed = slug.trim_matches('-');
    if trimmed.is_empty() { "plan".to_string() } else { trimmed.to_string() }
}

// ── Memory I/O ──

pub fn read_memory(seed: &str, tier: &str) -> String {
    let Some(path) = memory_path(seed, tier) else { return String::new(); };
    if !path.exists() { return String::new(); }
    let Ok(content) = std::fs::read_to_string(&path) else { return String::new(); };
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

pub fn write_memory(seed: &str, tier: &str, content: &str) {
    let Some(path) = memory_path(seed, tier) else { return };
    let _ = std::fs::create_dir_all(path.parent().unwrap());
    let _ = std::fs::write(&path, content);
}

pub fn append_memory(seed: &str, tier: &str, line: &str) {
    let Some(path) = memory_path(seed, tier) else { return };
    let _ = std::fs::create_dir_all(path.parent().unwrap());

    let mut existing = if path.exists() {
        std::fs::read_to_string(&path).unwrap_or_default()
    } else {
        String::new()
    };

    const MAX_CHARS: usize = 32000;
    if existing.len() > MAX_CHARS {
        let cut = existing.len().saturating_sub(MAX_CHARS / 2);
        existing = existing[cut..].to_string();
        if let Some(nl) = existing.find('\n') {
            existing = existing[nl + 1..].to_string();
        }
    }

    existing.push_str(line);
    existing.push('\n');
    let _ = std::fs::write(&path, &existing);
}

pub fn forget_memory_key(seed: &str, key: &str) {
    for tier in ["long", "short", "tasks"] {
        let Some(path) = memory_path(seed, tier) else { continue };
        if !path.exists() { continue; }
        if let Ok(content) = std::fs::read_to_string(&path) {
            let filtered: String = content
                .lines()
                .filter(|l| !l.contains(key))
                .collect::<Vec<_>>()
                .join("\n");
            let out = if filtered.is_empty() { filtered } else { format!("{}\n", filtered) };
            let _ = std::fs::write(&path, out);
        }
    }
}

// ── Pitfall I/O ──

pub fn load_pitfalls() -> PitfallGuide {
    let Some(path) = pitfalls_path() else { return PitfallGuide::default(); };
    if !path.exists() { return PitfallGuide::default(); }
    let Ok(data) = std::fs::read_to_string(&path) else { return PitfallGuide::default(); };
    serde_json::from_str(&data).unwrap_or_default()
}

pub fn save_pitfalls(guide: &PitfallGuide) {
    let Some(path) = pitfalls_path() else { return };
    let _ = std::fs::create_dir_all(path.parent().unwrap());
    if let Ok(json) = serde_json::to_string(guide) {
        let _ = std::fs::write(&path, json);
    }
}

// ── Plan I/O ──

pub fn write_plan(seed: &str, name: &str, goal: &str) -> Option<PathBuf> {
    let path = plan_path(seed, name)?;
    let _ = std::fs::create_dir_all(path.parent().unwrap());
    let now = now_epoch();
    let content = format!(
        "---\nstatus: draft\ncreated_at: {}\nupdated_at: {}\nsession: {}\n---\n\n# Plan: {}\n\n## Goal\n{}\n\n## Steps\n\n",
        now, now, seed, name, goal
    );
    let _ = std::fs::write(&path, &content);
    Some(path)
}

pub fn update_plan_status(seed: &str, name: &str, new_status: &str) -> Option<String> {
    let path = plan_path(seed, name)?;
    if !path.exists() { return None; }
    let content = std::fs::read_to_string(&path).ok()?;
    let now = now_epoch();
    let mut updated = String::with_capacity(content.len());
    for line in content.lines() {
        if line.starts_with("status: ") {
            updated.push_str(&format!("status: {}", new_status));
        } else if line.starts_with("updated_at: ") {
            updated.push_str(&format!("updated_at: {}", now));
        } else {
            updated.push_str(line);
        }
        updated.push('\n');
    }
    let _ = std::fs::write(&path, &updated);
    Some(updated)
}

pub fn read_plan_content(seed: &str, name: &str) -> Option<String> {
    let path = plan_path(seed, name)?;
    if !path.exists() { return None; }
    std::fs::read_to_string(&path).ok()
}

fn extract_frontmatter_field(content: &str, key: &str) -> String {
    if !content.starts_with("---\n") { return "unknown".to_string(); }
    for line in content.lines().skip(1) {
        if line == "---" { break; }
        if let Some(rest) = line.strip_prefix(&format!("{}: ", key)) {
            return rest.to_string();
        }
    }
    "unknown".to_string()
}

fn extract_plan_name(fname: &str, prefix: &str) -> String {
    let core = fname.strip_prefix(prefix).unwrap_or(fname);
    let core = core.strip_suffix(".md").unwrap_or(core);
    if core.len() > 11 {
        let date_part = &core[core.len().saturating_sub(11)..];
        if date_part.starts_with('-') && date_part[1..].chars().all(|c| c.is_ascii_digit() || c == '-') {
            return core[..core.len() - 11].to_string();
        }
    }
    core.to_string()
}

pub fn list_plans(seed: &str) -> Vec<(String, String, PathBuf)> {
    let Some(dir) = plans_dir() else { return vec![] };
    if !dir.exists() { return vec![]; }
    let Ok(entries) = std::fs::read_dir(&dir) else { return vec![] };
    let prefix = format!("{}-", seed);
    let mut plans = Vec::new();
    for entry in entries.flatten() {
        let fname = entry.file_name().to_string_lossy().to_string();
        if !fname.starts_with(&prefix) || !fname.ends_with(".md") { continue; }
        let path = entry.path();
        if let Ok(content) = std::fs::read_to_string(&path) {
            let status = extract_frontmatter_field(&content, "status");
            let name = extract_plan_name(&fname, &prefix);
            plans.push((name, status, path));
        }
    }
    plans
}
