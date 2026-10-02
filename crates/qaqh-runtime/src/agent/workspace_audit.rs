//! 工作区变更审计注入（PR2）。
//!
//! 模型可以不走 edit/write 工具，而是用 `exec` 跑脚本批量改文件。qaqh 的审计链
//! 只覆盖“工具自报的路径”——`permission::extract_target_paths` 对 exec 只取 `cwd`，
//! `journal::record_change` 的调用方里也没有 exec——脚本实际改了什么没有代码知道。
//! 本模块在**工具批边界**做一次全量状态扫描（qaqh-spy），把净变更渲染成限额报告，
//! 作为一条 `role=user` + `name=workspace` 的注入消息下发，让模型不必再 `read`/`cat`
//! 自查。设计依据见 `docs/plan-workspace-diff-injection.md`。
//!
//! 三条硬约束：
//!
//! 1. **必须排在整批 tool 结果之后**（D1）。Chat Completions 要求 assistant
//!    (tool_calls) 之后紧跟全部对应 tool 消息，中间插一条 user 直接 HTTP 400
//!    （见 `qaqh-gate/src/chat_completions_api.rs` 的 `convert_messages` 注释）。
//!    因此钩子放在批级，不做 per-call 插队。
//! 2. **canonical ToolResult 零改动**（D6）。本模块不碰 `execution.rs`，也不写
//!    `ToolResult.diff`（那是展示面专用字段，`project_for_model` 明确不携带）。
//! 3. **失败绝不阻断工具批**。扫描/存储/提交任何错误只记 warn，退化为“本批无报告”。

use std::path::{Path, PathBuf};
use std::time::Duration;

use qaqh_spy::{Change, ChangeStatus, ScanId, ScanOpts, Session, Trigger};
use qaqh_types::{ContentBlock, Message};
use std::collections::HashSet;

use crate::agent::types::{AdmittedTool, RingContext};

/// 报告字节预算（防上下文爆炸）。`QAQH_SPY_REPORT_BYTES` 可覆盖。
const DEFAULT_REPORT_BYTES: usize = 4_096;
/// 争锁等待上界：并发会话在同一工作区上扫到时，宁可跳过本批报告，
/// 也不把 agent 循环阻塞到 spy 默认的 15 秒。
const LOCK_WAIT: Duration = Duration::from_millis(500);
/// 注入头部里最多列举的 tool_call id 数（其余折叠计数）。
const MAX_LISTED_CALLS: usize = 8;
/// 多调用批写进 SMJ 的 tool 字段：变更无法归因到单个调用，如实标 scan。
const SCAN_TOOL_LABEL: &str = "scan";

/// 一批工具的审计边界：`begin` 打点，`finish` 收口、回填并注入。
pub struct BatchAudit {
    session: Session,
    mark: ScanId,
    calls: Vec<String>,
    /// 工作区根：把 spy 的相对路径解析成绝对路径，供账本与审计链使用。
    root: PathBuf,
    /// 本批工具自报的写路径（账本键口径）。spy 测到但落进这个集合的文件，工具
    /// 自己已经记过 SMJ / 刷新过账本，不得重复回填——否则审计流水翻倍，
    /// journal query 里会出现成对的假记录。
    declared_writes: HashSet<String>,
    /// 记入 SMJ 的 tool 字段：单调用批用真实工具名，多调用批用 scan。
    tool_label: String,
    /// 记入 SMJ 的 tool_use_id：单调用批填该 call id，否则留空（与既有调用方一致）。
    tool_use_id: String,
}

impl BatchAudit {
    /// 报告字节预算。
    fn report_bytes() -> usize {
        std::env::var("QAQH_SPY_REPORT_BYTES")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .filter(|n| (512..=65_536).contains(n))
            .unwrap_or(DEFAULT_REPORT_BYTES)
    }

    /// 灰度/排障开关：`QAQH_SPY_DISABLED=1` 时整条链路退化为无操作。
    fn disabled() -> bool {
        matches!(
            std::env::var("QAQH_SPY_DISABLED").as_deref(),
            Ok("1") | Ok("true")
        )
    }
}

/// 解析当前工作区根。空 / `.` 视为无工作区（不启用审计）。
fn workspace_root() -> Option<PathBuf> {
    let raw = qaqh_workspace::current_workspace();
    if raw.is_empty() || raw == "." {
        return None;
    }
    Some(PathBuf::from(raw))
}

