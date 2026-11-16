//! 市场插件 → 宿主安装投影（plan / execute 两段式）。
//!
//! 投影面（v1）：
//! - `skills/` —— 三宿主拷贝（qaqh `~/.qaqh/skills`、codex `~/.codex/skills`、
//!   claude `~/.claude/skills`；均为各宿主原生发现根，落盘即生效）。
//! - `.mcp.json` —— 仅 Qaqh 目标：走 [`qaqh_config::mcp_import::import_servers`]
//!   （D4 审批链 + env 占位符化）。codex/claude 目标**不写宿主配置文件**——
//!   外部宿主的 MCP 由宿主各自的 marketplace 机制安装（codex app-server
//!   `PluginInstall` / claude `/plugin install`），代写外部宿主配置是纯破坏面。
//! - `agents/` `commands/` `hooks/` —— 不投影：本仓无消费面，装了即骗人；
//!   需要这些能力的插件请走宿主原生 marketplace 安装。
//!
//! 安装账本：`~/.qaqh/marketplaces/installed.toml`（`[[installed]]` 表）记录
//! 每次投影实际写入的内容（skills 目录名、server 名、secrets 键名）——卸载
//! 的唯一事实源。`execute` 成功后追加，[`uninstall_execute`] 反向销毁并删记录。

use crate::manifest::PluginEntry;
use qaqh_config::mcp_import::{self, ExternalServer, ExternalSource};
use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize)]
#[serde(rename_all = "lowercase")]
pub enum HostTarget {
    Qaqh,
    Codex,
    Claude,
}

impl HostTarget {
    pub fn from_flag(value: &str) -> Result<Self, String> {
        match value {
            "qaqh" => Ok(Self::Qaqh),
            "codex" => Ok(Self::Codex),
            "claude" => Ok(Self::Claude),
            other => Err(format!(
                "unknown target {other:?}; expected qaqh|codex|claude"
            )),
        }
    }

    pub fn flag(self) -> &'static str {
        match self {
            Self::Qaqh => "qaqh",
            Self::Codex => "codex",
            Self::Claude => "claude",
        }
    }

    /// 用户级 skills 投影根（各宿主原生发现根）。
    pub fn skills_root(self, home: &Path) -> PathBuf {
        match self {
            Self::Qaqh => home.join(".qaqh/skills"),
            Self::Codex => home.join(".codex/skills"),
            Self::Claude => home.join(".claude/skills"),
        }
    }
}

/// 不派生 `PartialEq`/`Eq`：成员 [`ExternalServer`]（qaqh-config）未实现。
#[derive(Debug, Clone)]
pub struct InstallPlan {
    pub plugin: String,
    pub marketplace: String,
    pub target: HostTarget,
    /// `(源目录, 目的目录)`：`skills/<name>` → `<宿主 skills 根>/<name>`。
    pub skill_copies: Vec<(PathBuf, PathBuf)>,
    /// 仅 Qaqh 目标非空（来自插件 `.mcp.json` 的 `mcpServers`）。
    pub mcp_servers: Vec<ExternalServer>,
}

impl InstallPlan {
    /// dry-run 展示行（CLI / HTTP plan 复用）。
    pub fn describe(&self) -> Vec<String> {
        let mut lines = Vec::new();
        for (source, dest) in &self.skill_copies {
            lines.push(format!(
                "skill {}: {} -> {}",
                source.file_name().and_then(|n| n.to_str()).unwrap_or("?"),
                source.display(),
                dest.display()
            ));
        }
        for server in &self.mcp_servers {
            lines.push(format!(
                "mcp {}: command={:?} url={}",
                server.name, server.command, server.url
            ));
        }
        lines
    }
}

