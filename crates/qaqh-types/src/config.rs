use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;


// ── Config persistence ──

#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PersistentConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lang: Option<String>,

    /// UI 字体（WinUI 壳全局 FontFamily）。`None`/空 = 跟随系统默认字体。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub font_family: Option<String>,

    /// UI 主题偏好：`system` | `light` | `dark` | `dark-gray`；`None`/空 = 跟随系统。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,

    /// 桌面通知开关。`None` = 开启（缺省）。
    #[serde(default)]
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notifications_enabled: Option<bool>,

    // ── Daemon session policy ──
    /// 空闲会话 worker 自动卸载阈值（秒）。`None`/0 = 禁用（缺省）。
    /// 卸载 = 优雅 seal + 从 registry 摘除 + 释放内存；下次输入自动
    /// load_for_resume 恢复。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_idle_unload_secs: Option<u64>,

    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_allowlist_enabled: Option<bool>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tool_allowlist: Option<Vec<String>>,

    // ── Subagent defaults ──
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subagent: Option<PersistentSubagentConfig>,

    // ── Compliance / content filter ──
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compliance_enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compliance_allowlist: Option<Vec<String>>,

    // ── Permission ──
    /// Agent permission tier(2026-10-03 三档制): 1=read-only, 2=workspace-write,
    /// 3=skip-permissions (explicit dangerous bypass; all ordinary tools auto).
    /// 这是唯一 save 落盘的权限键。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_tier: Option<u8>,
    /// 旧 L1–L4 键(废弃):仅 load 时迁移(1/2→read-only, 3→workspace-write,
    /// 4→skip-permissions),save 不再写出,避免数字 3 的新旧歧义越权。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub permission_level: Option<u8>,

    /// Path to a HuggingFace tokenizer.json for accurate token counting.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub tokenizer_path: Option<String>,

    /// exec 工具的默认 shell。`None` / 空 / "auto" = 平台优先级自动探测。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exec: Option<PersistentExecConfig>,

    /// Auto-compact threshold: fraction of context_limit (0.0-1.0).
    /// When total tokens exceed context_limit * threshold, compact is triggered.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub auto_compact_threshold: Option<f64>,

    /// MCP 客户端配置。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mcp: Option<PersistentMcpConfig>,

    /// LSP 客户端配置。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub lsp: Option<PersistentLspConfig>,

    /// 沙箱执行配置（默认启用）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<PersistentSandboxConfig>,
}

/// Persistence-friendly sandbox defaults.
#[derive(Debug, Clone, Serialize, Deserialize, Default, PartialEq, Eq)]
pub struct PersistentSandboxConfig {
    /// Whether sandbox enforcement is enabled. `None` = enabled (default: true).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

/// Persistence-friendly exec defaults.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PersistentExecConfig {
    /// `None` / empty / "auto" = platform priority auto-detection.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_shell: Option<String>,
}

/// Persistence-friendly subagent config with all-Option fields.
/// Stored as a subsection of the main config. All fields are `Option` so that
/// partial overrides work — `None` = inherit from parent agent config.
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PersistentSubagentConfig {
    /// Override model for subagent. `None` = inherit from parent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Override base URL. `None` = inherit from parent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    /// Override API key. `None` = inherit from parent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    /// Override max output tokens. `None` = inherit from parent.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u32>,
    /// Max time in seconds before the subagent is killed. `None` = default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    /// Default tool allowlist for subagents. Empty = all tools available.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_tools: Option<Vec<String>>,
    /// Maximum subagent tree depth. `None` = default (1).
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_depth: Option<u32>,
    /// Max queued messages from one sender to one recipient. `None` = default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_in_flight_per_pair: Option<u64>,
    /// Max cumulative outbound message attempts per sender. `None` = default.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_outbound_per_sender: Option<u64>,
}

/// MCP 客户端配置持久层。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PersistentMcpConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// PR-M3-2：用户级外部配置（Codex/Claude）只读合并开关；缺省 true。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub import_external: Option<bool>,
    /// idle 回收阈值（秒）；None/0 = 常驻不回收。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idle_shutdown_secs: Option<u64>,
    /// 是否把 MCP 资源清单注入历史（trailing developer 消息）。
    /// 缺省 **false**：清单是「环境能力」不是「对话事实」，注入会随清单变化
    /// 追加历史并打断 prefix cache；模型按需调 `mcp list_resources` 能拿到
    /// 同一份本地缓存且不受 20 条封顶。开启仅用于调试/兼容。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub inject_resource_env_block: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub servers: Option<HashMap<String, PersistentMcpServerConfig>>,
}

