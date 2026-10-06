use crate::secrets::{CONFIG_MARKER, SecretSlot, SecretStore};
use qaqh_types::{
    ConfigStore, PersistentConfig, PersistentExecConfig, PersistentLspConfig,
    PersistentLspServerConfig, PersistentMcpConfig, PersistentMcpServerConfig,
    PersistentSubagentConfig,
};
use std::collections::HashMap; // still used by profiles
use std::sync::{Mutex, OnceLock};

/// Subagent default configuration.
///
/// These are defaults applied when spawning sub-agents. The `spawn_subagent`
/// tool schema only exposes `task_description` / `agent_name` / `context` /
/// `timeout_secs`; the remaining fields are set through settings, not per-call.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SubagentConfig {
    /// Override model. Empty = inherit from parent agent config.
    #[serde(default)]
    pub model: String,
    /// Override API base URL. Empty = inherit.
    #[serde(default)]
    pub base_url: String,
    /// Override API key. Empty = inherit.
    #[serde(default)]
    pub api_key: String,
    /// Max output tokens for subagent responses. Default: 4096.
    #[serde(default = "default_subagent_max_tokens")]
    pub max_tokens: u32,
    /// Maximum lifetime in seconds before the subagent is killed. Default: 120.
    #[serde(default = "default_subagent_timeout")]
    pub timeout_secs: u64,
    /// Default tool allowlist. Empty = all tools available.
    #[serde(default)]
    pub default_tools: Vec<String>,
    /// Maximum subagent tree depth. Default 1 (root -> child only).
    #[serde(default = "default_subagent_max_depth")]
    pub max_depth: u32,
    /// Max queued messages from one sender to one recipient. 0 = unlimited.
    #[serde(default = "default_subagent_message_in_flight")]
    pub message_in_flight_per_pair: u64,
    /// Max cumulative outbound message attempts per sender. 0 = unlimited.
    #[serde(default = "default_subagent_message_outbound")]
    pub message_outbound_per_sender: u64,
}

fn default_subagent_max_tokens() -> u32 {
    4096
}
fn default_subagent_timeout() -> u64 {
    120
}
fn default_subagent_max_depth() -> u32 {
    1
}
fn default_subagent_message_in_flight() -> u64 {
    16
}
fn default_subagent_message_outbound() -> u64 {
    1024
}

impl Default for SubagentConfig {
    fn default() -> Self {
        Self {
            model: String::new(),
            base_url: String::new(),
            api_key: String::new(),
            max_tokens: 4096,
            timeout_secs: 120,
            default_tools: vec!["read".into(), "exec".into()],
            max_depth: 1,
            message_in_flight_per_pair: 16,
            message_outbound_per_sender: 1024,
        }
    }
}

/// exec 工具默认执行配置。
#[derive(Debug, Clone, Default, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ExecConfig {
    /// 用户指定的默认 shell；`None` / 空 / "auto" = 平台优先级自动探测。
    pub default_shell: Option<String>,
}

/// Runtime agent configuration built from PersistentConfig + registry.
///
/// This is the fully-resolved config used by the agent at runtime. It combines
/// user settings from config.toml with provider registry defaults and profile
/// overrides. All fields are concrete (no Option wrapping).
#[derive(Debug, Clone)]
pub struct Config {
    /// API key for the active endpoint (never persisted in clear text).
    pub api_key: String,
    /// BYOK 的 endpoint：scheme + host + 可选前缀，不含 wire 自身路径。
    pub base_url: String,
    /// 该 endpoint 说的 wire 协议。
    pub wire: qaqh_types::Wire,
    /// 该 endpoint 与 wire 缺省的差异（请求形状开关）。
    pub compat: qaqh_types::EndpointCompat,
    /// Active model identifier.
    pub model: String,
    /// Max output tokens per turn.
    pub max_tokens: u32,
    /// 端点声明的上下文窗口（输入 token）——本地压缩的唯一分母：auto-compact 软
    /// 阈值（`context_length × auto_compact_threshold`）与发送前硬 pre-flight
    /// 都由它推得（BYOK 前的 `context_limit`/`context_window` 双口径已收敛）。
    pub context_length: u32,
    /// Reasoning effort: one of `low|medium|high|xhigh|max`, or empty (endpoint default).
    pub reasoning_effort: String,
    /// Named profiles for quick config switching.
    pub profiles: HashMap<String, qaqh_types::ProfileConfig>,
    /// Currently active profile name.
    pub active_profile: String,
    /// UI language preference.
    pub lang: Option<String>,
    /// UI font family（WinUI 壳全局字体；空 = 跟随系统默认）。
    pub font_family: String,
    /// UI 主题偏好：`system` | `light` | `dark` | `dark-gray`；`None`/空 = 跟随系统。
    pub theme: Option<String>,
    /// 桌面通知开关。`None` = 开启（缺省）。
    pub notifications_enabled: Option<bool>,
    /// Default configuration for sub-agent spawning.
    pub subagent: SubagentConfig,
    /// exec tool default shell selection.
    pub exec: ExecConfig,
    /// Whether the content filter is active.
    pub compliance_enabled: bool,
    /// Additional banned keywords for the content filter.
    pub compliance_extra_keywords: Vec<String>,
    /// Whitelisted patterns exempt from content filtering.
    pub compliance_allowlist: Vec<String>,
    /// Agent permission tier (裸 u8,wire 兼容;2026-10-03 三档制取代 L1–L4):
    /// 1 = read-only, 2 = workspace-write,
    /// 3 = skip-permissions (explicit dangerous bypass; all ordinary tools auto).
    /// 旧四档数值在 load 处经 `PermissionLevel::from_legacy_u8` 迁移。
    pub permission_level: u8,
    /// Path to a HuggingFace tokenizer.json. `None` = use heuristic fallback.
    pub tokenizer_path: Option<String>,
    /// Auto-compact threshold: fraction of context_limit (0.0-1.0).
    /// When total tokens exceed `context_limit * threshold`, compact is
    /// triggered before the next user message is processed. 0.0 disables.
    /// Default: 0.75 (compact at 75% capacity).
    pub auto_compact_threshold: f64,
    /// 空闲会话 worker 自动卸载阈值（秒）。0 = 禁用（缺省）。
    /// daemon 周期任务消费；运行时 Config 承载该值以保证 save 往返不丢
    /// 用户手写配置（save_with 从运行时 Config 全量重构 PersistentConfig）。
    pub session_idle_unload_secs: u64,
    /// MCP 客户端配置（docs/current/architecture.md；load 时已 fail-fast 校验）。
    pub mcp: McpConfig,
    /// LSP 客户端配置（docs/current/architecture.md；load 时已 fail-fast 校验）。
    pub lsp: LspConfig,
}

// ── MCP 客户端配置（docs/current/architecture.md）──

/// MCP server 传输形态。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub enum McpTransportKind {
    /// stdio 子进程（Phase 1 主形态；设计 D1/S1）。
    Stdio,
    /// streamable HTTP（M3）。
    Http,
}

/// 单个 MCP server 的运行时配置（已通过 fail-fast 校验）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct McpServerConfig {
    pub transport: McpTransportKind,
    /// stdio 启动命令（如 "npx"）；http 时为空。
    pub command: String,
    pub args: Vec<String>,
    /// 注入 server 进程的环境变量原值；`${secret:name}` 占位符由 qaqh-mcp
    /// 在启动时解析（qaqh-config 不做 secret 求值）。
    pub env: std::collections::BTreeMap<String, String>,
    /// streamable HTTP endpoint；stdio 时为空。
    pub url: String,
    pub headers: std::collections::BTreeMap<String, String>,
    /// 工具白名单（server 侧原始名，投影时加 `mcp__{server}__` 前缀）；None = 全部。
    pub tools: Option<Vec<String>>,
    pub resources_enabled: bool,
    /// 单次工具调用默认超时（秒）；1..=3600。
    pub default_timeout_secs: u64,
    /// per-server 并发上限；1..=16。
    pub max_concurrent_calls: u32,
    /// stdio 子进程工作目录；空 = 继承 daemon 进程 cwd（path-sensitive
    /// server，如按 cwd 推导 root 的索引服务，用此字段钉住工作区）。
    /// map 层 trim 后入库；http server 上忽略。
    pub cwd: String,
}

/// MCP 客户端运行时配置。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct McpConfig {
    pub enabled: bool,
    /// idle 回收阈值（秒）；0 = 常驻不回收。判定口径：`inflight == 0` 连续该时长。
    pub idle_shutdown_secs: u64,
    /// server 名（已校验 `[a-z0-9_-]+`）→ 配置。
    pub servers: std::collections::BTreeMap<String, McpServerConfig>,
    /// PR-M3-2：用户级外部 MCP 配置只读合并（Codex/Claude Code，A 路线）。
    /// 默认开启；关闭后仅认 [mcp.servers] 手写面。
    #[serde(default = "default_import_external")]
    pub import_external: bool,
    /// 是否把 MCP 资源清单注入历史（trailing developer 消息）。缺省 **false**：
    /// 清单属于「环境能力」而非「对话事实」，注入会随清单变化追加历史并打断
    /// prefix cache；模型按需调 `mcp list_resources` 拿到的是同一份本地缓存，
    /// 且不受注入侧的 20 条封顶。开启仅用于调试/兼容。
    #[serde(default)]
    pub inject_resource_env_block: bool,
}

fn default_import_external() -> bool {
    true
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            idle_shutdown_secs: 300,
            servers: std::collections::BTreeMap::new(),
            import_external: default_import_external(),
            inject_resource_env_block: false,
        }
    }
}

// ── LSP 客户端配置（docs/current/architecture.md；L1–L6 决策）──

