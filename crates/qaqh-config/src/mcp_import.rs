//! PR-M3-2：Codex / Claude Code MCP 配置兼容（owner 决策 A+B，2026-09-07）。
//!
//! 路线 A（只读合并，[`merge_external`]）：daemon 装配时扫描**用户级**
//! 外部配置（`~/.codex/config.toml` 的 `[mcp_servers]`、`~/.claude.json`
//! 顶层 `mcpServers`、opencode `~/.config/opencode/opencode.json` 顶层
//! `mcp` 段），以 `ext-<source>-<name>` 前缀并入运行时视图——
//! **只读不回写**、明文 env 直通子进程（不落 QAQH secrets）、本地手写
//! 面碰撞时本地优先。开关 `[mcp].import_external`（默认 true）。
//!
//! 路线 B（显式导入，[`import_servers`]）：CLI 驱动（`qaqh-daemon mcp
//! import`），dry-run 默认、`--exec` 才写；项目级 `.mcp.json`（供应链面）
//! 需调用方逐 server 审批回调放行。写入 config.toml `[mcp.servers]` +
//! secrets.toml `[secrets.mcp]`（env 占位符化，D4 信任边界不扩）。
//!
//! 信任边界（D4）：只读合并只碰**用户级**文件（用户自己机器上的个人配置
//! 与手写 config 同信任级）；**项目级**文件是仓库里他人提交的内容，绝不
//! 进入只读合并——只能经显式导入 + 审批。

use std::collections::BTreeMap;
use std::path::Path;

use crate::config::{McpConfig, McpServerConfig, McpTransportKind};

/// 外部 server 来源（决定只读合并的前缀与导入器的默认审批策略）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ExternalSource {
    Codex,
    ClaudeUser,
    ClaudeProject,
    /// opencode `~/.config/opencode/opencode.json` 顶层 `mcp` 段（用户级）。
    Opencode,
    /// 插件市场安装投影（qaqh-marketplace）：来自市场插件包的 `.mcp.json`。
    /// 只走 B 路线显式导入，从不进只读合并（市场内容是第三方供应链面）。
    Marketplace,
    /// 通用 JSON 源（[`scan_generic_json`]）：更多 IDE 的声明式接入面。
    /// 前缀 `ext-generic-`；后续按已核实的用户级路径接线即可，不改扫描逻辑。
    Generic,
}

impl ExternalSource {
    /// 只读合并前缀（`ext-<src>-<name>`；符合 server 名 `[a-z0-9_-]` 校验）。
    fn merge_prefix(self) -> &'static str {
        match self {
            ExternalSource::Codex => "ext-codex-",
            ExternalSource::ClaudeUser => "ext-claude-",
            ExternalSource::ClaudeProject => "ext-claproj-",
            ExternalSource::Opencode => "ext-opencode-",
            ExternalSource::Marketplace => "ext-market-",
            ExternalSource::Generic => "ext-generic-",
        }
    }

    /// 导入器 `--from` 值。
    pub fn from_flag(self) -> &'static str {
        match self {
            ExternalSource::Codex => "codex",
            ExternalSource::ClaudeUser => "claude",
            ExternalSource::ClaudeProject => "claude-project",
            ExternalSource::Opencode => "opencode",
            ExternalSource::Marketplace => "marketplace",
            ExternalSource::Generic => "generic",
        }
    }
}

/// 外部 server 草稿（源内原样，未做 transport 推导/校验）。
#[derive(Debug, Clone)]
pub struct ExternalServer {
    /// 源内原始名（导入后原样入 [mcp.servers]；合并时加前缀）。
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    pub env: BTreeMap<String, String>,
    /// http/streamable-http 类型的 endpoint（有 `url` 即 remote，ADR-0025）。
    pub url: String,
    pub headers: BTreeMap<String, String>,
    pub source: ExternalSource,
}

impl ExternalServer {
    /// → McpServerConfig（transport 推导与 map_mcp_config 同规则：
    /// command 非空 → stdio；否则 url 非空 → http）。
    pub fn to_server_config(&self) -> Result<McpServerConfig, String> {
        let transport = match (self.command.trim().is_empty(), self.url.trim().is_empty()) {
            (false, true) => McpTransportKind::Stdio,
            (true, false) => McpTransportKind::Http,
            _ => {
                return Err(format!(
                    "external server {:?}: 需要 command（stdio）或 url（http）二者其一",
                    self.name
                ));
            }
        };
        Ok(McpServerConfig {
            transport,
            command: self.command.clone(),
            args: self.args.clone(),
            env: self.env.clone(),
            url: self.url.clone(),
            headers: self.headers.clone(),
            tools: None,
            resources_enabled: true,
            default_timeout_secs: 60,
            max_concurrent_calls: 1,
            // 外部源（Codex/Claude/opencode）无 cwd 语义：
            // Codex/Claude 无此字段；opencode 的 cwd 相对 workspace 解析，
            // daemon 侧 workspace 轴不同，直接继承会钉错目录——继承 daemon cwd。
            cwd: String::new(),
        })
    }
}

