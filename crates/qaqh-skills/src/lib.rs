//! Agent Skills（极简运行时）：发现、frontmatter 解析、目录渲染、资源枚举。
//!
//! codex 式渐进披露，无激活状态机：
//! - 会话初始化把目录（仅元数据）+ 嵌入 bundled 技能正文注入 system prompt；
//! - 模型经 `skill_list` 列目录或列某技能的内部资源，再用标准 `read` 工具直读文件；
//! - 文件系统是唯一事实源，无信封、无预算回收、无跨回合激活集。

use serde::Deserialize;
use std::fs;
use std::path::{Component, Path, PathBuf};

/// 模型面唯一技能工具名。
pub const SKILL_LIST_TOOL: &str = "skill_list";

/// 嵌入 bundled 技能包：编译期打入二进制，不落盘，正文随会话初始化注入。
static BUNDLED: include_dir::Dir<'_> =
    include_dir::include_dir!("$CARGO_MANIFEST_DIR/assets/bundled");

/// include_dir 的 `files()` 只列直接子项，bundled 根下全是技能目录——递归收集。
fn collect_embedded_files<'a>(
    dir: &'a include_dir::Dir<'a>,
    out: &mut Vec<&'a include_dir::File<'a>>,
) {
    for entry in dir.entries() {
        match entry {
            include_dir::DirEntry::Dir(sub) => collect_embedded_files(sub, out),
            include_dir::DirEntry::File(file) => out.push(file),
        }
    }
}

const MAX_SKILL_BYTES: u64 = 512 * 1024;
const MAX_SCAN_DEPTH: usize = 2;
const MAX_SCANNED_DIRS: usize = 1_000;
const MAX_SKILLS: usize = 256;
const MAX_RESOURCES: usize = 200;
const MAX_CATALOG_CHARS: usize = 8_000;
const MAX_NAME_CHARS: usize = 64;
const MAX_DESCRIPTION_CHARS: usize = 1_024;
const MAX_CATALOG_DESCRIPTION_CHARS: usize = 600;

/// 磁盘发现位置：项目（工作区）或用户（home）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillScope {
    Project,
    User,
}

