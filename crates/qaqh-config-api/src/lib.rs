//! QAQ-Harness 配置契约层（wire DTO）。
//!
//! 前后端共享的**唯一真相**（当前架构见 docs/current/architecture.md；K1-K4 约束）：
//!
//! - K1 叶子 crate：只依赖 serde，不依赖 runtime/daemon/config 引擎；
//!   winui / ratatui / web(axum) 三端只依赖本 crate 即可参与配置读写。
//! - K2 键风格定死 camelCase（`rename_all`），snake/camel 双键 shim 就此终结。
//! - K3 写语义 = JSON Merge Patch（RFC 7386 风格）：[`ConfigPatch`] 只含
//!   `Option` 字段，缺失 = 不动；多端并发编辑互不覆盖，且天然免疫
//!   「未加载字段整包写回」类毒化（2026-08-25 设置页事故 R5）。
//! - K4 fingerprint/字节序属 ringing 传输层，本 crate 零耦合。
//!
//! 映射纪律：引擎侧 [`crate`] 与 `qaqh_config::Config` 的互转**不用**
//! `..Default::default()` 兜底——穷举字面量让"新增字段未同步映射"变成编译错误。
//!
//! 兼容策略（2026-09-15 改）：**不做向前兼容**。读路径的 snake_case `alias`、读模型
//! 的 struct 级 `#[serde(default)]` 均已删除——前后端共进退，不存在「另一个版本的
//! 对方」。删 `default` 不只是减重：留着它会让「旧形状」**静默**变成「一份全默认的
//! 配置」（实测：未知键被忽略 + 缺字段走 default ⇒ 解析成功但值全错），比失败更糟。
//! 详见 `docs/current/architecture.md`。
//!
//! **注意** [`ConfigPatch`] / [`SubagentPatch`] 的 struct 级 `default` **不在此列**：
//! 那是 K3 合并补丁的语义（字段缺失 = 不动），不是兼容。

use serde::{Deserialize, Serialize};

/// 读模型：daemon `config.load` 的完整投影。所有消费者（设置页/Info 面板/
/// TUI/web）从这里取值；`serde(default)` 保证旧 daemon 缺字段时向前兼容。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigDto {
    pub model: String,
    pub base_url: String,
    pub provider_id: String,
    pub endpoint: String,
    pub max_tokens: u64,
    pub context_limit: u64,
    pub reasoning_effort: String,
    pub auto_compact_threshold: f64,
    pub permission_level: u8,
    /// 密钥永不出 daemon：空串 = 未配置、`"****"` = 已配置（掩码）。
    pub api_key: String,
    pub lang: Option<String>,
    pub font_family: String,
    /// None/空 = 跟随系统。
    pub theme: Option<String>,
    /// 语义缺省（开）由**产出侧**决定：`qaqh-config/src/dto.rs::to_dto` 里
    /// `unwrap_or(true)`。读侧不再兜底——见 `dto_rejects_a_partial_payload`。
    pub notifications_enabled: bool,
    pub active_profile: String,
    /// profile 名列表（管理 UI 用；不含敏感字段）。
    pub profiles: Vec<String>,
    pub compliance_enabled: bool,
    pub providers: Vec<ProviderDto>,
    pub subagent: SubagentDto,
    /// MCP 客户端配置（Phase 1 只读；写模型随 workspace 隔离权限重构另立）。
    pub mcp: McpDto,
    /// LSP 客户端配置（M1 只读；写模型另立）。
    pub lsp: LspDto,
    pub tokenizer_path: Option<String>,
}

/// provider 目录项（endpoint 预设树）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ProviderDto {
    pub id: String,
    pub display: String,
    pub endpoints: Vec<EndpointDto>,
}

#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct EndpointDto {
    pub id: String,
    pub display: String,
    pub protocol: String,
    pub base_url: String,
    pub default_model: String,
    pub models: Vec<String>,
    pub stateful: bool,
    pub beta: bool,
}

/// 子代理配置段（读模型）。api_key 语义同顶层：空串/"****" 掩码。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SubagentDto {
    pub model: String,
    pub base_url: String,
    pub api_key: String,
    pub api_key_set: bool,
    pub max_tokens: u64,
    pub timeout_secs: u64,
    /// 空数组 = 全部工具可用（配置语义，非缺省）。
    pub default_tools: Vec<String>,
    /// Maximum subagent tree depth. Default 1.
    #[serde(default = "default_subagent_max_depth")]
    pub max_depth: u64,
    /// Max queued messages from one sender to one recipient. 0 = unlimited.
    #[serde(default = "default_subagent_message_in_flight")]
    pub message_in_flight_per_pair: u64,
    /// Max cumulative outbound message attempts per sender. 0 = unlimited.
    #[serde(default = "default_subagent_message_outbound")]
    pub message_outbound_per_sender: u64,
}

