use crate::error::{ConfigError, ConfigResult};
use crate::secrets::SecretStore;
use qaqh_types::{
    ConfigStore, PersistentConfig, PersistentExecConfig, PersistentLspConfig,
    PersistentLspServerConfig, PersistentMcpConfig, PersistentMcpServerConfig,
    PersistentSandboxConfig, PersistentSubagentConfig,
};
use std::sync::{Mutex, OnceLock};

/// Subagent default configuration.
/// These are defaults applied when spawning sub-agents. The `spawn_subagent`
/// tool schema only exposes `task_description` / `agent_name` / `context` /
/// `timeout_secs`; the remaining fields are set through settings, not per-call.
/// 端点族字段（model/baseUrl/apiKey/maxTokens）已退役：子代理的模型选择
/// 唯一真源是 providers.toml 目录 + 会话行 model_selection。
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct SubagentConfig {
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
            timeout_secs: 120,
            default_tools: vec!["read".into(), "exec_command".into()],
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

/// 沙箱执行配置。
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct SandboxConfig {
    pub enabled: bool,
}

impl Default for SandboxConfig {
    fn default() -> Self {
        Self { enabled: true }
    }
}

/// Runtime agent configuration built from PersistentConfig + registry.
/// This is the fully-resolved config used by the agent at runtime. It combines
/// user settings from config.toml with provider registry defaults and profile
/// overrides. All fields are concrete (no Option wrapping).
#[derive(Debug, Clone)]
pub struct Config {
    pub provider_id: String,
    pub provider_name: String,
    pub profile_id: String,
    pub profile_name: String,
    pub profiles_catalog: qaqh_profile::ProfilesDocument,
    pub default_model_selection: Option<qaqh_profile::ModelSelection>,
    pub model_capabilities: qaqh_types::ModelCapabilities,
    pub model_reasoning_levels: Vec<String>,
    pub model_native_web_search: bool,
    pub provider_headers: std::collections::BTreeMap<String, String>,
    /// API key for the active endpoint (never persisted in clear text).
    pub api_key: String,
    /// BYOK 的 endpoint：scheme + host + 可选前缀，不含 wire 自身路径。
    pub base_url: String,
    /// 该 endpoint 说的 wire 协议。
    pub wire: qaqh_types::Wire,
    /// 该 endpoint 与 wire 缺省的差异（请求形状开关）。
    pub compat: qaqh_types::EndpointCompat,
    /// 凭据来源：`api_key`（缺省）= 既有 BYOK；`chatgpt_plan` = 本机 ChatGPT 套餐额度。
    pub credential: qaqh_types::CredentialSource,
    /// Active model identifier.
    pub model: String,
    /// Max output tokens per turn.
    pub max_tokens: u32,
    /// 端点声明的上下文窗口（输入 token）——本地压缩的唯一分母：auto-compact 软
    /// 阈值（`context_length × auto_compact_threshold`）与发送前硬 pre-flight
    /// 都由它推得（BYOK 前的 `context_limit`/`context_window` 双口径已收敛）。
    pub context_length: u32,
    /// Reasoning effort: one of `low|medium|high|xhigh|max|ultra`, or empty (endpoint default).
    pub reasoning_effort: String,
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
    pub tool_allowlist_enabled: bool,
    pub tool_allowlist: Vec<String>,
    /// MCP 客户端配置。
    pub mcp: McpConfig,
    /// LSP 客户端配置。
    pub lsp: LspConfig,
    /// 沙箱执行配置。
    pub sandbox: SandboxConfig,
}

// ── MCP 客户端配置──

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

// ── LSP 客户端配置──

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
/// 校验规则（设计 §6 + PLAN PR-M1-1）：
/// - server 名非空、仅 `[a-z0-9_-]`、≤64 字符；
/// - stdio/http 互斥：stdio 必须有非空 command 且无 url；http 必须有
///   `http(s)://` url 且无 command；
/// - tools 白名单条目非空；
/// - `default_timeout_secs` ∈ 1..=3600（越界即错，不做静默 clamp——静默修正
///   会掩盖配置错误）；
/// - `max_concurrent_calls` ∈ 1..=16（T-8-1 / O-4：上限从 64 收紧，
///   单 server 并发调用是 MCP 侧的资源放大面，16 为安全上限）。
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
/// 校验规则：
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

/// BYOK 未声明上下文窗口时的缺省分母（与 `qaqh-profile::ModelDef` 缺省窗口同口径）。
///
/// 方向性：这个数只当**初始猜测**，运行时按 provider 实测（接受/超限）收敛；
/// 猜小了只是提早交接，猜大了才是撞墙。
pub const DEFAULT_CONTEXT_LENGTH: u32 = 383_000;

/// 首次启动落盘的预设配置（带注释，可直接编辑）。
/// 存在的前提是"配置文件不在"（`load` 只在 `!store.exists()` 时写），因此损坏的
/// 用户文件绝不会被它覆盖。模型和端点留空，由用户在设置中填写。
/// api key 走设置页 → `secrets.toml`，本文件不包含密钥。
const FIRST_RUN_CONFIG: &str = r#"# QAQ-Harness 配置（首次启动自动生成）。
#
# 本文件只承载**全局策略**（权限档位、子代理、MCP/LSP、沙箱等）。
# 服务商与模型不再写在这里：端点、凭据与模型的唯一真源是同目录的
# providers.toml 目录（设置页「服务商与模型」维护），会话按
# model_selection 从目录解析端点与凭据。
#
# 服务商（providers.toml）字段一览：
#   [providers.<id>]        name / base_url / wire / credential / api_key
#   [providers.<id>.models.<model_id>]  context_window / max_output_tokens / reasoning_levels
#   default_model_selection = { provider, model }   # 新会话的缺省选择
"#;

fn migrate_legacy_exec_tool_names(tools: &[String]) -> Vec<String> {
    tools
        .iter()
        .map(|tool| match tool.as_str() {
            "exec" => "exec_command",
            "process" => "write_stdin",
            _ => tool.as_str(),
        })
        .map(String::from)
        .collect()
}

impl Default for Config {
    fn default() -> Self {
        Self {
            provider_id: String::new(),
            provider_name: String::new(),
            profile_id: String::new(),
            profile_name: String::new(),
            profiles_catalog: Default::default(),
            default_model_selection: None,
            model_capabilities: Default::default(),
            model_reasoning_levels: Vec::new(),
            model_native_web_search: false,
            provider_headers: Default::default(),
            api_key: String::new(),
            base_url: String::new(),
            wire: qaqh_types::Wire::default(),
            compat: qaqh_types::EndpointCompat::default(),
            credential: qaqh_types::CredentialSource::ApiKey,
            model: String::new(),
            max_tokens: 16384,
            context_length: DEFAULT_CONTEXT_LENGTH,
            reasoning_effort: String::new(),
            lang: None,
            font_family: String::new(),
            theme: None,
            notifications_enabled: None,
            subagent: SubagentConfig::default(),
            exec: ExecConfig::default(),
            permission_level: 2, // workspace-write — safe autonomous default
            tokenizer_path: None,
            auto_compact_threshold: 0.75,
            session_idle_unload_secs: 0,
            tool_allowlist_enabled: false,
            tool_allowlist: Vec::new(),
            mcp: McpConfig::default(),
            lsp: LspConfig::default(),
            sandbox: SandboxConfig::default(),
        }
    }
}

impl Config {
    /// Serializes all process-local config read-modify-write cycles.
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
    pub fn load() -> ConfigResult<Self> {
        let _guard = Self::config_io_lock()
            .lock()
            .map_err(|_| ConfigError::LockPoisoned)?;
        Self::load_unlocked()
    }

    fn load_unlocked() -> ConfigResult<Self> {
        let store = ConfigStore::default_location();
        Self::load_from_paths_with(store, SecretStore::default_location())
    }

    /// The single config write port (BUG-008 / roadmap 刀6).
    /// Loads the current config, applies `mutate`, and persists it in one
    /// locked transaction. If `mutate` returns `Err`, nothing is written and
    /// the error is propagated. Daemon actions must mutate config exclusively
    /// through this method instead of `load()` + direct field writes + `save()`.
    pub fn update<F>(mutate: F) -> ConfigResult<Self>
    where
        F: FnOnce(&mut Self) -> Result<(), String>,
    {
        let _guard = Self::config_io_lock()
            .lock()
            .map_err(|_| ConfigError::LockPoisoned)?;
        let mut config = Self::load_unlocked()?;
        mutate(&mut config)?;
        config.save_unlocked()?;
        // P2-D1：磁盘先落盘，再广播内存快照（消费者永远读到已持久化状态）。
        crate::watch::publish(std::sync::Arc::new(config.clone()));
        Ok(config)
    }

    /// 从显式路径加载（公开给集成测试与多实例/数据根重定向场景；
    /// 生产路径走 [`Config::load`] 的全局 platform 路径）。
    pub fn load_from_paths_with(store: ConfigStore, secrets: SecretStore) -> ConfigResult<Self> {
        let mut cfg = Self::default();

        let mut pc = store.load().map_err(ConfigError::invalid)?;
        // 首次启动（文件不存在，而非解析失败）：落一份带注释的 BYOK 预设，
        // 让"配置在哪、要填哪六项"在磁盘上自解释，而不是静默跑在内存缺省值上。
        // 按 `exists()` 二次判定：损坏的用户文件走不到这里，绝不被模板覆盖。
        if pc.is_none() && !store.exists() {
            if store.write_content(FIRST_RUN_CONFIG) {
                log::info!(
                    "[config] 首次运行：已写入预设配置 {}",
                    store.path().display()
                );
                pc = store.load().map_err(ConfigError::invalid)?;
            }
        }

        let mut needs_rewrite = false;
        if let Some(mut pc) = pc {
            let provider_path = store.path().with_file_name("providers.toml");
            let provider_store = qaqh_profile::ProfilesStore::load_or_create(&provider_path)
                .map_err(|error| ConfigError::invalid(format!("load providers.toml: {error}")))?;
            cfg.profiles_catalog = provider_store.document().clone();
            cfg.default_model_selection = cfg.profiles_catalog.default_model_selection.clone();
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
                if let Some(ts) = s.timeout_secs {
                    cfg.subagent.timeout_secs = ts;
                }
                if let Some(ref tools) = s.default_tools {
                    // 存量迁移：工具改名后（file→read、read_file→read、
                    // edit_file_v2→edit）旧配置持久化的
                    // 默认白名单仍指向旧名；加载时归一化为当前正式词汇表，
                    // 否则 apply_init 会静默剔除旧名，子代理丢失读文件能力。
                    let migrated_names = migrate_legacy_exec_tool_names(tools);
                    needs_rewrite |= migrated_names.as_slice() != tools.as_slice();
                    cfg.subagent.default_tools = migrated_names
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
                            "[config] invalid permission_tier {tier} in config.toml (must be 1-4); \
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
            cfg.tool_allowlist_enabled = pc.tool_allowlist_enabled.unwrap_or(false);
            let persisted_tool_allowlist = pc.tool_allowlist.clone().unwrap_or_default();
            cfg.tool_allowlist = migrate_legacy_exec_tool_names(&persisted_tool_allowlist);
            needs_rewrite |= cfg.tool_allowlist != persisted_tool_allowlist;

            // ── Sandbox ──
            if let Some(ref sb) = pc.sandbox {
                if let Some(enabled) = sb.enabled {
                    cfg.sandbox.enabled = enabled;
                }
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

            // 迁移写回：config.toml 中旧工具名的默认白名单归一化为当前
            // 正式词汇表（重新 load 磁盘原值，只改确需归一化的字段）。
            if needs_rewrite && let Ok(Some(mut fresh)) = store.load() {
                if let Some(ref mut s) = fresh.subagent {
                    if let Some(ref mut tools) = s.default_tools {
                        *tools = migrate_legacy_exec_tool_names(tools);
                        *tools = tools
                            .iter()
                            .map(|tool| match tool.as_str() {
                                "file" | "read_file" => "read",
                                "edit_file_v2" => "edit",
                                _ => tool,
                            })
                            .map(String::from)
                            .collect();
                    }
                }
                if let Some(ref mut tools) = fresh.tool_allowlist {
                    *tools = migrate_legacy_exec_tool_names(tools);
                }
                let _ = store.save(&fresh);
            }
        }

        // Initialize tokenizer if configured
        if let Some(ref path) = cfg.tokenizer_path {
            let _ = qaqh_types::token::init_tokenizer(path);
        }

        Ok(cfg)
    }

    pub fn save(&self) -> ConfigResult<()> {
        let _guard = Self::config_io_lock()
            .lock()
            .map_err(|_| ConfigError::LockPoisoned)?;
        self.save_unlocked()
    }

    fn save_unlocked(&self) -> ConfigResult<()> {
        self.save_with(
            &ConfigStore::default_location(),
            &SecretStore::default_location(),
        )
    }

    /// 显式路径版 save（与 [`Self::load_from_paths_with`] 对称；集成测试与
    /// 多实例/数据根重定向场景使用）。
    pub fn save_with(&self, store: &ConfigStore, _secrets: &SecretStore) -> ConfigResult<()> {
        // 凭据纪律：api_key 不经 config.save 落盘——provider 密钥的唯一写口
        // 是 provider.upsert 的 SecretWrite（providers.<id> 加密槽）；
        // cfg.api_key 是会话期解析值，绝不回写。
        let pc = PersistentConfig {
            lang: self.lang.clone(),
            font_family: if self.font_family.is_empty() {
                None
            } else {
                Some(self.font_family.clone())
            },
            theme: self.theme.clone(),
            notifications_enabled: self.notifications_enabled,
            subagent: Some(PersistentSubagentConfig {
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
            permission_tier: Some(self.permission_level),
            // 旧键不再写出:防止数字 3 在新三档下被再次当成旧 WorkspaceFree
            permission_level: None,
            tokenizer_path: self.tokenizer_path.clone(),
            auto_compact_threshold: Some(self.auto_compact_threshold),
            session_idle_unload_secs: (self.session_idle_unload_secs > 0)
                .then_some(self.session_idle_unload_secs),
            tool_allowlist_enabled: Some(self.tool_allowlist_enabled),
            tool_allowlist: Some(self.tool_allowlist.clone()),
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
            sandbox: Some(PersistentSandboxConfig {
                enabled: Some(self.sandbox.enabled),
            }),
        };
        log::info!(
            "[Config::save] writing to {}",
            qaqh_types::platform::config_path().display()
        );
        if !store.save(&pc) {
            return Err(ConfigError::invalid(format!(
                "Failed to save config to {}",
                qaqh_types::platform::config_path().display()
            )));
        }

        Ok(())
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

    /// Spec §3.5 测试类型 3：旧格式（顶层模型族键）在 load 时被拒绝，
    /// 报错并提示重新配置；不自动转换，不静默忽略。
    #[test]
    fn legacy_model_config_is_rejected_with_reconfigure_hint() {
        let dir = temp_dir();
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let config_path = dir.join("config.toml");
        std::fs::write(
            &config_path,
            "provider_id = \"deepseek\"\napi_key = \"sk-legacy-secret\"\n",
        )
        .expect("write legacy config");

        let store = ConfigStore::new(config_path);
        let secrets = SecretStore::new(dir.join("secrets.toml"));
        let error = Config::load_from_paths_with(store, secrets)
            .expect_err("旧格式配置必须被拒绝");
        assert!(
            error.to_string().contains("重新配置"),
            "报错必须提示重新配置: {error}"
        );

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

    #[test]
    fn tool_allowlist_persists_empty_and_nonempty() {
        let (dir, store, secrets) = setup("tool-allowlist", "");
        let mut cfg = Config::load_from_paths_with(store.clone(), secrets.clone()).unwrap();
        cfg.tool_allowlist_enabled = true;
        cfg.tool_allowlist = vec!["read".into()];
        cfg.save_with(&store, &secrets).unwrap();
        let mut cfg = Config::load_from_paths_with(store.clone(), secrets.clone()).unwrap();
        assert!(cfg.tool_allowlist_enabled);
        assert_eq!(cfg.tool_allowlist, vec!["read"]);
        cfg.tool_allowlist.clear();
        cfg.save_with(&store, &secrets).unwrap();
        let cfg = Config::load_from_paths_with(store, secrets).unwrap();
        assert!(cfg.tool_allowlist_enabled);
        assert!(cfg.tool_allowlist.is_empty());
        let _ = std::fs::remove_dir_all(dir);
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

    /// 首次启动（无配置文件）：落一份带注释的 BYOK 预设，且必须能原样解析回
    /// 六字段形状——模板与 serde 结构一旦漂移，这条就红。
    #[test]
    fn first_run_writes_unconfigured_model_once() {
        let dir = temp_dir("first-run");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).expect("mkdir");
        let store = ConfigStore::new(dir.join("config.toml"));
        let secrets = SecretStore::new(dir.join("secrets.toml"));
        assert!(!store.exists(), "前置条件：还没有配置文件");

        let cfg = Config::load_from_paths_with(store.clone(), secrets.clone()).expect("load");
        assert!(cfg.model.is_empty());
        assert!(cfg.base_url.is_empty());
        assert!(cfg.default_model_selection.is_none());
        assert!(cfg.profiles_catalog.providers.is_empty());
        assert_eq!(cfg.wire, qaqh_types::Wire::Chat);
        assert_eq!(cfg.max_tokens, 16_384);
        assert_eq!(cfg.context_length, 383_000);
        assert!(cfg.reasoning_effort.is_empty());
        assert!(cfg.api_key.is_empty(), "预设不得带密钥");
        assert_eq!(
            cfg.compat,
            qaqh_types::EndpointCompat::default(),
            "compat 示例只存在于注释里，解析结果必须是 wire 缺省"
        );

        let text = std::fs::read_to_string(dir.join("config.toml")).expect("read preset");
        assert!(text.starts_with("# QAQ-Harness 配置"), "首启文件要自解释");
        assert!(
            text.contains("providers.toml"),
            "模板要指向 provider 目录真源"
        );
        assert!(!text.contains("\napi_key"), "写出的文件里不得有 api_key");

        // 幂等：再 load 不得改写（用户可能正在编辑它）。
        Config::load_from_paths_with(store.clone(), secrets.clone()).expect("reload");
        assert_eq!(
            std::fs::read_to_string(dir.join("config.toml")).expect("read again"),
            text,
            "预设只写一次"
        );

        // 损坏的用户文件绝不被模板覆盖（exists 判定与解析失败分开）；
        // 解析失败不再静默按缺省跑——明确报错（Spec §3.5），文件原样留盘等人修。
        std::fs::write(dir.join("config.toml"), "this is not = valid toml").expect("write junk");
        let error = Config::load_from_paths_with(store, secrets)
            .expect_err("损坏的配置必须报错而不是静默回退");
        assert!(error.to_string().contains("重新配置"), "{error}");
        assert_eq!(
            std::fs::read_to_string(dir.join("config.toml")).expect("read junk"),
            "this is not = valid toml",
            "损坏文件必须原样留在盘上等人修"
        );
        let _ = std::fs::remove_dir_all(&dir);
    }

}
