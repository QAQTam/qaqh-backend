//! ToolManager: tool registration, lookup, routing, and cancellation.
//!
//! Since v5: per-call execution metadata (ToolExecMeta) and cumulative
//! stats (ToolStats) are returned to the caller instead of being lost
//! to stderr. The caller (the runtime agent tool boundary, e.g.
//! `crates/qaqh-runtime/src/agent/tool_runtime.rs`) acts as a forwarding layer
//! that pushes these into UI events.

use std::collections::BTreeMap;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::Duration;

#[cfg(any(test, feature = "test-harness"))]
use crate::probe::ProbeTool;
use crate::tool_api::{
    DynamicDispatch, DynamicToolAdapter, ErasedTool, OutputBudget, ToolCapabilities,
    ToolDescriptor, ToolExposure, ToolName, ToolSource, TypedTool, TypedToolAdapter,
};
use crate::{SafetyVerdict, ToolRisk};

// ── Execution metadata ──

#[derive(Clone, Debug)]
pub struct ToolExecMeta {
    pub name: String,
    pub elapsed_ms: u64,
    pub output_size: usize,
    pub success: bool,
    pub args_summary: String,
}

#[derive(Clone, Debug)]
pub struct ToolExecReport {
    pub content: String,
    pub success: bool,
    pub meta: ToolExecMeta,
    pub files_affected: Vec<String>,
}

#[derive(Clone, Debug, Default)]
pub struct ToolStats {
    pub calls_total: u32,
    pub failures: u32,
    pub files_read: Vec<String>,
    pub files_written: Vec<String>,
}

/// 统一注册项：`ErasedTool` 是描述与执行面的唯一载体。
pub struct RegisteredTool {
    /// P2(d)：工具组 crate 测试经 `ToolManager::builtin` 读取描述符。
    pub descriptor: ToolDescriptor,
    pub(crate) erased: Arc<dyn ErasedTool>,
}

impl RegisteredTool {
    pub(crate) fn tool_def(&self) -> qaqh_types::ToolDef {
        let descriptor = self.erased.descriptor();
        qaqh_types::ToolDef {
            call_type: "function".into(),
            function: qaqh_types::ToolFunction {
                name: descriptor.name.as_str().to_owned(),
                description: descriptor.description.clone(),
                parameters: descriptor.input_schema.clone(),
            },
        }
    }
}

pub struct ToolManager {
    pub(crate) builtins: BTreeMap<String, RegisteredTool>,

    allowed: Option<Vec<String>>,
    /// PR-M2-2：set_allowed 的原始输入（未过 known 过滤）——动态层重建后
    /// 重应用用（观察项 ①：MCP refresh 换名后 custom 名单仍生效）。
    allowed_raw: Option<Vec<String>>,
    /// 动态工具（MCP；设计 §5.3）：完整前缀名 → 统一 ErasedTool 注册项。
    /// 动态描述/schema 是运行期 String，由 owned descriptor 承载（描述来自
    /// MCP server 运行期，非 `&'static str`）。
    dynamic: BTreeMap<String, RegisteredTool>,
    inflight_tasks: BTreeMap<String, Arc<AtomicBool>>,
    stats_total: u32,
    stats_failures: u32,
    files_read: Vec<String>,
    files_written: Vec<String>,
    /// 工具作者声明的展示投影（09-18 展示契约 §3.4）。未注册 = 保持 None，
    /// client 完整回退旧字段（H16）。
    display_projectors: BTreeMap<String, crate::tool_api::ToolDisplayFn>,
    /// P3-2：动态注册名 → dispatcher fn 指针（聚合入口复用 E-5 单一 dispatcher）。
    dynamic_dispatches: BTreeMap<String, DynamicDispatch>,
}

/// 动态工具名前缀（S2：`mcp__{server}__{tool}`；与内置 20 工具零碰撞）。
pub const MCP_DYNAMIC_PREFIX: &str = "mcp__";

/// 动态工具描述截断上限（设计 §5.3：防上下文膨胀）。
pub const DYNAMIC_DESCRIPTION_LIMIT: usize = 2048;

/// 描述截断标记（追加在被截断文本尾部）。
const DESCRIPTION_TRUNCATION_MARKER: &str = " [truncated]";

/// 按字符边界把描述截断到 [`DYNAMIC_DESCRIPTION_LIMIT`] 内，追加截断标记。
/// 未超限原样返回。
pub(crate) fn truncate_description(description: &str) -> String {
    if description.len() <= DYNAMIC_DESCRIPTION_LIMIT {
        return description.to_owned();
    }
    let budget = DYNAMIC_DESCRIPTION_LIMIT.saturating_sub(DESCRIPTION_TRUNCATION_MARKER.len());
    let mut end = budget;
    while end > 0 && !description.is_char_boundary(end) {
        end -= 1;
    }
    match description.get(..end) {
        // 字符边界回退后必命中；get 而非切片（工作区 string_slice=deny 红线）。
        Some(head) => format!("{head}{DESCRIPTION_TRUNCATION_MARKER}"),
        None => DESCRIPTION_TRUNCATION_MARKER.to_owned(),
    }
}

/// 投影纯函数：MCP server 工具 → 动态注册条目（设计 §5.3/S2）。
///
/// - 命名：`mcp__{server}__{tool}`（server 名已在配置层校验 `[a-z0-9_-]+`；
///   tool 名为 server 侧原文——完整名唯一的碰撞防御在
///   [`ToolManager::register_dynamic`]）；
/// - description 截断 2KB（字符边界 + 标记）；schema 直通（MCP inputSchema
///   与 QAQH `ToolFunction.parameters` 同为 JSON Schema，零转换）；
/// - risk 固定 [`ToolRisk::Administrative`]（MCP 调用不属本地安全模型，
///   见 DynamicTool 注）。
#[allow(clippy::too_many_arguments)]
pub fn build_dynamic_tool(
    server: &str,
    tool_name: &str,
    description: &str,
    schema: serde_json::Value,
    dispatch: DynamicDispatch,
    category: crate::permission::ToolCategory,
    default_timeout: Duration,
) -> (String, DynamicTool) {
    let name = format!("{MCP_DYNAMIC_PREFIX}{server}__{tool_name}");
    let def = qaqh_types::ToolDef {
        call_type: "function".into(),
        function: qaqh_types::ToolFunction {
            name: name.clone(),
            description: truncate_description(description),
            parameters: schema,
        },
    };
    (
        name.clone(),
        DynamicTool {
            def,
            effective_name: Some(tool_name.to_owned()),
            dispatch,
            category,
            risk: ToolRisk::Administrative,
            default_timeout,
        },
    )
}

/// 根据动态注册名推断来源；未知前缀按扩展处理。
fn dynamic_source(name: &str) -> ToolSource {
    if name.starts_with(MCP_DYNAMIC_PREFIX) {
        ToolSource::Mcp
    } else if name.starts_with("lsp__") {
        ToolSource::Lsp
    } else {
        ToolSource::Extension
    }
}

/// 动态工具注册条目（设计 §5.3/E-5；PR-M1-4）。
///
/// 与 typed 工具的差异：模型面（[`qaqh_types::ToolDef`]）与路由元数据合一，
/// description 为自有 String（server 侧动态文本，经 2KB 截断）。
/// `dispatch` 只作为注册输入；注册后包装为 [`DynamicToolAdapter`]，refresh
/// 换 def 不影响在飞调用。
#[derive(Clone)]
pub struct DynamicTool {
    /// 模型面（`mcp__{server}__{tool}` 命名 + schema 直通 + 截断后描述）。
    pub def: qaqh_types::ToolDef,
    /// 上游工具原名；动态注册条目由此写入，静态工具保持 None。
    pub effective_name: Option<String>,
    /// 路由 dispatcher（MCP 全体工具指向同一个 fn 指针，E-5）；typed 契约
    /// 见 [`DynamicDispatch`]。
    pub dispatch: DynamicDispatch,
    /// 能力类别（S3：stdio=Exec / http=Net）——权限决策单一事实源。
    pub category: crate::permission::ToolCategory,
    /// 安全档位：MCP 调用不属本地安全模型（副作用在 server 进程内），
    /// 取无条件 Allow 的 [`ToolRisk::Administrative`]；真实风险由 category
    /// 驱动的权限层/沙箱（actor.rs 旗标路径）处理。
    pub risk: ToolRisk,
    /// 来自 server 配置 `default_timeout_secs`（1..=3600）。
    pub default_timeout: Duration,
}