/// 单个 LSP server 的运行时配置（已通过 fail-fast 校验；stdio 唯一形态）。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LspServerConfig {
    /// stdio 启动命令（如 "rust-analyzer"）。
    pub command: String,
    pub args: Vec<String>,
    /// 注入 server 进程的环境变量原值；`${secret:name}` 占位符由 qaqh-lsp
    /// 在启动时解析（qaqh-config 不做 secret 求值；secret 段复用 `[secrets.mcp]`）。
    pub env: std::collections::BTreeMap<String, String>,
    /// 路由键：该 server 负责的文件扩展名（无点小写）；空 = 不参与路由。
    pub extensions: Vec<String>,
    /// 启动超时（秒；含进程拉起 + initialize + 索引门）；1..=600。
    pub startup_timeout_secs: u64,
    /// 单次 LSP 请求默认超时（秒）；1..=3600。
    pub default_timeout_secs: u64,
}

/// LSP 客户端运行时配置。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct LspConfig {
    pub enabled: bool,
    /// idle 回收阈值（秒）；0 = 常驻不回收。判定口径与 mcp 同款。
    pub idle_shutdown_secs: u64,
    /// server 名（已校验 `[a-z0-9_-]+`）→ 配置。
    pub servers: std::collections::BTreeMap<String, LspServerConfig>,
}

impl Default for LspConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            idle_shutdown_secs: 120,
            servers: std::collections::BTreeMap::new(),
        }
    }
}

/// PersistentMcpConfig → 运行时 [`McpConfig`]，含 fail-fast 校验。
///
/// 校验规则（设计 §6 + PLAN PR-M1-1）：
/// - server 名非空、仅 `[a-z0-9_-]`、≤64 字符；
/// - stdio/http 互斥：stdio 必须有非空 command 且无 url；http 必须有
///   `http(s)://` url 且无 command；
/// - tools 白名单条目非空；
/// - `default_timeout_secs` ∈ 1..=3600（越界即错，不做静默 clamp——静默修正
///   会掩盖配置错误）；
/// - `max_concurrent_calls` ∈ 1..=16（T-8-1 / O-4：上限从 64 收紧，
///   单 server 并发调用是 MCP 侧的资源放大面，16 为安全上限）。
///
/// 注意：TOML 层面的重复表（`[mcp.servers.x]` 写两次）由 toml 解析器拒绝，
/// 走 `ConfigStore::load` 返回 None → 整体回退默认的既有语义（见 PLAN §8）。
pub(crate) fn map_mcp_config(
    mcp: Option<qaqh_types::PersistentMcpConfig>,
) -> Result<McpConfig, String> {
    let Some(mcp) = mcp else {
        return Ok(McpConfig::default());
    };
    let mut servers = std::collections::BTreeMap::new();
    for (name, ps) in mcp.servers.unwrap_or_default() {
        validate_server_name(&name)?;
        let command = ps.command.unwrap_or_default();
        let url = ps.url.unwrap_or_default();
        let transport = match (command.trim().is_empty(), url.trim().is_empty()) {
            (false, true) => McpTransportKind::Stdio,
            (true, false) => {
                if !(url.starts_with("http://")
                    || url.starts_with("https://")
                    // PR-P2-2：unix domain socket（本机长驻 server）；
                    // 形如 unix:///path/to.sock——adapter 分发到 rmcp
                    // unix-socket client（仅 unix 平台）。
                    || url.starts_with("unix://"))
                {
                    return Err(format!(
                        "[mcp] server {name:?}: url 必须以 http:// 或 https:// 或 unix:// 开头"
                    ));
                }
                McpTransportKind::Http
            }
            (false, false) => {
                return Err(format!(
                    "[mcp] server {name:?}: command 与 url 互斥，只能配置其一"
                ));
            }
            (true, true) => {
                return Err(format!(
                    "[mcp] server {name:?}: 缺少 transport 配置——stdio 需要 command，http 需要 url"
                ));
            }
        };
        if let Some(tools) = &ps.tools
            && tools.iter().any(|t| t.trim().is_empty())
        {
            return Err(format!("[mcp] server {name:?}: tools 白名单包含空条目"));
        }
        let default_timeout_secs = match ps.default_timeout_secs {
            None => 60,
            Some(0) => {
                return Err(format!(
                    "[mcp] server {name:?}: default_timeout_secs 必须在 1..=3600（得到 0）"
                ));
            }
            Some(n) if n > 3600 => {
                return Err(format!(
                    "[mcp] server {name:?}: default_timeout_secs 必须在 1..=3600（得到 {n}）"
                ));
            }
            Some(n) => n,
        };
        let max_concurrent_calls = match ps.max_concurrent_calls {
            None => 1,
            Some(0) => {
                return Err(format!(
                    "[mcp] server {name:?}: max_concurrent_calls 必须在 1..=16（得到 0）"
                ));
            }
            Some(n) if n > 16 => {
                return Err(format!(
                    "[mcp] server {name:?}: max_concurrent_calls 必须在 1..=16（得到 {n}）"
                ));
            }
            Some(n) => n,
        };
        servers.insert(
            name,
            McpServerConfig {
                transport,
                command,
                args: ps.args.unwrap_or_default(),
                env: ps.env.unwrap_or_default().into_iter().collect(),
                url,
                headers: ps.headers.unwrap_or_default().into_iter().collect(),
                tools: ps
                    .tools
                    .map(|tools| tools.into_iter().map(|t| t.trim().to_owned()).collect()),
                resources_enabled: ps.resources.unwrap_or(true),
                default_timeout_secs,
                max_concurrent_calls,
                cwd: ps.cwd.unwrap_or_default().trim().to_owned(),
            },
        );
    }
    Ok(McpConfig {
        enabled: mcp.enabled.unwrap_or(!servers.is_empty()),
        idle_shutdown_secs: mcp.idle_shutdown_secs.unwrap_or(300),
        servers,
        import_external: mcp.import_external.unwrap_or(true),
        inject_resource_env_block: mcp.inject_resource_env_block.unwrap_or(false),
    })
}

/// PersistentLspConfig → 运行时 [`LspConfig`]，含 fail-fast 校验。
///
/// 校验规则（docs/current/architecture.md）：
/// - server 名非空、仅 `[a-z0-9_-]`、≤64 字符（与 mcp 同规）；
/// - command 非空（stdio 唯一形态）；
/// - extensions 条目：去点转小写后非空（空条目即错，不静默丢弃）；
/// - `startup_timeout_secs` 缺省 30，∈ 1..=600；
/// - `default_timeout_secs` 缺省 30，∈ 1..=3600。
pub(crate) fn map_lsp_config(
    lsp: Option<qaqh_types::PersistentLspConfig>,
) -> Result<LspConfig, String> {
    let Some(lsp) = lsp else {
        return Ok(LspConfig::default());
    };
    let mut servers = std::collections::BTreeMap::new();
    for (name, ps) in lsp.servers.unwrap_or_default() {
        validate_server_name(&name)?;
        let command = ps.command.unwrap_or_default();
        if command.trim().is_empty() {
            return Err(format!(
                "[lsp] server {name:?}: 缺少 command（stdio 需要启动命令）"
            ));
        }
        let mut extensions = Vec::new();
        for ext in ps.extensions.unwrap_or_default() {
            let normalized = ext.trim().trim_start_matches('.').to_ascii_lowercase();
            if normalized.is_empty() {
                return Err(format!("[lsp] server {name:?}: extensions 包含空条目"));
            }
            if !extensions.iter().any(|e| e == &normalized) {
                extensions.push(normalized);
            }
        }
        let startup_timeout_secs = match ps.startup_timeout_secs {
            None => 30,
            Some(0) => {
                return Err(format!(
                    "[lsp] server {name:?}: startup_timeout_secs 必须在 1..=600（得到 0）"
                ));
            }
            Some(n) if n > 600 => {
                return Err(format!(
                    "[lsp] server {name:?}: startup_timeout_secs 必须在 1..=600（得到 {n}）"
                ));
            }
            Some(n) => n,
        };
        let default_timeout_secs = match ps.default_timeout_secs {
            None => 30,
            Some(0) => {
                return Err(format!(
                    "[lsp] server {name:?}: default_timeout_secs 必须在 1..=3600（得到 0）"
                ));
            }
            Some(n) if n > 3600 => {
                return Err(format!(
                    "[lsp] server {name:?}: default_timeout_secs 必须在 1..=3600（得到 {n}）"
                ));
            }
            Some(n) => n,
        };
        servers.insert(
            name,
            LspServerConfig {
                command,
                args: ps.args.unwrap_or_default(),
                env: ps.env.unwrap_or_default().into_iter().collect(),
                extensions,
                startup_timeout_secs,
                default_timeout_secs,
            },
        );
    }
    Ok(LspConfig {
        enabled: lsp.enabled.unwrap_or(!servers.is_empty()),
        idle_shutdown_secs: lsp.idle_shutdown_secs.unwrap_or(120),
        servers,
    })
}

// ── `${secret:name}` 占位符（设计 §6/E-4）──
//
// 职责边界（M1-3 定稿）：
// - **load 时只校验不解析**：扫出占位符名 → `SecretStore::has_mcp`，缺失即
//   Err（fail-fast，防拼写错拖到首次调用才爆）；`Config.mcp` 中的 env/args
//   /headers 始终保留占位符原值 —— DTO（webUI）与 save 回写永不接触明文；
// - **解析在 qaqh-mcp 连接时**（子进程 env 组装前），见 adapter.rs；
// - 名字字符集与 server 名同规（`[a-z0-9_-]`），残缺/非法占位符一律 fail-fast。