/// 批执行**之前**打点：建基线/记录边界扫描。任何失败返回 `None`（静默退化）。
pub fn begin(admitted: &[AdmittedTool]) -> Option<BatchAudit> {
    if BatchAudit::disabled() || admitted.is_empty() {
        return None;
    }
    let root = workspace_root()?;
    let opts = ScanOpts {
        lock_wait: LOCK_WAIT,
        ..ScanOpts::default()
    };
    let session = Session::open(&root, None)
        .map(|s| s.with_opts(opts))
        .map_err(|e| log::warn!("[spy] 打开会话失败，本批跳过变更审计: {e}"))
        .ok()?;
    let outcome = session
        .scan(Trigger::ToolStart)
        .map_err(|e| log::warn!("[spy] 边界扫描失败，本批跳过变更审计: {e}"))
        .ok()?;
    let calls = admitted
        .iter()
        .map(|a| a.call_id.clone())
        .collect::<Vec<_>>();
    // 自报写路径按账本键归一化。exec 不在 conflict::file_write_paths 的名单里
    // （它返回空集），所以脚本副作用天然落不进这个集合——正是要补的那一类。
    let declared_writes = admitted
        .iter()
        .flat_map(|a| qaqh_workspace::conflict::file_write_paths(a.auth.tool_name(), a.auth.args()))
        .map(|p| qaqh_workspace::file_state::ledger_key(&absolutize(&root, &p)))
        .collect::<HashSet<_>>();
    let tool_label = if admitted.len() == 1 {
        admitted[0].auth.tool_name().to_string()
    } else {
        SCAN_TOOL_LABEL.to_string()
    };
    let tool_use_id = if admitted.len() == 1 {
        admitted[0].call_id.clone()
    } else {
        String::new()
    };
    Some(BatchAudit {
        session,
        mark: ScanId(outcome.id),
        calls,
        root,
        declared_writes,
        tool_label,
        tool_use_id,
    })
}

/// 批执行**之后**收口：补一次扫描 → 回填宿主审计链与文件账本 → 渲染限额报告
/// （含可执行回滚命令）→ 注入落盘。
///
/// 提交后立即 drain_turn_boundary，不能只 submit。Loop::drain_injections 在总线
/// 无投递时**提前返回**，不会排空 ContextFlow 的 pending 队列；只 submit 会让报告
/// 长期滞留在内存里永不进 messages.jsonl（即“落盘 != 传输”分叉的反向形态）。
/// 此刻整批 tool 结果均已回填，排空点与 lap 边界语义等价。
pub fn finish(ctx: &mut RingContext, audit: Option<BatchAudit>, turn_id: &str, round_num: u32) {
    let Some(audit) = audit else { return };
    let changes = match audit.session.changes_after_tool_end(&audit.mark) {
        Ok(changes) => changes,
        Err(error) => {
            log::warn!("[spy] 批尾扫描失败（不影响工具结果）: {error}");
            return;
        }
    };
    if changes.is_empty() {
        return; // 零变更：不注入、不回填、不占上下文。
    }
    let hints = backfill(ctx, &audit, &changes);
    let report = match audit
        .session
        .render_report(&changes, BatchAudit::report_bytes())
    {
        Ok(report) => report,
        Err(error) => {
            log::warn!("[spy] 变更报告渲染失败（不影响工具结果）: {error}");
            return;
        }
    };
    let text = render_injection(
        &audit.mark,
        &audit.calls,
        turn_id,
        round_num,
        &report,
        &hints,
    );
    let msg = Message {
        msg_id: None,
        role: Message::ROLE_USER.into(),
        name: Some(qaqh_message::builtin::WORKSPACE.into()),
        content: vec![ContentBlock::text(&text)],
    };
    // command_id 只用于注入回执追踪，**不做内容去重**（ContextFlow 不按它去重）。
    let command_id = format!("{turn_id}#{round_num}#spy");
    if let Err(error) = ctx
        .flow
        .submit(qaqh_message::builtin::WORKSPACE, msg, Some(command_id))
    {
        log::warn!("[spy] 变更报告注入提交失败: {error}");
        return;
    }
    let model = ctx.agent.config.model.clone();
    let effort = ctx.agent.config.reasoning_effort.clone();
    let (drained, _) = ctx
        .flow
        .drain_turn_boundary(&mut ctx.agent.msg, &model, &effort);
    log::debug!("[spy] 批尾注入变更报告（{drained} 条已落盘，turn={turn_id} round={round_num}）");
}