// ── Three-phase execution for parallel tool support ──

/// Prepared tool call, ready for execution without holding the manager lock.
pub(crate) struct PreparedCall {
    pub(crate) id: String,
    pub(crate) name: String,
    pub(crate) effective_tool_name: Option<String>,
    pub(crate) executor: Arc<dyn ErasedTool>,
    /// 生效超时：调用方显式值覆盖 descriptor 默认值。typed 执行面据此定稿
    /// [`crate::tool_api::ToolCallContext::timeout`]（显式上下文在准入侧构造，
    /// 超时未注入时为零值，此处补默认档）。
    pub(crate) effective_timeout: Duration,
    pub(crate) audit_args: serde_json::Value,
}

impl Default for ToolManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolManager {
    /// P2(d)：工具组 crate 的测试读取注册项的受限访问器
    /// （`builtins` 本体保持 pub(crate)，注册/注销路径不经此处）。
    pub fn builtin(&self, name: &str) -> Option<&RegisteredTool> {
        self.builtins.get(name)
    }
}

impl qaqh_tool_core::tool_api::RegistersTyped for ToolManager {
    fn register_typed_tool<T: qaqh_tool_core::tool_api::TypedTool + 'static>(&mut self, tool: T) {
        self.register_typed(tool)
    }

    fn register_display_fn(
        &mut self,
        name: &str,
        projector: qaqh_tool_core::tool_api::ToolDisplayFn,
    ) {
        self.register_display(name, projector)
    }
}

impl ToolManager {
    pub fn new() -> Self {
        Self {
            builtins: BTreeMap::new(),
            allowed: None,
            allowed_raw: None,
            dynamic: BTreeMap::new(),
            inflight_tasks: BTreeMap::new(),
            stats_total: 0,
            stats_failures: 0,
            files_read: Vec::new(),
            files_written: Vec::new(),
            display_projectors: BTreeMap::new(),
            dynamic_dispatches: BTreeMap::new(),
        }
    }

    /// 注册测试探针（`test-harness` 门控；见 [`crate::probe`]）。生产工具一律
    /// 走 [`Self::register_typed`]，本方法不出现在生产构建里。
    #[cfg(any(test, feature = "test-harness"))]
    pub fn register_probe(&mut self, probe: ProbeTool) {
        let key = probe.key.clone();
        let descriptor = ErasedTool::descriptor(&probe);
        descriptor
            .validate()
            .unwrap_or_else(|error| panic!("invalid probe descriptor for {key}: {error}"));
        self.builtins.insert(
            key,
            RegisteredTool {
                descriptor,
                erased: Arc::new(probe),
            },
        );
    }

    /// 注册新 typed 工具。描述符由 meta + 类型生成 schema 组装，执行统一走 [`ErasedTool`]；
    /// capabilities 由内置迁移表（[`crate::tool_capabilities`]）按名集中注入。
    pub fn register_typed<T>(&mut self, tool: T)
    where
        T: TypedTool + 'static,
    {
        let adapter = TypedToolAdapter::new(tool);
        let mut descriptor = adapter.descriptor();
        if let Some(capabilities) =
            crate::tool_capabilities::builtin_capabilities(descriptor.name.as_str())
        {
            descriptor.capabilities = capabilities;
        }
        descriptor
            .validate()
            .unwrap_or_else(|error| panic!("invalid typed tool descriptor: {error}"));
        let key = descriptor.name.as_str().to_owned();
        self.builtins.insert(
            key,
            RegisteredTool {
                descriptor,
                erased: Arc::new(adapter),
            },
        );
    }

    /// 注册工具作者声明的展示投影（09-18 展示契约 §3.4）。
    ///
    /// 与 `builtins` 分离：投影是展示面扩展，未注册的工具在 timeline 上保持
    /// `display = None`，由 client 回退旧字段。
    pub fn register_display(&mut self, name: &str, projector: crate::tool_api::ToolDisplayFn) {
        self.display_projectors.insert(name.to_string(), projector);
    }

    /// 按工具名调用展示投影；未声明投影时返回 `None`。
    ///
    /// MCP per-server 工具是唯一的 canonical fallback：server 工具面任意，无法
    /// 逐个手写投影。fallback 用完整注册名保住卡片身份，正文保持 `None` 以
    /// 避免与 `TimelineTool.output` 双写；summary 携带紧凑 args。
    pub fn project_display(
        &self,
        name: &str,
        args: &serde_json::Value,
        output: &str,
    ) -> Option<crate::tool_api::ToolDisplay> {
        if let Some(projector) = self.display_projectors.get(name) {
            return Some(projector(args, output));
        }
        (self.dynamic.contains_key(name) && name.starts_with(crate::MCP_DYNAMIC_PREFIX))
            .then(|| crate::display::project_mcp_fallback(name, args, output))
    }

    /// 注册动态工具（MCP 投影入口；仅回合边界由 actor 调用——无并发写面）。
    ///
    /// 碰撞拒绝（设计 §5.3）：与内置词汇表（`builtins`）或已注册动态名重名
    /// → Err 且**不**写入（防模型面膨胀出歧义名）。刷新批次应先
    /// [`Self::clear_dynamic`] 再逐条注册（MCP 名带 `mcp__` 前缀，与内置
    /// 零碰撞；此处防御面向未来形态）。
    pub fn register_dynamic(&mut self, name: String, tool: DynamicTool) -> Result<(), String> {
        if self.builtins.contains_key(&name) || self.dynamic.contains_key(&name) {
            return Err(format!("dynamic tool name collides: {name:?}"));
        }
        let descriptor = ToolDescriptor {
            name: ToolName::new(&name).map_err(|error| error.to_string())?,
            display_name: tool.effective_name.clone(),
            description: tool.def.function.description.clone(),
            input_schema: tool.def.function.parameters.clone(),
            output_schema: serde_json::json!({"type": "object"}),
            category: tool.category,
            risk: tool.risk.clone(),
            default_timeout: tool.default_timeout,
            exposure: ToolExposure::Direct,
            source: dynamic_source(&name),
            output_budget: OutputBudget::default(),
            capabilities: ToolCapabilities::default(),
        };
        self.dynamic_dispatches.insert(name.clone(), tool.dispatch);
        let adapter = DynamicToolAdapter::new(descriptor.clone(), tool.dispatch);
        self.dynamic.insert(
            name,
            RegisteredTool {
                descriptor,
                erased: Arc::new(adapter),
            },
        );
        Ok(())
    }

    /// 清空动态层（tools/list_changed 或重连后的全量重建，M2 起使用）。
    pub fn clear_dynamic(&mut self) {
        self.dynamic.clear();
        self.dynamic_dispatches.clear();
    }

    /// 动态层重建后重应用 allowlist（PR-M2-2 观察项 ①）。
    ///
    /// `set_allowed` 存的是**过滤后**静态快照：MCP refresh 换名后（clear +
    /// 重新注册），快照里旧 MCP 名失效/新名缺失 → custom 模式的 MCP 工具
    /// 静默消失。用 raw 原始名单重新过滤即可恢复（raw 含 MCP 名但当时
    /// 未注册的情况，在新动态层下变为有效）。
    pub fn reapply_allowed_after_dynamic_change(&mut self) {
        if let Some(raw) = self.allowed_raw.clone() {
            self.set_allowed(raw);
        }
    }

