//! 市场清单模型（正典 `.agents/plugins/marketplace.json`）。
//!
//! wire 形状（中立交集，双宿主原生可读）：
//!
//! ```json
//! {
//!   "name": "my-plugins",
//!   "owner": { "name": "..." },
//!   "plugins": [
//!     { "name": "commit-helper", "source": "./plugins/commit-helper",
//!       "description": "...", "version": "1.0.0", "category": "workflow" }
//!   ]
//! }
//! ```
//!
//! `source` 交集：裸字符串 = 市场内相对路径；`{"source":"url","url":…,"path"?}`；
//! `{"source":"npm","package":…,"version"?}`。Claude 的 `"github"`/`"git"` tag 与
//! Codex 的 `"local"`/`"git-subdir"` tag 不在交集（宿主侧兼容矩阵见 ADR-0022），
//! 出现在正典清单里即解析失败——校验器不做形态归一。

use serde::{Deserialize, Serialize};
use std::path::{Component, Path};

/// 正典清单相对路径（市场仓库根起算）。
pub const MARKETPLACE_MANIFEST_RELATIVE_PATH: &str = ".agents/plugins/marketplace.json";

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarketplaceManifest {
    pub name: String,
    #[serde(default)]
    pub owner: Option<MarketplaceOwner>,
    #[serde(default)]
    pub plugins: Vec<PluginEntry>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct MarketplaceOwner {
    #[serde(default)]
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PluginEntry {
    pub name: String,
    pub source: PluginSource,
    #[serde(default)]
    pub description: String,
    #[serde(default)]
    pub version: String,
    #[serde(default)]
    pub category: String,
}

/// 条目 source —— 中立交集形态（允许集见模块注）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(untagged)]
pub enum PluginSource {
    /// 市场仓库内相对路径（Codex `Path(String)` ∩ Claude 相对路径）。
    Path(String),
    Object(SourceObject),
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "source", rename_all = "lowercase")]
pub enum SourceObject {
    /// git 仓库 URL（可含仓库内 subdir `path`；双方宿主的下载型 tag 同名）。
    Url {
        url: String,
        #[serde(default)]
        path: String,
    },
    Npm {
        package: String,
        #[serde(default)]
        version: String,
    },
}

impl PluginSource {
    /// 市场内相对路径形态才可被 v1 安装器直接消费；npm/url 形态返回 `None`
    /// （走宿主原生安装）。
    pub fn market_relative_path(&self) -> Option<&str> {
        match self {
            PluginSource::Path(path) => Some(path),
            PluginSource::Object(_) => None,
        }
    }
}

/// 解析 + fail-fast 校验（校验点唯一：`PluginSource::market_relative_path`
/// 的消费方不做二次检查）。
pub fn parse_manifest(text: &str) -> Result<MarketplaceManifest, String> {
    let manifest: MarketplaceManifest = serde_json::from_str(text)
        .map_err(|error| format!("marketplace manifest parse failed: {error}"))?;
    validate(&manifest)?;
    Ok(manifest)
}

/// 读市场根下的正典清单。
pub fn load_manifest(market_root: &Path) -> Result<MarketplaceManifest, String> {
    let path = market_root.join(MARKETPLACE_MANIFEST_RELATIVE_PATH);
    let text = std::fs::read_to_string(&path)
        .map_err(|error| format!("read {}: {error}", path.display()))?;
    parse_manifest(&text)
}

fn validate(manifest: &MarketplaceManifest) -> Result<(), String> {
    validate_name(&manifest.name, "marketplace name")?;
    if manifest.plugins.is_empty() {
        return Err(format!("marketplace {:?} lists no plugins", manifest.name));
    }
    for plugin in &manifest.plugins {
        validate_name(&plugin.name, "plugin name")?;
        if let PluginSource::Path(relative) = &plugin.source {
            validate_market_relative_path(relative)?;
        }
    }
    Ok(())
}

/// 标识名口径与 qaqh-config MCP server 名一致：`[a-z0-9_-]{1,64}`。
pub(crate) fn validate_name(name: &str, what: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > 64 {
        return Err(format!("{what} must be 1..=64 chars, got {name:?}"));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
    {
        return Err(format!("{what} {name:?} must match [a-z0-9_-]+"));
    }
    Ok(())
}

/// 市场内相对路径：正斜杠、无根/父目录/盘符组件（防逃逸）。`./` 前缀是
/// Claude marketplace spec 的惯用写法（官方市场如此），CurDir 无逃逸语义，
/// 允许。
fn validate_market_relative_path(relative: &str) -> Result<(), String> {
    if relative.is_empty() {
        return Err("plugin source path must not be empty".to_string());
    }
    if relative.contains('\\') {
        return Err(format!(
            "plugin source path {relative:?} must use forward slashes"
        ));
    }
    for component in Path::new(relative).components() {
        match component {
            Component::Normal(_) | Component::CurDir => {}
            other => {
                return Err(format!(
                    "plugin source path {relative:?} contains a non-normal component: {other:?}"
                ));
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_neutral_canonical_manifest() {
        let text = r#"{
            "name": "my-plugins",
            "owner": { "name": "qaq" },
            "plugins": [
                { "name": "commit-helper", "source": "./plugins/commit-helper",
                  "description": "d", "version": "1.0.0", "category": "workflow" },
                { "name": "remote-tool", "source": { "source": "url", "url": "https://example.com/x.git", "path": "pkg" } },
                { "name": "npm-tool", "source": { "source": "npm", "package": "pkg" } }
            ]
        }"#;
        let manifest = parse_manifest(text).unwrap();
        assert_eq!(manifest.name, "my-plugins");
        assert_eq!(manifest.plugins.len(), 3);
        assert_eq!(
            manifest.plugins[0].source.market_relative_path(),
            Some("./plugins/commit-helper")
        );
        assert!(manifest.plugins[1].source.market_relative_path().is_none());
    }

    #[test]
    fn rejects_out_of_intersection_source_tags() {
        // "github" 是 Claude 简写、"git-subdir" 是 Codex 原生 tag —— 都不在交集。
        for tag in ["github", "git", "git-subdir", "local"] {
            let text = format!(
                r#"{{ "name": "m", "plugins": [ {{ "name": "p", "source": {{ "source": "{tag}", "url": "https://x" }} }} ] }}"#
            );
            assert!(
                parse_manifest(&text).is_err(),
                "tag {tag:?} must be rejected"
            );
        }
    }

    #[test]
    fn rejects_path_escape() {
        // "a\\\\b"：Rust 层 = a\\b，JSON 层 \\ 转义 = a\b（单反斜杠字符串）。
        for relative in ["../escape", "/abs", "a\\\\b", ""] {
            let text = format!(
                r#"{{ "name": "m", "plugins": [ {{ "name": "p", "source": "{relative}" }} ] }}"#
            );
            assert!(
                parse_manifest(&text).is_err(),
                "path {relative:?} must be rejected"
            );
        }
    }
}