/// 生成安装计划（不落任何盘）。清单已在 [`crate::manifest::parse_manifest`]
/// 校验过 path 形态，这里不做二次校验。
pub fn plan(
    entry: &PluginEntry,
    market_root: &Path,
    target: HostTarget,
    home: &Path,
    marketplace: &str,
) -> Result<InstallPlan, String> {
    let Some(relative) = entry.source.market_relative_path() else {
        return Err(format!(
            "插件 {:?} 的 source 不是市场内路径（v1 安装器仅投影 path source；npm/url 请用宿主原生 marketplace 安装）",
            entry.name
        ));
    };
    let plugin_root = market_root.join(relative);
    if !plugin_root.is_dir() {
        return Err(format!(
            "插件 {:?} 目录不存在: {}",
            entry.name,
            plugin_root.display()
        ));
    }

    let mut skill_copies = Vec::new();
    let skills_src = plugin_root.join("skills");
    if skills_src.is_dir() {
        for name in skill_dir_names(&skills_src)? {
            skill_copies.push((
                skills_src.join(&name),
                target.skills_root(home).join(&name),
            ));
        }
    }

    let mut mcp_servers = Vec::new();
    let mcp_json = plugin_root.join(".mcp.json");
    if mcp_json.is_file() {
        if target != HostTarget::Qaqh {
            return Err(format!(
                "插件 {:?} 含 .mcp.json；{} 目标不代写宿主 MCP 配置，请用宿主原生 marketplace 安装该插件",
                entry.name,
                target.flag()
            ));
        }
        mcp_servers = mcp_import::scan_claude(&mcp_json, ExternalSource::Marketplace);
        if mcp_servers.is_empty() {
            return Err(format!(
                "插件 {:?} 的 .mcp.json 存在但无可导入 server（解析失败或为空）",
                entry.name
            ));
        }
    }

    if skill_copies.is_empty() && mcp_servers.is_empty() {
        return Err(format!(
            "插件 {:?} 无可投影内容（skills/ 与 .mcp.json 均缺失；agents/commands/hooks 不在本仓投影面）",
            entry.name
        ));
    }

    Ok(InstallPlan {
        plugin: entry.name.clone(),
        marketplace: marketplace.to_string(),
        target,
        skill_copies,
        mcp_servers,
    })
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct InstallReport {
    pub plugin: String,
    pub target: HostTarget,
    pub copied_skills: Vec<String>,
    pub imported_servers: Vec<String>,
    pub skipped_existing: Vec<String>,
    pub rejected: Vec<String>,
}

/// 执行计划。`approve` 逐 server 审批（市场内容 = 第三方供应链面，CLI/HTTP
/// 调用方负责放行；skill 拷贝随计划整体执行——计划已在 plan 阶段呈现）。
/// 成功后把实际写入内容追加进安装账本（`ledger_home`）。
pub fn execute(
    plan: &InstallPlan,
    approve: &dyn Fn(&ExternalServer) -> bool,
    config_mcp: &mut qaqh_config::config::McpConfig,
    secrets: &qaqh_config::secrets::SecretStore,
    ledger_home: &Path,
) -> Result<InstallReport, String> {
    for (source, dest) in &plan.skill_copies {
        if dest.exists() {
            return Err(format!(
                "目标已存在（升级请先 remove 同名技能再 install）: {}",
                dest.display()
            ));
        }
        copy_dir_recursive(source, dest)?;
    }

    let mut report = InstallReport {
        plugin: plan.plugin.clone(),
        target: plan.target,
        copied_skills: plan
            .skill_copies
            .iter()
            .filter_map(|(source, _)| {
                source
                    .file_name()
                    .and_then(|n| n.to_str())
                    .map(str::to_string)
            })
            .collect(),
        imported_servers: Vec::new(),
        skipped_existing: Vec::new(),
        rejected: Vec::new(),
    };

    if !plan.mcp_servers.is_empty() {
        let import =
            mcp_import::import_servers(&plan.mcp_servers, approve, config_mcp, secrets)?;
        report.imported_servers = import.imported;
        report.skipped_existing = import.skipped_existing;
        report.rejected = import.rejected;
    }

    // 账本只记实际写入的（imported ≠ 计划全集：skipped/rejected 不入库）。
    if !report.copied_skills.is_empty() || !report.imported_servers.is_empty() {
        let record = InstalledRecord {
            plugin: plan.plugin.clone(),
            marketplace: plan.marketplace.clone(),
            target: plan.target.flag().to_string(),
            skills: report.copied_skills.clone(),
            mcp_servers: report.imported_servers.clone(),
            // 命名契约同 mcp_import::import_servers（`mcp-<server>-<envkey>`）。
            secret_keys: plan
                .mcp_servers
                .iter()
                .filter(|server| report.imported_servers.contains(&server.name))
                .flat_map(|server| {
                    server
                        .env
                        .keys()
                        .map(|key| format!("mcp-{}-{}", server.name, key.to_lowercase()))
                })
                .collect(),
        };
        ledger_append(ledger_home, record)?;
    }
    Ok(report)
}

/// `skills/` 下的直接子目录名（每个子目录 = 一个技能；混入普通文件 = 插件
/// 作者错，fail-fast 报出）。
fn skill_dir_names(skills_dir: &Path) -> Result<Vec<String>, String> {
    let read = std::fs::read_dir(skills_dir)
        .map_err(|error| format!("read {}: {error}", skills_dir.display()))?;
    let mut names = Vec::new();
    for item in read {
        let item = item.map_err(|error| format!("read {}: {error}", skills_dir.display()))?;
        let name = item
            .file_name()
            .to_str()
            .ok_or_else(|| format!("技能目录名非 UTF-8: {}", item.path().display()))?
            .to_string();
        if !item.path().is_dir() {
            return Err(format!(
                "skills/ 下出现非目录项（插件包结构错误）: {}",
                item.path().display()
            ));
        }
        names.push(name);
    }
    names.sort();
    Ok(names)
}

fn copy_dir_recursive(source: &Path, dest: &Path) -> Result<(), String> {
    std::fs::create_dir_all(dest)
        .map_err(|error| format!("create {}: {error}", dest.display()))?;
    let read = std::fs::read_dir(source)
        .map_err(|error| format!("read {}: {error}", source.display()))?;
    for item in read {
        let item = item.map_err(|error| format!("read {}: {error}", source.display()))?;
        let from = item.path();
        let to = dest.join(item.file_name());
        if from.is_dir() {
            copy_dir_recursive(&from, &to)?;
        } else {
            std::fs::copy(&from, &to).map_err(|error| {
                format!("copy {} -> {}: {error}", from.display(), to.display())
            })?;
        }
    }
    Ok(())
}

// ── 安装账本（卸载的唯一事实源） ──

/// 一次投影实际写入的内容。`secret_keys` 的命名契约同
/// `mcp_import::import_servers`（`mcp-<server>-<envkey>`）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct InstalledRecord {
    pub plugin: String,
    pub marketplace: String,
    /// `qaqh` | `codex` | `claude`（`HostTarget::flag`）。
    pub target: String,
    #[serde(default)]
    pub skills: Vec<String>,
    #[serde(default)]
    pub mcp_servers: Vec<String>,
    #[serde(default)]
    pub secret_keys: Vec<String>,
}

