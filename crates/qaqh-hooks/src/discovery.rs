//! Hook 发现:三个来源归一化为一张注册表。
//!
//! | 来源 | 路径 | 信任 |
//! |------|------|------|
//! | Config | config.toml `[[hooks.<Event>]]` | 始终可信 |
//! | Global | `<data_dir>/hooks/*.json` | 始终可信 |
//! | Project | `<workspace_root>/.qaqh/hooks/*.json` | 需 trust store 批准 |
//!
//! 项目作用域的信任粒度 = workspace + 声明 digest:hook 集任何增删改都会
//! 使既有批准失效。未知事件名/非法 matcher/非法 handler → 跳过该组并收集
//! 警告(grok 同律:共享的 Claude/Cursor settings 文件照常装载)。

use std::path::{Path, PathBuf};

use crate::config::{HandlerKind, HookEventsConfig, MatcherGroup};
use crate::event::HookEventName;
use crate::matcher::HookMatcher;
use crate::trust::{project_hooks_digest, HookTrustStore};

#[derive(Debug, Clone, PartialEq)]
pub enum HookSource {
    Config,
    GlobalFile { path: PathBuf },
    ProjectFile { path: PathBuf },
}

impl HookSource {
    pub fn trusted_by_default(&self) -> bool {
        matches!(self, Self::Config | Self::GlobalFile { .. })
    }

    pub fn tag(&self) -> String {
        match self {
            Self::Config => "config".to_string(),
            Self::GlobalFile { path } => format!("global:{}", path.display()),
            Self::ProjectFile { path } => format!("project:{}", path.display()),
        }
    }
}

#[derive(Debug, Clone)]
pub struct HookEntry {
    /// 展示名:`<Event>#<basename>#<序号>`。
    pub name: String,
    pub event: HookEventName,
    pub matcher: Option<HookMatcher>,
    pub handler: crate::config::HookHandler,
    pub handler_kind: HandlerKind,
    pub source: HookSource,
    /// 相对 command 的解析基准目录(声明文件所在目录)。
    pub source_dir: PathBuf,
}

impl HookEntry {
    /// 命令/URL 的稳定摘要(展示与 digest)。
    pub fn target(&self) -> &str {
        match self.handler_kind {
            HandlerKind::Command => self.handler.command.as_deref().unwrap_or(""),
            HandlerKind::Http => self.handler.url.as_deref().unwrap_or(""),
        }
    }
}

#[derive(Debug, Clone, Default)]
pub struct HookRegistry {
    entries: Vec<HookEntry>,
}

impl HookRegistry {
    pub fn is_empty(&self) -> bool {
        self.entries.is_empty()
    }

    pub fn entries(&self) -> &[HookEntry] {
        &self.entries
    }

    pub fn hooks_for(&self, event: HookEventName) -> Vec<&HookEntry> {
        let mut selected: Vec<&HookEntry> = self
            .entries
            .iter()
            .filter(|entry| entry.event == event)
            .collect();
        // 稳定序:先按来源(Config < Global < Project)再按声明序。
        selected.sort_by_key(|entry| match &entry.source {
            HookSource::Config => 0u8,
            HookSource::GlobalFile { .. } => 1,
            HookSource::ProjectFile { .. } => 2,
        });
        selected
    }

    /// 项目作用域声明的 digest 输入行(顺序无关,trust.rs 内部排序)。
    pub fn project_digest_lines(&self) -> Vec<String> {
        self.entries
            .iter()
            .filter(|entry| matches!(entry.source, HookSource::ProjectFile { .. }))
            .map(|entry| {
                format!(
                    "{}|{}|{}|{}|{}|{}",
                    entry.event.key(),
                    entry.matcher.as_ref().map(|m| m.pattern().to_string()).unwrap_or_default(),
                    entry.target(),
                    entry.handler.timeout.map(|t| t.to_string()).unwrap_or_default(),
                    entry.handler.async_run,
                    entry.handler.on_failure == crate::config::FailurePolicy::Block,
                )
            })
            .collect()
    }
}

