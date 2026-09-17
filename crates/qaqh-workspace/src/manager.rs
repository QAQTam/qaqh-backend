//! ToolManager: tool registration, lookup, routing, and cancellation.
//!
//! Since v5: per-call execution metadata (ToolExecMeta) and cumulative
//! stats (ToolStats) are returned to the caller instead of being lost
//! to stderr. The caller (agent tools.rs) acts as a forwarding layer
//! that pushes these into UI events.

use std::collections::BTreeMap;
use std::sync::atomic::AtomicBool;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use crate::{SafetyVerdict, ToolHandler, ToolRisk};

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

pub struct ToolManager {
    pub(crate) handlers: BTreeMap<String, ToolHandler>,
    allowed: Option<Vec<String>>,
    /// PR-M2-2：set_allowed 的原始输入（未过 known 过滤）——动态层重建后
    /// 重应用用（观察项 ①：MCP refresh 换名后 custom 名单仍生效）。
    allowed_raw: Option<Vec<String>>,
    /// 动态工具（MCP；设计 §5.3）：完整前缀名 → 模型面 + 无状态路由。
    /// 与 `handlers` 分层的原因：`ToolHandler.description` 是 `&'static str`，
    /// 而 MCP 的描述/schema 来自 server（运行期 String）——平行结构避免
    /// `Box::leak` hack；E-5 单一 dispatcher fn 指针照旧（在飞安全）。
    dynamic: BTreeMap<String, DynamicTool>,
    inflight_tasks: BTreeMap<String, Arc<AtomicBool>>,
    stats_total: u32,
    stats_failures: u32,
    files_read: Vec<String>,
    files_written: Vec<String>,
}

/// 动态工具名前缀（S2：`mcp__{server}__{tool}`；与内置 19 工具零碰撞）。
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
    handler_fn: fn(crate::ToolCallCtx) -> crate::ToolResult,
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
            handler_fn,
            category,
            risk: ToolRisk::Administrative,
            default_timeout,
        },
    )
}

/// 动态工具注册条目（设计 §5.3/E-5；PR-M1-4）。
///
/// 与 [`ToolHandler`] 的差异：模型面（[`qaqh_types::ToolDef`]）与路由元数据
/// 合一，description 为自有 String（server 侧动态文本，经 2KB 截断）。
/// `handler_fn` 指向**同一个** dispatcher（无状态 fn 指针，PreparedCall
/// 捕获后永不失效——refresh 换 def 不影响在飞调用）。
#[derive(Clone)]
pub struct DynamicTool {
    /// 模型面（`mcp__{server}__{tool}` 命名 + schema 直通 + 截断后描述）。
    pub def: qaqh_types::ToolDef,
    /// 路由 fn（MCP 全体工具指向同一个 dispatcher，E-5）。
    pub handler_fn: fn(crate::ToolCallCtx) -> crate::ToolResult,
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
    pub(crate) handler_fn: fn(crate::ToolCallCtx) -> crate::ToolResult,
    pub(crate) ctx: crate::ToolCallCtx,
    pub(crate) audit_args: serde_json::Value,
}

impl Default for ToolManager {
    fn default() -> Self {
        Self::new()
    }
}

impl ToolManager {
    pub fn new() -> Self {
        Self {
            handlers: BTreeMap::new(),
            allowed: None,
            allowed_raw: None,
            dynamic: BTreeMap::new(),
            inflight_tasks: BTreeMap::new(),
            stats_total: 0,
            stats_failures: 0,
            files_read: Vec::new(),
            files_written: Vec::new(),
        }
    }

    pub fn register(&mut self, handler: ToolHandler) {
        let key = handler.key.clone();
        self.handlers.insert(key, handler);
    }

    /// 注册动态工具（MCP 投影入口；仅回合边界由 actor 调用——无并发写面）。
    ///
    /// 碰撞拒绝（设计 §5.3）：与内置词汇表（`handlers`）或已注册动态名重名
    /// → Err 且**不**写入（防模型面膨胀出歧义名）。刷新批次应先
    /// [`Self::clear_dynamic`] 再逐条注册（MCP 名带 `mcp__` 前缀，与内置
    /// 零碰撞；此处防御面向未来形态）。
    pub fn register_dynamic(&mut self, name: String, tool: DynamicTool) -> Result<(), String> {
        if self.handlers.contains_key(&name) || self.dynamic.contains_key(&name) {
            return Err(format!("dynamic tool name collides: {name:?}"));
        }
        self.dynamic.insert(name, tool);
        Ok(())
    }