    /// 动态层当前名单（测试/指标用）。
    pub fn dynamic_names(&self) -> Vec<String> {
        self.dynamic.keys().cloned().collect()
    }

    pub fn lookup(&self, name: &str) -> Option<&ToolDescriptor> {
        self.builtins
            .get(name)
            .or_else(|| self.dynamic.get(name))
            .map(|tool| &tool.descriptor)
    }

    /// 查工具能力类别（权限决策单一事实源；内置 + 动态两层）。
    pub fn category_of(&self, name: &str) -> Option<crate::permission::ToolCategory> {
        self.lookup(name).map(|descriptor| descriptor.category)
    }

    /// 运行时重设工具白名单（工具模式切换的入口）：空列表 = 全量（标准模式）。
    /// 与 `apply_init` 共享 known 过滤语义；**不**改 session（区别于 apply_init）。
    pub fn set_allowed(&mut self, allowed_tools: Vec<String>) {
        // 防御：剔除不在注册表中的工具名。工具冻结期间改名/删除后（如
        // read_file→read、edit_file_v2→edit、web→web_fetch、search 移除），
        // 旧配置的 allowlist 会指向不存在的工具——保留则执行期报
        // "Unknown tool"，剔除则按当前正式词汇表生效。全部无效时回退
        // 到"全部工具"（空 allowlist 语义），宁全开不瘫痪。
        // PR-M2-2：raw 原始名单另存——动态层重建（MCP refresh 换名）后
        // [`Self::reapply_allowed_after_dynamic_change`] 用它重过滤，
        // 使名单中含 MCP 工具名的 custom 模式在 refresh 后仍生效（观察项 ①）。
        self.allowed_raw = Some(allowed_tools.clone());
        let total = allowed_tools.len();
        let known: Vec<String> = allowed_tools
            .into_iter()
            .filter(|name| self.builtins.contains_key(name) || self.dynamic.contains_key(name))
            .collect();
        if known.len() != total {
            log::warn!(
                "[TOOLS] allowlist filtered: {} of {total} tool names unknown (renamed/removed); kept: {}",
                total - known.len(),
                if known.is_empty() {
                    "<all tools>".to_string()
                } else {
                    known.join(", ")
                }
            );
        }
        self.allowed = if known.is_empty() { None } else { Some(known) };
    }

    pub fn apply_init(&mut self, allowed_tools: Vec<String>, session_id: &str) {
        self.set_allowed(allowed_tools);
        crate::set_current_session(session_id);
    }

    pub fn all_defs(&self) -> Vec<qaqh_types::ToolDef> {
        // P3-3：exposure 策略化——只有 Direct（或被 tool_search 提升回的
        // Deferred）进入模型面；Hidden/Internal 永不出现。
        let mut defs: Vec<qaqh_types::ToolDef> = self
            .builtins
            .values()
            .filter(|tool| tool.descriptor.exposure == ToolExposure::Direct)
            .map(RegisteredTool::tool_def)
            .collect();
        // 动态层（MCP）合并在后：模型面 = 内置词汇表 + 动态投影。
        defs.extend(self.dynamic.values().map(RegisteredTool::tool_def));
        defs
    }

    pub fn filtered_defs(&self) -> Vec<qaqh_types::ToolDef> {
        match &self.allowed {
            Some(allowed) => self
                .all_defs()
                .into_iter()
                .filter(|d| allowed.contains(&d.function.name))
                .collect(),
            None => self.all_defs(),
        }
    }

    // ── Three-phase execution for parallel tool support ──

    /// Phase 1: validate, safety-check, register inflight. Returns a [`PreparedCall`]
    /// that can be executed without the manager lock.
    #[cfg(test)]
    #[allow(clippy::result_large_err)] // 错误装箱属结构塑形，另立项
    pub(crate) fn prepare_req(
        &mut self,
        id: String,
        name: &str,
        args: serde_json::Value,
        workspace_root: &std::path::Path,
        timeout_secs: Option<u64>,
        progress_tx: Option<crate::ExecProgressSender>,
    ) -> Result<PreparedCall, ToolExecReport> {
        self.prepare_req_with_cancel(
            id,
            name,
            args,
            workspace_root,
            timeout_secs,
            progress_tx,
            Arc::new(AtomicBool::new(false)),
        )
    }

    /// Phase 1 variant for a runtime-owned cancellation token.
    ///
    /// `progress_tx` 目前在 typed 执行面无消费方（streaming 能力字段无生产
    /// 读取点，见审计 §6）；保留入参以免调用侧（runtime 进度通道）连带改动。
    #[allow(clippy::too_many_arguments)]
    #[allow(clippy::result_large_err)] // 错误装箱属结构塑形，另立项
    pub(crate) fn prepare_req_with_cancel(
        &mut self,
        id: String,
        name: &str,
        args: serde_json::Value,
        workspace_root: &std::path::Path,
        timeout_secs: Option<u64>,
        _progress_tx: Option<crate::ExecProgressSender>,
        cancel_flag: Arc<AtomicBool>,
    ) -> Result<PreparedCall, ToolExecReport> {
        if let Some(ref allowed) = self.allowed
            && !allowed.contains(&name.to_string())
        {
            let msg = format!(
                "[ERROR] Tool '{}' is not in the allowed list for this subagent. Allowed tools: [{}]",
                name,
                allowed.join(", ")
            );
            return Err(ToolExecReport {
                success: false,
                content: msg.clone(),
                files_affected: Vec::new(),
                meta: ToolExecMeta {
                    name: name.to_string(),
                    elapsed_ms: 0,
                    output_size: msg.len(),
                    success: false,
                    args_summary: String::new(),
                },
            });
        }

        // P3-1：`tool_search` 元工具 prepare 期拦截——检索 + 提升都在锁内
        // 完成，执行体只回放快照结果。
        if name == TOOL_SEARCH_NAME {
            let query = args
                .get("query")
                .and_then(serde_json::Value::as_str)
                .unwrap_or_default();
            let max_results = args
                .get("max_results")
                .and_then(serde_json::Value::as_u64)
                .unwrap_or(8) as usize;
            let hits = self.search_tools(query, max_results);
            // 命中的 Deferred 工具升回 Direct：下一轮 defs 注入。
            let deferred_hits: Vec<&String> = hits
                .iter()
                .filter(|hit| hit.exposure == "deferred")
                .map(|hit| &hit.name)
                .collect();
            self.promote_tools(deferred_hits.iter().copied());
            let promoted: Vec<String> = deferred_hits.iter().map(|name| (*name).clone()).collect();
            let response = serde_json::json!({
                "timeis": qaqh_types::platform::now_utc8(),
                "status": "ok",
                "query": query,
                "hits": hits,
                "promoted_next_round": promoted,
            });
            let descriptor = ToolDescriptor {
                name: ToolName::new(TOOL_SEARCH_NAME).expect("tool_search is a valid name"),
                display_name: None,
                description: "tool registry search".to_owned(),
                input_schema: serde_json::json!({"type": "object"}),
                output_schema: serde_json::json!({"type": "object"}),
                category: crate::permission::ToolCategory::Read,
                risk: ToolRisk::ReadOnly,
                default_timeout: std::time::Duration::from_secs(15),
                exposure: ToolExposure::Direct,
                source: ToolSource::Builtin,
                output_budget: OutputBudget::default(),
                capabilities: ToolCapabilities::default(),
            };
            self.inflight_tasks.insert(id.clone(), cancel_flag);
            return Ok(PreparedCall {
                id,
                name: TOOL_SEARCH_NAME.to_owned(),
                effective_tool_name: None,
                executor: Arc::new(ToolSearchExecutor {
                    descriptor,
                    response,
                }),
                effective_timeout: std::time::Duration::from_secs(15),
                audit_args: args,
            });
        }

        // 内置/动态统一路由视图：执行元数据只从 descriptor 读取；执行统一走
        // `ErasedTool`（v1 handler 已在注册时经适配器包装）。
        let tool = match self.builtins.get(name).or_else(|| self.dynamic.get(name)) {
            Some(tool) => tool,
            None => {
                let msg = format!("[ERROR] Unknown tool: {}", name);
                return Err(ToolExecReport {
                    success: false,
                    content: msg.clone(),
                    files_affected: Vec::new(),
                    meta: ToolExecMeta {
                        name: name.to_string(),
                        elapsed_ms: 0,
                        output_size: msg.len(),
                        success: false,
                        args_summary: String::new(),
                    },
                });
            }
        };
        let descriptor = &tool.descriptor;

        let timeout_secs = timeout_secs.unwrap_or(descriptor.default_timeout.as_secs());
        // 工区判定用显式 workspace（调用方上下文），不再读线程局部——prepare
        // 在工具 worker 线程上跑，per-call 视图退场后那里的 TLS 不再是调用上下文。
        let in_workspace =
            is_path_in_workspace(&args, workspace_root, &descriptor.risk, descriptor.category);
        match crate::safety::SafetyPolicy::evaluate(descriptor.risk.clone(), in_workspace) {
            SafetyVerdict::Block(reason) => {
                let msg = format!("[ERROR] {}", reason);
                return Err(ToolExecReport {
                    success: false,
                    content: msg.clone(),
                    files_affected: Vec::new(),
                    meta: ToolExecMeta {
                        name: name.to_string(),
                        elapsed_ms: 0,
                        output_size: msg.len(),
                        success: false,
                        args_summary: String::new(),
                    },
                });
            }
            SafetyVerdict::Allow => {}
        }

        self.inflight_tasks.insert(id.clone(), cancel_flag);

        Ok(PreparedCall {
            id,
            name: name.to_string(),
            effective_tool_name: descriptor.display_name.clone(),
            executor: tool.erased.clone(),
            effective_timeout: Duration::from_secs(timeout_secs),
            audit_args: args,
        })
    }