/// 单个 MCP server（stdio 与 streamable HTTP 互斥；config 声明即信任，D4/D5）。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PersistentMcpServerConfig {
    /// stdio 启动命令（如 "npx"）。与 url 互斥。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
    /// 注入 server 进程的环境变量；值可为 `${secret:name}` 占位符
    /// （secret 本体在 secrets.toml；qaqh-config 不求值）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<HashMap<String, String>>,
    /// streamable HTTP endpoint（M3）。与 command 互斥。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub headers: Option<HashMap<String, String>>,
    /// 工具白名单（server 侧原始名）；None = 全部暴露。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tools: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resources: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_timeout_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_concurrent_calls: Option<u32>,
    /// stdio 子进程工作目录；None/空 = 继承 daemon 进程 cwd。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
}

/// LSP 客户端配置持久层。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PersistentLspConfig {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// idle 回收阈值（秒）；None/0 = 常驻不回收。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub idle_shutdown_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub servers: Option<HashMap<String, PersistentLspServerConfig>>,
}

/// 单个 LSP server（stdio 子进程；config 声明即信任，D4 同 mcp）。
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct PersistentLspServerConfig {
    /// stdio 启动命令（如 "rust-analyzer"）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub command: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub args: Option<Vec<String>>,
    /// 注入 server 进程的环境变量；值可为 `${secret:name}` 占位符
    /// （secret 本体在 secrets.toml `[secrets.mcp]` 段复用；qaqh-config 不求值）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub env: Option<HashMap<String, String>>,
    /// 路由键：该 server 负责的文件扩展名（无点小写，如 ["rs"]）；
    /// None/空 = 不参与路由（仅允许显式 server 参数直调，M2）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extensions: Option<Vec<String>>,
    /// 启动超时（秒；含进程拉起 + initialize + 索引门）；None = 30。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub startup_timeout_secs: Option<u64>,
    /// 单次 LSP 请求默认超时（秒）；None = 30。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_timeout_secs: Option<u64>,
}

// ── Profile / Preferences ──
/// One BYOK endpoint record: the six fields the user owns, plus the optional
/// request-shape notes for endpoints that deviate from their wire.
// ── ConfigStore: unified config I/O with atomic writes ──

/// Unified config I/O with atomic writes.
/// Writes use a temp-file + rename pattern to prevent corruption from
/// partial writes.
#[derive(Debug, Clone)]
pub struct ConfigStore {
    path: PathBuf,
}

impl ConfigStore {
    /// Create a ConfigStore for a specific file path.
    pub fn new(path: PathBuf) -> Self {
        Self { path }
    }

    /// Create a ConfigStore for the default config.toml location.
    pub fn default_location() -> Self {
        Self::new(crate::platform::config_path())
    }

    /// 配置文件路径（P2-1 文件轮询器等只读场景用）。
    pub fn path(&self) -> &std::path::Path {
        &self.path
    }

    /// Check whether the config file exists on disk.
    pub fn exists(&self) -> bool {
        self.path.exists()
    }

    /// Load and deserialize the config. Returns `None` if the file doesn't
    /// exist or is invalid TOML.
    pub fn load(&self) -> Option<PersistentConfig> {
        let data = std::fs::read_to_string(&self.path).ok()?;
        toml::from_str(&data).ok()
    }

    /// Atomically write the config to disk using temp-file + rename.
    /// Returns `true` on success.
    pub fn save(&self, config: &PersistentConfig) -> bool {
        match toml::to_string_pretty(config) {
            Ok(content) => self.write_content(&content),
            Err(e) => {
                eprintln!("ConfigStore: serialization failed: {e}");
                false
            }
        }
    }

    /// 原子写入给定的 TOML 文本（首启预设模板用；与 [`Self::save`] 同一
    /// temp-file + rename 语义）。
    pub fn write_content(&self, content: &str) -> bool {
        let tmp = self.path.with_extension("toml.tmp");
        if let Some(parent) = self.path.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            eprintln!(
                "ConfigStore: create_dir_all({}) failed: {e}",
                parent.display()
            );
            return false;
        }
        if let Err(e) = std::fs::write(&tmp, &content) {
            eprintln!("ConfigStore: write({}) failed: {e}", tmp.display());
            return false;
        }
        if let Err(e) = std::fs::rename(&tmp, &self.path) {
            eprintln!(
                "ConfigStore: rename({} -> {}) failed: {e}",
                tmp.display(),
                self.path.display()
            );
            return false;
        }
        true
    }
}

// ── ProfileStore: independent model presets store with atomic writes ──

