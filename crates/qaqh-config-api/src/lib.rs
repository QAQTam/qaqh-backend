//! QAQ-Harness 配置契约层（wire DTO）。
//! 前后端共享的**唯一真相**：
//! - K1 叶子 crate：只依赖 serde，不依赖 runtime/daemon/config 引擎；
//!   winui / ratatui / web(axum) 三端只依赖本 crate 即可参与配置读写。
//! - K2 键风格定死 camelCase（`rename_all`），snake/camel 双键 shim 就此终结。
//! - K3 写语义 = JSON Merge Patch（RFC 7386 风格）：[`ConfigPatch`] 只含
//!   `Option` 字段，缺失 = 不动；多端并发编辑互不覆盖，且天然免疫
//!   「未加载字段整包写回」类毒化（2026-08-25 设置页事故 R5）。
//! - K4 fingerprint/字节序属 ringing 传输层，本 crate 零耦合。
//! 映射纪律：引擎侧 [`crate`] 与 `qaqh_config::Config` 的互转**不用**
//! `..Default::default()` 兜底——穷举字面量让"新增字段未同步映射"变成编译错误。
//! 兼容策略（2026-09-15 改）：**不做向前兼容**。读路径的 snake_case `alias`、读模型
//! 的 struct 级 `#[serde(default)]` 均已删除——前后端共进退，不存在「另一个版本的
//! 对方」。删 `default` 不只是减重：留着它会让「旧形状」**静默**变成「一份全默认的
//! 配置」（实测：未知键被忽略 + 缺字段走 default ⇒ 解析成功但值全错），比失败更糟。
//! **注意** [`ConfigPatch`] 的 struct 级 `default` **不在此列**：
//! 那是 K3 合并补丁的语义（字段缺失 = 不动），不是兼容。
//! 子代理配置段（`SubagentDto`/`SubagentPatch`）已随 ADR-0018 退役：
//! 子代理=会话，预算/工具面与父同律，限额是内核常量，不再经设置面暴露。

#![cfg_attr(not(test), forbid(unsafe_code))]
use serde::{Deserialize, Serialize};


/// 读模型：daemon `config.load` 的完整投影。所有消费者（设置页/Info 面板/
/// TUI/web）从这里取值；不做向前兼容——缺字段即解析失败（见 crate 级兼容策略）。
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ConfigDto {
    pub auto_compact_threshold: f64,
    pub permission_level: u8,
    pub lang: Option<String>,
    pub font_family: String,
    /// None/空 = 跟随系统。
    pub theme: Option<String>,
    /// 语义缺省（开）由**产出侧**决定：`qaqh-config/src/dto.rs::to_dto` 里
    /// `unwrap_or(true)`。读侧不再兜底——见 `dto_rejects_a_partial_payload`。
    pub notifications_enabled: bool,
    /// Goal 模式配置段(ADR-0015)。
    pub goals: GoalsDto,
    /// MCP 客户端配置（Phase 1 只读；写模型随 workspace 隔离权限重构另立）。
    pub mcp: McpDto,
    /// LSP 客户端配置（M1 只读；写模型另立）。
    pub lsp: LspDto,
    /// Path to a HuggingFace tokenizer.json。
    /// 空 = 启发式估算兜底（agent 状态层的 `<heuristic>`）；变更需**重启 daemon**
    /// （tokenizer 是 qaqh-types 的进程级 `OnceLock`，仅在 load 时初始化一次）。
    pub tokenizer_path: Option<String>,
    /// exec 工具默认执行配置（热字段；`exec.default_shell` 被 exec 真正消费）。
    pub exec: ExecDto,
    /// 沙箱执行配置（读模型）。
    pub sandbox: SandboxDto,
    /// 空闲会话 worker 自动卸载阈值（秒）。0 = 禁用（缺省）；由 daemon 周期任务读取。
    pub session_idle_unload_secs: u64,
    /// Global ceiling on model-visible and executable tools.
    pub tool_allowlist_enabled: bool,
    /// Exact registered names; when enabled, an empty list disables all tools.
    pub tool_allowlist: Vec<String>,
}

/// Goal 模式配置段(读模型,ADR-0015)。
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct GoalsDto {
    /// goal 模式总开关;语义缺省 true 由产出侧决定。
    pub enabled: bool,
    /// 单 goal token 预算上限,兼作 create_goal 的默认预算。
    /// `null` = 无上限/无默认预算。
    pub max_token_budget: Option<u64>,
}

/// exec 工具默认执行配置（读模型）。
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct ExecDto {
    /// `None` / 空 / "auto" = 平台优先级自动探测。
    pub default_shell: Option<String>,
}

/// 沙箱执行配置（读模型）。
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct SandboxDto {
    /// 沙箱是否启用（默认 true）。关闭后命令在无隔离环境下执行。
    pub enabled: bool,
}

/// MCP 客户端配置读模型。
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct McpDto {
    pub enabled: bool,
    pub idle_shutdown_secs: u64,
    /// 按 server 名排序（BTreeMap → Vec，确定性 wire 顺序）。
    pub servers: Vec<McpServerDto>,
}