// ── 扫描 ──

/// 单一 entry 收口（ADR-0025）：所有外部源共用，**不写 per-IDE adapter**。
///
/// 判定优先级：`url`（字符串）→ remote（Streamable HTTP）；否则 `command`
/// （字符串或数组首项）→ stdio；两者皆无 → warn 后跳过（不再静默 `?` 丢弃）。
fn build_external(
    name: String,
    source: ExternalSource,
    command: Option<String>,
    args: Vec<String>,
    env: BTreeMap<String, String>,
    url: String,
    headers: BTreeMap<String, String>,
) -> Option<ExternalServer> {
    if !url.trim().is_empty() {
        return Some(ExternalServer {
            name,
            command: String::new(),
            args: vec![],
            env: BTreeMap::new(),
            url,
            headers,
            source,
        });
    }
    if let Some(command) = command.filter(|command| !command.trim().is_empty()) {
        return Some(ExternalServer {
            name,
            command,
            args,
            env,
            url: String::new(),
            headers: BTreeMap::new(),
            source,
        });
    }
    log::warn!("[mcp import] {source:?} server {name:?}: 缺 url 与 command，跳过（不再静默丢弃）");
    None
}

/// JSON 文件读取（缺文件/解析失败 → None，不是错误）。
fn read_json(path: &Path) -> Option<serde_json::Value> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return None;
    };
    match serde_json::from_str::<serde_json::Value>(&text) {
        Ok(value) => Some(value),
        Err(_) => {
            log::warn!("[mcp import] {} 解析失败，跳过 JSON 源", path.display());
            None
        }
    }
}