    /// Phase 3: deregister inflight, accumulate stats, build report.
    pub(crate) fn finalize_req(
        &mut self,
        prepared: PreparedCall,
        result: crate::ToolResult,
        elapsed_ms: u64,
    ) -> ToolExecReport {
        self.inflight_tasks.remove(&prepared.id);

        let output_size = result.model_text().len();
        let success = result.is_success();

        self.stats_total += 1;
        if !success {
            self.stats_failures += 1;
        }
        let args_summary = audit_args_summary(&prepared.name, &prepared.audit_args);
        let files_affected = extract_files_affected(&prepared.name, &prepared.audit_args);
        if success {
            match prepared.name.as_str() {
                "read" => {
                    for f in &files_affected {
                        if !self.files_read.contains(f) {
                            self.files_read.push(f.clone());
                        }
                    }
                }
                "edit" | "todo" => {
                    for f in &files_affected {
                        if !self.files_written.contains(f) {
                            self.files_written.push(f.clone());
                        }
                    }
                }
                "exec" | "git_commit" | "git_add" => {
                    // These mutate the workspace but don't have a single 'path' argument
                }
                _ => {}
            }
        }
        let meta = ToolExecMeta {
            name: prepared.name,
            elapsed_ms,
            output_size,
            success,
            args_summary,
        };
        ToolExecReport {
            success,
            content: result.model_text().to_string(),
            meta,
            files_affected,
        }
    }

    pub fn stats(&self) -> ToolStats {
        ToolStats {
            calls_total: self.stats_total,
            failures: self.stats_failures,
            files_read: self.files_read.clone(),
            files_written: self.files_written.clone(),
        }
    }

    pub fn cancel_tool(&mut self, id: Option<&str>) {
        match id {
            Some(specific) => {
                if let Some(flag) = self.inflight_tasks.get(specific) {
                    flag.store(true, std::sync::atomic::Ordering::SeqCst);
                }
            }
            None => {
                crate::set_cancel(true);
                for flag in self.inflight_tasks.values() {
                    flag.store(true, std::sync::atomic::Ordering::SeqCst);
                }
            }
        }
    }
}

/// Extract file paths from tool args.
/// 从 args 提取受影响的文件路径（审计对象 + 统计共用；execution 侧
/// 在派发前用同一口径快照 before 指纹）。
pub(crate) fn extract_files_affected(_tool_name: &str, args: &serde_json::Value) -> Vec<String> {
    let obj = match args.as_object() {
        Some(o) => o,
        None => return Vec::new(),
    };
    let mut files = Vec::new();
    if let Some(v) = obj.get("path").and_then(|v| v.as_str()) {
        files.push(v.to_string());
    }
    if let Some(arr) = obj.get("paths").and_then(|v| v.as_array()) {
        for v in arr {
            if let Some(s) = v.as_str() {
                files.push(s.to_string());
            }
        }
    }
    for key in ["file_a", "file_b", "dest", "target"] {
        if let Some(v) = obj.get(key).and_then(|v| v.as_str()) {
            files.push(v.to_string());
        }
    }
    files
}

/// Determine whether the tool call is operating within the current workspace.
///
/// `risk`/`category` only matter on the **fail-closed** path: a file-scoped
/// destructive tool ([`ToolRisk::Destructive`] + [`ToolCategory::Write`], e.g.
/// `delete`) that carries no `path` argument cannot prove its target is inside
/// the workspace, so it is treated as outside and blocked by
/// [`crate::safety::SafetyPolicy`] (P0-2). Without this, such a call fell
/// through to the old unconditional `true` and short-circuited the block.
///
/// Exec/Net destructive tools (`exec`) declare no `path` at all by design
/// (`command`, workdir defaults to the workspace root); failing them
/// closed here would block the tool at every permission level, so their
/// containment stays with the permission layer (`classify_risk` already reports
/// Exec/Net as [`crate::PermissionRisk::High`]). Tools that never touch the file
/// system (`ask`, `skills`, …) are `ReadOnly`/`Write`/`Administrative`
/// and keep the permissive default.
fn is_path_in_workspace(
    args: &serde_json::Value,
    workspace_root: &std::path::Path,
    risk: &ToolRisk,
    category: crate::permission::ToolCategory,
) -> bool {
    if let Some(path) = args.get("path").and_then(|v| v.as_str()) {
        if path.is_empty() || path == "." {
            return true;
        }
        let ws = workspace_root;
        if ws.as_os_str().is_empty() || ws == std::path::Path::new(".") {
            return true;
        }
        let abs_path = if std::path::Path::new(path).is_absolute() {
            std::path::PathBuf::from(path)
        } else {
            ws.join(path)
        };
        // M13：组件级比较 + `..` 词法归一化。原先对字符串做 starts_with，
        // sibling 目录（`proj` vs `proj-backup`）与未解析的 `..` 逃逸都会
        // 被误判为在工内，导致 Destructive 出工区阻断被绕过。
        let ws_norm = crate::permission::normalize_lexically(ws);
        let path_norm = crate::permission::normalize_lexically(&abs_path);
        path_norm.starts_with(&ws_norm)
    } else {
        // No `path` arg — assume workspace operation for non-destructive tools
        // (e.g. skills, ask). A **file-scoped** Destructive tool without a
        // `path` fails closed (see doc comment above).
        !(matches!(risk, ToolRisk::Destructive)
            && matches!(category, crate::permission::ToolCategory::Write))
    }
}