/// 扫描 `value` 中的 `${secret:name}` 占位符，返回按出现顺序去重的名字。
/// 残缺（未闭合）/空名/非法字符 → Err（fail-fast）。
pub fn secret_placeholder_names(value: &str) -> Result<Vec<String>, String> {
    const PREFIX: &str = "${secret:";
    let mut names = Vec::new();
    let mut rest = value;
    while let Some(start) = rest.find(PREFIX) {
        // 工作区红线 string_slice=deny：一律用 `get`（Option，无 panic 面）。
        // PREFIX 为 ASCII，start+PREFIX.len() 必在字符边界；防御式处理仍保留。
        let Some(after) = rest.get(start + PREFIX.len()..) else {
            return Err(format!("占位符位置非字符边界：{value:?}"));
        };
        let Some(end) = after.find('}') else {
            return Err(format!("占位符未闭合：{value:?} 中的 {PREFIX}…"));
        };
        let Some(name) = after.get(..end) else {
            return Err(format!("占位符名位置非字符边界：{value:?}"));
        };
        if name.is_empty()
            || !name
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
        {
            return Err(format!(
                "占位符名非法（仅允许 [a-z0-9_-]）：{value:?} 中的 {PREFIX}{name}}}"
            ));
        }
        if !names.iter().any(|n| n == name) {
            names.push(name.to_owned());
        }
        let Some(tail) = after.get(end + 1..) else {
            return Err(format!("占位符尾部非字符边界：{value:?}"));
        };
        rest = tail;
    }
    Ok(names)
}

/// 把 `value` 中所有 `${secret:name}` 替换为 `resolve(name)` 的结果。
/// 解析失败（未注册/解密失败）→ Err，报错只含名字不含值（E-6 红线）。
/// 由 qaqh-mcp 在连接时调用；qaqh-config 的 load/save 不调用本函数。
pub fn interpolate_secret_placeholders<F>(value: &str, resolve: F) -> Result<String, String>
where
    F: Fn(&str) -> Option<String>,
{
    let names = secret_placeholder_names(value)?;
    if names.is_empty() {
        return Ok(value.to_owned());
    }
    let mut resolved = std::collections::BTreeMap::new();
    for name in &names {
        let Some(value) = resolve(name) else {
            return Err(format!("secret {name:?} 不可用（未注册或解密失败）"));
        };
        resolved.insert(name.clone(), value);
    }
    let mut out = value.to_owned();
    for (name, secret) in &resolved {
        let placeholder = format!("${{secret:{name}}}");
        out = out.replace(&placeholder, secret);
    }
    Ok(out)
}

/// 启动校验：扫 server 配置里所有可能携带占位符的字符串字段，
/// 引用的 secret 名必须在 store 中已注册（E-4：缺 key 不静默）。
fn validate_mcp_secret_refs(mcp: &McpConfig, secrets: &SecretStore) -> Result<(), String> {
    for (name, server) in &mcp.servers {
        let available = || secrets.list_mcp();
        let check = |field: &str, value: &str| -> Result<(), String> {
            for secret_name in secret_placeholder_names(value)? {
                if !secrets.has_mcp(&secret_name) {
                    return Err(format!(
                        "[mcp] server {name:?} {field}: 引用的 secret {secret_name:?} 未注册；已注册：{:?}",
                        available()
                    ));
                }
            }
            Ok(())
        };
        for (key, value) in &server.env {
            check(&format!("env[{key}]"), value)?;
        }
        for (key, value) in &server.headers {
            check(&format!("headers[{key}]"), value)?;
        }
        for (index, arg) in server.args.iter().enumerate() {
            check(&format!("args[{index}]"), arg)?;
        }
    }
    Ok(())
}

/// LSP 启动校验：env/args 里的 `${secret:name}` 必须在 `[secrets.mcp]` 已注册
/// （与 mcp 同段复用：LSP 也是本地子进程 env 注入，同信任面）。
fn validate_lsp_secret_refs(lsp: &LspConfig, secrets: &SecretStore) -> Result<(), String> {
    for (name, server) in &lsp.servers {
        let available = || secrets.list_mcp();
        let check = |field: &str, value: &str| -> Result<(), String> {
            for secret_name in secret_placeholder_names(value)? {
                if !secrets.has_mcp(&secret_name) {
                    return Err(format!(
                        "[lsp] server {name:?} {field}: 引用的 secret {secret_name:?} 未注册；已注册：{:?}",
                        available()
                    ));
                }
            }
            Ok(())
        };
        for (key, value) in &server.env {
            check(&format!("env[{key}]"), value)?;
        }
        for (index, arg) in server.args.iter().enumerate() {
            check(&format!("args[{index}]"), arg)?;
        }
    }
    Ok(())
}

/// server 名校验：非空、仅 `[a-z0-9_-]`、≤64 字符。
fn validate_server_name(name: &str) -> Result<(), String> {
    if name.is_empty() || name.len() > 64 {
        return Err(format!("[mcp] server 名长度必须 1..=64（得到 {name:?}）"));
    }
    if !name
        .chars()
        .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_' || c == '-')
    {
        return Err(format!(
            "[mcp] server 名 {name:?} 含非法字符：仅允许小写字母、数字、'_'、'-'"
        ));
    }
    Ok(())
}

/// BYOK 未声明上下文窗口时的保守分母：宁可早压缩，也不把超限推给上游换 400。
pub const DEFAULT_CONTEXT_LENGTH: u32 = 128_000;

/// 首次启动落盘的预设配置（带注释，可直接编辑）。
///
/// 存在的前提是"配置文件不在"（`load` 只在 `!store.exists()` 时写），因此损坏的
/// 用户文件绝不会被它覆盖。示例端点用的是 DeepSeek 的 OpenAI 兼容端点——换成自己的
/// 服务商只改 `base_url` / `wire` / `model` / `context_length` 四项；这里**不含任何
/// 密钥**（api key 走设置页 → `secrets.toml`，本文件只留 `api_key = "set"` 标记）。
const FIRST_RUN_CONFIG: &str = r#"# QAQ-Harness 配置（首次启动自动生成，改这份即可）。
#
# BYOK：一个端点只需要六个字段——
#   1. endpoint        → base_url       （scheme + host + 可选前缀，不含协议自身路径）
#   2. wire            → 协议            （openai | responses | anthropic）
#   3. apikey          → 设置页填一次，密文进同目录 secrets.toml（这里只留 api_key = "set"）
#   4. model           → model
#   5. max_token       → max_tokens     （单次回复上限）
#   6. context_length  → 端点声明的上下文窗口，也是本地压缩的唯一分母
#
# 下面是一份可直接改写的起点（DeepSeek 的 OpenAI 兼容端点）。换服务商时改
# base_url / wire / model / context_length 四项即可；要多几个端点就整段复制
# [profiles.<名字>]，切换走设置页的 profile。

active_profile = "default"

[profiles.default]
model = "deepseek-chat"
max_tokens = 16384
effort = "high"
context_length = 128000
base_url = "https://api.deepseek.com"
wire = "openai"

# 可选：只有该端点与 wire 缺省语义不一致时才需要写；整段删掉即全用缺省。
# 这些字段描述的是"这一个端点的请求形状"，gate 照原样转发，不做厂商名判断。
# [profiles.default.compat]
# path = "/v1/chat/completions"           # 覆写 wire 的规范路径
# thinking_mode = "OpenAi"                # OpenAi | QwenEnableThinking | MiniMaxAdaptive
# cache_field = "PromptCacheHitTokens"    # PromptCacheHitTokens | PromptDetailsCached | UsageCachedTokens | None
# include_stream_usage = true             # 请求末帧 usage（部分兼容端点不接受）
# supports_thinking = false
# thinking_budget_large = true            # Anthropic 大上下文 thinking 预算档（16k-96k）
# supports_reasoning_effort = false
# effort_allowlist = ["high", "max"]      # 稀疏档位白名单（路由器常见）
# tool_call_content_null = true
# supports_reasoning_content = false
# require_provider_parameters = true
# do_sample = false
# user_id_mode = "Body"
# responses_web_search = false
# responses_effort_max = "xhigh"          # reasoning.effort 上限（超出即钳制）
# responses_search_function_alias = "web_search"
# supports_image_tool = true              # 打开后模型才会看到 read_image 工具
# image_models = ["vision-*"]             # 逐模型视觉白名单（`*` 后缀 = 前缀匹配）
# [profiles.default.compat.retry]         # 不写 = 统一传输层缺省（5 次 / 1s / 30s / 空闲 300s）
# max_retries = 8
# idle_timeout_secs = 600
"#;

/// 旧形状 profile → BYOK 记录。
///
/// 判据是 `endpoint` 键（预设内的 endpoint id）——BYOK 记录不会写出这个键，
/// 而迁移前的保存路径每次都写。命中即按 BYOK 前语义从迁移表补 `wire` /
/// `base_url` / `compat`，并把 `context_limit` + `context_window` 双口径折叠进
/// `context_length`。返回是否发生了迁移，调用方据此决定要不要重写磁盘。
fn migrate_profile(
    mut profile: qaqh_types::ProfileConfig,
    active_preset: Option<&crate::registry::LegacyPreset>,
    legacy_provider_id: &str,
) -> (qaqh_types::ProfileConfig, bool) {
    let mut migrated = false;
    if let Some(endpoint_id) = profile
        .preset_endpoint
        .take()
        .filter(|endpoint| !endpoint.is_empty())
    {
        migrated = true;
        match crate::registry::legacy_preset(legacy_provider_id, &endpoint_id)
            .or_else(|| active_preset.cloned())
        {
            Some(preset) => {
                profile.wire = preset.wire;
                if profile.base_url.is_empty() {
                    profile.base_url = preset.base_url;
                }
                if profile.model.is_empty() && !preset.model.is_empty() {
                    profile.model = preset.model;
                }
                if profile.compat.is_none() {
                    profile.compat = Some(preset.compat);
                }
            }
            None => log::warn!(
                "[config] {legacy_provider_id}/{endpoint_id} 不在 BYOK 迁移表内，\
                 按 wire 缺省继续（用户已保存的值不受影响）"
            ),
        }
    }
    let folded = profile.resolved_context_length(DEFAULT_CONTEXT_LENGTH);
    if profile.context_length != folded {
        profile.context_length = folded;
        migrated = true;
    }
    // 双口径旧键已并入 context_length，内存里也不再保留。
    migrated |= profile.context_limit.take().is_some();
    migrated |= profile.context_window.take().is_some();
    (profile, migrated)
}