/// serde_json entry → [`ExternalServer`]（通用；Claude/opencode/IDE 共用）。
///
/// 有 `url` 即 remote —— 覆盖 `streamable-http`/`streamableHttp`/无 type 等
/// 新形态；`command` 支持字符串或数组（数组首项为 binary、余项为 args）；
/// `env` 与 `environment` 都认。
fn parse_json_entry(
    name: &str,
    entry: &serde_json::Value,
    source: ExternalSource,
) -> Option<ExternalServer> {
    let obj = entry.as_object()?;
    let server_type = obj.get("type").and_then(|v| v.as_str()).unwrap_or("");
    if server_type.eq_ignore_ascii_case("sse") {
        // MCP 官方已把 HTTP+SSE 标为 deprecated（2026-07-28 仅 stdio +
        // Streamable HTTP）；本期不写 SSE 转换层，明确告警而非假装能用。
        log::warn!(
            "[mcp import] {source:?} server {name:?}: 旧版 HTTP+SSE 未支持，将按 Streamable HTTP 尝试（可能连不上）"
        );
    }
    let url = obj
        .get("url")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_owned();
    let headers = string_map(obj.get("headers"));
    let (command, args) = match obj.get("command") {
        Some(value) if value.is_array() => {
            let parts: Vec<String> = value
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|part| part.as_str().map(str::to_owned))
                .collect();
            let mut iter = parts.into_iter();
            (iter.next(), iter.collect())
        }
        Some(value) if value.is_string() => (
            value.as_str().map(str::to_owned),
            obj.get("args")
                .and_then(|v| v.as_array())
                .map(|args| {
                    args.iter()
                        .filter_map(|arg| arg.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default(),
        ),
        _ => (None, Vec::new()),
    };
    let env = string_map(obj.get("env").or_else(|| obj.get("environment")));
    build_external(name.to_owned(), source, command, args, env, url, headers)
}

/// 通用键收集（单一实现，所有 JSON 源复用）。
fn collect_json_entries(
    value: &serde_json::Value,
    key: &str,
    source: ExternalSource,
    opencode_semantics: bool,
) -> Vec<ExternalServer> {
    let Some(servers) = value.get(key).and_then(|v| v.as_object()) else {
        return Vec::new();
    };
    servers
        .iter()
        .filter_map(|(name, entry)| {
            if opencode_semantics {
                if entry.get("enabled").and_then(|v| v.as_bool()) == Some(false) {
                    return None;
                }
                let server_type = entry
                    .get("type")
                    .and_then(|v| v.as_str())
                    .unwrap_or("local");
                if server_type != "local" && server_type != "remote" {
                    log::warn!(
                        "[mcp import] opencode server {name:?}: 未知 type {server_type:?}，跳过"
                    );
                    return None;
                }
            }
            parse_json_entry(name, entry, source)
        })
        .collect()
}

/// Codex CLI：`~/.codex/config.toml` 的 `[mcp_servers.<name>]`（官方
/// config-reference 确认用户级路径）。stdio 用 `command`/`args`/`env`；
/// remote 用 `url`/`headers`（通用写法，ADR-0025）。
pub fn scan_codex(path: &Path) -> Vec<ExternalServer> {
    let Ok(text) = std::fs::read_to_string(path) else {
        return Vec::new();
    };
    let Ok(value) = toml::from_str::<toml::Value>(&text) else {
        log::warn!("[mcp import] {} 解析失败，跳过 codex 源", path.display());
        return Vec::new();
    };
    let Some(servers) = value.get("mcp_servers").and_then(|v| v.as_table()) else {
        return Vec::new();
    };
    servers
        .iter()
        .filter_map(|(name, entry)| {
            let command = entry
                .get("command")
                .and_then(|v| v.as_str())
                .map(str::to_owned);
            let args = entry
                .get("args")
                .and_then(|v| v.as_array())
                .map(|args| {
                    args.iter()
                        .filter_map(|arg| arg.as_str().map(str::to_owned))
                        .collect()
                })
                .unwrap_or_default();
            let url = entry
                .get("url")
                .and_then(|v| v.as_str())
                .unwrap_or_default()
                .to_owned();
            build_external(
                name.clone(),
                ExternalSource::Codex,
                command,
                args,
                toml_string_map(entry.get("env")),
                url,
                toml_string_map(entry.get("headers")),
            )
        })
        .collect()
}

/// Claude Code：`mcpServers` 键（`~/.claude.json` 用户级 / 项目级 `.mcp.json`）。
/// 有 `url` 即 remote —— 覆盖 `streamable-http`/`streamableHttp` 等新形态，
/// 不再因未知 `type` 静默丢弃（ADR-0025）。
pub fn scan_claude(path: &Path, source: ExternalSource) -> Vec<ExternalServer> {
    let Some(value) = read_json(path) else {
        return Vec::new();
    };
    collect_json_entries(&value, "mcpServers", source, false)
}

/// opencode：`~/.config/opencode/opencode.json` 顶层 `mcp` 段（用户级）。
/// `local`/`remote` 语义与 `enabled:false` 跳过保留；entry 复用通用解析器。
pub fn scan_opencode(path: &Path) -> Vec<ExternalServer> {
    let Some(value) = read_json(path) else {
        return Vec::new();
    };
    collect_json_entries(&value, "mcp", ExternalSource::Opencode, true)
}

/// 通用 JSON 源（ADR-0025）：自动识别 `mcpServers` / `mcp` /
/// `context_servers` / `servers` 四个顶层键，entry 走同一解析器。这是
/// 「更多 IDE」的声明式接入面 —— 新 IDE 只要给出用户级文件路径即可，
/// 不需要再写一份 adapter。
///
/// 本期**不硬编码未核实的用户级路径**：官方搜索工具在当前 profile 未启用，
/// 抓到的官方文档只确认了 Codex `~/.codex/config.toml`、Zed 的
/// `context_servers` 结构，未给出 Cursor/VS Code/Windsurf/Cline 的用户级路径。
/// 本函数作为公开 API 供 B 路线/后续接线使用，并由测试覆盖。
pub fn scan_generic_json(path: &Path, source: ExternalSource) -> Vec<ExternalServer> {
    let Some(value) = read_json(path) else {
        return Vec::new();
    };
    for key in ["mcpServers", "mcp", "context_servers", "servers"] {
        if value.get(key).and_then(|v| v.as_object()).is_some() {
            return collect_json_entries(&value, key, source, false);
        }
    }
    Vec::new()
}

/// 默认用户级扫描路径组（A 路线用；文件缺失 → 空结果，不报错）。
///
/// 结构体形态：新增源只加字段，[`merge_external`] 与 service.rs 两处调用点
/// 签名稳定。`opencode` 为 `~/.config/opencode/opencode.json`。
#[derive(Debug, Clone)]
pub struct UserPaths {
    pub codex: std::path::PathBuf,
    pub claude: std::path::PathBuf,
    pub opencode: std::path::PathBuf,
}

pub fn default_user_paths() -> UserPaths {
    let home = std::env::var("USERPROFILE")
        .or_else(|_| std::env::var("HOME"))
        .unwrap_or_default();
    UserPaths {
        codex: Path::new(&home).join(".codex").join("config.toml"),
        claude: Path::new(&home).join(".claude.json"),
        opencode: Path::new(&home)
            .join(".config")
            .join("opencode")
            .join("opencode.json"),
    }
}

fn string_map(value: Option<&serde_json::Value>) -> BTreeMap<String, String> {
    value
        .and_then(|v| v.as_object())
        .map(|map| {
            map.iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_owned())))
                .collect()
        })
        .unwrap_or_default()
}