/// 技能来源。Bundled 表示编译期嵌入包（只读，正文随初始化注入）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SkillOrigin {
    Project,
    User,
    Bundled,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillMetadata {
    pub name: String,
    pub description: String,
    /// 磁盘技能为 SKILL.md 绝对路径；bundled 为展示用伪路径。
    pub path: PathBuf,
    pub scope: SkillScope,
    pub origin: SkillOrigin,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DiagnosticSeverity {
    Warning,
    Error,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillDiagnostic {
    pub path: PathBuf,
    pub severity: DiagnosticSeverity,
    pub message: String,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct SkillCatalog {
    pub skills: Vec<SkillMetadata>,
    pub diagnostics: Vec<SkillDiagnostic>,
}

/// 一个技能的内部资源。磁盘技能给绝对路径（模型用 read 直读）；
/// 嵌入技能直接给内容（read 够不到二进制内嵌文件）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum SkillResourceSource {
    Disk { absolute_path: PathBuf },
    Embedded { content: String },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SkillResource {
    pub skill_name: String,
    pub relative_path: String,
    pub source: SkillResourceSource,
}

/// 技能根（按优先级降序）：工作区 `.qaqh/skills` > 工作区 `.agents/skills`
/// > 用户 `~/.qaqh/skills` > 用户 `~/.agents/skills`。
fn discovery_roots_with_home(workspace: &Path, home: Option<&Path>) -> Vec<(PathBuf, SkillScope)> {
    let workspace = absolutize(workspace);
    let mut roots = vec![
        (workspace.join(".qaqh/skills"), SkillScope::Project),
        (workspace.join(".agents/skills"), SkillScope::Project),
    ];
    if let Some(home) = home {
        roots.push((home.join(".qaqh/skills"), SkillScope::User));
        roots.push((home.join(".agents/skills"), SkillScope::User));
    }
    roots
}

/// 磁盘技能发现根（bundled 嵌入包不在内：只读、不可写）。
/// 权限引擎把它当权威指令写入面（审计 2026-10-01 H2）。
pub fn skill_roots(workspace: &Path) -> Vec<PathBuf> {
    discovery_roots_with_home(workspace, home_dir().as_deref())
        .into_iter()
        .map(|(root, _)| root)
        .collect()
}

/// 动态发现：磁盘四根 + 嵌入 bundled。同名技能只保留优先级最高者，
/// 被遮蔽者记 warning。
pub fn discover(workspace: &Path) -> SkillCatalog {
    discover_with_home(workspace, home_dir().as_deref())
}

/// 测试/宿主钩子：显式指定 home。生产走 [`discover`]。
pub fn discover_with_home(workspace: &Path, home: Option<&Path>) -> SkillCatalog {
    let mut catalog = SkillCatalog::default();
    let mut seen_names: std::collections::HashMap<String, ()> = std::collections::HashMap::new();
    let mut scanned_dirs = 0usize;

    for (root, scope) in discovery_roots_with_home(workspace, home) {
        scan_root(&root, scope, &mut catalog, &mut seen_names, &mut scanned_dirs);
        if scanned_dirs >= MAX_SCANNED_DIRS || catalog.skills.len() >= MAX_SKILLS {
            return catalog;
        }
    }
    append_embedded(&mut catalog, &mut seen_names);
    catalog
}

fn scan_root(
    root: &Path,
    scope: SkillScope,
    catalog: &mut SkillCatalog,
    seen_names: &mut std::collections::HashMap<String, ()>,
    scanned_dirs: &mut usize,
) {
    if !root.is_dir() {
        return;
    }
    let mut stack = vec![(root.to_path_buf(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        if *scanned_dirs >= MAX_SCANNED_DIRS || catalog.skills.len() >= MAX_SKILLS {
            return;
        }
        *scanned_dirs += 1;
        let Ok(entries) = fs::read_dir(&dir) else {
            return;
        };
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            let Ok(file_type) = entry.file_type() else { continue };
            let name = entry.file_name().to_string_lossy().into_owned();
            if !file_type.is_dir() || file_type.is_symlink() || should_skip_dir(&name) {
                continue;
            }
            let skill_path = entry.path().join("SKILL.md");
            if skill_path.is_file() {
                add_disk_skill(&skill_path, scope, catalog, seen_names);
            }
            if depth < MAX_SCAN_DEPTH {
                stack.push((entry.path(), depth + 1));
            }
        }
    }
}

fn add_disk_skill(
    skill_path: &Path,
    scope: SkillScope,
    catalog: &mut SkillCatalog,
    seen_names: &mut std::collections::HashMap<String, ()>,
) {
    match parse_disk_metadata(skill_path, scope) {
        Ok((metadata, warnings)) => {
            for message in warnings {
                catalog.diagnostics.push(SkillDiagnostic {
                    path: metadata.path.clone(),
                    severity: DiagnosticSeverity::Warning,
                    message,
                });
            }
            if seen_names.contains_key(&metadata.name) {
                catalog.diagnostics.push(SkillDiagnostic {
                    path: metadata.path.clone(),
                    severity: DiagnosticSeverity::Warning,
                    message: format!("skill '{}' shadowed by higher-priority root", metadata.name),
                });
                return;
            }
            seen_names.insert(metadata.name.clone(), ());
            catalog.skills.push(metadata);
        }
        Err(message) => catalog.diagnostics.push(SkillDiagnostic {
            path: skill_path.to_path_buf(),
            severity: DiagnosticSeverity::Error,
            message,
        }),
    }
}

/// 嵌入 bundled：仅收录未被磁盘技能遮蔽的名字。
fn append_embedded(
    catalog: &mut SkillCatalog,
    seen_names: &mut std::collections::HashMap<String, ()>,
) {
    let mut files = Vec::new();
    collect_embedded_files(&BUNDLED, &mut files);
    for file in files {
        let Some(path) = file.path().to_str() else { continue };
        if !path.ends_with("SKILL.md") {
            continue;
        }
        let raw = match std::str::from_utf8(file.contents()) {
            Ok(raw) => raw,
            Err(_) => continue,
        };
        let pseudo_path = PathBuf::from("<bundled>").join(file.path());
        match parse_str_metadata(raw) {
            Ok((name, description)) => {
                if seen_names.contains_key(&name) {
                    continue;
                }
                seen_names.insert(name.clone(), ());
                catalog.skills.push(SkillMetadata {
                    name,
                    description,
                    path: pseudo_path,
                    scope: SkillScope::User,
                    origin: SkillOrigin::Bundled,
                });
            }
            Err(message) => catalog.diagnostics.push(SkillDiagnostic {
                path: pseudo_path,
                severity: DiagnosticSeverity::Error,
                message,
            }),
        }
    }
}

fn should_skip_dir(name: &str) -> bool {
    name.starts_with('.') || matches!(name, "node_modules" | "target" | ".git")
}

/// 防注入：元数据压成单行，防止目录/信封被伪造换行拆开。
fn sanitize_single_line(raw: &str) -> String {
    raw.chars()
        .map(|character| if character.is_control() { ' ' } else { character })
        .collect::<String>()
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
}

#[derive(Deserialize)]
struct Frontmatter {
    name: String,
    description: String,
}

/// 解析 `---\n…\n---\n` 边界，返回 (frontmatter, body)。嵌入与磁盘统一
/// 在此归一换行（Windows 检出的 CRLF 同样成立），并剥离 BOM。
fn split_skill_file(raw: &str) -> Result<(String, String), String> {
    let raw = raw.strip_prefix('\u{feff}').unwrap_or(raw);
    let normalized = if raw.contains('\r') {
        raw.replace("\r\n", "\n").replace('\r', "\n")
    } else {
        raw.to_string()
    };
    if !normalized.starts_with("---\n") {
        return Err("SKILL.md must start with YAML frontmatter (---)".into());
    }
    let remainder = normalized
        .get(4..)
        .ok_or("invalid YAML frontmatter opening boundary")?;
    let close = remainder
        .find("\n---\n")
        .or_else(|| remainder.strip_suffix("\n---").map(|body| body.len()))
        .ok_or("unclosed YAML frontmatter")?;
    // body 按 `normalized` 坐标取：remainder 偏移 4，闭合标记 `\n---\n` 长 5。
    let body_start = close + 4 + 5;
    let body = normalized.get(body_start..).unwrap_or("").trim().to_string();
    let frontmatter = remainder.get(..close).ok_or("invalid frontmatter")?.to_string();
    Ok((frontmatter, body))
}

fn parse_frontmatter(frontmatter: &str) -> Result<(String, String), String> {
    let parsed: Frontmatter = serde_saphyr::from_str(frontmatter)
        .map_err(|error| format!("invalid YAML frontmatter: {error}"))?;
    Ok((parsed.name, parsed.description))
}

fn validate_fields(name: &str, description: &str) -> Vec<String> {
    let mut warnings = Vec::new();
    let name_chars = name.chars().count();
    if name_chars == 0 || name_chars > MAX_NAME_CHARS {
        warnings.push(format!("name must contain 1-{MAX_NAME_CHARS} characters"));
    }
    let valid_chars = name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-');
    if !valid_chars || name.starts_with('-') || name.ends_with('-') || name.contains("--") {
        warnings.push(
            "name is not Agent Skills compliant (lowercase letters, digits, single hyphens only)"
                .into(),
        );
    }
    if description.chars().count() > MAX_DESCRIPTION_CHARS {
        warnings.push(format!(
            "description must contain 1-{MAX_DESCRIPTION_CHARS} characters"
        ));
    }
    warnings
}

fn parse_str_metadata(raw: &str) -> Result<(String, String), String> {
    let (frontmatter, _) = split_skill_file(raw)?;
    let (raw_name, raw_description) = parse_frontmatter(&frontmatter)?;
    let name = sanitize_single_line(&raw_name);
    let description = sanitize_single_line(&raw_description);
    if name.is_empty() {
        return Err("missing skill name".into());
    }
    if description.is_empty() {
        return Err("missing skill description".into());
    }
    Ok((name, description))
}

fn read_bounded(path: &Path) -> Result<String, String> {
    let metadata = fs::metadata(path).map_err(|error| format!("cannot stat file: {error}"))?;
    if metadata.len() > MAX_SKILL_BYTES {
        return Err(format!("skill file exceeds {} bytes", MAX_SKILL_BYTES));
    }
    fs::read_to_string(path)
        .map(|text| text.replace("\r\n", "\n").replace('\r', "\n"))
        .map_err(|error| format!("cannot read skill: {error}"))
}

fn parse_disk_metadata(path: &Path, scope: SkillScope) -> Result<(SkillMetadata, Vec<String>), String> {
    let raw = read_bounded(path)?;
    let (name, description) = parse_str_metadata(&raw)?;
    let mut warnings = validate_fields(&name, &description);
    if let Some(parent_name) = path
        .parent()
        .and_then(Path::file_name)
        .and_then(|value| value.to_str())
        && parent_name != name
    {
        warnings.push(format!("name '{name}' does not match parent directory '{parent_name}'"));
    }
    Ok((
        SkillMetadata {
            name,
            description,
            path: absolutize(path),
            scope,
            origin: match scope {
                SkillScope::Project => SkillOrigin::Project,
                SkillScope::User => SkillOrigin::User,
            },
        },
        warnings,
    ))
}

/// 渲染技能目录（仅元数据）。磁盘技能按 scope 排序后列名；
/// bundled 技能的正文已在 system section 注入，目录仅提示存在。
pub fn render_catalog(catalog: &SkillCatalog) -> String {
    if catalog.skills.is_empty() {
        return String::new();
    }
    let mut output = format!(
        "Skills use progressive disclosure: the catalog lists names and descriptions only.\n\
         For a disk skill, call `{SKILL_LIST_TOOL}` with its name to list internal files, then read \
         them with the standard read tool. Bundled skills' full text is already present below.\n\n\
         <available_skills>\n"
    );
    let mut skills = catalog.skills.iter().collect::<Vec<_>>();
    skills.sort_by(|a, b| {
        let rank = |scope: SkillScope| match scope {
            SkillScope::Project => 0,
            SkillScope::User => 1,
        };
        rank(a.scope)
            .cmp(&rank(b.scope))
            .then_with(|| a.name.cmp(&b.name))
            .then_with(|| a.path.cmp(&b.path))
    });
    let mut omitted = 0usize;
    for skill in skills {
        let description: String = sanitize_single_line(&skill.description)
            .chars()
            .take(MAX_CATALOG_DESCRIPTION_CHARS)
            .collect();
        let line = format!("- {}: {}\n", skill.name, description);
        if output.chars().count() + line.chars().count() > MAX_CATALOG_CHARS {
            omitted += 1;
        } else {
            output.push_str(&line);
        }
    }
    if omitted > 0 {
        output.push_str(&format!("- ... {omitted} skills omitted by catalog budget\n"));
    }
    output.push_str("</available_skills>");
    output
}

/// 会话初始化注入块：`[Skills]` 目录 + 嵌入 bundled 技能正文。
/// 无任何技能时返回空串。
pub fn render_system_section(workspace: &Path) -> String {
    render_system_section_with_home(workspace, home_dir().as_deref())
}

pub fn render_system_section_with_home(workspace: &Path, home: Option<&Path>) -> String {
    let catalog = discover_with_home(workspace, home);
    let mut section = String::new();
    let disk_catalog = SkillCatalog {
        skills: catalog
            .skills
            .iter()
            .filter(|skill| skill.origin != SkillOrigin::Bundled)
            .cloned()
            .collect(),
        diagnostics: catalog.diagnostics.clone(),
    };
    let catalog_text = render_catalog(&disk_catalog);
    let mut bundled_bodies = String::new();
    // 被磁盘同名技能遮蔽的 bundled 不再注入正文，避免双份权威指令。
    let bundled_shadowed = disk_catalog
        .skills
        .iter()
        .map(|skill| skill.name.as_str())
        .collect::<std::collections::HashSet<_>>();
    let mut files = Vec::new();
    collect_embedded_files(&BUNDLED, &mut files);
    for file in files {
        let Some(path) = file.path().to_str() else { continue };
        if !path.ends_with("SKILL.md") {
            continue;
        }
        let Ok(raw) = std::str::from_utf8(file.contents()) else { continue };
        let Ok((name, _)) = parse_str_metadata(raw) else { continue };
        if bundled_shadowed.contains(name.as_str()) {
            continue;
        }
        let body = split_skill_file(raw)
            .map(|(_, body)| body)
            .unwrap_or_default();
        bundled_bodies.push_str(&format!("### skill: {name}\n\n{body}\n\n"));
    }
    if catalog_text.is_empty() && bundled_bodies.is_empty() {
        return section;
    }
    section.push_str("\n[Skills]\n");
    if !catalog_text.is_empty() {
        section.push_str(&catalog_text);
        section.push('\n');
    }
    if !bundled_bodies.is_empty() {
        section.push_str("The following bundled skills are embedded and authoritative; their full text is here, do not re-read them from disk.\n\n");
        section.push_str(bundled_bodies.trim_end());
        section.push('\n');
    }
    section
}

/// 枚举一个技能的内部资源（含 SKILL.md 本身）。
/// 磁盘技能返回绝对路径；bundled 技能直接返回内容。
pub fn skill_resources(workspace: &Path, name: &str) -> Result<Vec<SkillResource>, String> {
    skill_resources_with_home(workspace, name, home_dir().as_deref())
}

pub fn skill_resources_with_home(
    workspace: &Path,
    name: &str,
    home: Option<&Path>,
) -> Result<Vec<SkillResource>, String> {
    let catalog = discover_with_home(workspace, home);
    let metadata = catalog
        .skills
        .iter()
        .find(|skill| skill.name == name)
        .ok_or_else(|| format!("unknown skill '{name}'"))?
        .clone();
    if metadata.origin == SkillOrigin::Bundled {
        let prefix = PathBuf::from(&metadata.name);
        let mut resources = Vec::new();
        let mut files = Vec::new();
        collect_embedded_files(&BUNDLED, &mut files);
        for file in files {
            let Some(path) = file.path().to_str() else { continue };
            let Ok(relative) = Path::new(path).strip_prefix(&prefix) else { continue };
            if resources.len() >= MAX_RESOURCES {
                break;
            }
            resources.push(SkillResource {
                skill_name: metadata.name.clone(),
                relative_path: relative.to_string_lossy().into_owned(),
                source: SkillResourceSource::Embedded {
                    content: String::from_utf8_lossy(file.contents()).into_owned(),
                },
            });
        }
        resources.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
        return Ok(resources);
    }
    let skill_dir = metadata
        .path
        .parent()
        .ok_or("skill has no parent directory")?
        .to_path_buf();
    let mut resources = Vec::new();
    let mut stack = vec![(skill_dir.clone(), 0usize)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(entries) = fs::read_dir(&dir) else { continue };
        let mut entries: Vec<_> = entries.flatten().collect();
        entries.sort_by_key(|entry| entry.file_name());
        for entry in entries {
            if resources.len() >= MAX_RESOURCES {
                return Ok(resources);
            }
            let Ok(file_type) = entry.file_type() else { continue };
            let entry_name = entry.file_name().to_string_lossy().into_owned();
            if file_type.is_symlink() {
                continue;
            }
            let path = entry.path();
            if file_type.is_dir() {
                if !should_skip_dir(&entry_name) && depth < MAX_SCAN_DEPTH {
                    stack.push((path, depth + 1));
                }
                continue;
            }
            let Ok(relative) = path.strip_prefix(&skill_dir) else { continue };
            resources.push(SkillResource {
                skill_name: metadata.name.clone(),
                relative_path: relative.to_string_lossy().replace('\\', "/"),
                source: SkillResourceSource::Disk {
                    absolute_path: path,
                },
            });
        }
    }
    resources.sort_by(|a, b| a.relative_path.cmp(&b.relative_path));
    Ok(resources)
}

fn absolutize(path: &Path) -> PathBuf {
    match path.canonicalize() {
        Ok(path) => path,
        Err(_) => normalize(path),
    }
}

fn normalize(path: &Path) -> PathBuf {
    let mut components = Vec::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                components.pop();
            }
            other => components.push(other),
        }
    }
    components.iter().collect()
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
        .filter(|path| !path.as_os_str().is_empty())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_skill(root: &Path, name: &str, description: &str, body: &str) -> PathBuf {
        let dir = root.join(name);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join("SKILL.md");
        fs::write(
            &path,
            format!("---\nname: {name}\ndescription: {description}\n---\n\n{body}"),
        )
        .unwrap();
        path
    }

    #[test]
    fn workspace_qaqh_root_shadows_agents_root() {
        let temp = tempfile::tempdir().unwrap();
        write_skill(&temp.path().join(".qaqh/skills"), "alpha", "from qaqh", "QAQH");
        write_skill(&temp.path().join(".agents/skills"), "alpha", "from agents", "AGENTS");
        write_skill(&temp.path().join(".agents/skills"), "beta", "only beta", "BETA");
        let catalog = discover_with_home(temp.path(), Some(temp.path()));
        assert_eq!(catalog.skills.len(), 11, "2 disk skills + 9 bundled");
        let alpha = catalog.skills.iter().find(|s| s.name == "alpha").unwrap();
        assert!(alpha.path.to_string_lossy().contains(".qaqh"));
        assert!(catalog
            .diagnostics
            .iter()
            .any(|d| d.message.contains("shadowed")));
    }

    #[test]
    fn frontmatter_missing_description_is_error() {
        let temp = tempfile::tempdir().unwrap();
        let dir = temp.path().join(".qaqh/skills/bad");
        fs::create_dir_all(&dir).unwrap();
        fs::write(dir.join("SKILL.md"), "---\nname: bad\n---\n\nBODY").unwrap();
        let catalog = discover_with_home(temp.path(), Some(temp.path()));
        assert!(catalog.skills.iter().all(|s| s.name != "bad"));
        assert!(catalog
            .diagnostics
            .iter()
            .any(|d| d.severity == DiagnosticSeverity::Error));
    }

    #[test]
    fn embedded_bundled_survives_discovery_and_system_section() {
        let temp = tempfile::tempdir().unwrap();
        let catalog = discover_with_home(temp.path(), Some(temp.path()));
        assert!(
            catalog.skills.iter().any(|s| s.name == "qaqh-core" && s.origin == SkillOrigin::Bundled),
            "bundled pack must surface"
        );
        let section = render_system_section_with_home(temp.path(), Some(temp.path()));
        assert!(section.starts_with("\n[Skills]\n"));
        assert!(section.contains("### skill: qaqh-core"));
    }

    #[test]
    fn disk_skill_shadows_embedded_same_name() {
        let temp = tempfile::tempdir().unwrap();
        write_skill(&temp.path().join(".qaqh/skills"), "qaqh-core", "user override", "OVERRIDE");
        let catalog = discover_with_home(temp.path(), Some(temp.path()));
        let core = catalog.skills.iter().find(|s| s.name == "qaqh-core").unwrap();
        assert_eq!(core.origin, SkillOrigin::Project);
        let section = render_system_section_with_home(temp.path(), Some(temp.path()));
        assert!(
            !section.contains("### skill: qaqh-core"),
            "shadowed bundled body must not be injected"
        );
    }

    #[test]
    fn resources_list_disk_paths_and_embedded_contents() {
        let temp = tempfile::tempdir().unwrap();
        write_skill(&temp.path().join(".qaqh/skills"), "alpha", "alpha", "BODY");
        let resources = skill_resources_with_home(temp.path(), "alpha", Some(temp.path())).unwrap();
        assert_eq!(resources.len(), 1);
        let SkillResourceSource::Disk { absolute_path } = &resources[0].source else {
            panic!("disk resource expected");
        };
        assert!(absolute_path.ends_with("SKILL.md"));

        let bundled = skill_resources_with_home(temp.path(), "qaqh-core", Some(temp.path())).unwrap();
        assert!(!bundled.is_empty());
        assert!(matches!(
            bundled[0].source,
            SkillResourceSource::Embedded { .. }
        ));
    }
}
