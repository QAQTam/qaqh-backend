//! 已知市场注册表 + git 缓存。
//!
//! 用户级布局：
//! - `~/.qaqh/marketplaces/registry.toml` —— 注册表（name → url | local 二选一）
//! - `~/.qaqh/marketplaces/cache/<name>/` —— url 型市场的 git clone（`--depth 1`）
//!
//! local 型市场不 clone，直接引用磁盘路径（本地随磁盘实时，update 是 no-op）。
//! git 操作走子进程——不引 git2，资源受限机不添重依赖。

use crate::manifest::{MarketplaceManifest, load_manifest, validate_name};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

#[derive(Debug, Default, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct Registry {
    #[serde(default, rename = "marketplace")]
    pub marketplaces: Vec<RegisteredMarketplace>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RegisteredMarketplace {
    pub name: String,
    /// 远程 git URL（与 local 二选一）。
    #[serde(default)]
    pub url: String,
    /// 本地市场根（与 url 二选一）。
    #[serde(default)]
    pub local: String,
}

pub fn registry_path(home: &Path) -> PathBuf {
    home.join(".qaqh/marketplaces/registry.toml")
}

pub fn cache_root(home: &Path) -> PathBuf {
    home.join(".qaqh/marketplaces/cache")
}

pub fn market_cache_path(home: &Path, name: &str) -> PathBuf {
    cache_root(home).join(name)
}

/// 读注册表；缺文件 = 空注册表（首次使用的明确语义），解析失败 = 错误。
pub fn load(home: &Path) -> Result<Registry, String> {
    let path = registry_path(home);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Registry::default());
        }
        Err(error) => return Err(format!("read {}: {error}", path.display())),
    };
    toml::from_str(&text).map_err(|error| format!("parse {}: {error}", path.display()))
}

pub fn save(home: &Path, registry: &Registry) -> Result<(), String> {
    let path = registry_path(home);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("create {}: {error}", parent.display()))?;
    }
    let text =
        toml::to_string_pretty(registry).map_err(|error| format!("serialize registry: {error}"))?;
    std::fs::write(&path, text).map_err(|error| format!("write {}: {error}", path.display()))
}

/// 注册市场。url 型当场 clone 并校验正典清单可读；local 型校验目录与清单。
pub fn add(
    home: &Path,
    name: &str,
    url: &str,
    local: &str,
) -> Result<RegisteredMarketplace, String> {
    validate_name(name, "marketplace name")?;
    if url.is_empty() == local.is_empty() {
        return Err("marketplace add: 需要 url 或 local 二选一".to_string());
    }
    let mut registry = load(home)?;
    if registry.marketplaces.iter().any(|m| m.name == name) {
        return Err(format!("marketplace {name:?} 已注册（先 remove 再 add）"));
    }
    let entry = RegisteredMarketplace {
        name: name.to_string(),
        url: url.to_string(),
        local: local.to_string(),
    };
    if url.is_empty() {
        let root = Path::new(&entry.local);
        if !root.is_dir() {
            return Err(format!("local marketplace root 不存在: {}", root.display()));
        }
        load_manifest(root)?;
    } else {
        let dest = market_cache_path(home, name);
        if dest.exists() {
            return Err(format!(
                "缓存目录已存在（先 remove 或手删）: {}",
                dest.display()
            ));
        }
        git(&[
            "clone",
            "--depth",
            "1",
            url,
            &dest.as_os_str().to_string_lossy(),
        ])?;
        if let Err(error) = load_manifest(&dest) {
            let _ = std::fs::remove_dir_all(&dest);
            return Err(format!("clone 成功但清单不可读，已回滚缓存: {error}"));
        }
    }
    registry.marketplaces.push(entry.clone());
    save(home, &registry)?;
    Ok(entry)
}

/// 注销市场并删除 url 型缓存目录。
pub fn remove(home: &Path, name: &str) -> Result<(), String> {
    let mut registry = load(home)?;
    let before = registry.marketplaces.len();
    registry.marketplaces.retain(|m| m.name != name);
    if registry.marketplaces.len() == before {
        return Err(format!("marketplace {name:?} 未注册"));
    }
    save(home, &registry)?;
    let dest = market_cache_path(home, name);
    if dest.exists() {
        std::fs::remove_dir_all(&dest)
            .map_err(|error| format!("remove {}: {error}", dest.display()))?;
    }
    Ok(())
}