impl Default for Config {
    fn default() -> Self {
        // BYOK：没有内置默认端点。endpoint/model/api_key 为空即"未配置"，
        // 由设置面填入——曾经 deepseek 预设兜底是最后一处运营商特判。
        let profile = qaqh_types::ProfileConfig {
            model: String::new(),
            max_tokens: 16384,
            effort: Some("high".into()),
            context_length: DEFAULT_CONTEXT_LENGTH,
            base_url: String::new(),
            wire: qaqh_types::Wire::default(),
            compat: None,
            preset_endpoint: None,
            context_limit: None,
            context_window: None,
        };
        let wire = profile.wire;
        let compat = profile.compat_or_default();
        let mut profiles = HashMap::new();
        profiles.insert("default".into(), profile);

        Self {
            api_key: String::new(),
            base_url: String::new(),
            wire,
            compat,
            model: String::new(),
            max_tokens: 16384,
            context_length: DEFAULT_CONTEXT_LENGTH,
            reasoning_effort: "high".into(),
            profiles,
            active_profile: "default".into(),
            lang: None,
            font_family: String::new(),
            theme: None,
            notifications_enabled: None,
            subagent: SubagentConfig::default(),
            exec: ExecConfig::default(),
            compliance_enabled: true,
            compliance_extra_keywords: Vec::new(),
            compliance_allowlist: Vec::new(),
            permission_level: 2, // workspace-write — safe autonomous default
            tokenizer_path: None,
            auto_compact_threshold: 0.75,
            session_idle_unload_secs: 0,
            mcp: McpConfig::default(),
            lsp: LspConfig::default(),
        }
    }
}

