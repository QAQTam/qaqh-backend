//! Plan file persistence.

use std::path::PathBuf;

fn extract_frontmatter_field(content: &str, key: &str) -> String {
    let in_frontmatter = content.starts_with("---\n");
    if !in_frontmatter { return "unknown".to_string(); }
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
    // Remove trailing date: -YYYY-MM-DD
    if core.len() > 11 {
        let date_part = &core[core.len().saturating_sub(11)..];
        if date_part.starts_with('-') && date_part[1..].chars().all(|c| c.is_ascii_digit() || c == '-') {
            return core[..core.len() - 11].to_string();
        }
    }
    core.to_string()
}

/// List all plan files for a session, returning (name, status, path).
pub fn list_plans(seed: &str) -> Vec<(String, String, PathBuf)> {
    let Some(dir) = super::plans_dir() else { return vec![] };
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

pub fn read_plan_content(seed: &str, name: &str) -> Option<String> {
    let path = super::plan_path(seed, name)?;
    if !path.exists() { return None; }
    std::fs::read_to_string(&path).ok()
}

pub fn write_plan(seed: &str, name: &str, goal: &str) -> Option<PathBuf> {
    let path = super::plan_path(seed, name)?;
    let _ = std::fs::create_dir_all(path.parent().unwrap());
    let now = super::now_epoch();
    let content = format!(
        "---\nstatus: draft\ncreated_at: {}\nupdated_at: {}\nsession: {}\n---\n\n# Plan: {}\n\n## Goal\n{}\n\n## Steps\n\n",
        now, now, seed, name, goal
    );
    let _ = std::fs::write(&path, &content);
    Some(path)
}

pub fn update_plan_status(seed: &str, name: &str, new_status: &str) -> Option<String> {
    let path = super::plan_path(seed, name)?;
    if !path.exists() { return None; }
    let content = std::fs::read_to_string(&path).ok()?;
    let now = super::now_epoch();
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