/// toml::Value 版（scan_codex 专用；env 表全部为字符串值）。
fn toml_string_map(value: Option<&toml::Value>) -> BTreeMap<String, String> {
    value
        .and_then(|v| v.as_table())
        .map(|table| {
            table
                .iter()
                .filter_map(|(k, v)| v.as_str().map(|s| (k.clone(), s.to_owned())))
                .collect()
        })
        .unwrap_or_default()
}

// ── A：只读合并 ──

/// 合并报告（daemon 启动日志 + 观测）。
#[derive(Debug, Default, PartialEq, Eq)]
pub struct MergeReport {
    pub merged: Vec<String>,
    pub skipped_collisions: Vec<String>,
    pub skipped_invalid: Vec<String>,
}

/// 用户级外部配置 → 运行时视图（A 路线；`cfg.import_external == false` →
/// 返回 None，不做扫描）。
///
/// 合并规则：`ext-<source>-<name>` 前缀避撞；与手写面同名 → **本地优先**
/// （skip 碰撞 + warn）；transport 推导失败的条目 → skip_invalid。
///
/// 参数改为 [`UserPaths`] 结构体（codex/claude/opencode 三源）：调用方一律经
/// [`default_user_paths`] 拿全量，新增源不再改签名（service.rs 两处调用点
/// 零改动）。
pub fn merge_external(cfg: &mut McpConfig, paths: &UserPaths) -> Option<MergeReport> {
    if !cfg.import_external {
        return None;
    }
    let mut report = MergeReport::default();
    let mut candidates = scan_codex(&paths.codex);
    candidates.extend(scan_claude(&paths.claude, ExternalSource::ClaudeUser));
    candidates.extend(scan_opencode(&paths.opencode));
    for external in candidates {
        let merged_name = format!("{}{}", external.source.merge_prefix(), external.name);
        if cfg.servers.contains_key(&merged_name) {
            report.skipped_collisions.push(merged_name);
            continue;
        }
        match external.to_server_config() {
            Ok(server_cfg) => {
                cfg.servers.insert(merged_name.clone(), server_cfg);
                report.merged.push(merged_name);
            }
            Err(reason) => report
                .skipped_invalid
                .push(format!("{}: {reason}", external.name)),
        }
    }
    Some(report)
}

// ── B：显式导入 ──

/// 导入报告。
#[derive(Debug, Default)]
pub struct ImportReport {
    pub imported: Vec<String>,
    pub skipped_existing: Vec<String>,
    pub rejected: Vec<String>,
}

/// 显式导入（B 路线）：逐 server 写入 config.toml `[mcp.servers]` +
/// secrets.toml `[secrets.mcp]`（env 值占位符化：`mcp-<server>-<envkey>`）。
///
/// `confirm`：审批回调（CLI 交互；用户级源默认 true，项目级必须逐个放行）。
/// 返回 false → 该 server 进 `rejected`。env 为空 map 时留空键不写（写空
/// 段会污染 secrets 文件）。
pub fn import_servers(
    servers: &[ExternalServer],
    confirm: &dyn Fn(&ExternalServer) -> bool,
    config: &mut McpConfig,
    secrets: &crate::secrets::SecretStore,
) -> Result<ImportReport, String> {
    let mut report = ImportReport::default();
    for external in servers {
        if !confirm(external) {
            report.rejected.push(external.name.clone());
            continue;
        }
        if config.servers.contains_key(&external.name) {
            report.skipped_existing.push(external.name.clone());
            continue;
        }
        let mut server_cfg = external.to_server_config()?;
        // env 占位符化：明文值进 secrets（三段式契约，DTO/save 永不见明文）。
        let mut env = BTreeMap::new();
        for (key, plaintext) in &server_cfg.env {
            let secret_name = format!("mcp-{}-{}", external.name, key.to_lowercase());
            secrets.set_mcp(&secret_name, plaintext)?;
            env.insert(key.clone(), format!("${{secret:{secret_name}}}"));
        }
        server_cfg.env = env;
        config.servers.insert(external.name.clone(), server_cfg);
        report.imported.push(external.name.clone());
    }
    // 显式导入 = 用户明确启用（Some 而非推导值），不受后续外部合并改写。
    config.enabled = Some(true);
    Ok(report)
}