#[derive(Debug, Default, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct InstalledLedger {
    #[serde(default, rename = "installed")]
    pub records: Vec<InstalledRecord>,
}

pub fn ledger_path(ledger_home: &Path) -> PathBuf {
    ledger_home.join(".qaqh/marketplaces/installed.toml")
}

/// 读账本；缺文件 = 空账本（首次使用的明确语义），解析失败 = 错误。
pub fn installed_list(ledger_home: &Path) -> Result<Vec<InstalledRecord>, String> {
    let path = ledger_path(ledger_home);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok(Vec::new());
        }
        Err(error) => return Err(format!("read {}: {error}", path.display())),
    };
    let ledger: InstalledLedger =
        toml::from_str(&text).map_err(|error| format!("parse {}: {error}", path.display()))?;
    Ok(ledger.records)
}

fn ledger_append(ledger_home: &Path, record: InstalledRecord) -> Result<(), String> {
    let mut ledger = InstalledLedger {
        records: installed_list(ledger_home)?,
    };
    ledger.records.push(record);
    ledger_save(ledger_home, ledger)
}

fn ledger_save(ledger_home: &Path, ledger: InstalledLedger) -> Result<(), String> {
    let path = ledger_path(ledger_home);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)
            .map_err(|error| format!("create {}: {error}", parent.display()))?;
    }
    let text = toml::to_string_pretty(&ledger)
        .map_err(|error| format!("serialize ledger: {error}"))?;
    std::fs::write(&path, text).map_err(|error| format!("write {}: {error}", path.display()))
}