fn default_subagent_max_depth() -> u64 {
    1
}

fn default_subagent_message_in_flight() -> u64 {
    16
}

fn default_subagent_message_outbound() -> u64 {
    1024
}

/// MCP 客户端配置读模型（docs/current/architecture.md）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpDto {
    pub enabled: bool,
    pub idle_shutdown_secs: u64,
    /// 按 server 名排序（BTreeMap → Vec，确定性 wire 顺序）。
    pub servers: Vec<McpServerDto>,
}

/// 单个 MCP server 读模型。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpServerDto {
    pub name: String,
    /// "stdio" | "http"
    pub transport: String,
    pub command: String,
    pub args: Vec<String>,
    /// 原样回显（值可为 `${secret:name}` 占位符；secret 本体在 secrets.toml）。
    pub env: std::collections::BTreeMap<String, String>,
    pub url: String,
    pub headers: std::collections::BTreeMap<String, String>,
    /// None = 全部暴露。
    pub tools: Option<Vec<String>>,
    pub resources_enabled: bool,
    pub default_timeout_secs: u64,
    pub max_concurrent_calls: u32,
    /// stdio 子进程工作目录；空 = 继承 daemon 进程 cwd。
    pub cwd: String,
}

/// LSP 客户端配置读模型（docs/current/architecture.md）。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LspDto {
    pub enabled: bool,
    pub idle_shutdown_secs: u64,
    /// 按 server 名排序（BTreeMap → Vec，确定性 wire 顺序）。
    pub servers: Vec<LspServerDto>,
}

/// 单个 LSP server 读模型。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LspServerDto {
    pub name: String,
    pub command: String,
    pub args: Vec<String>,
    /// 原样回显（值可为 `${secret:name}` 占位符；secret 本体在 secrets.toml）。
    pub env: std::collections::BTreeMap<String, String>,
    pub extensions: Vec<String>,
    pub startup_timeout_secs: u64,
    pub default_timeout_secs: u64,
}

/// 写模型：JSON Merge Patch（K3）。反序列化时缺失即 None = 不动；
/// 序列化时跳过 None，保证 wire 上永不出现 `"field": null`。
///
/// 刻意**不含**：providers/profiles 名录（服务端派生）、active_profile
/// （切换走 `profile.apply`）、api_key_set（服务端派生）。
/// `permissionLevel` 在 patch 中受 1..=4 值域校验（BUG-2026-09-13-15）。
///
/// 特例语义冻结：`apiKey`/`subagentApiKey` 沿用既有守卫——`"****"` 或空串 =
/// 保持现值（显式删除须专用接口）；其余字符串字段 Some(空串) = 显式置空。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ConfigPatch {
    /// 主密钥：仅用户显式输入新值时 Some；掩码 `"****"`/空串 = 保持现值。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub provider_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub endpoint: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub context_limit: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reasoning_effort: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    /// 值域 `[0,1]`；`0` = 关闭自动压缩。
    pub auto_compact_threshold: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub compliance_enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub lang: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub font_family: Option<String>,
    /// None = 不动；Some("") = 跟随系统。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub theme: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub notifications_enabled: Option<bool>,
    /// 权限档位（1=MaxLockdown，2=ReadFree，3=WorkspaceFree，
    /// 4=Unrestricted：显式危险 bypass，普通工具全部自动放行）。
    ///
    /// BUG-2026-09-13-15：历史上该字段刻意缺席写模型，只有
    /// `config.set_permission_level` 单写口；但写口校验缺失时非法档位仍能从
    /// `config.save` 的裸 `permissionLevel` 载荷漏进配置。现在并入 patch 并在
    /// [`Self::validate`] 中做值域校验（非法即拒绝，不落成 Level 4）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permission_level: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokenizer_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub subagent: Option<SubagentPatch>,
}

/// 子代理配置段（写模型），嵌套于 [`ConfigPatch::subagent`]。
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SubagentPatch {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub base_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub api_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_tokens: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
    /// 允许空数组（= 全部工具可用）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_tools: Option<Vec<String>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub max_depth: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_in_flight_per_pair: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message_outbound_per_sender: Option<u64>,
}

