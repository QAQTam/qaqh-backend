use crate::agent::AgentState;

/// Score the current turn based on tool calls executed.
/// Higher score = more valuable information worth preserving.
pub fn score_current_turn(state: &AgentState) -> f32 {
    let mut max_score = 0.2f32; // base: text-only response
    for (name, result) in &state.tool_results {
        let tool_score = match name.as_str() {
            "exec" if result.contains("explore") || result.contains("── explore ──") => 0.9,
            "file" if result.contains("[OK]") && (result.contains("write") || result.contains("edit")) => 0.7,
            "file" if result.contains("read") => 0.4,
            "exec" if result.contains("[ERROR]") || result.contains("[FAIL]") => 0.8,
            "exec" if result.starts_with("[OK] exec: sudo") => 0.6,
            "exec" => 0.3,
            "agent" if result.contains("commit") => 0.5,
            "web" => 0.5,
            "file" if result.contains("search") => 0.4,
            "agent" if result.contains("git_status") || result.contains("git_diff") || result.contains("git_log") => 0.2,
            _ => 0.3,
        };
        if tool_score > max_score { max_score = tool_score; }
    }
    max_score.min(1.0)
}

/// Detect if an exec command duplicates an existing tool. Returns (tool_name, cmd_summary).
pub fn detect_tool_equivalent(cmd: &str) -> Option<(String, String)> {
    let cmd = cmd.trim();
    // cat/head/tail/bat/less → read_file
    if let Some(rest) = cmd.strip_prefix("cat ").or_else(|| cmd.strip_prefix("head "))
        .or_else(|| cmd.strip_prefix("tail ")).or_else(|| cmd.strip_prefix("bat "))
        .or_else(|| cmd.strip_prefix("less "))
    {
        let target = rest.trim().split_whitespace().next().unwrap_or("?");
        if target.contains('.') {
            return Some(("read_file".into(), format!("cat {}", target)));
        }
    }
    // sed -i → edit_file
    if cmd.starts_with("sed ") && cmd.contains(" -i") {
        let target = cmd.split_whitespace().last().unwrap_or("?");
        if target.contains('.') {
            return Some(("edit_file".into(), format!("sed -i ... {}", target)));
        }
    }
    // grep/rg → search
    if let Some(rest) = cmd.strip_prefix("grep ").or_else(|| cmd.strip_prefix("rg ")) {
        let parts: Vec<&str> = rest.split_whitespace().collect();
        if parts.len() >= 2 && parts.last().unwrap_or(&"").contains('.') {
            return Some(("search".into(), format!("grep {} {}", parts[0], parts.last().unwrap())));
        }
    }
    // tee file / >> file / > file → write_file (append)
    if cmd.contains("tee ") || cmd.contains(">> ") || cmd.contains("> ") {
        let pos = cmd.find("> ").or_else(|| cmd.find(">> ")).or_else(|| cmd.find("tee "));
        if let Some(p) = pos {
            let rest = &cmd[p..];
            let target = rest.split_whitespace().nth(1).unwrap_or("?");
            if target.contains('.') {
                return Some(("write_file".into(), format!("redirect → {}", target)));
            }
        }
    }
    None
}

/// Extract "why" from assistant text for a given file path + tool.
/// Falls back to a generic description if the extracted text is junk.
pub fn extract_why(text: &str, path: &str, _tool: &str) -> String {
    let basename = std::path::Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(path);

    for line in text.lines() {
        if line.contains(basename) && line.len() > 10 {
            let cleaned = line.trim()
                .trim_start_matches("- ")
                .trim_start_matches("* ")
                .trim_start_matches("// ");
            let short: String = cleaned.chars().take(120).collect();
            if is_valid_why(&short) {
                return if short.len() < 120 { short } else { format!("{}...", short) };
            }
        }
    }
    fallback_why(_tool)
}

fn is_valid_why(s: &str) -> bool {
    if s.len() < 4 { return false; }
    let meaningful: String = s.chars().filter(|c| c.is_alphanumeric()).collect();
    if meaningful.len() < 3 { return false; }
    let lower = s.to_lowercase();
    for bad in &["fuck", "shit", "damn", "去死", "idiot", "stupid"] {
        if lower.contains(bad) { return false; }
    }
    true
}

fn fallback_why(tool: &str) -> String {
    match tool {
        "read_file" => "了解当前实现".to_string(),
        "write_file" => "创建新文件".to_string(),
        "edit_file" => "修改逻辑".to_string(),
        _ => "执行操作".to_string(),
    }
}

// ── Path trust levels ──

#[derive(Clone, Copy, PartialEq, Eq)]
pub enum PathTrust {
    /// /tmp/, /var/tmp/ — no gate, no sandbox, write+exec freely
    Trusted,
    /// src/**/*.rs, Cargo.toml — full intent gate + sandbox
    HighStake,
}

pub fn classify_path(path: &str) -> PathTrust {
    let normalized = std::path::Path::new(path)
        .components()
        .collect::<std::path::PathBuf>()
        .to_string_lossy()
        .to_string();

    // For absolute paths, check directly.
    if normalized.starts_with('/') {
        if normalized.starts_with("/tmp/") || normalized.starts_with("/var/tmp/")
            || normalized.starts_with("/exam_sctrip/") || normalized.starts_with("/exam/")
        {
            return PathTrust::Trusted;
        }
    } else if normalized.starts_with("..") {
        // Parent-relative — resolve to absolute for trust check.
        if let Ok(abs) = std::path::absolute(&normalized) {
            let abs = abs.to_string_lossy();
            if abs.starts_with("/tmp/") || abs.starts_with("/var/tmp/") {
                return PathTrust::Trusted;
            }
        }
    }
    // Relative paths under the project dir are always HighStake.
    PathTrust::HighStake
}