/// 拉取市场更新。url 型 `pull --ff-only` 后校验清单；local 型 no-op（随磁盘）。
pub fn update(home: &Path, name: &str) -> Result<(), String> {
    let registry = load(home)?;
    let entry = registry
        .marketplaces
        .iter()
        .find(|m| m.name == name)
        .ok_or_else(|| format!("marketplace {name:?} 未注册"))?;
    if !entry.local.is_empty() {
        return Ok(());
    }
    let cache = market_cache_path(home, name);
    git(&[
        "-C",
        &cache.as_os_str().to_string_lossy(),
        "pull",
        "--ff-only",
    ])?;
    load_manifest(&cache)?;
    Ok(())
}

/// 市场根（local 直引 / url 走缓存）。不校验清单存在——消费方读清单时自然报错。
pub fn resolve_root(home: &Path, name: &str) -> Result<PathBuf, String> {
    let registry = load(home)?;
    let entry = registry
        .marketplaces
        .iter()
        .find(|m| m.name == name)
        .ok_or_else(|| format!("marketplace {name:?} 未注册（先 add）"))?;
    if entry.local.is_empty() {
        Ok(market_cache_path(home, name))
    } else {
        Ok(PathBuf::from(&entry.local))
    }
}

fn git(args: &[&str]) -> Result<String, String> {
    let output = std::process::Command::new("git")
        .args(args)
        .output()
        .map_err(|error| format!("运行 git 失败（PATH 里没有 git？）: {error}"))?;
    if !output.status.success() {
        return Err(format!(
            "git {} 失败: {}",
            args.join(" "),
            String::from_utf8_lossy(&output.stderr).trim()
        ));
    }
    Ok(String::from_utf8_lossy(&output.stdout).to_string())
}

/// 列出全部市场与其清单（清单不可读的市场带错误占位，不断整表）。
pub fn list_with_manifests(
    home: &Path,
) -> Vec<(RegisteredMarketplace, Result<MarketplaceManifest, String>)> {
    let Ok(registry) = load(home) else {
        return Vec::new();
    };
    registry
        .marketplaces
        .iter()
        .map(|entry| {
            let root = if entry.local.is_empty() {
                market_cache_path(home, &entry.name)
            } else {
                PathBuf::from(&entry.local)
            };
            (entry.clone(), load_manifest(&root))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn write_market(root: &Path, name: &str, plugin: &str) {
        let dir = root.join(".agents/plugins");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("marketplace.json"),
            format!(
                r#"{{ "name": "{name}", "plugins": [ {{ "name": "{plugin}", "source": "./plugins/{plugin}" }} ] }}"#
            ),
        )
        .unwrap();
    }

    #[test]
    fn add_local_roundtrip_and_remove() {
        let home = tempfile::tempdir().unwrap();
        let market = tempfile::tempdir().unwrap();
        write_market(market.path(), "my-plugins", "commit-helper");

        let entry = add(
            home.path(),
            "my-plugins",
            "",
            market.path().to_string_lossy().as_ref(),
        )
        .unwrap();
        assert_eq!(entry.name, "my-plugins");

        let registry = load(home.path()).unwrap();
        assert_eq!(registry.marketplaces.len(), 1);

        let root = resolve_root(home.path(), "my-plugins").unwrap();
        let manifest = load_manifest(&root).unwrap();
        assert_eq!(manifest.plugins[0].name, "commit-helper");

        remove(home.path(), "my-plugins").unwrap();
        assert!(load(home.path()).unwrap().marketplaces.is_empty());
    }

    #[test]
    fn add_requires_exactly_one_of_url_local() {
        let home = tempfile::tempdir().unwrap();
        assert!(add(home.path(), "m", "", "").is_err());
        assert!(add(home.path(), "m", "https://x", "D:/y").is_err());
    }

    #[test]
    fn add_rejects_market_without_manifest() {
        let home = tempfile::tempdir().unwrap();
        let market = tempfile::tempdir().unwrap();
        assert!(
            add(
                home.path(),
                "m",
                "",
                market.path().to_string_lossy().as_ref()
            )
            .is_err()
        );
    }
}