impl ConfigPatch {
    /// 边界值域校验（写入前必过；P3 收敛为统一 validate 层的第一块）。
    /// 返回首个违规字段的人类可读错误。
    pub fn validate(&self) -> Result<(), String> {
        // 0.0 合法：= 关闭自动压缩（开关 OFF 时前端显式上报；2026-08-25
        // 真机回归抓出开区间误杀禁用流）。仅拒绝 NaN 与 [0,1] 之外。
        if let Some(t) = self.auto_compact_threshold
            && (t.is_nan() || !(0.0..=1.0).contains(&t))
        {
            return Err(format!(
                "autoCompactThreshold 必须在 [0, 1] 区间（0=关闭），收到 {t}"
            ));
        }
        if let Some(v) = self.max_tokens
            && v == 0
        {
            return Err("maxTokens 必须大于 0".to_string());
        }
        if let Some(v) = self.context_limit
            && v == 0
        {
            return Err("contextLimit 必须大于 0".to_string());
        }
        if let Some(e) = &self.reasoning_effort
            && !matches!(e.as_str(), "low" | "medium" | "high" | "xhigh" | "max")
        {
            return Err(format!(
                "reasoningEffort 仅允许 low|medium|high|xhigh|max，收到 {e}"
            ));
        }
        if let Some(level) = self.permission_level
            && !(1..=4).contains(&level)
        {
            return Err(format!(
                "permissionLevel 仅允许 1..=4（1=MaxLockdown, 2=ReadFree, 3=WorkspaceFree, 4=Unrestricted），收到 {level}"
            ));
        }
        if let Some(sub) = &self.subagent {
            if let Some(v) = sub.max_tokens
                && v == 0
            {
                return Err("subagent.maxTokens 必须大于 0".to_string());
            }
            if let Some(v) = sub.timeout_secs
                && v == 0
            {
                return Err("subagent.timeoutSecs 必须大于 0".to_string());
            }
            if let Some(v) = sub.max_depth
                && !(1..=16).contains(&v)
            {
                return Err(format!("subagent.maxDepth 仅允许 1..=16，收到 {v}"));
            }
        }
        Ok(())
    }

