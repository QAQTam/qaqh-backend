use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::sync::OnceLock;

// ── Semantic Knowledge Entry ──

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FileSemanticEntry {
    pub path: String,
    pub symbols: Vec<String>,
    pub purpose: Option<String>,
    pub last_seen: u64,
    pub touch_count: u32,
    pub language: Option<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ArchDecision {
    pub summary: String,
    pub rationale: String,
    pub timestamp: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ErrorPattern {
    pub pattern: String,
    pub fix: String,
    pub file: Option<String>,
    pub frequency: u32,
}

// ── Semantic Memory ──

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SemanticMemory {
    pub entries: BTreeMap<String, FileSemanticEntry>,
    pub arch_decisions: Vec<ArchDecision>,
    pub error_patterns: Vec<ErrorPattern>,
    pub token_budget: u32,
    pub total_updates: u64,
}

impl Default for SemanticMemory {
    fn default() -> Self {
        Self {
            entries: BTreeMap::new(),
            arch_decisions: Vec::new(),
            error_patterns: Vec::new(),
            token_budget: 2000,
            total_updates: 0,
        }
    }
}

impl SemanticMemory {
    pub fn new() -> Self { Self::default() }

    pub fn is_empty(&self) -> bool {
        self.entries.is_empty() && self.arch_decisions.is_empty() && self.error_patterns.is_empty()
    }

    /// Insert or update a file entry, merging symbols and bumping counts.
    pub fn upsert(&mut self, path: String, entry: FileSemanticEntry) {
        let now = now_epoch();
        self.entries.entry(path)
            .and_modify(|existing| {
                let mut syms: Vec<String> =
                    existing.symbols.iter().chain(&entry.symbols).cloned().collect();
                syms.sort();
                syms.dedup();
                syms.truncate(12);
                existing.symbols = syms;
                existing.touch_count += entry.touch_count;
                existing.last_seen = now;
                if entry.purpose.is_some() {
                    existing.purpose = entry.purpose.clone();
                }
                if entry.language.is_some() {
                    existing.language = entry.language.clone();
                }
            })
            .or_insert_with(|| {
                let mut e = entry;
                e.last_seen = now;
                let mut syms = e.symbols.clone();
                syms.sort();
                syms.dedup();
                syms.truncate(12);
                e.symbols = syms;
                e
            });
    }

    /// Add an architecture decision (capped at 10, oldest evicted).
    pub fn add_arch_decision(&mut self, summary: &str, rationale: &str) {
        if self.arch_decisions.len() >= 10 {
            self.arch_decisions.remove(0);
        }
        self.arch_decisions.push(ArchDecision {
            summary: summary.to_string(),
            rationale: rationale.to_string(),
            timestamp: now_epoch(),
        });
    }

    /// Record an error pattern (merge with existing if same pattern).
    pub fn add_error_pattern(&mut self, pattern: &str, fix: &str, file: Option<&str>) {
        if let Some(existing) = self.error_patterns.iter_mut()
            .find(|ep| ep.pattern == pattern)
        {
            existing.frequency += 1;
            if let Some(f) = file {
                existing.file = Some(f.to_string());
            }
            return;
        }
        if self.error_patterns.len() >= 8 {
            // Evict least frequent
            if let Some(min_idx) = self.error_patterns.iter()
                .enumerate()
                .min_by_key(|(_, ep)| ep.frequency)
                .map(|(i, _)| i)
            {
                self.error_patterns.remove(min_idx);
            }
        }
        self.error_patterns.push(ErrorPattern {
            pattern: pattern.to_string(),
            fix: fix.to_string(),
            file: file.map(|s| s.to_string()),
            frequency: 1,
        });
    }

    /// Evict low-value entries until the rendered output fits within max_tokens.
    pub fn enforce_budget(&mut self, max_tokens: u32) {
        let current = crate::tokenizer::count_tokens(&self.render());
        if current <= max_tokens || self.entries.is_empty() {
            return;
        }

        let now = now_epoch();
        let mut scored: Vec<(String, f64)> = self.entries.iter().map(|(path, entry)| {
            let recency_days = (now.saturating_sub(entry.last_seen)) as f64 / 86400.0;
            let recency = if recency_days < 0.5 { 1.0 }
                else { 1.0 / (1.0 + recency_days) };
            let freq = (entry.touch_count as f64).ln_1p();
            (path.clone(), freq * 0.4 + recency * 0.6)
        }).collect();

        scored.sort_by(|a, b| a.1.partial_cmp(&b.1).unwrap_or(std::cmp::Ordering::Equal));

        for (path, _) in &scored {
            self.entries.remove(path);
            if crate::tokenizer::count_tokens(&self.render()) <= max_tokens {
                break;
            }
        }
    }

    /// Render the semantic memory block as a stable-ordered text blob.
    /// BTreeMap guarantees alphabetic iteration → byte-identical for the same file set.
    pub fn render(&self) -> String {
        let mut out = String::new();

        if !self.entries.is_empty() {
            out.push_str("## Semantic Memory\n");
            for (path, entry) in &self.entries {
                let lang = entry.language.as_deref().unwrap_or("?");
                let n = entry.symbols.len();
                let syms = if n == 0 {
                    "(scan)".to_string()
                } else {
                    entry.symbols.iter().take(8).cloned().collect::<Vec<_>>().join(",")
                };
                let purpose = entry.purpose.as_ref()
                    .map(|p| format!(" // {}", p))
                    .unwrap_or_default();
                out.push_str(&format!("{} | {} | syms:{} | {}{}\n", path, lang, n, syms, purpose));
            }
        }

        if !self.arch_decisions.is_empty() {
            out.push_str("## Architecture\n");
            for ad in self.arch_decisions.iter().rev().take(5) {
                out.push_str(&format!("- {} → {}\n", ad.summary, ad.rationale));
            }
        }

        if !self.error_patterns.is_empty() {
            out.push_str("## Recurring Errors\n");
            for ep in self.error_patterns.iter().take(4) {
                let loc = ep.file.as_deref().unwrap_or("-");
                out.push_str(&format!("- {} → {} | {}\n", ep.pattern, ep.fix, loc));
            }
        }

        out
    }
}

// ── Knowledge Extractor ──

pub struct KnowledgeExtractor;

impl KnowledgeExtractor {
    /// Extract file knowledge from a tool call result.
    pub fn extract(tool_name: &str, tool_args: &str, tool_output: &str) -> Vec<FileSemanticEntry> {
        match tool_name {
            "write_file" | "edit_file" => Self::extract_from_write(tool_args, tool_output),
            "edit_file_diff" => Self::extract_from_diff(tool_args, tool_output),
            "read_file" => Self::extract_from_read(tool_args, tool_output),
            _ => vec![],
        }
    }

    /// Parse explore/project_map output into lightweight file entries.
    pub fn extract_project_map(raw: &str) -> Vec<FileSemanticEntry> {
        let mut entries = Vec::new();
        for line in raw.lines() {
            let trimmed = line.trim();
            // Lines like: "  src/tools.rs (958 lines, 28K)"
            if !trimmed.contains('(') { continue; }
            let path_end = trimmed.find(" (").unwrap_or(trimmed.len());
            let path = trimmed[..path_end].trim().to_string();
            if path.is_empty() || path == "." || path.ends_with('/') { continue; }
            // Only accept file-like paths (have an extension)
            if !path.contains('.') { continue; }
            let lang = detect_language(&path);
            entries.push(FileSemanticEntry {
                path,
                symbols: Vec::new(),
                purpose: None,
                last_seen: 0,
                touch_count: 0,
                language: lang,
            });
        }
        entries
    }

    fn extract_from_read(args_json: &str, output: &str) -> Vec<FileSemanticEntry> {
        let path = extract_arg_path(args_json);
        if path.is_empty() { return vec![]; }

        // Strip line-number prefixes from read_file output: "  1| code" → "code"
        let code = strip_line_numbers(output);
        let lang = detect_language(&path);
        let symbols = extract_symbols(&code, &lang);
        let purpose = extract_purpose(&code);

        vec![FileSemanticEntry {
            path,
            symbols,
            purpose,
            last_seen: 0,
            touch_count: 1,
            language: lang,
        }]
    }

    fn extract_from_write(args_json: &str, _output: &str) -> Vec<FileSemanticEntry> {
        let path = extract_arg_path(args_json);
        if path.is_empty() { return vec![]; }

        // Parse the content/old_string/new_string from args
        let code_json = serde_json::from_str::<serde_json::Value>(args_json)
            .ok()
            .and_then(|v| {
                v.get("content")
                    .or_else(|| v.get("new_string"))
                    .and_then(|c| c.as_str())
                    .map(|s| s.to_string())
            })
            .unwrap_or_default();

        let lang = detect_language(&path);
        let symbols = extract_symbols(&code_json, &lang);
        let purpose = extract_purpose(&code_json);

        vec![FileSemanticEntry {
            path,
            symbols,
            purpose,
            last_seen: 0,
            touch_count: 1,
            language: lang,
        }]
    }

    fn extract_from_diff(args_json: &str, output: &str) -> Vec<FileSemanticEntry> {
        let path = extract_arg_path(args_json);
        if path.is_empty() { return vec![]; }
        let mut purpose = None;
        let mut added = 0u32;
        let mut removed = 0u32;
        for line in output.lines() {
            if let Some(rest) = line.strip_prefix("[CHANGE]") {
                let rest = rest.trim();
                if let Some(desc) = rest.split("|").nth(1) {
                    purpose = Some(desc.trim().to_string());
                }
                if let Some(counts) = rest.split("|").next() {
                    for part in counts.split_whitespace() {
                        if let Some(n) = part.strip_prefix('+') { added = n.parse().unwrap_or(0); }
                        if let Some(n) = part.strip_prefix('-') { removed = n.parse().unwrap_or(0); }
                    }
                }
            }
        }
        let lang = detect_language(&path);
        vec![FileSemanticEntry {
            path,
            symbols: Vec::new(),
            purpose,
            last_seen: 0,
            touch_count: added.max(removed).max(1),
            language: lang,
        }]
    }
}

// ── Language detection ──

fn detect_language(path: &str) -> Option<String> {
    let ext = std::path::Path::new(path)
        .extension()
        .and_then(|e| e.to_str())
        .unwrap_or("");
    match ext {
        "rs" => Some("rust".into()),
        "py" => Some("python".into()),
        "go" => Some("go".into()),
        "ts" | "tsx" => Some("typescript".into()),
        "js" | "jsx" | "mjs" => Some("javascript".into()),
        "c" | "h" => Some("c".into()),
        "cpp" | "cc" | "cxx" | "hpp" | "hxx" => Some("cpp".into()),
        "java" => Some("java".into()),
        "sh" | "bash" => Some("shell".into()),
        "toml" | "yaml" | "yml" | "json" => Some("config".into()),
        "md" | "markdown" => Some("markdown".into()),
        "sql" => Some("sql".into()),
        _ => None,
    }
}

// ── Symbol extraction ──

fn extract_symbols(code: &str, language: &Option<String>) -> Vec<String> {
    let lang = language.as_deref().unwrap_or("");
    let re = regex_for(lang);
    let mut syms: Vec<String> = Vec::new();
    for caps in re.captures_iter(code) {
        // Iterate all capture groups (group 0 is the full match, skip it).
        for i in 1..caps.len() {
            if let Some(m) = caps.get(i) {
                let s = m.as_str();
                if s.len() > 1 && s.len() < 64 {
                    syms.push(s.to_string());
                }
            }
        }
    }
    syms.sort();
    syms.dedup();
    syms.truncate(12);
    syms
}

fn regex_for(lang: &str) -> &'static regex::Regex {
    static RUST_RE: OnceLock<regex::Regex> = OnceLock::new();
    static PYTHON_RE: OnceLock<regex::Regex> = OnceLock::new();
    static GO_RE: OnceLock<regex::Regex> = OnceLock::new();
    static TS_RE: OnceLock<regex::Regex> = OnceLock::new();
    static GENERIC_RE: OnceLock<regex::Regex> = OnceLock::new();

    let (re_cell, pattern) = match lang {
        "rust" => (&RUST_RE, r"(?:pub\s+)?(?:async\s+)?(?:unsafe\s+)?fn\s+(\w+)|pub\s+(?:unsafe\s+)?struct\s+(\w+)|pub\s+enum\s+(\w+)|pub\s+trait\s+(\w+)|pub\s+type\s+(\w+)|pub\s+const\s+(\w+)|macro_rules!\s+(\w+)"),
        "python" => (&PYTHON_RE, r"(?:async\s+)?def\s+(\w+)|class\s+(\w+)"),
        "go" => (&GO_RE, r"func\s+(?:\([^)]*\)\s+)?(\w+)|type\s+(\w+)\s+struct|type\s+(\w+)\s+interface"),
        "typescript" | "javascript" => (&TS_RE, r"(?:export\s+)?(?:async\s+)?function\s+(\w+)|(?:export\s+)?class\s+(\w+)|(?:export\s+)?const\s+(\w+)\s*="),
        _ => (&GENERIC_RE, r"(?:fn|def|func|function|class|struct|enum|trait|interface)\s+(\w+)"),
    };

    re_cell.get_or_init(|| regex::Regex::new(pattern).unwrap())
}

// ── Purpose extraction ──

fn extract_purpose(code: &str) -> Option<String> {
    for line in code.lines().take(30) {
        let trimmed = line.trim();
        let doc = trimmed.strip_prefix("//!")
            .or_else(|| trimmed.strip_prefix("///"))
            .or_else(|| trimmed.strip_prefix("# "))
            .or_else(|| trimmed.strip_prefix("// "))
            .map(|s| s.trim());
        if let Some(doc) = doc {
            if doc.len() > 4 && doc.len() < 120 && !doc.starts_with("Copyright") && !doc.starts_with("SPDX") {
                return Some(doc.to_string());
            }
        }
    }
    None
}

// ── Helpers ──

pub(super) fn now_epoch() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
}

fn extract_arg_path(args_json: &str) -> String {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(args_json) {
        if let Some(path) = v.get("path").and_then(|p| p.as_str()) {
            return path.to_string();
        }
    }
    String::new()
}

/// Strip line-number prefixes from read_file tool output.
/// Lines look like: "  1| code" or " 12| code" or "123| code"
fn strip_line_numbers(output: &str) -> String {
    let mut result = String::with_capacity(output.len());
    for line in output.lines() {
        if let Some(rest) = line.find("| ") {
            result.push_str(&line[rest + 2..]);
        } else {
            result.push_str(line);
        }
        result.push('\n');
    }
    result
}

// ── Working Memory (in-memory only, not persisted) ──


/// Extract files from a tool execution result for round indexing.
pub fn extract_files_from_tool(tool_name: &str, args: &str) -> Vec<String> {
    match tool_name {
        "write_file" | "edit_file" | "read_file" => {
            let p = extract_arg_path(args);
            if p.is_empty() { vec![] } else { vec![p] }
        }
        "search" | "explore" | "git_diff" | "git_status" => {
            vec![]
        }
        _ => vec![],
    }
}