impl Config {
    /// Serializes all process-local config read-modify-write cycles.
    ///
    /// `ConfigStore` already uses atomic rename for corruption safety, but
    /// concurrent daemon actions (config.save + set_permission_level + profile
    /// actions) used to perform independent load→mutate→save transactions and
    /// could overwrite each other. Every public load/save/update now goes
    /// through this lock.
    fn config_io_lock() -> &'static Mutex<()> {
        static LOCK: OnceLock<Mutex<()>> = OnceLock::new();
        LOCK.get_or_init(|| Mutex::new(()))
    }

    /// Load config from disk (TOML primary store).
    pub fn load() -> Result<Self, String> {
        let _guard = Self::config_io_lock()
            .lock()
            .map_err(|_| "config lock poisoned".to_string())?;
        Self::load_unlocked()
    }

    fn load_unlocked() -> Result<Self, String> {
        let store = ConfigStore::default_location();
        Self::load_from_paths_with(store, SecretStore::default_location())
    }

    /// The single config write port (BUG-008 / roadmap 刀6).
    ///
    /// Loads the current config, applies `mutate`, and persists it in one
    /// locked transaction. If `mutate` returns `Err`, nothing is written and
    /// the error is propagated. Daemon actions must mutate config exclusively
    /// through this method instead of `load()` + direct field writes + `save()`.
    pub fn update<F>(mutate: F) -> Result<Self, String>
    where
        F: FnOnce(&mut Self) -> Result<(), String>,
    {
        let _guard = Self::config_io_lock()
            .lock()
            .map_err(|_| "config lock poisoned".to_string())?;
        let mut config = Self::load_unlocked()?;
        mutate(&mut config)?;
        config.save_unlocked()?;
        // P2-D1：磁盘先落盘，再广播内存快照（消费者永远读到已持久化状态）。
        crate::watch::publish(std::sync::Arc::new(config.clone()));
        Ok(config)
    }

    /// 从显式路径加载（公开给集成测试与多实例/数据根重定向场景；
    /// 生产路径走 [`Config::load`] 的全局 platform 路径）。
    pub fn load_from_paths_with(store: ConfigStore, secrets: SecretStore) -> Result<Self, String> {
        let mut cfg = Self::default();

        let mut pc = store.load();
        // 首次启动（文件不存在，而非解析失败）：落一份带注释的 BYOK 预设，
        // 让"配置在哪、要填哪六项"在磁盘上自解释，而不是静默跑在内存缺省值上。
        // 按 `exists()` 二次判定：损坏的用户文件走不到这里，绝不被模板覆盖。
        if pc.is_none() && !store.exists() {
            if store.write_content(FIRST_RUN_CONFIG) {
                log::info!(
                    "[config] 首次运行：已写入预设配置 {}",
                    store.path().display()
                );
                pc = store.load();
            }
        }

        let mut needs_rewrite = false;
        // 审计 P0-1：API key 不落 config.toml 明文。
        // - 标记 "set" → 从 secrets.toml 解密（失败 = 无 key，绝不回退旧明文）；
        // - 旧明文（升级前配置）→ 迁移入 secrets.toml，config.toml 写回标记；
        // - None/空 → 未配置。

        if let Some(mut pc) = pc {
            // P1-C3：扁平模型族六字段退役检测——存在即触发一次性迁移写回
            // （值固化进 profile 后从顶层剥离，见下方 needs_rewrite 块）。
            let legacy_flat_fields = pc.model.is_some()
                || pc.base_url.is_some()
                || pc.max_tokens.is_some()
                || pc.context_limit.is_some()
                || pc.endpoint.is_some()
                || pc.reasoning_effort.is_some();
            needs_rewrite |= legacy_flat_fields;
            // ── BYOK 迁移：旧 `(provider_id, endpoint)` 坐标 → 具体端点记录 ──
            // 预设不再是可选项，只在这里被读一次：把 wire / base_url / compat 补进
            // 老配置，使升级后请求形状不变。迁移结果随 needs_rewrite 落盘一次，
            // 之后配置文件里不再有 provider_id / endpoint 键。
            let legacy_provider_id = pc.provider_id.clone().unwrap_or_default();
            let legacy_endpoint_id = pc
                .endpoint
                .clone()
                .filter(|ep| !ep.is_empty())
                .or_else(|| crate::registry::legacy_first_endpoint(&legacy_provider_id))
                .unwrap_or_default();
            let legacy = crate::registry::legacy_preset(&legacy_provider_id, &legacy_endpoint_id);

            // 顶层六字段的老形态：wire/compat 取自预设；用户已保存的值（含自定义
            // URL）绝不覆盖——空值才兜底。
            if let Some(ref preset) = legacy {
                cfg.wire = preset.wire;
                cfg.compat = preset.compat.clone();
                if pc.base_url.as_deref().is_none_or(|u| u.is_empty())
                    && !preset.base_url.is_empty()
                {
                    cfg.base_url = preset.base_url.clone();
                }
                if pc.model.as_deref().is_none_or(|m| m.is_empty()) && !preset.model.is_empty() {
                    cfg.model = preset.model.clone();
                }
            }

            // profile 是持久化真相：逐条迁移后接管 cfg 的端点族字段。
            if let Some(profiles) = pc.profiles {
                for (name, profile) in profiles {
                    let (profile, changed) =
                        migrate_profile(profile, legacy.as_ref(), &legacy_provider_id);
                    needs_rewrite |= changed;
                    cfg.profiles.insert(name, profile);
                }
            }
            if let Some(ref active) = pc.active_profile {
                cfg.active_profile = active.clone();
                if let Some(profile) = cfg.profiles.get(active) {
                    cfg.model = profile.model.clone();
                    cfg.max_tokens = profile.max_tokens;
                    cfg.reasoning_effort = profile.effort.clone().unwrap_or_else(|| "high".into());
                    cfg.context_length = profile.context_length;
                    cfg.base_url = profile.base_url.clone();
                    cfg.wire = profile.wire;
                    cfg.compat = profile.compat_or_default();
                }
            }
            if let Some(k) = pc.api_key
                && !k.is_empty()
            {
                if k == CONFIG_MARKER {
                    cfg.api_key = secrets.load(SecretSlot::Main).unwrap_or_default();
                } else {
                    match secrets.set(SecretSlot::Main, &k) {
                        Ok(()) => needs_rewrite = true,
                        Err(e) => log::warn!(
                            "[config] migrate main api key to secret store failed: {e}; keeping plaintext until retry"
                        ),
                    }
                    cfg.api_key = k;
                }
            }
            if let Some(m) = pc.model
                && !m.is_empty()
            {
                cfg.model = m;
            }
            // User base_url override: 用户显式保存的值（含自定义 URL）无条件优先。
            // 预设仅在配置文件为空（base_url 缺失）时兜底，用户修改后不再预设。
            if let Some(ref u) = pc.base_url
                && !u.is_empty()
            {
                cfg.base_url = u.clone();
            }
            if let Some(mt) = pc.max_tokens {
                cfg.max_tokens = mt;
            }
            if let Some(cl) = pc.context_limit {
                cfg.context_length = cl;
            }
            if let Some(ref l) = pc.lang
                && !l.is_empty()
            {
                cfg.lang = Some(l.clone());
            }
            // ── UI 字体（空 = 跟随系统默认）──
            if let Some(ref f) = pc.font_family
                && !f.is_empty()
            {
                cfg.font_family = f.clone();
            }
            // ── UI 主题（空/缺失 = 跟随系统）──
            match pc.theme.as_deref() {
                Some(theme) if !theme.is_empty() => cfg.theme = Some(theme.to_string()),
                _ => cfg.theme = None,
            }
            // ── 桌面通知（缺失 = 开启）──
            if let Some(enabled) = pc.notifications_enabled {
                cfg.notifications_enabled = Some(enabled);
            }
            // ── Subagent defaults ──
            if let Some(ref mut s) = pc.subagent {
                if let Some(ref m) = s.model
                    && !m.is_empty()
                {
                    cfg.subagent.model = m.clone();
                }
                if let Some(ref u) = s.base_url
                    && !u.is_empty()
                {
                    cfg.subagent.base_url = u.clone();
                }
                if let Some(k) = s.api_key.clone()
                    && !k.is_empty()
                {
                    if k == CONFIG_MARKER {
                        cfg.subagent.api_key =
                            secrets.load(SecretSlot::Subagent).unwrap_or_default();
                    } else {
                        match secrets.set(SecretSlot::Subagent, &k) {
                            Ok(()) => needs_rewrite = true,
                            Err(e) => log::warn!(
                                "[config] migrate subagent api key to secret store failed: {e}; keeping plaintext until retry"
                            ),
                        }
                        cfg.subagent.api_key = k;
                    }
                }
                if let Some(mt) = s.max_tokens {
                    cfg.subagent.max_tokens = mt;
                }
                if let Some(ts) = s.timeout_secs {
                    cfg.subagent.timeout_secs = ts;
                }
                if let Some(ref tools) = s.default_tools {
                    // 存量迁移：工具改名后（file→read、read_file→read、
                    // edit_file_v2→edit）旧配置持久化的
                    // 默认白名单仍指向旧名；加载时归一化为当前正式词汇表，
                    // 否则 apply_init 会静默剔除旧名，子代理丢失读文件能力。
                    cfg.subagent.default_tools = tools
                        .iter()
                        .map(|t| match t.as_str() {
                            "file" | "read_file" => "read",
                            "edit_file_v2" => "edit",
                            _ => t,
                        })
                        .map(String::from)
                        .collect();
                }
                if let Some(depth) = s.max_depth {
                    cfg.subagent.max_depth = depth;
                }
                if let Some(limit) = s.message_in_flight_per_pair {
                    cfg.subagent.message_in_flight_per_pair = limit;
                }
                if let Some(limit) = s.message_outbound_per_sender {
                    cfg.subagent.message_outbound_per_sender = limit;
                }
            }

            // ── exec defaults ──
            if let Some(exec) = pc.exec {
                cfg.exec.default_shell = exec.default_shell.and_then(|shell| {
                    let shell = shell.trim().to_ascii_lowercase();
                    (!shell.is_empty() && shell != "auto").then_some(shell)
                });
            }

            // ── Compliance ──
            if let Some(enabled) = pc.compliance_enabled {
                cfg.compliance_enabled = enabled;
            }
            if let Some(ref keywords) = pc.compliance_extra_keywords {
                cfg.compliance_extra_keywords = keywords.clone();
            }
            if let Some(ref allowlist) = pc.compliance_allowlist {
                cfg.compliance_allowlist = allowlist.clone();
            }

            // ── Multimodal (vision) ──
            // 2026-08 移除：外挂视觉模型配置已废弃（read_image 工具直接走
            // 主模型视觉输入）。旧 config.toml 中的 [multimodal] 段被
            // serde 忽略；残留的 secrets.toml multimodal 槽位不再读取。

            // ── Permission ──
            // 2026-10-03 三档制(read-only / workspace-write / skip-permissions)
            // 取代旧 L1–L4。落盘键为 `permission_tier`(严格 1..=3);旧键
            // `permission_level`(L1–L4)仅 load 时迁移——数字 3 的新旧语义不同
            // (旧 WorkspaceFree ≈ 新 workspace-write),不能按数字直读,否则
            // 旧配置升级后越权。无法识别的值 fail-closed 收敛到最严档 1
            // (read-only),绝不反向放大权限(BUG-2026-09-13-15)。仅打日志。
            if let Some(tier) = pc.permission_tier {
                match qaqh_policy::PermissionLevel::try_from_u8(tier) {
                    Ok(t) => cfg.permission_level = t.to_u8(),
                    Err(_) => {
                        log::warn!(
                            "[config] invalid permission_tier {tier} in config.toml (must be 1-3); \
falling back to 1 (read-only)"
                        );
                        cfg.permission_level = 1;
                    }
                }
            } else if let Some(pl) = pc.permission_level {
                match qaqh_policy::PermissionLevel::from_legacy_u8(pl) {
                    Some(tier) => cfg.permission_level = tier.to_u8(),
                    None => {
                        log::warn!(
                            "[config] invalid permission_level {pl} in config.toml; \
falling back to 1 (read-only)"
                        );
                        cfg.permission_level = 1;
                    }
                }
            }

            // ── Tokenizer ──
            if let Some(ref tp) = pc.tokenizer_path {
                cfg.tokenizer_path = Some(tp.clone());
            }

            // ── Auto-compact ──
            if let Some(act) = pc.auto_compact_threshold {
                cfg.auto_compact_threshold = act;
            }

            // ── Idle unload（daemon 级会话策略；0 = 禁用）──
            if let Some(v) = pc.session_idle_unload_secs {
                cfg.session_idle_unload_secs = v;
            }

            // ── MCP 客户端（fail-fast：非法名/互斥冲突/缺 command 直接 load 失败）──
            cfg.mcp = map_mcp_config(pc.mcp.clone())?;
            // E-4 fail-fast：`${secret:name}` 引用的名字必须在 secrets.toml
            // 已注册（拼错名启动即报，不拖到首次调用；DTO/save 永不见明文）。
            validate_mcp_secret_refs(&cfg.mcp, &secrets)?;
            // ── LSP 客户端（fail-fast 口径同 mcp：非法名/缺 command 直接 load 失败）──
            cfg.lsp = map_lsp_config(pc.lsp.clone())?;
            // LSP env 的 `${secret:name}` 复用 mcp 校验（同 secrets.toml `[secrets.mcp]` 段）。
            validate_lsp_secret_refs(&cfg.lsp, &secrets)?;

            // 迁移写回：config.toml 中旧明文已入 secret store，把明文替换为
            // "set" 标记（重新 load 磁盘原值，只改"确为明文"的槽位——迁移
            // 失败的槽位保持明文，下次 load 重试；已标记/未配置的原样保留）。
            if needs_rewrite && let Some(mut fresh) = store.load() {
                let is_plain = |k: &Option<String>| {
                    k.as_deref()
                        .is_some_and(|v| !v.is_empty() && v != CONFIG_MARKER)
                };
                if is_plain(&fresh.api_key) {
                    fresh.api_key = Some(CONFIG_MARKER.to_owned());
                }
                if let Some(ref mut s) = fresh.subagent
                    && is_plain(&s.api_key)
                {
                    s.api_key = Some(CONFIG_MARKER.to_owned());
                }
                // BYOK 迁移写回：把磁盘上的旧形状 profile（带 `endpoint` 预设键）
                // 补成自述的六字段记录。此后 `[profiles.*]` 里不再有预设坐标，
                // `provider_id` / `endpoint` 键也不再落盘。
                if let Some(profiles) = fresh.profiles.as_mut() {
                    for (name, profile) in profiles.iter_mut() {
                        let (migrated, _) = migrate_profile(
                            std::mem::take(profile),
                            legacy.as_ref(),
                            &legacy_provider_id,
                        );
                        *profile = migrated;
                        log::info!("[config] profile {name} 已迁移为 BYOK 记录");
                    }
                }
                // C3 迁移：扁平值先固化进 active/default profile（**无条件覆盖**
                // 同名条目——扁平值为最新意图），再剥离顶层键。fresh 无 profiles
                // 时就地建表；缺失的分量用合并结果 cfg 兜底，确保零丢失。
                if legacy_flat_fields {
                    let active = fresh
                        .active_profile
                        .clone()
                        .unwrap_or_else(|| "default".to_string());
                    // 先在 profiles 借用前固化兜底条目，避免可变借用重叠。
                    // 旧键（context_limit / endpoint 预设坐标）原样带上，交给
                    // migrate_profile 统一折叠与补全。
                    let raw_fallback = qaqh_types::ProfileConfig {
                        model: fresh.model.clone().unwrap_or_else(|| cfg.model.clone()),
                        max_tokens: fresh.max_tokens.unwrap_or(cfg.max_tokens),
                        effort: Some(
                            fresh
                                .reasoning_effort
                                .clone()
                                .unwrap_or_else(|| cfg.reasoning_effort.clone()),
                        ),
                        context_length: 0,
                        base_url: fresh.base_url.clone().unwrap_or_default(),
                        wire: cfg.wire,
                        compat: None,
                        preset_endpoint: fresh.endpoint.clone(),
                        context_limit: fresh.context_limit.or(Some(cfg.context_length)),
                        context_window: None,
                    };
                    let (fallback, _) =
                        migrate_profile(raw_fallback, legacy.as_ref(), &legacy_provider_id);
                    let profiles = fresh.profiles.get_or_insert_with(HashMap::new);
                    // 无条件以扁平值覆盖：历史读语义是"扁平胜出"，扁平即用户
                    // 最新意图；旧条目只可能是更早一次保存的陈值。
                    profiles.insert(active, fallback);
                    fresh.model = None;
                    fresh.base_url = None;
                    fresh.max_tokens = None;
                    fresh.context_limit = None;
                    fresh.endpoint = None;
                    fresh.reasoning_effort = None;
                }
                // 预设坐标已固化进 profile：顶层 provider_id 不再写出。
                fresh.provider_id = None;
                log::info!("[config] legacy flat model fields migrated into [profiles.*]");
                let _ = store.save(&fresh);
            }
        }

        if !cfg.profiles.contains_key("default") {
            cfg.profiles.insert(
                "default".into(),
                qaqh_types::ProfileConfig {
                    model: cfg.model.clone(),
                    max_tokens: cfg.max_tokens,
                    effort: Some(cfg.reasoning_effort.clone()),
                    context_length: cfg.context_length,
                    base_url: cfg.base_url.clone(),
                    wire: cfg.wire,
                    compat: Some(cfg.compat.clone()),
                    preset_endpoint: None,
                    context_limit: None,
                    context_window: None,
                },
            );
        }

        // Initialize tokenizer if configured
        if let Some(ref path) = cfg.tokenizer_path {
            let _ = qaqh_types::token::init_tokenizer(path);
        }

        Ok(cfg)
    }

    pub fn save(&self) -> Result<(), String> {
        let _guard = Self::config_io_lock()
            .lock()
            .map_err(|_| "config lock poisoned".to_string())?;
        self.save_unlocked()
    }

    fn save_unlocked(&self) -> Result<(), String> {
        self.save_with(
            &ConfigStore::default_location(),
            &SecretStore::default_location(),
        )
    }

    /// 显式路径版 save（与 [`Self::load_from_paths_with`] 对称；集成测试与
    /// 多实例/数据根重定向场景使用）。
    pub fn save_with(&self, store: &ConfigStore, secrets: &SecretStore) -> Result<(), String> {
        // 审计 P0-1：凭据先入 secret store；失败则中止保存（config.toml 永不
        // 出现明文）。cfg.api_key 为空时删除对应 secret 槽位——2026-08 起
        // config.save 不再把空串当"删除"（空串/掩码 = 保持现值，防前端误发空
        // 清密钥），此处仅在配置本身被清空（如未来显式删除接口）时收尾删除。
        if self.api_key.is_empty() {
            let _ = secrets.delete(SecretSlot::Main);
        } else {
            secrets
                .set(SecretSlot::Main, &self.api_key)
                .map_err(|e| format!("failed to store main api key: {e}"))?;
        }
        if self.subagent.api_key.is_empty() {
            let _ = secrets.delete(SecretSlot::Subagent);
        } else {
            secrets
                .set(SecretSlot::Subagent, &self.subagent.api_key)
                .map_err(|e| format!("failed to store subagent api key: {e}"))?;
        }

        let mut profiles = self.profiles.clone();
        profiles.insert(
            self.active_profile.clone(),
            qaqh_types::ProfileConfig {
                model: self.model.clone(),
                max_tokens: self.max_tokens,
                effort: Some(self.reasoning_effort.clone()),
                context_length: self.context_length,
                base_url: self.base_url.clone(),
                wire: self.wire,
                compat: Some(self.compat.clone()),
                preset_endpoint: None,
                context_limit: None,
                context_window: None,
            },
        );
        let pc = PersistentConfig {
            api_key: if self.api_key.is_empty() {
                None
            } else {
                Some(CONFIG_MARKER.to_owned())
            },
            // P1-C3：扁平模型族字段退役——一律 None（serde skip_serializing），
            // 唯一持久化真相是上方 upsert 进 profiles[active] 的条目。
            model: None,
            base_url: None,
            max_tokens: None,
            context_limit: None,
            // BYOK：预设坐标不再写出（profile 自身就是端点记录）。
            provider_id: None,
            endpoint: None,
            reasoning_effort: None,
            profiles: Some(profiles),
            active_profile: Some(self.active_profile.clone()),
            lang: self.lang.clone(),
            font_family: if self.font_family.is_empty() {
                None
            } else {
                Some(self.font_family.clone())
            },
            theme: self.theme.clone(),
            notifications_enabled: self.notifications_enabled,
            subagent: Some(PersistentSubagentConfig {
                model: if self.subagent.model.is_empty() {
                    None
                } else {
                    Some(self.subagent.model.clone())
                },
                base_url: if self.subagent.base_url.is_empty() {
                    None
                } else {
                    Some(self.subagent.base_url.clone())
                },
                api_key: if self.subagent.api_key.is_empty() {
                    None
                } else {
                    Some(CONFIG_MARKER.to_owned())
                },
                max_tokens: Some(self.subagent.max_tokens),
                timeout_secs: Some(self.subagent.timeout_secs),
                default_tools: if self.subagent.default_tools.is_empty() {
                    None
                } else {
                    Some(self.subagent.default_tools.clone())
                },
                max_depth: Some(self.subagent.max_depth),
                message_in_flight_per_pair: Some(self.subagent.message_in_flight_per_pair),
                message_outbound_per_sender: Some(self.subagent.message_outbound_per_sender),
            }),
            exec: Some(PersistentExecConfig {
                default_shell: self
                    .exec
                    .default_shell
                    .as_ref()
                    .filter(|shell| {
                        let shell = shell.trim();
                        !shell.is_empty() && !shell.eq_ignore_ascii_case("auto")
                    })
                    .cloned(),
            }),
            compliance_enabled: Some(self.compliance_enabled),
            compliance_extra_keywords: if self.compliance_extra_keywords.is_empty() {
                None
            } else {
                Some(self.compliance_extra_keywords.clone())
            },
            compliance_allowlist: if self.compliance_allowlist.is_empty() {
                None
            } else {
                Some(self.compliance_allowlist.clone())
            },
            permission_tier: Some(self.permission_level),
            // 旧键不再写出:防止数字 3 在新三档下被再次当成旧 WorkspaceFree
            permission_level: None,
            tokenizer_path: self.tokenizer_path.clone(),
            auto_compact_threshold: Some(self.auto_compact_threshold),
            session_idle_unload_secs: (self.session_idle_unload_secs > 0)
                .then_some(self.session_idle_unload_secs),
            mcp: Some(PersistentMcpConfig {
                enabled: Some(self.mcp.enabled),
                import_external: Some(self.mcp.import_external),
                idle_shutdown_secs: (self.mcp.idle_shutdown_secs > 0)
                    .then_some(self.mcp.idle_shutdown_secs),
                inject_resource_env_block: Some(self.mcp.inject_resource_env_block),
                servers: (!self.mcp.servers.is_empty()).then(|| {
                    self.mcp
                        .servers
                        .iter()
                        .map(|(name, s)| {
                            (
                                name.clone(),
                                PersistentMcpServerConfig {
                                    command: (!s.command.is_empty()).then(|| s.command.clone()),
                                    args: (!s.args.is_empty()).then(|| s.args.clone()),
                                    env: (!s.env.is_empty())
                                        .then(|| s.env.clone().into_iter().collect()),
                                    url: (!s.url.is_empty()).then(|| s.url.clone()),
                                    headers: (!s.headers.is_empty())
                                        .then(|| s.headers.clone().into_iter().collect()),
                                    tools: s.tools.clone(),
                                    resources: Some(s.resources_enabled),
                                    default_timeout_secs: Some(s.default_timeout_secs),
                                    max_concurrent_calls: Some(s.max_concurrent_calls),
                                    cwd: (!s.cwd.trim().is_empty())
                                        .then(|| s.cwd.trim().to_owned()),
                                },
                            )
                        })
                        .collect()
                }),
            }),
            lsp: Some(PersistentLspConfig {
                enabled: Some(self.lsp.enabled),
                idle_shutdown_secs: (self.lsp.idle_shutdown_secs > 0)
                    .then_some(self.lsp.idle_shutdown_secs),
                servers: (!self.lsp.servers.is_empty()).then(|| {
                    self.lsp
                        .servers
                        .iter()
                        .map(|(name, s)| {
                            (
                                name.clone(),
                                PersistentLspServerConfig {
                                    command: Some(s.command.clone()),
                                    args: (!s.args.is_empty()).then(|| s.args.clone()),
                                    env: (!s.env.is_empty())
                                        .then(|| s.env.clone().into_iter().collect()),
                                    extensions: (!s.extensions.is_empty())
                                        .then(|| s.extensions.clone()),
                                    startup_timeout_secs: Some(s.startup_timeout_secs),
                                    default_timeout_secs: Some(s.default_timeout_secs),
                                },
                            )
                        })
                        .collect()
                }),
            }),
        };
        log::info!(
            "[Config::save] writing to {}",
            qaqh_types::platform::config_path().display()
        );
        if !store.save(&pc) {
            return Err(format!(
                "Failed to save config to {}",
                qaqh_types::platform::config_path().display()
            ));
        }

        Ok(())
    }

    /// Pure profile switch. Persistence is the caller's responsibility via
    /// [`Config::update`] — profile methods must not create another write port.
    pub fn apply_profile(&mut self, name: &str) -> Option<String> {
        let profile = self.profiles.get(name)?.clone();
        // BYOK：profile 自述端点，切换 profile 就是切换 endpoint / wire / compat。
        // 不再有"回退到预设 base_url"那一步——它曾把自定义 URL 改回预设并落盘。
        self.compat = profile.compat_or_default();
        self.wire = profile.wire;
        self.model = profile.model;
        self.max_tokens = profile.max_tokens;
        self.reasoning_effort = profile.effort.unwrap_or_else(|| "high".into());
        self.context_length = profile.context_length;
        self.base_url = profile.base_url;
        self.active_profile = name.to_string();
        Some(name.to_string())
    }

    pub fn save_profile(&mut self, name: &str) {
        self.profiles.insert(
            name.to_string(),
            qaqh_types::ProfileConfig {
                model: self.model.clone(),
                max_tokens: self.max_tokens,
                effort: Some(self.reasoning_effort.clone()),
                context_length: self.context_length,
                base_url: self.base_url.clone(),
                wire: self.wire,
                compat: Some(self.compat.clone()),
                preset_endpoint: None,
                context_limit: None,
                context_window: None,
            },
        );
        self.active_profile = name.to_string();
    }

    pub fn delete_profile(&mut self, name: &str) -> bool {
        if name == "default" {
            return false;
        }
        self.profiles.remove(name).is_some()
    }
}