// ── 卸载（账本驱动反向投影） ──

/// 卸载计划（dry-run 展示面）。来自账本记录，不含 config/secrets 实况——
/// 实况差异在 execute 阶段按幂等语义处理。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UninstallPlan {
    pub plugin: String,
    pub marketplace: String,
    pub target: HostTarget,
    /// 将删除的技能目录绝对路径。
    pub skill_dirs: Vec<PathBuf>,
    /// 将移除的 MCP server 名（qaqh 目标）。
    pub mcp_servers: Vec<String>,
    /// 将删除的 secrets 键名。
    pub secret_keys: Vec<String>,
}

impl UninstallPlan {
    pub fn describe(&self) -> Vec<String> {
        let mut lines = Vec::new();
        for dir in &self.skill_dirs {
            lines.push(format!("skill dir: {}", dir.display()));
        }
        for server in &self.mcp_servers {
            lines.push(format!("mcp server: {server}"));
        }
        for key in &self.secret_keys {
            lines.push(format!("secret: {key}"));
        }
        lines
    }
}

/// 从账本取卸载计划。`plugin + target` 精确匹配；无记录 = Err。
pub fn uninstall_plan(
    ledger_home: &Path,
    plugin: &str,
    target: HostTarget,
    home: &Path,
) -> Result<UninstallPlan, String> {
    let records = installed_list(ledger_home)?;
    let record = records
        .iter()
        .find(|r| r.plugin == plugin && r.target == target.flag())
        .ok_or_else(|| {
            format!(
                "插件 {plugin:?} 无 target={} 的安装记录（账本无此条目）",
                target.flag()
            )
        })?;
    Ok(UninstallPlan {
        plugin: record.plugin.clone(),
        marketplace: record.marketplace.clone(),
        target,
        skill_dirs: record
            .skills
            .iter()
            .map(|name| target.skills_root(home).join(name))
            .collect(),
        mcp_servers: record.mcp_servers.clone(),
        secret_keys: record.secret_keys.clone(),
    })
}

#[derive(Debug, Default, Clone, serde::Serialize)]
pub struct UninstallReport {
    pub removed_skills: Vec<String>,
    pub removed_servers: Vec<String>,
    pub removed_secrets: Vec<String>,
}