#[derive(Debug)]
pub struct Discovery {
    pub registry: HookRegistry,
    pub warnings: Vec<String>,
    /// 项目作用域 digest(即使未信任也要算,信任批准面用它)。
    pub project_digest: Option<String>,
    /// 项目 hooks 当前是否已信任(digest 匹配)。
    pub project_trusted: bool,
}

/// 归一化装载。`config_dir` = config.toml 所在目录(相对 command 的解析基准,
/// None = data_dir)。
pub fn load(
    config_hooks: &HookEventsConfig,
    data_dir: &Path,
    config_dir: Option<&Path>,
    workspace_root: Option<&Path>,
    trust: &HookTrustStore,
) -> Discovery {
    let mut registry = HookRegistry::default();
    let mut warnings = Vec::new();

    let config_dir = config_dir.unwrap_or(data_dir).to_path_buf();
    ingest_groups(
        config_hooks,
        HookSource::Config,
        &config_dir,
        &mut registry,
        &mut warnings,
    );

    let global_dir = data_dir.join("hooks");
    for path in json_files(&global_dir) {
        match read_hooks_file(&path) {
            Ok(file_hooks) => {
                let source_dir = path.parent().unwrap_or(data_dir).to_path_buf();
                ingest_groups(
                    &file_hooks,
                    HookSource::GlobalFile { path: path.clone() },
                    &source_dir,
                    &mut registry,
                    &mut warnings,
                );
            }
            Err(error) => warnings.push(format!("skip global hooks file {}: {error}", path.display())),
        }
    }

    let mut project_digest = None;
    let mut project_trusted = false;
    if let Some(root) = workspace_root {
        let project_dir = root.join(".qaqh").join("hooks");
        let before = registry.entries.len();
        for path in json_files(&project_dir) {
            match read_hooks_file(&path) {
                Ok(file_hooks) => {
                    let source_dir = path.parent().unwrap_or(&project_dir).to_path_buf();
                    ingest_groups(
                        &file_hooks,
                        HookSource::ProjectFile { path: path.clone() },
                        &source_dir,
                        &mut registry,
                        &mut warnings,
                    );
                }
                Err(error) => warnings.push(format!("skip project hooks file {}: {error}", path.display())),
            }
        }
        if registry.entries.len() > before {
            let digest = project_hooks_digest(&registry.project_digest_lines());
            project_trusted = trust
                .record(root)
                .map(|record| record.trusted && record.hook_digest == digest)
                .unwrap_or(false);
            project_digest = Some(digest);
        }
    }

    Discovery {
        registry,
        warnings,
        project_digest,
        project_trusted,
    }
}

fn json_files(dir: &Path) -> Vec<PathBuf> {
    let Ok(read) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<PathBuf> = read
        .flatten()
        .map(|entry| entry.path())
        .filter(|path| path.extension().is_some_and(|ext| ext == "json"))
        .collect();
    files.sort();
    files
}

/// hooks JSON 文件形态:`{"hooks": {"<Event>": [...]}}`(grok/Claude 同形)。
#[derive(serde::Deserialize)]
struct HooksFile {
    hooks: HookEventsConfig,
}

fn read_hooks_file(path: &Path) -> Result<HookEventsConfig, String> {
    let text = std::fs::read_to_string(path).map_err(|error| error.to_string())?;
    let file: HooksFile =
        serde_json::from_str(&text).map_err(|error| error.to_string())?;
    Ok(file.hooks)
}

fn ingest_groups(
    hooks: &HookEventsConfig,
    source: HookSource,
    source_dir: &Path,
    registry: &mut HookRegistry,
    warnings: &mut Vec<String>,
) {
    for key in hooks.unknown_events.keys() {
        warnings.push(format!("skip unknown hook event name `{key}`"));
    }
    for event in HookEventName::ALL {
        for (group_index, group) in hooks.groups_for(*event).iter().enumerate() {
            ingest_group(*event, group_index, group, &source, source_dir, registry, warnings);
        }
    }
}