/// Compact args summary for audit log — path and key values only.
fn audit_args_summary(_tool: &str, args: &serde_json::Value) -> String {
    let obj = match args.as_object() {
        Some(o) => o,
        None => return String::new(),
    };
    // Show path-like args first, then command, then truncate to 80 chars
    let mut parts: Vec<String> = Vec::new();
    for key in [
        "path", "file_a", "file_b", "dest", "target", "command", "pattern", "query", "question",
    ] {
        if let Some(v) = obj.get(key).and_then(|v| v.as_str()) {
            let short = if v.len() > 50 {
                // Show last path segment
                let seg = v.rsplit(&['/', '\\']).next().unwrap_or(v);
                format!("{key}=\"{seg}\"")
            } else {
                format!("{key}=\"{v}\"")
            };
            parts.push(short);
        }
    }
    let s = parts.join(", ");
    if s.len() > 80 {
        let end = s.floor_char_boundary(77);
        format!("{}…", s.get(..end).unwrap_or(&s))
    } else {
        s
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::probe::{ProbeBody, ProbeTool};
    use crate::{ToolResult, ToolRisk};

    fn noop(_ctx: &crate::tool_api::ToolCallContext, _args: serde_json::Value) -> ToolResult {
        ToolResult::ok("noop")
    }

    fn probe(key: &str) -> ProbeTool {
        ProbeTool {
            key: key.to_string(),
            description: "test handler",
            input_schema: serde_json::json!({ "type": "object" }),
            handler: noop as ProbeBody,
            risk: ToolRisk::ReadOnly,
            category: crate::permission::ToolCategory::Read,
            default_timeout: std::time::Duration::from_secs(10),
        }
    }

    fn names(mgr: &ToolManager) -> Vec<String> {
        let mut v: Vec<String> = mgr
            .filtered_defs()
            .into_iter()
            .map(|d| d.function.name)
            .collect();
        v.sort();
        v
    }

    #[test]
    fn apply_init_filters_unknown_renamed_tool_names() {
        let mut mgr = ToolManager::new();
        mgr.register_probe(probe("read"));
        mgr.register_probe(probe("exec"));
        // 冻结期间改名/删除的旧名（read/edit/web/search）必须被剔除。
        mgr.apply_init(
            vec![
                "read".to_string(),
                "edit".to_string(),
                "read".to_string(),
                "exec".to_string(),
            ],
            "s1",
        );
        assert_eq!(names(&mgr), vec!["exec", "read"]);
    }

    #[test]
    fn apply_init_all_unknown_falls_back_to_all_tools() {
        let mut mgr = ToolManager::new();
        mgr.register_probe(probe("exec"));
        // 旧配置全是已移除的工具名：回退到"全部工具"（空 allowlist 语义），
        // 而不是把子代理锁死成零工具。
        mgr.apply_init(vec!["read".to_string(), "edit".to_string()], "s1");
        assert_eq!(names(&mgr), vec!["exec"]);
    }

    #[test]
    fn apply_init_empty_stays_all_tools() {
        let mut mgr = ToolManager::new();
        mgr.register_probe(probe("exec"));
        mgr.apply_init(vec![], "s1");
        assert_eq!(names(&mgr), vec!["exec"]);
    }

    // ── set_allowed（4.1：工具模式运行时切换入口）──

    #[test]
    fn set_allowed_restricts_and_restores() {
        let mut mgr = ToolManager::new();
        mgr.register_probe(probe("exec"));
        mgr.register_probe(probe("read"));
        mgr.register_probe(probe("write"));
        // 切到极限白名单
        mgr.set_allowed(vec!["exec".to_string(), "read".to_string()]);
        assert_eq!(names(&mgr), vec!["exec", "read"]);
        // 切回全量（标准模式）
        mgr.set_allowed(vec![]);
        assert_eq!(names(&mgr), vec!["exec", "read", "write"]);
    }

    #[test]
    fn set_allowed_filters_unknown_names() {
        let mut mgr = ToolManager::new();
        mgr.register_probe(probe("exec"));
        // 未知名剔除（不静默吞掉，log warn）；全无效 → 全量
        mgr.set_allowed(vec!["exec".to_string(), "ghost".to_string()]);
        assert_eq!(names(&mgr), vec!["exec"]);
        mgr.set_allowed(vec!["ghost".to_string()]);
        assert_eq!(names(&mgr), vec!["exec"]);
    }

    #[test]
    fn set_allowed_does_not_touch_session() {
        let mut mgr = ToolManager::new();
        mgr.register_probe(probe("exec"));
        mgr.apply_init(vec![], "seed-A");
        // set_allowed 只改工具集，不动 session（区别于 apply_init）
        mgr.set_allowed(vec!["exec".to_string()]);
        assert_eq!(
            *crate::CURRENT_SESSION.lock().unwrap(),
            Some("seed-A".to_string())
        );
    }

    #[test]
    fn set_allowed_gates_prepare_req() {
        let mut mgr = ToolManager::new();
        mgr.register_probe(probe("exec"));
        mgr.register_probe(probe("read"));
        mgr.set_allowed(vec!["read".to_string()]);
        // 白名单外工具在执行层被拦截（纵深防御）
        let err = mgr.prepare_req(
            "c1".to_string(),
            "exec",
            serde_json::json!({"command": "echo hi"}),
            std::path::Path::new("."),
            None,
            None,
        );
        assert!(err.is_err());
        let ok = mgr.prepare_req(
            "c2".to_string(),
            "read",
            serde_json::json!({"path": "x"}),
            std::path::Path::new("."),
            None,
            None,
        );
        assert!(ok.is_ok());
    }
    // ── 动态层路由（PR-M1-4）：prepare 走注入的 dispatcher fn ──

    fn marker_fn(
        _name: &str,
        _ctx: &crate::tool_api::ToolCallContext,
        _args: serde_json::Value,
    ) -> Result<crate::tool_api::ToolOutcome, crate::tool_api::FatalToolError> {
        Ok(crate::tool_api::map_tool_result(ToolResult::ok(
            "mcp-dispatched",
        )))
    }

    #[test]
    fn prepare_routes_dynamic_tool_to_injected_fn() {
        let mut mgr = ToolManager::new();
        let (name, tool) = crate::build_dynamic_tool(
            "demo",
            "echo",
            "dynamic test tool",
            serde_json::json!({ "type": "object" }),
            marker_fn,
            crate::permission::ToolCategory::Exec,
            std::time::Duration::from_secs(30),
        );
        mgr.register_dynamic(name.clone(), tool)
            .expect("register dynamic");

        let prepared = mgr
            .prepare_req(
                "id-1".to_owned(),
                &name,
                serde_json::json!({}),
                std::path::Path::new("."),
                None,
                None,
            )
            .map_err(|report| report.content)
            .expect("dynamic tool prepare should succeed");
        assert_eq!(prepared.effective_tool_name.as_deref(), Some("echo"));
        assert_eq!(
            prepared.effective_timeout,
            std::time::Duration::from_secs(30),
            "生效超时 = descriptor 默认（调用方未显式给定）"
        );
        let outcome = prepared
            .executor
            .execute(
                crate::tool_api::ToolCallContext {
                    call_id: "id-1".to_owned(),
                    session_id: "s1".to_owned(),
                    workspace_root: std::path::PathBuf::from("/tmp/ws"),
                    mode: crate::tool_api::AgentMode::Code,
                    permission_level: crate::permission::PermissionLevel::ReadOnly,
                    sandbox: crate::tool_api::SandboxMode::Main,
                    sandbox_spec: crate::tool_api::SandboxSpec::workspace_write(
                        std::path::PathBuf::from("/tmp/ws"),
                    ),
                    exec_default_shell: None,
                    timeout: prepared.effective_timeout,
                    cancellation: crate::tool_api::CancellationToken::new(),
                    progress: None,
                    source: crate::tool_api::ToolCallSource::Model,
                },
                serde_json::json!({}),
            )
            .expect("dynamic tool executes through ErasedTool");
        assert_eq!(outcome.model.text, "mcp-dispatched");

        let report = match mgr.prepare_req(
            "id-2".to_owned(),
            "mcp__demo__nope",
            serde_json::json!({}),
            std::path::Path::new("."),
            None,
            None,
        ) {
            Err(report) => report,
            Ok(_) => panic!("未知动态名应报 Unknown tool"),
        };
        assert!(
            report.content.contains("Unknown tool"),
            "{}",
            report.content
        );
    }
}

#[cfg(test)]
mod m13_tests {
    use super::*;
    use crate::ToolRisk;

    fn ctx_with_path(path: &str) -> serde_json::Value {
        ctx_with_args(serde_json::json!({ "path": path }))
    }

    fn ctx_with_args(args: serde_json::Value) -> serde_json::Value {
        args
    }

    /// 测试桥：`is_path_in_workspace` 现在吃显式 workspace，本测试沿用
    /// `set_workspace` 装好的全局工区。
    fn in_ws(args: &serde_json::Value, risk: ToolRisk) -> bool {
        in_ws_cat(args, &risk, crate::permission::ToolCategory::Write)
    }

    fn in_ws_cat(
        args: &serde_json::Value,
        risk: &ToolRisk,
        category: crate::permission::ToolCategory,
    ) -> bool {
        let ws = std::path::PathBuf::from(crate::current_workspace());
        is_path_in_workspace(args, ws.as_path(), risk, category)
    }

    #[test]
    fn sibling_prefix_and_dotdot_cannot_pose_as_workspace() {
        let _serial = crate::TEST_RUNTIME_SERIAL
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws-proj");
        std::fs::create_dir_all(&ws).unwrap();
        let old_ws = crate::current_workspace();
        crate::set_workspace(ws.to_str().unwrap());

        // 工作区内：相对与绝对路径均通过。
        assert!(in_ws(&ctx_with_path("src/main.rs"), ToolRisk::Write));
        assert!(in_ws(
            &ctx_with_path(ws.join("src/main.rs").to_str().unwrap()),
            ToolRisk::Write
        ));
        // sibling 前缀（ws-proj-backup）不再被字符串前缀误判。
        assert!(!in_ws(
            &ctx_with_path(tmp.path().join("ws-proj-backup/x").to_str().unwrap()),
            ToolRisk::Write
        ));
        // `..` 逃逸被词法归一化捕获。
        assert!(!in_ws(&ctx_with_path("../outside.txt"), ToolRisk::Write));
        assert!(!in_ws(
            &ctx_with_path("a/../../outside.txt"),
            ToolRisk::Write
        ));
        // 无 path 参数：非 Destructive 工具默认放行（ask/skills 不碰文件系统）。
        assert!(in_ws_cat(
            &ctx_with_args(serde_json::json!({})),
            &ToolRisk::ReadOnly,
            crate::permission::ToolCategory::Read,
        ));

        crate::set_workspace(&old_ws);
    }

    /// P0-2：文件型 Destructive 工具（`delete`：Destructive + Write）缺 `path`
    /// 时必须 fail-closed，否则会短路 `SafetyPolicy` 的出工区阻断。
    #[test]
    fn destructive_tool_without_path_is_treated_as_outside_workspace() {
        let _serial = crate::TEST_RUNTIME_SERIAL
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws-proj");
        std::fs::create_dir_all(&ws).unwrap();
        let old_ws = crate::current_workspace();
        crate::set_workspace(ws.to_str().unwrap());

        let write_cat = crate::permission::ToolCategory::Write;
        // `delete` 形态：Destructive + Write，却没有 path。
        let no_path = ctx_with_args(serde_json::json!({}));
        assert!(
            !in_ws_cat(&no_path, &ToolRisk::Destructive, write_cat),
            "文件型 Destructive 工具缺 path 必须判为工区外（fail-closed）"
        );
        // 同样的参数形状下，非 Destructive 工具不受影响（不误伤 ask/task/skills）。
        assert!(in_ws_cat(&no_path, &ToolRisk::Write, write_cat));
        assert!(in_ws_cat(
            &no_path,
            &ToolRisk::ReadOnly,
            crate::permission::ToolCategory::Read
        ));
        assert!(in_ws_cat(
            &no_path,
            &ToolRisk::Administrative,
            crate::permission::ToolCategory::Read
        ));
        // 端到端判定：SafetyPolicy 必须把它阻断。
        assert!(matches!(
            crate::safety::SafetyPolicy::evaluate(
                ToolRisk::Destructive,
                in_ws_cat(&no_path, &ToolRisk::Destructive, write_cat)
            ),
            SafetyVerdict::Block(_)
        ));
        // Destructive 工具带工区内 path 时仍放行（`delete` 的正常形态）。
        assert!(in_ws_cat(
            &ctx_with_path(ws.join("trash-me.txt").to_str().unwrap()),
            &ToolRisk::Destructive,
            write_cat
        ));
        // Destructive 工具带工区外 path 时阻断。
        assert!(!in_ws_cat(
            &ctx_with_path(tmp.path().join("outside.txt").to_str().unwrap()),
            &ToolRisk::Destructive,
            write_cat
        ));
        // Exec/Net 型 Destructive 工具（`exec`：无 path 参数是设计使然，workdir
        // 缺省 = 工区根）保持放行——它们的围栏在权限层（classify_risk 已报 High）。
        assert!(in_ws_cat(
            &ctx_with_args(serde_json::json!({ "command": "rm -rf /tmp/x" })),
            &ToolRisk::Destructive,
            crate::permission::ToolCategory::Exec
        ));

        crate::set_workspace(&old_ws);
    }
}

/// P0-2 e2e：skip-permissions bypass 下，`SafetyPolicy` 仍是文件型 Destructive 工具
/// 进入 handler 前的最后出工区闸门。缺 `path` 的 `delete` 形态必须被它阻断
/// （`prepare_req` 真实路径，不经 handler）。
#[cfg(test)]
mod safety_e2e_tests {
    use super::*;
    use crate::probe::ProbeTool;

    fn destructive_probe(key: &str) -> ProbeTool {
        ProbeTool {
            key: key.to_string(),
            description: "test destructive handler",
            input_schema: serde_json::json!({ "type": "object" }),
            handler: |_ctx, _args| crate::ToolResult::ok("ran"),
            risk: ToolRisk::Destructive,
            category: crate::permission::ToolCategory::Write,
            default_timeout: std::time::Duration::from_secs(5),
        }
    }

    #[test]
    fn destructive_tool_without_path_is_blocked_by_safety_policy() {
        let _serial = crate::TEST_RUNTIME_SERIAL
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let old_ws = crate::current_workspace();
        crate::set_workspace(ws.to_str().unwrap());

        let mut mgr = ToolManager::new();
        mgr.register_probe(destructive_probe("delete"));

        // skip-permissions bypass；本测试直接调用 `prepare_req`，
        // `SafetyPolicy` 是文件型 Destructive 工具进入 handler 前的最后闸门。
        let report = mgr
            .prepare_req(
                "c1".to_string(),
                "delete",
                serde_json::json!({}),
                ws.as_path(),
                None,
                None,
            )
            .err()
            .expect("file-scoped Destructive tool without path must be blocked");
        assert!(
            report.content.contains("outside workspace is blocked"),
            "{}",
            report.content
        );

        // 反向：Destructive 工具带工区内 `path` 时仍可 prepare。
        assert!(
            mgr.prepare_req(
                "c2".to_string(),
                "delete",
                serde_json::json!({ "path": ws.join("trash-me.txt").to_str().unwrap() }),
                ws.as_path(),
                None,
                None,
            )
            .is_ok(),
            "in-workspace destructive target must stay allowed"
        );

        // 反向：Destructive 工具带工区外 `path` 时被阻断。
        let report = mgr
            .prepare_req(
                "c3".to_string(),
                "delete",
                serde_json::json!({ "path": tmp.path().join("outside.txt").to_str().unwrap() }),
                ws.as_path(),
                None,
                None,
            )
            .err()
            .expect("outside-workspace destructive target must be blocked");
        assert!(
            report.content.contains("outside workspace is blocked"),
            "{}",
            report.content
        );

        crate::set_workspace(&old_ws);
    }
}

// ── P3：Deferred + tool_search（研究文档 §5.1）/ MCP 命名空间聚合（§5.2）──

/// `tool_search` 元工具名：模型首轮即见，按需检索其余 Deferred 工具。
pub const TOOL_SEARCH_NAME: &str = "tool_search";

/// 一条工具检索命中。
#[derive(Debug, Clone, serde::Serialize)]
pub struct SearchHit {
    pub name: String,
    pub description: String,
    /// 命中时的暴露面（direct = 本轮已注入 defs；deferred = 提升后下轮注入）。
    pub exposure: &'static str,
}

/// P3-1：`tool_search` 的执行体（prepare 期对命中做一次快照 + 提升）。
struct ToolSearchExecutor {
    descriptor: ToolDescriptor,
    response: serde_json::Value,
}

impl ErasedTool for ToolSearchExecutor {
    fn descriptor(&self) -> ToolDescriptor {
        self.descriptor.clone()
    }

    fn execute(
        &self,
        _ctx: qaqh_tool_core::tool_api::ToolCallContext,
        _args: serde_json::Value,
    ) -> Result<qaqh_tool_core::tool_api::ToolOutcome, qaqh_tool_core::tool_api::FatalToolError>
    {
        Ok(qaqh_tool_core::tool_api::result::map_tool_result(
            qaqh_types::ToolResult::ok_data(self.response.clone(), "tool_search"),
        ))
    }
}

/// P3-2：`mcp__{server}` 聚合入口的执行体。
///
/// 模型面只有一个入口（`name` + `args` 两参），执行面按 `name` 还原
/// `mcp__{server}__{tool}` 全名，复用 E-5 单一 dispatcher 路由——上游
/// MCP dispatcher 只认全名，聚合对它是透明的。
struct NamespaceAggregatedAdapter {
    server: String,
    dispatch: DynamicDispatch,
    descriptor: ToolDescriptor,
}

impl ErasedTool for NamespaceAggregatedAdapter {
    fn descriptor(&self) -> ToolDescriptor {
        self.descriptor.clone()
    }

    fn execute(
        &self,
        ctx: qaqh_tool_core::tool_api::ToolCallContext,
        args: serde_json::Value,
    ) -> Result<qaqh_tool_core::tool_api::ToolOutcome, qaqh_tool_core::tool_api::FatalToolError>
    {
        let tool = args
            .get("name")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_owned();
        if tool.is_empty() {
            return Ok(qaqh_tool_core::tool_api::result::map_tool_result(
                qaqh_types::ToolResult::error_with(
                    "missing_tool_name",
                    format!(
                        "aggregate tool 'mcp__{}' requires {{\"name\": <tool>}}; see description for the tool list",
                        self.server
                    ),
                    false,
                    Some(
                        "pass the upstream tool name in `name` and its arguments in `args`"
                            .to_string(),
                    ),
                ),
            ));
        }
        let inner = if args.get("args").is_some_and(serde_json::Value::is_object) {
            args["args"].clone()
        } else {
            args.clone()
        };
        (self.dispatch)(&format!("mcp__{}__{}", self.server, tool), &ctx, inner)
    }
}

impl ToolManager {
    /// P3-1：把已注册的内置工具降为 Deferred（注册仍在、首轮 defs 不再携带，
    /// 仅可被 `tool_search` 检索）。返回实际降级的数量。
    pub fn defer_tools<I, S>(&mut self, names: I) -> usize
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut deferred = 0;
        for name in names {
            if let Some(tool) = self.builtins.get_mut(name.as_ref())
                && tool.descriptor.exposure == ToolExposure::Direct
            {
                tool.descriptor.exposure = ToolExposure::Deferred;
                deferred += 1;
            }
        }
        deferred
    }

    /// P3-1：把检索命中的 Deferred 工具升回 Direct（下一轮 defs 注入）。
    /// 返回实际提升的数量。
    pub fn promote_tools<I, S>(&mut self, names: I) -> usize
    where
        I: IntoIterator<Item = S>,
        S: AsRef<str>,
    {
        let mut promoted = 0;
        for name in names {
            if let Some(tool) = self.builtins.get_mut(name.as_ref())
                && tool.descriptor.exposure == ToolExposure::Deferred
            {
                tool.descriptor.exposure = ToolExposure::Direct;
                promoted += 1;
            }
        }
        promoted
    }

    /// P3-1：注册表检索（内置 + 动态，跳过 Hidden/Internal 与元工具自身）。
    ///
    /// 打分：名字精确/前缀/子串 > 名词分词命中 > 描述子串。大小写不敏感。
    pub fn search_tools(&self, query: &str, max_results: usize) -> Vec<SearchHit> {
        let query = query.trim().to_ascii_lowercase();
        if query.is_empty() {
            return Vec::new();
        }
        let max_results = max_results.max(1);
        let mut scored: Vec<(i64, SearchHit)> = Vec::new();
        let mut push = |scored: &mut Vec<(i64, SearchHit)>,
                        name: &str,
                        description: &str,
                        exposure: ToolExposure| {
            if matches!(exposure, ToolExposure::Hidden | ToolExposure::Internal)
                || name == TOOL_SEARCH_NAME
            {
                return;
            }
            let lower_name = name.to_ascii_lowercase();
            let lower_desc = description.to_ascii_lowercase();
            let mut score = 0;
            if lower_name == query {
                score += 100;
            } else if lower_name.starts_with(&query) {
                score += 60;
            } else if lower_name.contains(&query) {
                score += 40;
            } else if query
                .split(['-', '_', ' ', '.'])
                .filter(|token| !token.is_empty())
                .any(|token| lower_name.contains(token))
            {
                score += 20;
            }
            if lower_desc.contains(&query) {
                score += 10;
            }
            if score > 0 {
                scored.push((
                    score,
                    SearchHit {
                        name: name.to_owned(),
                        description: description.to_owned(),
                        exposure: match exposure {
                            ToolExposure::Direct => "direct",
                            _ => "deferred",
                        },
                    },
                ));
            }
        };
        for (name, tool) in &self.builtins {
            let exposure = tool.descriptor.exposure;
            push(&mut scored, name, &tool.descriptor.description, exposure);
        }
        for (name, tool) in &self.dynamic {
            push(
                &mut scored,
                name,
                &tool.descriptor.description,
                tool.descriptor.exposure,
            );
        }
        scored.sort_by(|a, b| b.0.cmp(&a.0).then_with(|| a.1.name.cmp(&b.1.name)));
        scored.truncate(max_results);
        scored.into_iter().map(|(_, hit)| hit).collect()
    }

    /// P3-1：`tool_search` 元工具注册（描述面 Direct；真实执行被 prepare
    /// 拦截，注册的 execute 仅兜底不可达路径）。
    pub fn register_tool_search(&mut self) {
        let descriptor = ToolDescriptor {
            name: ToolName::new(TOOL_SEARCH_NAME).expect("tool_search is a valid name"),
            display_name: None,
            description: "Search the tool registry by keyword when you need a capability not in \
                          your current tool list. Returns matching tool names and descriptions; \
                          matching deferred tools become available in the next turn."
                .to_owned(),
            input_schema: serde_json::json!({
                "type": "object",
                "properties": {
                    "query": {
                        "type": "string",
                        "description": "keyword(s) for the capability you need"
                    },
                    "max_results": {
                        "type": "integer",
                        "description": "maximum hits to return (default 8)"
                    }
                },
                "required": ["query"],
                "additionalProperties": false
            }),
            output_schema: serde_json::json!({"type": "object"}),
            category: crate::permission::ToolCategory::Read,
            risk: ToolRisk::ReadOnly,
            default_timeout: std::time::Duration::from_secs(15),
            exposure: ToolExposure::Direct,
            source: ToolSource::Builtin,
            output_budget: OutputBudget::default(),
            capabilities: ToolCapabilities::default(),
        };
        self.builtins.insert(
            TOOL_SEARCH_NAME.to_owned(),
            RegisteredTool {
                descriptor: descriptor.clone(),
                erased: Arc::new(ToolSearchExecutor {
                    descriptor,
                    response: serde_json::json!({"status": "unreachable"}),
                }),
            },
        );
    }

    /// P3-2：MCP 命名空间聚合——同一 server 的 `mcp__{server}__{tool}` 全部
    /// 合成一个入口 `mcp__{server}`（args: {name, args}）。N 个 defs 缩成 1 个。
    /// 返回聚合出的入口名列表（单工具 server 不聚合，保留直连语义）。
    pub fn aggregate_mcp_namespaces(&mut self) -> Vec<String> {
        let mut by_server: std::collections::BTreeMap<String, Vec<(String, RegisteredTool)>> =
            std::collections::BTreeMap::new();
        let keys: Vec<String> = self
            .dynamic
            .keys()
            .filter(|name| {
                name.strip_prefix(MCP_DYNAMIC_PREFIX)
                    .is_some_and(|rest| rest.contains("__"))
            })
            .cloned()
            .collect();
        for key in keys {
            let Some(rest) = key.strip_prefix(MCP_DYNAMIC_PREFIX) else {
                continue;
            };
            let Some((server, tool)) = rest.split_once("__") else {
                continue;
            };
            let server = server.to_owned();
            if tool.is_empty() || server.is_empty() || tool.contains("__") {
                continue;
            }
            if let Some(tool_entry) = self.dynamic.remove(&key) {
                by_server.entry(server).or_default().push((key, tool_entry));
            }
        }
        let mut aggregated = Vec::new();
        for (server, mut entries) in by_server {
            if entries.len() < 2 {
                // 单工具 server：恢复原样（聚合无收益，反而破坏直连语义）。
                for (key, entry) in entries {
                    self.dynamic.insert(key, entry);
                }
                continue;
            }
            entries.sort_by(|a, b| a.0.cmp(&b.0));
            let mut tool_names: Vec<String> = entries
                .iter()
                .map(|(key, _)| {
                    key.strip_prefix(MCP_DYNAMIC_PREFIX)
                        .and_then(|rest| rest.strip_prefix(&format!("{server}__")))
                        .unwrap_or_default()
                        .to_owned()
                })
                .collect();
            tool_names.sort();
            // 聚合入口取组内最宽的 capability 类别，保证不放宽、只收紧。
            let category = entries
                .iter()
                .map(|(_, entry)| entry.descriptor.category)
                .max_by_key(|category| match category {
                    crate::permission::ToolCategory::Read => 0,
                    crate::permission::ToolCategory::Write => 1,
                    crate::permission::ToolCategory::Net => 2,
                    crate::permission::ToolCategory::Exec => 3,
                })
                .unwrap_or(crate::permission::ToolCategory::Exec);
            let default_timeout = entries
                .iter()
                .map(|(_, entry)| entry.descriptor.default_timeout)
                .max()
                .unwrap_or(std::time::Duration::from_secs(30));
            // dispatcher 全体 MCP 工具共享（E-5 单一 fn 指针）。
            let Some(dispatch) = self.dynamic_dispatch_of(&entries[0].0) else {
                for (key, entry) in entries {
                    self.dynamic.insert(key, entry);
                }
                continue;
            };
            let description = format!(
                "Aggregate entry for MCP server '{server}'. Pass {{\"name\": <tool>, \"args\": \
                 {{...}}}}. Available tools: {}",
                tool_names.join(", ")
            );
            let descriptor = ToolDescriptor {
                name: ToolName::new(&format!("{MCP_DYNAMIC_PREFIX}{server}"))
                    .expect("aggregate name is valid"),
                display_name: None,
                description: truncate_description(&description),
                input_schema: serde_json::json!({
                    "type": "object",
                    "properties": {
                        "name": {
                            "type": "string",
                            "enum": tool_names,
                            "description": "upstream tool name"
                        },
                        "args": {
                            "type": "object",
                            "description": "arguments forwarded to the upstream tool"
                        }
                    },
                    "required": ["name"],
                    "additionalProperties": false
                }),
                output_schema: serde_json::json!({"type": "object"}),
                category,
                risk: ToolRisk::Administrative,
                default_timeout,
                exposure: ToolExposure::Direct,
                source: ToolSource::Mcp,
                output_budget: OutputBudget::default(),
                capabilities: ToolCapabilities::default(),
            };
            let name = descriptor.name.as_str().to_owned();
            self.dynamic.insert(
                name.clone(),
                RegisteredTool {
                    descriptor: descriptor.clone(),
                    erased: Arc::new(NamespaceAggregatedAdapter {
                        server: server.clone(),
                        dispatch,
                        descriptor,
                    }),
                },
            );
            aggregated.push(name);
        }
        aggregated
    }

    /// 取动态注册项的 dispatcher fn 指针（register_dynamic 时随 entry 存入
    /// [`Self::dynamic_dispatches`]）。
    fn dynamic_dispatch_of(&self, name: &str) -> Option<DynamicDispatch> {
        self.dynamic_dispatches.get(name).copied()
    }
}