    /// 是否为空补丁（无任何字段要改）——调用方可据此跳过落盘与 reload 广播。
    pub fn is_empty(&self) -> bool {
        *self == Self::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// K2：键风格必须是 camelCase（wire 契约冻结，防回退到 snake 双键时代）。
    #[test]
    fn dto_serializes_camel_case() {
        let dto = ConfigDto {
            base_url: "https://x/v1".into(),
            auto_compact_threshold: 0.95,
            notifications_enabled: true,
            ..Default::default()
        };
        let s = serde_json::to_string(&dto).expect("serialize");
        assert!(s.contains("\"baseUrl\""), "{s}");
        assert!(s.contains("\"autoCompactThreshold\""), "{s}");
        assert!(s.contains("\"notificationsEnabled\""), "{s}");
        assert!(!s.contains("base_url"), "{s}");
    }

    /// 缺字段**必须失败**（原先这条叫 `dto_tolerates_missing_fields`，断言的
    /// 是相反的行为）。按 spec §0b：读模型不接受残缺载荷——因为 struct 级
    /// `#[serde(default)]` 会让「旧形状」**静默**变成「一份全默认的配置」，
    /// 设置页会显示一堆空值而不是报错。失败比错值好。
    #[test]
    fn dto_rejects_a_partial_payload() {
        let err = serde_json::from_value::<ConfigDto>(json!({ "model": "m" }))
            .expect_err("缺字段必须失败");
        assert!(err.to_string().contains("missing field"), "{err}");
    }

    /// 完整读模型 fixture：**当前** wire 形状（K2 camelCase）必须无损解析进
    /// `ConfigDto`。
    ///
    /// 历史：本 fixture 原为 2026-08-25 对运行中 daemon 的实拍（snake_case），
    /// 靠 `alias` 与 struct 级 `#[serde(default)]` 才解析得动。按兼容政策
    /// （spec §0b）这两样都已删除，故 fixture 改写为当前契约形状。
    ///
    /// **顺手补了 `mcp`/`lsp`**：原先它俩根本不在 fixture 里，靠 struct 级 default
    /// 顶成缺省——也就是说这条测试此前**没有真的在验「完整形状」**。补上后它才
    /// 名副其实：字段缺一个就红。
    #[test]
    fn dto_parses_the_full_wire_shape() {
        let payload = json!({
            "model": "ox-alpha-free",
            "baseUrl": "https://opencode.ai/zen/go/v1",
            "providerId": "opencode-go",
            "endpoint": "openai",
            "maxTokens": 96000,
            "contextLimit": 1000000,
            "reasoningEffort": "max",
            "autoCompactThreshold": 0.95,
            "permissionLevel": 4,
            "apiKey": "****",
            "lang": null,
            "fontFamily": "",
            "theme": null,
            "notificationsEnabled": true,
            "activeProfile": "default",
            "profiles": ["default"],
            "complianceEnabled": false,
            "providers": [{
                "id": "opencode-go",
                "display": "OpenCode",
                "endpoints": [{
                    "id": "openai",
                    "display": "OpenAI",
                    "protocol": "openai",
                    "baseUrl": "https://opencode.ai/zen/go/v1",
                    "defaultModel": "",
                    "models": ["ox-alpha-free"],
                    "stateful": false,
                    "beta": false
                }]
            }],
            "subagent": {
                "model": "",
                "baseUrl": "",
                "apiKey": "",
                "apiKeySet": false,
                "maxTokens": 4096,
                "timeoutSecs": 120,
                "defaultTools": ["read"]
            },
            "mcp": { "enabled": false, "idleShutdownSecs": 300, "servers": [] },
            "lsp": { "enabled": false, "idleShutdownSecs": 600, "servers": [] },
            "tokenizerPath": null
        });
        let dto: ConfigDto = serde_json::from_value(payload).expect("完整 wire 形状必须可解析");
        assert_eq!(dto.context_limit, 1_000_000);
        assert_eq!(dto.reasoning_effort, "max");
        assert!((dto.auto_compact_threshold - 0.95).abs() < f64::EPSILON);
        assert_eq!(dto.api_key, "****");
        assert_eq!(dto.subagent.timeout_secs, 120);
        assert_eq!(dto.subagent.default_tools, vec!["read".to_string()]);
        assert!(!dto.mcp.enabled && !dto.lsp.enabled);
    }

    /// K3：空 Patch 序列化为 `{}`——wire 上不发 null、不发未改动字段。
    #[test]
    fn empty_patch_serializes_to_empty_object() {
        let s = serde_json::to_string(&ConfigPatch::default()).expect("serialize");
        assert_eq!(s, "{}");
    }

    #[test]
    fn patch_roundtrip_with_nested_subagent() {
        let patch = ConfigPatch {
            model: Some("m".into()),
            context_limit: Some(1_000_000),
            auto_compact_threshold: Some(0.95),
            subagent: Some(SubagentPatch {
                timeout_secs: Some(240),
                default_tools: Some(vec![]),
                ..Default::default()
            }),
            ..Default::default()
        };
        let v = serde_json::to_value(&patch).expect("serialize");
        assert_eq!(v["model"], json!("m"));
        assert_eq!(v["contextLimit"], json!(1_000_000));
        assert_eq!(v["autoCompactThreshold"], json!(0.95));
        assert_eq!(v["subagent"]["timeoutSecs"], json!(240));
        assert_eq!(v["subagent"]["defaultTools"], json!([]));
        // None 字段不得出现在 wire 上。
        assert!(v.get("baseUrl").is_none());
        assert!(v.get("theme").is_none());
        let back: ConfigPatch = serde_json::from_value(v).expect("deserialize");
        assert_eq!(back, patch);

        // 历史 snake 键**不再**被接受（`alias` 已按 spec §0b 删除）。未知键被
        // serde 静默忽略，而 Patch 的每字段都是 Option——所以结果是「什么都没改」，
        // 不是报错。这正是 Patch 语义（缺失=不动）下的正确结果，不是漏洞。
        let legacy: ConfigPatch = serde_json::from_value(json!({
            "context_limit": 500_000,
            "subagent": { "timeout_secs": 60 }
        }))
        .expect("未知键不报错");
        assert_eq!(legacy.context_limit, None, "snake_case 键已不再生效");
        assert_eq!(legacy.subagent.expect("subagent").timeout_secs, None);
    }

    #[test]
    fn patch_validate_rejects_out_of_range() {
        let bad = ConfigPatch {
            auto_compact_threshold: Some(1.5),
            ..Default::default()
        };
        assert!(bad.validate().is_err());
        let bad_effort = ConfigPatch {
            reasoning_effort: Some("ultra".into()),
            ..Default::default()
        };
        assert!(bad_effort.validate().is_err());
        // 0.0 = 关闭自动压缩，合法（真机回归：开区间曾误杀禁用流）。
        let disabled = ConfigPatch {
            auto_compact_threshold: Some(0.0),
            ..Default::default()
        };
        assert!(disabled.validate().is_ok());
        // permissionLevel 值域 1..=4（BUG-2026-09-13-15：非法档位曾能从
        // config.save 的裸载荷漏进配置、落成 Level 4）。
        let bad_level = ConfigPatch {
            permission_level: Some(5),
            ..Default::default()
        };
        assert!(bad_level.validate().is_err(), "档位 5 必须被拒");
        for level in 1..=4u64 {
            let ok_level = ConfigPatch {
                permission_level: Some(level),
                ..Default::default()
            };
            assert!(ok_level.validate().is_ok(), "档位 {level} 应合法");
        }
        let good = ConfigPatch {
            auto_compact_threshold: Some(0.95),
            reasoning_effort: Some("max".into()),
            max_tokens: Some(96_000),
            ..Default::default()
        };
        assert!(good.validate().is_ok());
        assert!(!good.is_empty());
        assert!(ConfigPatch::default().is_empty());
    }
}