/// 单个 MCP server 读模型。
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
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

/// LSP 客户端配置读模型。
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase")]
pub struct LspDto {
    pub enabled: bool,
    pub idle_shutdown_secs: u64,
    /// 按 server 名排序（BTreeMap → Vec，确定性 wire 顺序）。
    pub servers: Vec<LspServerDto>,
}

/// 单个 LSP server 读模型。
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
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
/// 刻意**不含**：端点族字段（model/baseUrl/wire/credential/apiKey 等）——
/// 端点与凭据的唯一真源是 providers.toml 目录，经 provider.* 命令面维护，
/// 全局补丁写了也不生效；本写模型只承载全局策略。
/// `permissionLevel` 受 1..=4 值域校验（BUG-2026-09-13-15）。
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ConfigPatch {
    #[serde(skip_serializing_if = "Option::is_none")]
    /// 值域 `[0,1]`；`0` = 关闭自动压缩。
    pub auto_compact_threshold: Option<f64>,
    // 客户端本地偏好（theme / lang / fontFamily / notificationsEnabled）刻意
    // **不出现在写模型里**：它们不是 daemon 设置，只是桌面壳的本地展示偏好，
    // 仍保留在读模型 [`ConfigDto`] 供其它客户端读取。
    /// 权限档位（1=read-only，2=workspace-write，
    /// 3=skip-permissions：显式危险 bypass，普通工具全部自动放行；
    /// 4=sandbox-run：沙箱强制隔离运行，常规命令自动放行）。
    /// BUG-2026-09-13-15：历史上该字段刻意缺席写模型，只有
    /// `config.set_permission_level` 单写口；但写口校验缺失时非法档位仍能从
    /// `config.save` 的裸 `permissionLevel` 载荷漏进配置。现在并入 patch 并在
    /// [`Self::validate`] 中做值域校验（非法即拒绝，1..=4 之外不落盘）。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub permission_level: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tokenizer_path: Option<String>,
    /// exec 工具默认 shell。空串 / "auto" = 平台自动探测。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub exec: Option<ExecPatch>,
    /// 沙箱执行配置。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<SandboxPatch>,
    /// 空闲会话卸载阈值（秒）；0 = 禁用。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub session_idle_unload_secs: Option<u64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_allowlist_enabled: Option<bool>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub tool_allowlist: Option<Vec<String>>,
    /// Goal 模式配置(ADR-0015)。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub goals: Option<GoalsPatch>,
}

/// 沙箱执行配置（写模型），嵌套于 [`ConfigPatch::sandbox`]。
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct SandboxPatch {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
}

/// Goal 模式配置段(写模型,ADR-0015),嵌套于 [`ConfigPatch::goals`]。
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct GoalsPatch {
    #[serde(skip_serializing_if = "Option::is_none")]
    pub enabled: Option<bool>,
    /// 双层 Option:外层缺席 = 不动;`null` = 清除预算上限;数值 = 设置(必须 > 0)。
    /// 这是 K3「wire 永不出现 null」纪律的**有意例外**:清除是显式语义,
    /// 不是缺省兜底。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_token_budget: Option<Option<u64>>,
}