#[cfg(test)]
mod p3_tests {
    use super::*;

    #[test]
    fn tool_search_finds_and_promotes_deferred_tools() {
        let mut mgr = ToolManager::new();
        mgr.register_typed(qaqh_file_tools::file_glob::GlobTool);
        mgr.register_tool_search();
        assert_eq!(mgr.defer_tools(["glob"]), 1, "glob 降为 Deferred");
        assert!(
            !mgr.all_defs().iter().any(|d| d.function.name == "glob"),
            "Deferred 工具不进首轮 defs"
        );
        assert!(mgr.builtins.contains_key("glob"), "注册仍在，可执行");

        let hits = mgr.search_tools("glob", 8);
        assert_eq!(hits.first().expect("hit").name, "glob");
        assert_eq!(hits.first().expect("hit").exposure, "deferred");
        mgr.promote_tools(["glob"]);
        assert!(
            mgr.all_defs().iter().any(|d| d.function.name == "glob"),
            "提升后下一轮 defs 注入"
        );
    }

    #[test]
    fn tool_search_hides_internal_and_skips_itself() {
        let mut mgr = ToolManager::new();
        mgr.register_tool_search();
        let hits = mgr.search_tools("search", 8);
        assert!(
            !hits.iter().any(|hit| hit.name == TOOL_SEARCH_NAME),
            "元工具自身不出现在命中里"
        );
        assert!(mgr.search_tools("", 8).is_empty(), "空查询不命中");
    }