    /// 清空动态层（tools/list_changed 或重连后的全量重建，M2 起使用）。
    pub fn clear_dynamic(&mut self) {
        self.dynamic.clear();
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

    pub fn lookup(&self, name: &str) -> Option<&ToolHandler> {
        self.handlers.get(name)
    }

    /// 查工具能力类别（权限决策单一事实源；内置 + 动态两层）。
    pub fn category_of(&self, name: &str) -> Option<crate::permission::ToolCategory> {
        if let Some(handler) = self.handlers.get(name) {
            return Some(handler.category);
        }
        self.dynamic.get(name).map(|tool| tool.category)
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
            .filter(|name| self.handlers.contains_key(name) || self.dynamic.contains_key(name))
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

    pub fn apply_init(&mut self, allowed_tools: Vec<String>, session_seed: &str) {
        self.set_allowed(allowed_tools);
        crate::set_current_session(session_seed);
    }

    pub fn all_defs(&self) -> Vec<qaqh_types::ToolDef> {
        let mut defs: Vec<qaqh_types::ToolDef> =
            self.handlers.values().map(|h| h.to_tool_def()).collect();
        // 动态层（MCP）合并在后：模型面 = 内置词汇表 + 动态投影。
        defs.extend(self.dynamic.values().map(|tool| tool.def.clone()));
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
    #[allow(clippy::result_large_err)] // 错误装箱属结构塑形，另立项
    pub(crate) fn prepare_req(
        &mut self,
        id: String,
        name: &str,
        action: &str,
        args: serde_json::Value,
        timeout_secs: Option<u64>,
        progress_tx: Option<crate::ExecProgressSender>,
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

        // 内置/动态统一路由视图：PreparedCall 只需要 fn 指针 + 超时 + risk；
        // ToolHandler 的 'static description 不参与执行路径。`category` 只供
        // 出工区安全判定区分「文件型」与「执行/网络型」Destructive 工具。
        struct ResolvedRoute {
            handler_fn: fn(crate::ToolCallCtx) -> crate::ToolResult,
            default_timeout: Duration,
            risk: ToolRisk,
            category: crate::permission::ToolCategory,
        }
        let route = match self.handlers.get(name) {
            Some(handler) => ResolvedRoute {
                handler_fn: handler.handler,
                default_timeout: handler.default_timeout,
                risk: handler.risk.clone(),
                category: handler.category,
            },
            None => match self.dynamic.get(name) {
                Some(tool) => ResolvedRoute {
                    handler_fn: tool.handler_fn,
                    default_timeout: tool.default_timeout,
                    risk: tool.risk.clone(),
                    category: tool.category,
                },
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
            },
        };

        let timeout_secs = timeout_secs.unwrap_or(route.default_timeout.as_secs());
        let cancel_flag = Arc::new(AtomicBool::new(false));
        let skill_effects = Arc::new(Mutex::new(Vec::new()));
        let ctx = crate::ToolCallCtx {
            id: id.clone(),
            name: name.to_string(),
            action: action.to_string(),
            args: args.clone(),
            tx_progress: progress_tx.clone(),
            timeout_secs: Some(timeout_secs),
            cancel: cancel_flag.clone(),
            skill_effects: skill_effects.clone(),
        };
        let in_workspace = is_path_in_workspace(&ctx, &route.risk, route.category);
        match crate::safety::SafetyPolicy::evaluate(route.risk.clone(), in_workspace) {
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

        self.inflight_tasks.insert(id.clone(), cancel_flag.clone());

        let audit_args = args.clone();
        let ctx = crate::ToolCallCtx {
            id: id.clone(),
            name: name.to_string(),
            action: action.to_string(),
            args,
            tx_progress: progress_tx,
            timeout_secs: Some(timeout_secs),
            cancel: cancel_flag,
            skill_effects,
        };

        Ok(PreparedCall {
            id,
            name: name.to_string(),
            handler_fn: route.handler_fn,
            ctx,
            audit_args,
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
                "read" | "skills" => {
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
fn extract_files_affected(_tool_name: &str, args: &serde_json::Value) -> Vec<String> {
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
/// (`command`/`argv`, workdir defaults to the workspace root); failing them
/// closed here would block the tool at every permission level, so their
/// containment stays with the permission layer (`classify_risk` already reports
/// Exec/Net as [`crate::PermissionRisk::High`]). Tools that never touch the file
/// system (`ask`, `task`, `skills`, …) are `ReadOnly`/`Write`/`Administrative`
/// and keep the permissive default.
fn is_path_in_workspace(
    ctx: &crate::ToolCallCtx,
    risk: &ToolRisk,
    category: crate::permission::ToolCategory,
) -> bool {
    if let Some(path) = ctx.args.get("path").and_then(|v| v.as_str()) {
        if path.is_empty() || path == "." {
            return true;
        }
        let ws = crate::current_workspace();
        if ws.is_empty() || ws == "." {
            return true;
        }
        let abs_path = if std::path::Path::new(path).is_absolute() {
            std::path::PathBuf::from(path)
        } else {
            std::path::Path::new(&ws).join(path)
        };
        // M13：组件级比较 + `..` 词法归一化。原先对字符串做 starts_with，
        // sibling 目录（`proj` vs `proj-backup`）与未解析的 `..` 逃逸都会
        // 被误判为在工内，导致 Destructive 出工区阻断被绕过。
        let ws_norm = crate::permission::normalize_lexically(std::path::Path::new(&ws));
        let path_norm = crate::permission::normalize_lexically(&abs_path);
        path_norm.starts_with(&ws_norm)
    } else {
        // No `path` arg — assume workspace operation for non-destructive tools
        // (e.g. task, skills, ask). A **file-scoped** Destructive tool without a
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
    use crate::{ToolCallCtx, ToolResult, ToolRisk};

    fn noop(_ctx: ToolCallCtx) -> ToolResult {
        ToolResult::ok("noop")
    }

    fn handler(key: &str) -> ToolHandler {
        ToolHandler {
            key: key.to_string(),
            description: "test handler",
            input_schema: serde_json::json!({ "type": "object" }),
            handler: noop,
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
        mgr.register(handler("read"));
        mgr.register(handler("exec"));
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
        mgr.register(handler("exec"));
        // 旧配置全是已移除的工具名：回退到"全部工具"（空 allowlist 语义），
        // 而不是把子代理锁死成零工具。
        mgr.apply_init(vec!["read".to_string(), "edit".to_string()], "s1");
        assert_eq!(names(&mgr), vec!["exec"]);
    }

    #[test]
    fn apply_init_empty_stays_all_tools() {
        let mut mgr = ToolManager::new();
        mgr.register(handler("exec"));
        mgr.apply_init(vec![], "s1");
        assert_eq!(names(&mgr), vec!["exec"]);
    }

    // ── set_allowed（4.1：工具模式运行时切换入口）──

    #[test]
    fn set_allowed_restricts_and_restores() {
        let mut mgr = ToolManager::new();
        mgr.register(handler("exec"));
        mgr.register(handler("read"));
        mgr.register(handler("write"));
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
        mgr.register(handler("exec"));
        // 未知名剔除（不静默吞掉，log warn）；全无效 → 全量
        mgr.set_allowed(vec!["exec".to_string(), "ghost".to_string()]);
        assert_eq!(names(&mgr), vec!["exec"]);
        mgr.set_allowed(vec!["ghost".to_string()]);
        assert_eq!(names(&mgr), vec!["exec"]);
    }

    #[test]
    fn set_allowed_does_not_touch_session() {
        let mut mgr = ToolManager::new();
        mgr.register(handler("exec"));
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
        mgr.register(handler("exec"));
        mgr.register(handler("read"));
        mgr.set_allowed(vec!["read".to_string()]);
        // 白名单外工具在执行层被拦截（纵深防御）
        let err = mgr.prepare_req(
            "c1".to_string(),
            "exec",
            "exec",
            serde_json::json!({"argv": ["echo", "hi"]}),
            None,
            None,
        );
        assert!(err.is_err());
        let ok = mgr.prepare_req(
            "c2".to_string(),
            "read",
            "read",
            serde_json::json!({"path": "x"}),
            None,
            None,
        );
        assert!(ok.is_ok());
    }
    // ── 动态层路由（PR-M1-4）：prepare 走注入的 dispatcher fn ──

    fn marker_fn(_ctx: ToolCallCtx) -> ToolResult {
        ToolResult::ok("mcp-dispatched")
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
                "",
                serde_json::json!({}),
                None,
                None,
            )
            .map_err(|report| report.content)
            .expect("dynamic tool prepare should succeed");
        assert_eq!(
            prepared.handler_fn as *const () as usize, marker_fn as *const () as usize,
            "路由必须指向注入的 dispatcher fn（E-5 单一 fn 指针）"
        );

        let report = match mgr.prepare_req(
            "id-2".to_owned(),
            "mcp__demo__nope",
            "",
            serde_json::json!({}),
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

    fn ctx_with_path(path: &str) -> crate::ToolCallCtx {
        ctx_with_args(serde_json::json!({ "path": path }))
    }

    fn ctx_with_args(args: serde_json::Value) -> crate::ToolCallCtx {
        crate::ToolCallCtx {
            id: "t".to_string(),
            name: "write_file".to_string(),
            action: String::new(),
            args,
            tx_progress: None,
            timeout_secs: None,
            cancel: std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false)),
            skill_effects: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
        }
    }

    fn in_ws(ctx: &crate::ToolCallCtx, risk: ToolRisk) -> bool {
        is_path_in_workspace(ctx, &risk, crate::permission::ToolCategory::Write)
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
        assert!(!in_ws(&ctx_with_path("a/../../outside.txt"), ToolRisk::Write));
        // 无 path 参数：非 Destructive 工具默认放行（ask/task/skills 不碰文件系统）。
        assert!(is_path_in_workspace(
            &crate::ToolCallCtx {
                id: "t".to_string(),
                name: "ask".to_string(),
                ..ctx_with_args(serde_json::json!({}))
            },
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
            !is_path_in_workspace(&no_path, &ToolRisk::Destructive, write_cat),
            "文件型 Destructive 工具缺 path 必须判为工区外（fail-closed）"
        );
        // 同样的参数形状下，非 Destructive 工具不受影响（不误伤 ask/task/skills）。
        assert!(is_path_in_workspace(&no_path, &ToolRisk::Write, write_cat));
        assert!(is_path_in_workspace(
            &no_path,
            &ToolRisk::ReadOnly,
            crate::permission::ToolCategory::Read
        ));
        assert!(is_path_in_workspace(
            &no_path,
            &ToolRisk::Administrative,
            crate::permission::ToolCategory::Read
        ));
        // 端到端判定：SafetyPolicy 必须把它阻断。
        assert!(matches!(
            crate::safety::SafetyPolicy::evaluate(
                ToolRisk::Destructive,
                is_path_in_workspace(&no_path, &ToolRisk::Destructive, write_cat)
            ),
            SafetyVerdict::Block(_)
        ));
        // Destructive 工具带工区内 path 时仍放行（`delete` 的正常形态）。
        assert!(is_path_in_workspace(
            &ctx_with_path(ws.join("trash-me.txt").to_str().unwrap()),
            &ToolRisk::Destructive,
            write_cat
        ));
        // Destructive 工具带工区外 path 时阻断。
        assert!(!is_path_in_workspace(
            &ctx_with_path(tmp.path().join("outside.txt").to_str().unwrap()),
            &ToolRisk::Destructive,
            write_cat
        ));
        // Exec/Net 型 Destructive 工具（`exec`：无 path 参数是设计使然，workdir
        // 缺省 = 工区根）保持放行——它们的围栏在权限层（classify_risk 已报 High）。
        assert!(is_path_in_workspace(
            &ctx_with_args(serde_json::json!({ "command": "rm -rf /tmp/x" })),
            &ToolRisk::Destructive,
            crate::permission::ToolCategory::Exec
        ));

        crate::set_workspace(&old_ws);
    }
}

/// P0-2 e2e：Level 4（`needs_permission` 全自动批准）下，`SafetyPolicy` 是
/// 文件型 Destructive 工具唯一的出工区闸门。缺 `path` 的 `delete` 形态必须
/// 被它阻断（`prepare_req` 真实路径，不经 handler）。
#[cfg(test)]
mod safety_e2e_tests {
    use super::*;

    fn destructive_handler(key: &str) -> ToolHandler {
        ToolHandler {
            key: key.to_string(),
            description: "test destructive handler",
            input_schema: serde_json::json!({ "type": "object" }),
            handler: |_ctx| crate::ToolResult::ok("ran"),
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
        mgr.register(destructive_handler("delete"));

        // Level 4 = 无权限检查（`needs_permission` 自动批准）；`prepare_req`
        // 里的 `SafetyPolicy` 是唯一剩下的闸门。
        let report = mgr
            .prepare_req(
                "c1".to_string(),
                "delete",
                "",
                serde_json::json!({}),
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
                "",
                serde_json::json!({ "path": ws.join("trash-me.txt").to_str().unwrap() }),
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
                "",
                serde_json::json!({ "path": tmp.path().join("outside.txt").to_str().unwrap() }),
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