/// 把 spy 测得的净变更回填进两条**既有**宿主设施，并产出可执行回滚命令。
///
/// 1. SMJ（journal::record_change）：让模型用已经存在的 journal 工具
///    query/replay 找回脚本改动——不新增工具面。
/// 2. file_state 账本：脚本改过的文件此前没有刷新指纹，导致下一次
///    edit/write 命中 stale_file、被迫重新 read。回填后该假阳性消失。
///
/// 跳过规则：
/// - 落在本批自报写路径集合里的文件（工具自己已记过，重复记会污染审计流水）；
/// - 任一侧内容不是合法 UTF-8 的文件——SMJ 的 store_blob/read_blob 走
///   String + read_to_string，**只存文本**，喂二进制会在 replay 时才炸。
fn backfill(ctx: &RingContext, audit: &BatchAudit, changes: &[Change]) -> Vec<String> {
    let mut hints = Vec::new();
    let mut skipped_binary = 0usize;
    for change in changes {
        let abs = absolutize(&audit.root, &change.path);
        if audit
            .declared_writes
            .contains(&qaqh_workspace::file_state::ledger_key(&abs))
        {
            continue;
        }
        let before = change
            .before
            .as_deref()
            .and_then(|sha| audit.session.cat(sha).ok());
        let after = change
            .after
            .as_deref()
            .and_then(|sha| audit.session.cat(sha).ok());
        let before_text = before
            .as_deref()
            .and_then(|bytes| std::str::from_utf8(bytes).ok());
        let after_text = after
            .as_deref()
            .and_then(|bytes| std::str::from_utf8(bytes).ok());
        // 有内容但解不出 UTF-8 才算二进制；删除/新增的另一侧为 None 是正常形态。
        let binary =
            before.is_some() && before_text.is_none() || after.is_some() && after_text.is_none();
        if binary {
            skipped_binary += 1;
            continue;
        }
        let seq = qaqh_workspace::journal::record_change(
            &ctx.agent.session.session_id,
            &audit.tool_use_id,
            &audit.tool_label,
            &change.path,
            op_name(change.status),
            before_text,
            after_text,
            "ok",
        );
        // 账本刷新与 SMJ 解耦：即便 SMJ 写失败（返回 None），磁盘内容已经是新的，
        // 指纹必须跟上，否则 stale_file 假阳性照旧。
        match change.status {
            ChangeStatus::Deleted => qaqh_workspace::file_state::record_delete(&abs),
            _ => {
                if let Some(text) = after_text {
                    qaqh_workspace::file_state::record_write(&abs, text);
                }
            }
        }
        if let Some(seq) = seq {
            // at = seq-1 一律正确：replay 到本步之前 => 撤销本步。
            // 新增文件此前不存在 → replay 返回 None → 删除；删除文件则取回旧内容。
            hints.push(format!(
                "撤销 {}: journal action=replay file={} at={} out={}",
                change.path,
                change.path,
                seq.saturating_sub(1),
                change.path
            ));
        }
    }
    if skipped_binary > 0 {
        hints.push(format!(
            "（{skipped_binary} 个二进制文件已快照但无法进 SMJ/账本：SMJ 只存文本）"
        ));
    }
    hints
}

/// SMJ 的 op 字段：直接用变更类型，避免与既有工具的 replace/append/overwrite 混淆。
fn op_name(status: ChangeStatus) -> &'static str {
    match status {
        ChangeStatus::Added => "scan_added",
        ChangeStatus::Modified => "scan_modified",
        ChangeStatus::Deleted => "scan_deleted",
    }
}

/// spy 的 Change.path 是工作区相对路径（'/' 分隔）；账本与审计链需要绝对路径。
fn absolutize(root: &Path, rel: &str) -> String {
    let candidate = Path::new(rel);
    if candidate.is_absolute() {
        return rel.to_string();
    }
    root.join(rel).to_string_lossy().to_string()
}

/// 组装注入正文：自标签头 + 归因口径声明 + spy 的限额报告。
///
/// `name` 只在 Chat Completions 活到 wire（Responses/Anthropic 均不携带），
/// 所以来源与归因信息必须写进文本本身——这是硬依赖而非装饰。
fn render_injection(
    mark: &ScanId,
    calls: &[String],
    turn_id: &str,
    round_num: u32,
    report: &str,
    hints: &[String],
) -> String {
    let listed = calls.len().min(MAX_LISTED_CALLS);
    let mut call_list = calls
        .iter()
        .take(listed)
        .cloned()
        .collect::<Vec<_>>()
        .join(",");
    if calls.len() > listed {
        call_list.push_str(&format!(", …(+{})", calls.len() - listed));
    }
    // 只标起始边界 id：批尾扫描 id 由 report 侧自己推进，而模型要回滚需要的
    // 是 SMJ 序号，已在 hints 里逐文件给出。
    let mut out = String::new();
    out.push_str(&format!(
        "[workspace-changes turn={turn_id} round={round_num} mark={} calls={call_list}]\n",
        mark.0
    ));
    out.push_str(ATTRIBUTION_NOTE);
    out.push('\n');
    out.push_str(report);
    if !hints.is_empty() {
        out.push_str("\n可执行回滚（journal 工具）:\n");
        for hint in hints {
            out.push_str("  ");
            out.push_str(hint);
            out.push('\n');
        }
    }
    out
}

/// 归因口径：扫描测的是**工作区状态转移**，不是系统调用级归因。并行工具与
/// 后台进程写入都会被计入同一批。措辞必须诚实，否则模型会把别人的改动
/// 当成自己脚本的后果。
const ATTRIBUTION_NOTE: &str = concat!(
    "（按工作区状态扫描测得，覆盖本批全部工具的净效果；并行工具与脚本派生的",
    "后台写入同样计入，不等同于单个调用的系统调用归因）\n",
);