    #[test]
    fn namespace_aggregation_collapses_per_server_and_restores_singletons() {
        let mut mgr = ToolManager::new();
        for (server, tool) in [("alpha", "list"), ("alpha", "get"), ("beta", "ping")] {
            let (name, dyn_tool) = crate::build_dynamic_tool(
                server,
                tool,
                &format!("{server} {tool}"),
                serde_json::json!({"type": "object"}),
                |_name, _ctx, _args| {
                    Ok(qaqh_tool_core::tool_api::result::map_tool_result(
                        qaqh_types::ToolResult::ok("ok"),
                    ))
                },
                crate::permission::ToolCategory::Read,
                std::time::Duration::from_secs(30),
            );
            mgr.register_dynamic(name, dyn_tool).expect("register");
        }
        eprintln!("keys before: {:?}", {
            let mut v: Vec<String> = mgr.dynamic.keys().cloned().collect();
            v.sort();
            v
        });
        let aggregated = mgr.aggregate_mcp_namespaces();
        eprintln!("keys after: {:?}", {
            let mut v: Vec<String> = mgr.dynamic.keys().cloned().collect();
            v.sort();
            v
        });
        assert_eq!(
            aggregated,
            vec!["mcp__alpha".to_string()],
            "双工具 server 聚合"
        );
        assert!(
            mgr.dynamic.contains_key("mcp__beta__ping"),
            "单工具 server 保留直连原语义"
        );
        assert_eq!(mgr.dynamic.len(), 2, "3 → 2 个动态条目");
        let entry = &mgr.dynamic["mcp__alpha"];
        let schema = entry.descriptor.input_schema.to_string();
        assert!(
            schema.contains("\"list\"") && schema.contains("\"get\""),
            "入口 schema 列出工具"
        );
    }
}