/// exec 工具默认执行配置（写模型），嵌套于 [`ConfigPatch::exec`]。
#[cfg_attr(feature = "ts", derive(ts_rs::TS), ts(export, export_to = "qaqh/"))]
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
#[serde(rename_all = "camelCase", default)]
pub struct ExecPatch {
    /// 空串 / "auto" = 平台自动探测；其它值 = 指定默认 shell。
    #[serde(skip_serializing_if = "Option::is_none")]
    pub default_shell: Option<String>,
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
        if let Some(level) = self.permission_level
            && !(1..=4).contains(&level)
        {
            return Err(format!(
                "permissionLevel 仅允许 1..=4（1=read-only, 2=workspace-write, 3=skip-permissions, 4=sandbox-run），收到 {level}"
            ));
        }
        if let Some(goals) = &self.goals
            && let Some(Some(v)) = goals.max_token_budget
            && v == 0
        {
            return Err("goals.maxTokenBudget 必须大于 0（null = 清除上限）".to_string());
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
            auto_compact_threshold: 0.95,
            notifications_enabled: true,
            ..Default::default()
        };
        let s = serde_json::to_string(&dto).expect("serialize");
        assert!(s.contains("\"autoCompactThreshold\""), "{s}");
        assert!(s.contains("\"notificationsEnabled\""), "{s}");
    }

    /// 缺字段**必须失败**（原先这条叫 `dto_tolerates_missing_fields`，断言的
    /// 是相反的行为）。按 spec §0b：读模型不接受残缺载荷——因为 struct 级
    /// `#[serde(default)]` 会让「旧形状」**静默**变成「一份全默认的配置」，
    /// 设置页会显示一堆空值而不是报错。失败比错值好。
    #[test]
    fn dto_rejects_a_partial_payload() {
        let err = serde_json::from_value::<ConfigDto>(json!({ "lang": "zh" }))
            .expect_err("缺字段必须失败");
        assert!(err.to_string().contains("missing field"), "{err}");
    }

    /// 完整读模型 fixture：字段形状为当前契约形状（K2 camelCase），必须无损
    /// 解析进 `ConfigDto`。端点族字段（model/baseUrl/wire/credential/apiKey/
    /// profiles）已随全局配置退役——真源是 providers.toml 目录。
    #[test]
    fn dto_parses_the_full_wire_shape() {
        let payload = json!({
            "autoCompactThreshold": 0.95,
            "permissionLevel": 4,
            "lang": null,
            "fontFamily": "",
            "theme": null,
            "notificationsEnabled": true,
            "goals": { "enabled": true, "maxTokenBudget": null },
            "mcp": { "enabled": false, "idleShutdownSecs": 300, "servers": [] },
            "lsp": { "enabled": false, "idleShutdownSecs": 600, "servers": [] },
            "tokenizerPath": null,
            "exec": { "defaultShell": "bash" },
            "sandbox": { "enabled": true },
            "sessionIdleUnloadSecs": 900,
            "toolAllowlistEnabled": false,
            "toolAllowlist": []
        });
        let dto: ConfigDto = serde_json::from_value(payload).expect("完整 wire 形状必须可解析");
        assert_eq!(dto.permission_level, 4);
        assert!((dto.auto_compact_threshold - 0.95).abs() < f64::EPSILON);
        assert!(dto.goals.enabled);
        assert_eq!(dto.goals.max_token_budget, None);
        assert_eq!(dto.exec.default_shell.as_deref(), Some("bash"));
        assert!(dto.sandbox.enabled);
        assert_eq!(dto.session_idle_unload_secs, 900);
        assert!(!dto.mcp.enabled && !dto.lsp.enabled);
    }

    /// K3：空 Patch 序列化为 `{}`——wire 上不发 null、不发未改动字段。
    #[test]
    fn empty_patch_serializes_to_empty_object() {
        let s = serde_json::to_string(&ConfigPatch::default()).expect("serialize");
        assert_eq!(s, "{}");
    }

    #[test]
    fn patch_roundtrip_with_nested_goals() {
        let patch = ConfigPatch {
            auto_compact_threshold: Some(0.95),
            goals: Some(GoalsPatch {
                enabled: Some(true),
                max_token_budget: Some(Some(4096)),
            }),
            ..Default::default()
        };
        let v = serde_json::to_value(&patch).expect("serialize");
        assert_eq!(v["autoCompactThreshold"], json!(0.95));
        assert_eq!(v["goals"]["enabled"], json!(true));
        assert_eq!(v["goals"]["maxTokenBudget"], json!(4096));
        // None 字段不得出现在 wire 上。
        assert!(v.get("model").is_none());
        assert!(v.get("theme").is_none());
        let back: ConfigPatch = serde_json::from_value(v).expect("deserialize");
        assert_eq!(back, patch);

        // 历史 snake 键**不再**被接受（`alias` 已按 spec §0b 删除）。未知键被
        // serde 静默忽略，而 Patch 的每字段都是 Option——所以结果是「什么都没改」，
        // 不是报错。这正是 Patch 语义（缺失=不动）下的正确结果，不是漏洞。
        let legacy: ConfigPatch = serde_json::from_value(json!({
            "max_tokens": 96_000,
            "goals": { "max_token_budget": 60 }
        }))
        .expect("未知键不报错");
        assert_eq!(legacy.auto_compact_threshold, None, "snake_case 键已不再生效");
        // 外层 goals 键合法 → Some(全 None)；内层 snake 键被忽略 = 什么都没改。
        assert_eq!(legacy.goals, Some(GoalsPatch::default()));
    }

    #[test]
    fn patch_validate_rejects_out_of_range() {
        let bad = ConfigPatch {
            auto_compact_threshold: Some(1.5),
            ..Default::default()
        };
        assert!(bad.validate().is_err());
        // 0.0 = 关闭自动压缩，合法（真机回归：开区间曾误杀禁用流）。
        let disabled = ConfigPatch {
            auto_compact_threshold: Some(0.0),
            ..Default::default()
        };
        assert!(disabled.validate().is_ok());
        // permissionLevel 值域 1..=4（1=read-only, 2=workspace-write, 3=skip-permissions, 4=sandbox-run）。
        let bad_level = ConfigPatch {
            permission_level: Some(5),
            ..Default::default()
        };
        assert!(bad_level.validate().is_err(), "档位 5 必须被拒");
        let zero_level = ConfigPatch {
            permission_level: Some(0),
            ..Default::default()
        };
        assert!(zero_level.validate().is_err(), "档位 0 必须被拒");
        for level in 1..=4u64 {
            let ok_level = ConfigPatch {
                permission_level: Some(level),
                ..Default::default()
            };
            assert!(ok_level.validate().is_ok(), "档位 {level} 应合法");
        }
        let good = ConfigPatch {
            auto_compact_threshold: Some(0.95),
            ..Default::default()
        };
        assert!(good.validate().is_ok());
        assert!(!good.is_empty());
        assert!(ConfigPatch::default().is_empty());
    }
}