fn ingest_group(
    event: HookEventName,
    group_index: usize,
    group: &MatcherGroup,
    source: &HookSource,
    source_dir: &Path,
    registry: &mut HookRegistry,
    warnings: &mut Vec<String>,
) {
    if event.traits().matcher == crate::event::MatcherPolicy::Ignored && group.matcher.is_some() {
        warnings.push(format!(
            "matcher on {} is ignored (event has no match value)",
            event.key()
        ));
    }
    let matcher = match HookMatcher::parse(group.matcher.as_deref()) {
        Ok(matcher) => matcher,
        Err(error) => {
            warnings.push(format!("{} group #{group_index}: {error}", event.key()));
            return;
        }
    };
    for (handler_index, handler) in group.hooks.iter().enumerate() {
        let kind = match handler.validate() {
            Ok(kind) => kind,
            Err(error) => {
                warnings.push(format!("{} group #{group_index} handler #{handler_index}: {error}", event.key()));
                continue;
            }
        };
        let name = format!(
            "{}#{}#{}",
            event.key(),
            Path::new(handler.command.as_deref().or(handler.url.as_deref()).unwrap_or("hook"))
                .file_name()
                .map(|n| n.to_string_lossy().to_string())
                .unwrap_or_else(|| "hook".to_string()),
            handler_index,
        );
        registry.entries.push(HookEntry {
            name,
            event,
            matcher: matcher.clone(),
            handler: handler.clone(),
            handler_kind: kind,
            source: source.clone(),
            source_dir: source_dir.to_path_buf(),
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::trust::HookTrustStore;
    use std::fs;

    fn scratch(tag: u32) -> PathBuf {
        let dir = std::env::temp_dir().join(format!(
            "qaqh-hooks-discovery-{}-{tag}",
            std::process::id()
        ));
        let _ = fs::remove_dir_all(&dir);
        fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn loads_config_global_and_project_with_trust() {
        let data = scratch(1);
        let root = scratch(2);
        let trust_path = data.join("security/workspace-hook-trust-v1.json");
        let trust = HookTrustStore::load(trust_path);

        let mut config_hooks = HookEventsConfig::default();
        config_hooks.pre_tool_use.push(MatcherGroup {
            matcher: Some("Bash".to_string()),
            hooks: vec![serde_json::from_value(serde_json::json!({
                "type": "command", "command": "bin/guard.sh", "timeout": 10
            })).unwrap()],
        });

        fs::create_dir_all(data.join("hooks")).unwrap();
        fs::write(
            data.join("hooks/global.json"),
            serde_json::json!({ "hooks": { "Stop": [ { "hooks": [ { "type": "command", "command": "notify.sh" } ] } ] } }).to_string(),
        )
        .unwrap();

        fs::create_dir_all(root.join(".qaqh/hooks")).unwrap();
        fs::write(
            root.join(".qaqh/hooks/project.json"),
            serde_json::json!({ "hooks": { "PostToolUse": [ { "hooks": [ { "type": "command", "command": "log.sh" } ] } ] } }).to_string(),
        )
        .unwrap();

        let discovery = load(&config_hooks, &data, Some(&data), Some(&root), &trust);
        assert!(discovery.warnings.is_empty(), "{:?}", discovery.warnings);
        assert_eq!(discovery.registry.hooks_for(HookEventName::PreToolUse).len(), 1);
        assert_eq!(discovery.registry.hooks_for(HookEventName::Stop).len(), 1);
        assert_eq!(discovery.registry.hooks_for(HookEventName::PostToolUse).len(), 1);
        // 项目 hooks 未批准:digest 在,trusted = false。
        assert!(discovery.project_digest.is_some());
        assert!(!discovery.project_trusted);
    }

    #[test]
    fn unknown_event_and_bad_handler_are_skipped_with_warnings() {
        let data = scratch(3);
        let trust = HookTrustStore::load(data.join("trust.json"));
        fs::create_dir_all(data.join("hooks")).unwrap();
        fs::write(
            data.join("hooks/mixed.json"),
            serde_json::json!({ "hooks": {
                "NotAnEvent": [ { "hooks": [ { "type": "command", "command": "x" } ] } ],
                "Stop": [ { "hooks": [ { "type": "carrier-pigeon" } ] } ]
            } }).to_string(),
        )
        .unwrap();

        let discovery = load(&HookEventsConfig::default(), &data, Some(&data), None, &trust);
        assert!(discovery.registry.is_empty());
        assert_eq!(discovery.warnings.len(), 2);
    }
}
