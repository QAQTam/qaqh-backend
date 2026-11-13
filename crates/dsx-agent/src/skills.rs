use std::path::PathBuf;
use std::sync::OnceLock;

static GLOBAL_SKILLS: OnceLock<SkillIndex> = OnceLock::new();

/// Initialize or get the global skill index.
pub fn global_skills() -> &'static SkillIndex {
    GLOBAL_SKILLS.get_or_init(SkillIndex::scan)
}

// ── Skill metadata ──

#[derive(Debug, Clone)]
pub struct SkillMeta {
    pub name: String,
    pub description: String,
    pub dir: PathBuf,
}

#[derive(Debug, Clone)]
pub struct SkillIndex {
    skills: Vec<SkillMeta>,
}

impl SkillIndex {
    /// Scan ~/.config/dsx/skills/ for */SKILL.md and parse frontmatter.
    pub fn scan() -> Self {
        let skills = discover_skills();
        Self { skills }
    }

    /// Return all known skills (for listing).
    pub fn all(&self) -> &[SkillMeta] {
        &self.skills
    }

    /// Match user input against skill descriptions.
    /// Returns up to 3 best-matching skills, sorted by relevance.
    pub fn match_skills(&self, input: &str) -> Vec<&SkillMeta> {
        if self.skills.is_empty() || input.is_empty() {
            return Vec::new();
        }

        let input_lower = input.to_lowercase();
        let keywords: Vec<&str> = input_lower
            .split_whitespace()
            .filter(|w| w.len() > 2)
            .collect();

        if keywords.is_empty() {
            return Vec::new();
        }

        let mut scored: Vec<(&SkillMeta, usize)> = self
            .skills
            .iter()
            .map(|s| {
                let desc_lower = s.description.to_lowercase();
                let score = keywords
                    .iter()
                    .filter(|kw| desc_lower.contains(**kw))
                    .count();
                (s, score)
            })
            .filter(|(_, score)| *score > 0)
            .collect();

        scored.sort_by(|a, b| b.1.cmp(&a.1)); // desc by score
        scored.truncate(3);
        scored.into_iter().map(|(s, _)| s).collect()
    }

    /// Load the full SKILL.md body (after frontmatter) for a named skill.
    pub fn load_skill_body(&self, name: &str) -> Option<String> {
        let dir = self.skills.iter().find(|s| s.name == name)?.dir.clone();
        let path = dir.join("SKILL.md");
        let content = std::fs::read_to_string(&path).ok()?;
        // Strip YAML frontmatter (between --- markers)
        let body = strip_frontmatter(&content);
        Some(body)
    }

    /// Load a reference file from a skill's references/ directory.
    pub fn load_reference(&self, name: &str, ref_path: &str) -> Option<String> {
        let dir = self.skills.iter().find(|s| s.name == name)?.dir.clone();
        let path = dir.join("references").join(ref_path);
        if !path.exists() {
            return None;
        }
        std::fs::read_to_string(&path).ok()
    }
}

// ── Skill discovery ──

fn discover_skills() -> Vec<SkillMeta> {
    let skills_dir = skills_dir();
    let Some(dir) = skills_dir else {
        return Vec::new();
    };
    if !dir.exists() {
        return Vec::new();
    }

    let mut skills = Vec::new();
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };

    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }

        let skill_file = path.join("SKILL.md");
        if !skill_file.exists() {
            continue;
        }

        let content = match std::fs::read_to_string(&skill_file) {
            Ok(c) => c,
            _ => continue,
        };

        if let Some(meta) = parse_frontmatter(&content, &path) {
            skills.push(meta);
        }
    }

    // Sort by name for stable ordering
    skills.sort_by(|a, b| a.name.cmp(&b.name));
    skills
}

fn skills_dir() -> Option<PathBuf> {
    Some(dsx_types::platform::skills_dir())
}

// ── Frontmatter parsing (no YAML dependency) ──

/// Parse `name` and `description` from YAML frontmatter between `---` markers.
fn parse_frontmatter(content: &str, dir: &PathBuf) -> Option<SkillMeta> {
    let lines: Vec<&str> = content.lines().collect();
    if lines.len() < 3 {
        return None;
    }

    // Find first ---
    let start = lines.iter().position(|l| l.trim() == "---")?;
    // Find second --- after start
    let end = lines[start + 1..]
        .iter()
        .position(|l| l.trim() == "---")?;
    let fm_lines = &lines[start + 1..start + 1 + end];

    let mut name = String::new();
    let mut description = String::new();
    let mut in_desc = false;

    for line in fm_lines {
        let trimmed = line.trim();
        if let Some(val) = trimmed.strip_prefix("name:") {
            name = val.trim().to_string();
        } else if let Some(val) = trimmed.strip_prefix("description:") {
            let first = val.trim();
            if first.is_empty() {
                in_desc = true; // multi-line description follows
            } else {
                description = first.to_string();
            }
        } else if in_desc {
            // Multi-line description: lines starting with whitespace continue it
            if line.starts_with(' ') || line.starts_with('\t') {
                if !description.is_empty() {
                    description.push(' ');
                }
                description.push_str(trimmed);
            } else {
                in_desc = false;
            }
        }
    }

    if name.is_empty() {
        return None;
    }

    Some(SkillMeta {
        name,
        description,
        dir: dir.clone(),
    })
}

// ── Tool execution helpers ──

/// List all available skills. Used by the list_skills tool.
pub fn tool_list_skills() -> String {
    let idx = global_skills();
    let all = idx.all();
    if all.is_empty() {
        return "[OK] No skills installed. Add skills to ~/.config/dsx/skills/<name>/SKILL.md".to_string();
    }
    let mut out = format!("[OK] {} skills available:\n", all.len());
    for s in all {
        out.push_str(&format!("  {} — {}\n", s.name, s.description));
    }
    out
}

/// Read a skill's SKILL.md or reference file. Used by the read_skill_ref tool.
pub fn tool_read_skill_ref(args: &str) -> String {
    let (name, ref_path) = match serde_json::from_str::<serde_json::Value>(args) {
        Ok(v) => (
            v.get("name").and_then(|n| n.as_str()).unwrap_or("").to_string(),
            v.get("path").and_then(|p| p.as_str()).unwrap_or("SKILL.md").to_string(),
        ),
        Err(_) => return "[ERROR] Invalid arguments. Expected: {name, path?}".to_string(),
    };

    if name.is_empty() {
        return "[ERROR] Missing required field: name".to_string();
    }

    let idx = global_skills();
    let body = match ref_path.as_str() {
        "SKILL.md" => idx.load_skill_body(&name),
        r => idx.load_reference(&name, r),
    };

    match body {
        Some(content) => format!("[OK] -- {}:{} --\n{}", name, ref_path, content),
        None => format!("[ERROR] Skill '{}' not found or path '{}' does not exist\n[HINT] Use list_skills to see available skills.", name, ref_path),
    }
}

/// Strip YAML frontmatter from SKILL.md content, returning the body only.
fn strip_frontmatter(content: &str) -> String {
    let lines: Vec<&str> = content.lines().collect();
    let start = lines.iter().position(|l| l.trim() == "---");
    let Some(first) = start else {
        return content.to_string();
    };
    let end = lines[first + 1..]
        .iter()
        .position(|l| l.trim() == "---");
    let Some(second) = end else {
        return content.to_string();
    };
    let body_start = first + 1 + second + 1;
    if body_start >= lines.len() {
        return String::new();
    }
    lines[body_start..].join("\n")
}