#[cfg(test)]
mod secret_tests {
    use super::*;
    use crate::secrets::{SecretSlot, SecretStore};
    use std::path::PathBuf;

    fn temp_dir() -> PathBuf {
        static SEQ: std::sync::atomic::AtomicU32 = std::sync::atomic::AtomicU32::new(0);
        let n = SEQ.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        std::env::temp_dir().join(format!(
            "qaqh-config-secret-test-{}-{n}",
            std::process::id()
        ))
    }

    #[test]
    fn legacy_plaintext_migrates_to_secret_store() {
        let dir = temp_dir();
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let config_path = dir.join("config.toml");
        let secrets_path = dir.join("secrets.toml");
        std::fs::write(
            &config_path,
            "provider_id = \"deepseek\"\napi_key = \"sk-legacy-secret\"\n",
        )
        .expect("write legacy config");

        let store = ConfigStore::new(config_path.clone());
        let secrets = SecretStore::new(secrets_path.clone());
        let cfg = Config::load_from_paths_with(store, secrets.clone()).expect("load");

        // 运行时拿到明文（内存），secrets 已迁移，config.toml 不再有明文。
        assert_eq!(cfg.api_key, "sk-legacy-secret");
        assert!(secrets.has(SecretSlot::Main));
        assert_eq!(
            secrets.load(SecretSlot::Main).as_deref(),
            Some("sk-legacy-secret")
        );
        let on_disk = std::fs::read_to_string(&config_path).expect("read back");
        assert!(
            !on_disk.contains("sk-legacy-secret"),
            "plaintext must not remain"
        );
        assert!(on_disk.contains("api_key = \"set\""), "marker written back");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn marker_reads_secret_from_store() {
        let dir = temp_dir();
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let config_path = dir.join("config.toml");
        let secrets_path = dir.join("secrets.toml");
        std::fs::write(&config_path, "api_key = \"set\"\n").expect("write marker config");
        let secrets = SecretStore::new(secrets_path);
        secrets
            .set(SecretSlot::Main, "sk-from-store")
            .expect("set secret");

        let store = ConfigStore::new(config_path);
        let cfg = Config::load_from_paths_with(store, secrets).expect("load");
        assert_eq!(cfg.api_key, "sk-from-store");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn save_writes_marker_not_plaintext() {
        let dir = temp_dir();
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let config_path = dir.join("config.toml");
        let secrets_path = dir.join("secrets.toml");

        let cfg = Config {
            api_key: "sk-new-secret".to_owned(),
            ..Default::default()
        };
        let store = ConfigStore::new(config_path.clone());
        let secrets = SecretStore::new(secrets_path.clone());
        cfg.save_with(&store, &secrets).expect("save");

        let on_disk = std::fs::read_to_string(&config_path).expect("read back");
        assert!(
            !on_disk.contains("sk-new-secret"),
            "plaintext must not be written"
        );
        assert!(on_disk.contains("api_key = \"set\""));
        assert_eq!(
            secrets.load(SecretSlot::Main).as_deref(),
            Some("sk-new-secret")
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn empty_key_deletes_from_store() {
        let dir = temp_dir();
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let secrets = SecretStore::new(dir.join("secrets.toml"));
        secrets.set(SecretSlot::Main, "sk-to-delete").expect("set");
        assert!(secrets.has(SecretSlot::Main));

        // service 层空串语义 → save 时 delete。
        secrets.delete(SecretSlot::Main).expect("delete");
        assert!(!secrets.has(SecretSlot::Main));
        assert!(secrets.load(SecretSlot::Main).is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }
}

// ── P1-C3：双真相源收敛迁移测试 ─────────────────────────────────────
#[cfg(test)]
mod c3_migration_tests {
    use super::*;
    use std::path::PathBuf;
    use std::sync::atomic::{AtomicU32, Ordering};

    fn temp_dir(tag: &str) -> PathBuf {
        static SEQ: AtomicU32 = AtomicU32::new(0);
        let n = SEQ.fetch_add(1, Ordering::SeqCst);
        std::env::temp_dir().join(format!("qaqh-config-c3-{tag}-{}-{n}", std::process::id()))
    }

    fn setup(tag: &str, toml_text: &str) -> (PathBuf, ConfigStore, SecretStore) {
        let dir = temp_dir(tag);
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        std::fs::write(dir.join("config.toml"), toml_text).expect("write toml");
        let store = ConfigStore::new(dir.join("config.toml"));
        let secrets = SecretStore::new(dir.join("secrets.toml"));
        (dir, store, secrets)
    }

    fn top_level_absent(text: &str) -> bool {
        // 顶层模型族键必须全部消失（[profiles.*] 内的同名键不算）。
        let doc: toml::Value = toml::from_str(text).expect("parse toml");
        doc.get("model").is_none()
            && doc.get("base_url").is_none()
            && doc.get("max_tokens").is_none()
            && doc.get("context_limit").is_none()
            && doc.get("endpoint").is_none()
            && doc.get("reasoning_effort").is_none()
            && doc.get("provider_id").is_none()
    }

    #[test]
    fn exec_default_shell_loads_normalizes_and_roundtrips() {
        let (dir, store, secrets) = setup("exec-shell", "[exec]\ndefault_shell = \"zsh\"\n");
        let cfg = Config::load_from_paths_with(store.clone(), secrets.clone()).expect("load");
        assert_eq!(cfg.exec.default_shell.as_deref(), Some("zsh"));

        cfg.save_with(&store, &secrets).expect("save");
        let text = std::fs::read_to_string(dir.join("config.toml")).expect("read back");
        assert!(text.contains("[exec]"), "{text}");
        assert!(text.contains("default_shell = \"zsh\""), "{text}");

        let auto = Config {
            exec: ExecConfig {
                default_shell: Some("auto".into()),
            },
            ..Default::default()
        };
        auto.save_with(&store, &secrets).expect("save auto");
        let reloaded =
            Config::load_from_paths_with(store, secrets).expect("reload normalized config");
        assert_eq!(reloaded.exec.default_shell, None);
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 旧形态（纯扁平）：load 即触发一次性迁移——值固化进 profile、顶层剥离；
    /// 重载后值不丢。BYOK 后还多一条：profile 自述端点，不再有预设坐标。
    #[test]
    fn legacy_flat_migrates_on_first_load() {
        let (dir, store, secrets) = setup(
            "legacy",
            "provider_id = \"deepseek\"
             model = \"legacy-model\"
             base_url = \"https://legacy/v1\"
             max_tokens = 8192
             context_limit = 256000
             endpoint = \"openai\"
             reasoning_effort = \"max\"
             active_profile = \"default\"
",
        );
        let cfg = Config::load_from_paths_with(store.clone(), secrets.clone()).expect("load");
        assert_eq!(cfg.model, "legacy-model");
        assert_eq!(cfg.context_length, 256_000);

        // load 的 needs_rewrite 已完成迁移写回。
        let text = std::fs::read_to_string(dir.join("config.toml")).expect("read back");
        assert!(
            top_level_absent(&text),
            "top-level flats must be stripped: {text}"
        );
        let doc: toml::Value = toml::from_str(&text).expect("toml");
        let profile = &doc["profiles"]["default"];
        assert_eq!(profile["model"].as_str(), Some("legacy-model"));
        // 双口径窗口折叠成单一分母，且写回的是 context_length。
        assert_eq!(profile["context_length"].as_integer(), Some(256_000));
        assert!(profile.get("context_limit").is_none(), "{text}");
        // 预设坐标退役：wire 落到记录里，endpoint/provider_id 不再出现。
        assert_eq!(profile["wire"].as_str(), Some("openai"));
        assert!(profile.get("endpoint").is_none(), "{text}");

        // 重载幂等：值经 profile 回来，不再有扁平覆盖。
        let cfg2 = Config::load_from_paths_with(store, secrets).expect("reload");
        assert_eq!(cfg2.model, "legacy-model");
        assert_eq!(cfg2.context_length, 256_000);
        assert_eq!(cfg2.reasoning_effort, "max");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 混合形态：扁平与 profile 并存且值不同 → 读时扁平胜出（历史语义），
    /// 迁移写回后收敛为扁平值入 profile。
    #[test]
    fn mixed_shape_flat_wins_then_converges() {
        let (dir, store, secrets) = setup(
            "mixed",
            "model = \"flat-model\"
             active_profile = \"default\"
             [profiles.default]
             model = \"profile-model\"
             max_tokens = 4096
             context_limit = 128000
             base_url = \"https://p/v1\"
             endpoint = \"openai\"
",
        );
        let cfg = Config::load_from_paths_with(store.clone(), secrets.clone()).expect("load");
        assert_eq!(cfg.model, "flat-model");

        let text = std::fs::read_to_string(dir.join("config.toml")).expect("read back");
        assert!(top_level_absent(&text), "{text}");
        let doc: toml::Value = toml::from_str(&text).expect("toml");
        // 扁平值为最新意图：迁移时无条件覆盖同名 profile 条目。
        let profile = &doc["profiles"]["default"];
        assert_eq!(profile["model"].as_str(), Some("flat-model"));
        assert_eq!(profile["context_length"].as_integer(), Some(128_000));
        let cfg2 = Config::load_from_paths_with(store, secrets).expect("reload");
        assert_eq!(cfg2.model, "flat-model");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// BYOK 形态（自述端点）：读写稳定，不发生任何迁移改写。
    #[test]
    fn byok_profile_round_trips_without_migration() {
        let source = "active_profile = \"default\"
[profiles.default]
model = \"m1\"
max_tokens = 96000
effort = \"max\"
context_length = 1000000
base_url = \"https://x/v1\"
wire = \"responses\"

[profiles.default.compat]
responses_effort_max = \"max\"
supports_image_tool = true
";
        let (dir, store, secrets) = setup("byok", source);
        let cfg = Config::load_from_paths_with(store.clone(), secrets.clone()).expect("load");
        assert_eq!(cfg.model, "m1");
        assert_eq!(cfg.wire, qaqh_types::Wire::Responses);
        assert_eq!(cfg.context_length, 1_000_000);
        assert_eq!(cfg.compat.responses_effort_max, "max");
        assert!(cfg.compat.supports_image_tool);
        assert_eq!(
            cfg.profiles.get("default").map(|p| p.wire),
            Some(qaqh_types::Wire::Responses)
        );

        cfg.save_with(&store, &secrets).expect("save");
        let text = std::fs::read_to_string(dir.join("config.toml")).expect("read back");
        let doc: toml::Value = toml::from_str(&text).expect("toml");
        let profile = &doc["profiles"]["default"];
        assert_eq!(profile["wire"].as_str(), Some("responses"));
        assert_eq!(
            profile["compat"]["responses_effort_max"].as_str(),
            Some("max")
        );
        assert!(
            profile.get("endpoint").is_none() && profile.get("context_limit").is_none(),
            "BYOK 记录不得被写回预设坐标: {text}"
        );

        let cfg2 = Config::load_from_paths_with(store, secrets).expect("reload");
        assert_eq!(cfg2.wire, qaqh_types::Wire::Responses);
        assert_eq!(cfg2.compat.responses_effort_max, "max");
        assert_eq!(cfg2.reasoning_effort, "max");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 首次启动（无配置文件）：落一份带注释的 BYOK 预设，且必须能原样解析回
    /// 六字段形状——模板与 serde 结构一旦漂移，这条就红。
    #[test]
    fn first_run_writes_a_parsable_preset_once() {
        let dir = temp_dir("first-run");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let store = ConfigStore::new(dir.join("config.toml"));
        let secrets = SecretStore::new(dir.join("secrets.toml"));
        assert!(!store.exists(), "前置条件：还没有配置文件");

        let cfg = Config::load_from_paths_with(store.clone(), secrets.clone()).expect("load");
        assert_eq!(cfg.model, "deepseek-chat");
        assert_eq!(cfg.base_url, "https://api.deepseek.com");
        assert_eq!(cfg.wire, qaqh_types::Wire::OpenAi);
        assert_eq!(cfg.max_tokens, 16_384);
        assert_eq!(cfg.context_length, 128_000);
        assert_eq!(cfg.reasoning_effort, "high");
        assert!(cfg.api_key.is_empty(), "预设不得带密钥");
        assert_eq!(
            cfg.compat,
            qaqh_types::EndpointCompat::default(),
            "compat 示例只存在于注释里，解析结果必须是 wire 缺省"
        );

        let text = std::fs::read_to_string(dir.join("config.toml")).expect("read preset");
        assert!(text.starts_with("# QAQ-Harness 配置"), "首启文件要自解释");
        assert!(
            text.contains("[profiles.default.compat]"),
            "compat 例子要在（注释里）"
        );
        assert!(!text.contains("\napi_key"), "写出的文件里不得有 api_key");

        // 幂等：再 load 不得改写（用户可能正在编辑它）。
        Config::load_from_paths_with(store.clone(), secrets.clone()).expect("reload");
        assert_eq!(
            std::fs::read_to_string(dir.join("config.toml")).expect("read again"),
            text,
            "预设只写一次"
        );

        // 损坏的用户文件绝不被模板覆盖（exists 判定与解析失败分开）。
        std::fs::write(dir.join("config.toml"), "this is not = valid toml").expect("write junk");
        let broken = Config::load_from_paths_with(store, secrets).expect("load survives junk");
        assert!(broken.model.is_empty(), "解析失败时按缺省跑");
        assert_eq!(
            std::fs::read_to_string(dir.join("config.toml")).expect("read junk"),
            "this is not = valid toml",
            "损坏文件必须原样留在盘上等人修"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 预设坐标 → 端点记录：wire 与 compat 从迁移表落地，请求形状不变。
    #[test]
    fn legacy_preset_coordinate_materializes_wire_and_compat() {
        let (dir, store, secrets) = setup(
            "preset-compat",
            "provider_id = \"minimax\"
active_profile = \"default\"
[profiles.default]
model = \"mm\"
max_tokens = 8192
context_limit = 1000000
base_url = \"https://api.minimaxi.com/v1\"
endpoint = \"openai\"
",
        );
        let cfg = Config::load_from_paths_with(store.clone(), secrets.clone()).expect("load");
        assert_eq!(cfg.wire, qaqh_types::Wire::OpenAi);
        assert_eq!(
            cfg.compat.thinking_mode,
            qaqh_types::ThinkingParamMode::MiniMaxAdaptive,
            "旧预设的 thinking 参数口径必须由 compat 原样带过来"
        );
        assert_eq!(cfg.compat.cache_field, qaqh_types::CacheTokenField::None);

        let text = std::fs::read_to_string(dir.join("config.toml")).expect("read back");
        let doc: toml::Value = toml::from_str(&text).expect("toml");
        let profile = &doc["profiles"]["default"];
        assert_eq!(
            profile["compat"]["thinking_mode"].as_str(),
            Some("MiniMaxAdaptive"),
            "compat 要落到盘上，下次 load 不再依赖迁移表: {text}"
        );
        assert!(profile.get("endpoint").is_none(), "{text}");
        assert!(doc.get("provider_id").is_none(), "{text}");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 预设的 responses 端点：wire 迁移后 gate 才走对协议；空 base_url 由预设兜底。
    #[test]
    fn legacy_preset_responses_coordinate_maps_to_responses_wire() {
        let (dir, store, secrets) = setup(
            "preset-responses",
            "provider_id = \"deepseek\"
active_profile = \"default\"
[profiles.default]
model = \"ds\"
max_tokens = 8192
context_limit = 128000
endpoint = \"responses\"
",
        );
        let cfg = Config::load_from_paths_with(store, secrets).expect("load");
        assert_eq!(cfg.wire, qaqh_types::Wire::Responses);
        assert_eq!(cfg.base_url, "https://api.deepseek.com");
        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 双口径窗口折叠：端点硬窗口优先于用户口径；两者皆无时用内置保守分母。
    #[test]
    fn dual_window_keys_fold_into_one_context_length() {
        let (dir, store, secrets) = setup(
            "fold-window",
            "active_profile = \"default\"
[profiles.default]
model = \"m1\"
max_tokens = 96000
context_limit = 1000000
context_window = 200000
base_url = \"https://x/v1\"
endpoint = \"openai\"
",
        );
        let cfg = Config::load_from_paths_with(store.clone(), secrets.clone()).expect("load");
        assert_eq!(
            cfg.context_length, 200_000,
            "端点声明的窗口是压缩分母的真值"
        );

        cfg.save_with(&store, &secrets).expect("save");
        let text = std::fs::read_to_string(dir.join("config.toml")).expect("read back");
        let doc: toml::Value = toml::from_str(&text).expect("toml");
        let profile = &doc["profiles"]["default"];
        assert_eq!(profile["context_length"].as_integer(), Some(200_000));
        assert!(profile.get("context_window").is_none(), "{text}");
        assert!(profile.get("context_limit").is_none(), "{text}");
        let _ = std::fs::remove_dir_all(&dir);

        // 只有用户口径的老配置：照常解析，取该值。
        let (dir2, store2, secrets2) = setup(
            "fold-limit",
            "active_profile = \"default\"
[profiles.default]
model = \"m1\"
max_tokens = 96000
context_limit = 1000000
base_url = \"https://x/v1\"
endpoint = \"openai\"
",
        );
        let only_limit = Config::load_from_paths_with(store2, secrets2).expect("load old");
        assert_eq!(only_limit.context_length, 1_000_000);
        let _ = std::fs::remove_dir_all(&dir2);

        // 两个键都没有（全新 BYOK 记录）：内置保守分母，零值不留。
        let (dir3, store3, secrets3) = setup(
            "fold-none",
            "active_profile = \"default\"
[profiles.default]
model = \"m1\"
max_tokens = 96000
base_url = \"https://x/v1\"
wire = \"openai\"
",
        );
        let bare = Config::load_from_paths_with(store3, secrets3).expect("load bare");
        assert_eq!(bare.context_length, DEFAULT_CONTEXT_LENGTH);
        let _ = std::fs::remove_dir_all(&dir3);
    }

    /// 切 profile = 切端点：wire / compat / 分母都要跟着换，
    /// 否则会沿用上一个 profile 的端点身份。
    #[test]
    fn apply_profile_switches_endpoint_identity() {
        let (dir, store, secrets) = setup(
            "switch-endpoint",
            "active_profile = \"a\"
[profiles.a]
model = \"ma\"
max_tokens = 1000
context_length = 200000
base_url = \"https://a/v1\"
wire = \"anthropic\"

[profiles.a.compat]
thinking_budget_large = true

[profiles.b]
model = \"mb\"
max_tokens = 1000
context_length = 32000
base_url = \"https://b/v1\"
wire = \"responses\"
",
        );
        let mut cfg = Config::load_from_paths_with(store, secrets).expect("load");
        assert_eq!(cfg.wire, qaqh_types::Wire::Anthropic);
        assert!(cfg.compat.thinking_budget_large);
        cfg.apply_profile("b");
        assert_eq!(
            cfg.wire,
            qaqh_types::Wire::Responses,
            "wire 必须跟着 profile 换"
        );
        assert!(
            !cfg.compat.thinking_budget_large,
            "上一个端点的 compat 不得残留"
        );
        assert_eq!(cfg.context_length, 32_000);
        assert_eq!(cfg.base_url, "https://b/v1");
        cfg.apply_profile("a");
        assert_eq!(cfg.wire, qaqh_types::Wire::Anthropic);
        assert!(cfg.compat.thinking_budget_large);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