/// 执行卸载。技能目录不存在则跳过（部分完成的卸载可重试——账本在最后才删，
/// 任何失败都会保留记录供重试）。qaqh 目标同时移除 `[mcp.servers]` 条目与
/// secrets 键；secrets/配置键已不存在 = 幂等 no-op。
pub fn uninstall_execute(
    plan: &UninstallPlan,
    config_mcp: &mut qaqh_config::config::McpConfig,
    secrets: &qaqh_config::secrets::SecretStore,
    ledger_home: &Path,
) -> Result<UninstallReport, String> {
    let mut report = UninstallReport::default();
    for dir in &plan.skill_dirs {
        if dir.exists() {
            std::fs::remove_dir_all(dir)
                .map_err(|error| format!("remove {}: {error}", dir.display()))?;
        }
        report.removed_skills.push(
            dir.file_name()
                .and_then(|n| n.to_str())
                .ok_or_else(|| format!("技能目录名非 UTF-8: {}", dir.display()))?
                .to_string(),
        );
    }
    for server in &plan.mcp_servers {
        config_mcp.servers.remove(server);
        report.removed_servers.push(server.clone());
    }
    for key in &plan.secret_keys {
        secrets.delete_mcp(key)?;
        report.removed_secrets.push(key.clone());
    }
    let mut records = installed_list(ledger_home)?;
    records.retain(|r| {
        !(r.plugin == plan.plugin && r.target == plan.target.flag())
    });
    ledger_save(
        ledger_home,
        InstalledLedger { records },
    )?;
    Ok(report)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::manifest::PluginSource;

    fn write_skill_plugin(root: &Path, name: &str, with_mcp: bool) -> PluginEntry {
        let plugin_dir = root.join("plugins").join(name);
        std::fs::create_dir_all(plugin_dir.join("skills/commit")).unwrap();
        std::fs::write(
            plugin_dir.join("skills/commit/SKILL.md"),
            "---\nname: commit\ndescription: commit helper\n---\n\nbody",
        )
        .unwrap();
        if with_mcp {
            std::fs::write(
                plugin_dir.join(".mcp.json"),
                r#"{ "mcpServers": { "context7": { "command": "npx", "args": ["-y", "@upstash/context7-mcp"], "env": { "TOKEN": "plain" } } } }"#,
            )
            .unwrap();
        }
        PluginEntry {
            name: name.to_string(),
            source: PluginSource::Path(format!("./plugins/{name}")),
            description: String::new(),
            version: String::new(),
            category: String::new(),
        }
    }

    #[test]
    fn plan_and_execute_qaqh_projection() {
        let market = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let entry = write_skill_plugin(market.path(), "commit-helper", true);

        let plan = plan(&entry, market.path(), HostTarget::Qaqh, home.path(), "fixture").unwrap();
        assert_eq!(plan.skill_copies.len(), 1);
        assert_eq!(plan.mcp_servers.len(), 1);
        assert_eq!(plan.mcp_servers[0].source, ExternalSource::Marketplace);
        assert_eq!(plan.marketplace, "fixture");

        // secrets 落临时路径——测试绝不碰真实用户 secrets 文件。
        let secrets =
            qaqh_config::secrets::SecretStore::new(home.path().join("secrets-test.toml"));
        let mut mcp = qaqh_config::config::McpConfig::default();
        let report = execute(&plan, &|_| true, &mut mcp, &secrets, home.path()).unwrap();
        assert_eq!(report.copied_skills, vec!["commit".to_string()]);
        assert_eq!(report.imported_servers, vec!["context7".to_string()]);

        let dest = home.path().join(".qaqh/skills/commit/SKILL.md");
        assert!(dest.is_file());
        assert!(mcp.servers.contains_key("context7"));
        // 账本已记录本次投影（含 secrets 键名）。
        let records = installed_list(home.path()).unwrap();
        assert_eq!(records.len(), 1);
        assert_eq!(records[0].secret_keys, vec!["mcp-context7-token".to_string()]);
    }

    #[test]
    fn codex_target_rejects_mcp_plugin() {
        let market = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let entry = write_skill_plugin(market.path(), "with-mcp", true);
        assert!(plan(&entry, market.path(), HostTarget::Codex, home.path(), "fixture").is_err());
    }

    #[test]
    fn skill_only_plugin_projects_to_codex() {
        let market = tempfile::tempdir().unwrap();
        let home = tempfile::tempdir().unwrap();
        let entry = write_skill_plugin(market.path(), "skill-only", false);
        let plan = plan(&entry, market.path(), HostTarget::Codex, home.path(), "fixture").unwrap();
        assert!(plan.mcp_servers.is_empty());
        assert_eq!(
            plan.skill_copies[0].1,
            home.path().join(".codex/skills/commit")
        );
    }
}
