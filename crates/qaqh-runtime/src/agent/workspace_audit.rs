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

use std::path::PathBuf;
use std::time::Duration;

use qaqh_spy::{ScanId, ScanOpts, Session, Trigger};
use qaqh_types::{ContentBlock, Message};

use crate::agent::types::{AdmittedTool, RingContext};

/// 报告字节预算（防上下文爆炸）。`QAQH_SPY_REPORT_BYTES` 可覆盖。
const DEFAULT_REPORT_BYTES: usize = 4_096;
/// 争锁等待上界：并发会话在同一工作区上扫到时，宁可跳过本批报告，
/// 也不把 agent 循环阻塞到 spy 默认的 15 秒。
const LOCK_WAIT: Duration = Duration::from_millis(500);
/// 注入头部里最多列举的 tool_call id 数（其余折叠计数）。
const MAX_LISTED_CALLS: usize = 8;

/// 一批工具的审计边界：`begin` 打点，`finish` 收口并注入。
pub struct BatchAudit {
    session: Session,
    mark: ScanId,
    calls: Vec<String>,
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
    Some(BatchAudit {
        session,
        mark: ScanId(outcome.id),
        calls,
    })
}

/// 批执行**之后**收口：补一次扫描、渲染限额报告、注入并提交落盘。
///
/// 提交后立即 `drain_turn_boundary`——不能只 submit。`Loop::drain_injections`
/// 在总线无投递时**提前返回**，不会排空 `ContextFlow` 的 pending 队列；只 submit
/// 会让报告长期滞留在内存里永不进 `messages.jsonl`（即“落盘 ≠ 传输”那个分叉的
/// 反向形态）。此刻整批 tool 结果均已回填，排空点与 lap 边界语义等价。
pub fn finish(ctx: &mut RingContext, audit: Option<BatchAudit>, turn_id: &str, round_num: u32) {
    let Some(audit) = audit else { return };
    let report = match audit
        .session
        .report_since(&audit.mark, BatchAudit::report_bytes())
    {
        Ok(text) => text,
        Err(error) => {
            log::warn!("[spy] 变更报告生成失败（不影响工具结果）: {error}");
            return;
        }
    };
    if report.trim().is_empty() {
        return; // 零变更：不注入，不占上下文。
    }
    let text = render_injection(&audit.mark, &audit.calls, turn_id, round_num, &report);
    let msg = Message {
        msg_id: None,
        role: Message::ROLE_USER.into(),
        name: Some(qaqh_message::builtin::WORKSPACE.into()),
        content: vec![ContentBlock::text(&text)],
    };
    // command_id 兼作崩溃/重放幂等键：同一批不会二次注入。
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
    // 只标起始边界 id：report_since 内部补的 ToolEnd 扫描 id 不回传，
    // 而 undo/restore 定位需要的正是这条起始 id。
    let mut out = String::new();
    out.push_str(&format!(
        "[workspace-changes turn={turn_id} round={round_num} mark={} calls={call_list}]\n",
        mark.0
    ));
    out.push_str(ATTRIBUTION_NOTE);
    out.push('\n');
    out.push_str(report);
    out
}

/// 归因口径：扫描测的是**工作区状态转移**，不是系统调用级归因。并行工具与
/// 后台进程写入都会被计入同一批。措辞必须诚实，否则模型会把别人的改动
/// 当成自己脚本的后果。
const ATTRIBUTION_NOTE: &str = concat!(
    "（按工作区状态扫描测得，覆盖本批全部工具的净效果；并行工具与脚本派生的",
    "后台写入同样计入，不等同于单个调用的系统调用归因）\n",
);
