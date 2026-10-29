//! 工作区变更审计注入（PR2）。
//!
//! 模型可以不走 edit/write 工具，而是用 `exec` 跑脚本批量改文件。qaqh 的审计链
//! 只覆盖“工具自报的路径”——`permission::extract_target_paths` 对 exec 只取 `cwd`，
//! `journal::record_change` 的调用方里也没有 exec——脚本实际改了什么没有代码知道。
//! 本模块在**工具批边界**做一次全量状态扫描（qaqh-spy），把净变更渲染成限额报告，
//! 作为一条 `role=user` + `name=workspace` 的注入消息下发，让模型不必再 `read`/`cat`
//! 自查。设计依据见 `docs/plan-workspace-diff-injection.md`。
//!
//! 扫描**不是无条件**的：只有当批存在自报不了写路径的调用才打点（见 `batch_needs_scan`），
//! 且整条注入消息受总字节预算约束（见 `clamp_injection`）——后者是 2026-10-15 修
//! 「一次目录删除报出 9337 个净变更、单条注入 ≈3.5 MB」时加的硬闸门。
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
/// 注入**整条消息**的总字节预算。此前 4 KB 只管住报告正文，而头部、归因声明和
/// 逐文件回滚提示都排在预算之外：实测一次目录删除产生 9337 个净变更后，单条
/// 注入 ≈1.2–3.5 MB（≈30 万–90 万 token）。`QAQH_SPY_INJECTION_BYTES` 可覆盖。
const DEFAULT_INJECTION_BYTES: usize = 6_144;
/// 回滚提示区的字节预算。
const HINT_BUDGET_BYTES: usize = 1_024;
/// 头部 + 归因声明的预留字节。二者都是承重件不是装饰：`name` 只在 Chat Completions
/// 活到 wire，来源与归因口径必须写进文本本身（见 `render_injection` 说明）。
const HEADER_RESERVE_BYTES: usize = 640;
/// 回滚提示的行数上限，与字节预算同时生效（先到者折叠）。
const MAX_HINTS: usize = 8;
/// 总闸门触发时的收尾标记。
const TRUNCATION_NOTE: &str =
    "\n[workspace] 注入超出总预算，已截断（完整流水：spy action=journal）\n";
/// 「会话离开过」的默认时间门槛（毫秒）。低于它就不算漂移，避免每个正常批都
/// 多读一遍 journal、多塞一行。`QAQH_SPY_DRIFT_GAP_MILLIS` 可覆盖（0 = 每次都算）。
const DEFAULT_DRIFT_GAP_MILLIS: u64 = 3_600_000;

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
    /// 归属标识 = 本会话 session id，透传给 spy 的 `Session::with_owner`。
    /// 共享同一 store 的并发会话/子代理据此把自己的变更与别人的分开（PR-C）。
    owner: String,
    /// **本 owner 上次看到这个 store 的扫描点**（PR-D）。`None` = 该会话在这里没有
    /// 历史（首次运行）。注意不能用 `last_scan` 替代：那是全局最新边界，别人的批
    /// 会不停推进它，本会话的"离开期间"就永远量不出来。
    drift_from: Option<ScanId>,
    /// exec 调用级归因的账本键（finish 时由 tool_runtime 传入）。backfill
    /// 跳过这些路径：exec_audit 已按 call_id 记过 SMJ 并刷新账本。
    attributed: HashSet<String>,
}

impl BatchAudit {
    /// 报告段字节预算的**上限**。`QAQH_SPY_REPORT_BYTES` 可覆盖，但真正用的预算
    /// 还要被 [`BatchAudit::report_budget`] 按注入总预算再收一次。
    fn report_bytes() -> usize {
        std::env::var("QAQH_SPY_REPORT_BYTES")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .filter(|n| (512..=65_536).contains(n))
            .unwrap_or(DEFAULT_REPORT_BYTES)
    }

    /// 整条注入消息的总字节预算。`QAQH_SPY_INJECTION_BYTES` 可覆盖。
    fn injection_bytes() -> usize {
        std::env::var("QAQH_SPY_INJECTION_BYTES")
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .filter(|n| (1_024..=65_536).contains(n))
            .unwrap_or(DEFAULT_INJECTION_BYTES)
    }

    /// 报告段实际可用的预算：总预算扣掉头尾预留，再取 `report_bytes` 这个上限。
    /// 两处封顶**取小**，所以把 `QAQH_SPY_REPORT_BYTES` 调大不会重新打开总闸门——
    /// 否则那个 env 自己就成了新的爆炸入口。
    fn report_budget() -> usize {
        Self::injection_bytes()
            .saturating_sub(HINT_BUDGET_BYTES + HEADER_RESERVE_BYTES)
            .min(Self::report_bytes())
    }

    /// 「离开多久才算离开」的时间门槛（毫秒）。`QAQH_SPY_DRIFT_GAP_MILLIS` 可覆盖，
    /// 设 0 则每个新边界都算一次漂移（测试与排障用）。
    fn drift_gap_millis() -> u64 {
        std::env::var("QAQH_SPY_DRIFT_GAP_MILLIS")
            .ok()
            .and_then(|v| v.trim().parse::<u64>().ok())
            .unwrap_or(DEFAULT_DRIFT_GAP_MILLIS)
    }

    /// 灰度/排障开关：已拔除，默认为 true（QAQH_SPY_DISABLED 恒真）。
    fn disabled() -> bool {
        true
    }

    /// 排障/回归对比开关：`QAQH_SPY_ALWAYS_SCAN=1` 恢复"每批无条件两扫"的旧行为，
    /// 用于确认 [`batch_needs_scan`] 的收紧没有漏掉真实盲区。
    fn always_scan() -> bool {
        matches!(
            std::env::var("QAQH_SPY_ALWAYS_SCAN").as_deref(),
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

/// 这批工具值不值得打扫描边界。
///
/// 判据不是"有没有写工具"，而是**写路径能否被工具自报**：
/// `conflict::file_write_paths` 对 exec 返回空集（脚本副作用宿主无从声明，这正是
/// 本模块当初立项的盲区），这类批必须扫。反之纯读批（read/grep/lsp/…）根本不写盘，
/// 扫它有两处纯亏：
///
/// 1. 每批白付两次全量遍历（walk + stat，本仓稳态约百毫秒级，含 trash 时更贵）；
/// 2. ToolStart 扫描会把**别人**的变更吸进基线、而报告只看 `mark` 之后，于是
///    既报不出去（模型无从得知），又把窗口暴露成本批背上——归因污染但不产出信息。
///
/// 查不到 category 的（未注册工具、动态 MCP 名字漂移）按"需要扫"处理：fail-open
/// 到安全侧，宁可多报也不制造新的归因盲区。
fn batch_needs_scan(admitted: &[AdmittedTool]) -> bool {
    if BatchAudit::always_scan() {
        return true;
    }
    admitted.iter().any(|a| {
        let name = a.auth.tool_name();
        match qaqh_workspace::runtime::lookup_category(name) {
            // 任意代码执行 / 进程控制：脚本改了什么没有代码知道。
            Some(qaqh_workspace::permission::ToolCategory::Exec) => true,
            // 出网通路（web_fetch）能落到工作区外的下载再被搬进来，同类盲区。
            Some(qaqh_workspace::permission::ToolCategory::Net) => true,
            // 声明式写工具：能自报路径就交给它自己的 SMJ + 账本，不必扫。
            Some(qaqh_workspace::permission::ToolCategory::Write) => {
                qaqh_workspace::conflict::file_write_paths(name, a.auth.args()).is_empty()
            }
            Some(qaqh_workspace::permission::ToolCategory::Read) => false,
            None => true,
        }
    })
}

/// 批执行**之前**打点：建基线/记录边界扫描。任何失败返回 `None`（静默退化）。
///
/// `ctx` 只用来取 session id 作归属标识——扫描本身必须发生在整批工具之前。
pub fn begin(ctx: &RingContext, admitted: &[AdmittedTool]) -> Option<BatchAudit> {
    if BatchAudit::disabled() || admitted.is_empty() || !batch_needs_scan(admitted) {
        return None;
    }
    let root = workspace_root()?;
    let opts = ScanOpts {
        lock_wait: LOCK_WAIT,
        ..ScanOpts::default()
    };
    let owner = ctx.agent.session.session_id.clone();
    let session = Session::open(&root, None)
        .map(|s| s.with_opts(opts).with_owner(owner.clone()))
        .map_err(|e| log::warn!("[spy] 打开会话失败，本批跳过变更审计: {e}"))
        .ok()?;
    let outcome = session
        .scan(Trigger::ToolStart)
        .map_err(|e| log::warn!("[spy] 边界扫描失败，本批跳过变更审计: {e}"))
        .ok()?;
    let mark = ScanId(outcome.id.clone());
    // 恢复检测参照：owner 上次看到的边界（全局 `last_scan` 会被别人的批推进，不可用）。
    let drift_from = outcome.prior_mark.map(ScanId);
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
        mark,
        calls,
        root,
        declared_writes,
        tool_label,
        tool_use_id,
        owner,
        drift_from,
        attributed: HashSet::new(),
    })
}

/// 批执行**之后**收口：补一次扫描 → 回填宿主审计链与文件账本 → 渲染限额报告
/// （含可执行回滚命令）→ 注入落盘。
///
/// `exec_attributed`：批内 exec 调用经 `exec_audit` 双扫归因、**已记入
/// SMJ（tool=exec）并刷新 file_state** 的路径账本键（CLEAN-3/T9）。回填
/// 时跳过这些路径——同一变更不得既记 exec 又记 scan。
///
/// 提交后立即 drain_turn_boundary，不能只 submit。Loop::drain_injections 在总线
/// 无投递时**提前返回**，不会排空 ContextFlow 的 pending 队列；只 submit 会让报告
/// 长期滞留在内存里永不进 messages.jsonl（即“落盘 != 传输”分叉的反向形态）。
/// 此刻整批 tool 结果均已回填，排空点与 lap 边界语义等价。
pub fn finish(
    ctx: &mut RingContext,
    audit: Option<BatchAudit>,
    turn_id: &str,
    round_num: u32,
    exec_attributed: &[String],
) {
    let Some(mut audit) = audit else { return };
    audit.attributed.extend(exec_attributed.iter().cloned());
    let changes = match audit.session.changes_after_tool_end(&audit.mark) {
        Ok(changes) => changes,
        Err(error) => {
            log::warn!("[spy] 批尾扫描失败（不影响工具结果）: {error}");
            return;
        }
    };
    if changes.is_empty() && audit.drift_from.is_none() {
        return; // 零变更：不注入、不回填、不占上下文。
    }
    // 恢复摘要（PR-D）：本会话离开期间攒下的漂移。批级报告只看 `mark` 之后，
    // 外部变更会在 ToolStart 被静默吸进基线、永不对模型说话——这一行补的就是那处沉默。
    let drift_files = drift_away_files(&audit.session, audit.drift_from.as_ref(), &audit.mark);
    if changes.is_empty() && drift_files == 0 {
        return; // 只有指针在动、没有任何实质变化：不值得占用上下文。
    }
    // 归属切分（PR-C）：store 按工作区路径哈希共享，`c.scan > mark` 只做时间过滤，
    // 所以并发会话 / 子代理 / 无主外部扫描的变更都会落进这个窗口。只展开并回填
    // **本会话可归因**的那部分，其余压成一行计数——否则每个 agent 都要整批重述
    // 别人的 diff，还把它当成自己脚本的后果。
    let (own, external) = qaqh_spy::split_by_owner(&changes, &audit.owner);
    // 先渲染、再按「正文里实际展开了哪些文件」产回滚提示。顺序不能颠倒：
    // hints 此前对**每个**净变更文件各出一行（一行里路径重复三遍），且排在报告的
    // 4 KB 预算之外，是整条注入超出上限的主因。
    let report = match audit
        .session
        .render_report(&own, BatchAudit::report_budget())
    {
        Ok(report) => report,
        Err(error) => {
            log::warn!("[spy] 变更报告渲染失败（不影响工具结果）: {error}");
            return;
        }
    };
    let hints = backfill(ctx, &audit, &own, &report.expanded);
    let text = render_injection(
        &audit.mark,
        &audit.calls,
        turn_id,
        round_num,
        InjectionBody {
            report: &report.text,
            hints: &hints,
            external: external.len(),
            drift_files,
        },
    );
    // 最后一道闸：分层预算失手时，也要保证整条消息有界。
    let text = clamp_injection(text, BatchAudit::injection_bytes());
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
///
/// `expanded` 只约束**产多少条回滚提示**（文本量），不约束回填：SMJ 记账与
/// file_state 账本刷新仍覆盖全部变更，所以收窄注入不会削减恢复能力。
fn backfill(
    ctx: &RingContext,
    audit: &BatchAudit,
    changes: &[Change],
    expanded: &[String],
) -> Vec<String> {
    let mut hints = Vec::new();
    let mut skipped_binary = 0usize;
    let mut hinted = 0usize;
    let mut folded = 0usize;
    let expanded: HashSet<&str> = expanded.iter().map(String::as_str).collect();
    for change in changes {
        let abs = absolutize(&audit.root, &change.path);
        let ledger_key = qaqh_workspace::file_state::ledger_key(&abs);
        // exec 调用级归因已记过（tool=exec, use_id=call_id），不得重复记 scan。
        if audit.attributed.contains(&ledger_key) {
            continue;
        }
        if audit.declared_writes.contains(&ledger_key) {
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
            // `out` 不能省：journal 的 replay_to_path 在 out=None 时写 stdout，不落回原文件。
            // 未展开的文件不给命令——它连 diff 都没看到，给了也不会用。
            if !expanded.contains(change.path.as_str()) || hinted >= MAX_HINTS {
                folded += 1;
                continue;
            }
            hinted += 1;
            hints.push(format!(
                "journal action=replay file={p} at={} out={p}",
                seq.saturating_sub(1),
                p = change.path
            ));
        }
    }
    if folded > 0 {
        hints.push(format!(
            "…另有 {folded} 个变更的回滚点未列出：spy action=journal 取 change_id 后 undo"
        ));
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

/// 恢复摘要计数（PR-D）：本会话离开期间（`drift_from` → `mark`）攒下的去重
/// 文件数。批级报告只看 `mark` 之后，外部变更会在 ToolStart 被静默吸进基线、
/// 永不对模型说话——这里补的就是那处沉默。
///
/// 时间门槛（[`BatchAudit::drift_gap_millis`]）以内不算漂移：连续运行时 owner
/// 前界就在前一个批，窗口几乎为空，不值得多读 journal 多塞一行。时间差取不到
/// （manifest 被容量闸挤掉）视为离开很久，fail-open 报计数。journal 读不了则
/// 宁缺勿滥——审计注入永远不影响工具结果。
fn drift_away_files(
    session: &qaqh_spy::Session,
    drift_from: Option<&ScanId>,
    mark: &ScanId,
) -> usize {
    let Some(from) = drift_from else {
        return 0; // 本会话首次出现在这个 store：没有「上次」，谈不上漂移。
    };
    let Ok((files, gap)) = session.drift_window(from, mark) else {
        return 0;
    };
    match gap {
        Some(gap) if gap < BatchAudit::drift_gap_millis() => 0,
        _ => files,
    }
}

/// 组装注入正文：自标签头 + 归因口径声明 + spy 的限额报告。
///
/// `name` 只在 Chat Completions 活到 wire（Responses/Anthropic 均不携带），
/// 所以来源与归因信息必须写进文本本身——这是硬依赖而非装饰。
/// 渲染素材：限额报告正文、回滚提示与两行计数（漂移 / 外部）。
struct InjectionBody<'a> {
    report: &'a str,
    hints: &'a [String],
    external: usize,
    drift_files: usize,
}

fn render_injection(
    mark: &ScanId,
    calls: &[String],
    turn_id: &str,
    round_num: u32,
    body: InjectionBody<'_>,
) -> String {
    let InjectionBody {
        report,
        hints,
        external,
        drift_files,
    } = body;
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
    // 恢复批先交代"你不在的时候攒了多少变更"：这些变更已被 ToolStart 扫进基线，
    // 本批报告里看不到，不补这句就永远沉默。只给计数，不给路径/diff——十天积累
    // 可能上万个文件，展开就回到了本次要修的那个爆炸。
    if drift_files > 0 {
        out.push_str(&format!(
            "[workspace] 距本会话上次运行，工作区另有 {drift_files} 个文件发生变化（外部所为，未展开）：spy action=journal\n"
        ));
    }
    out.push_str(report);
    // 外部变更只报计数：它们属于别的会话/进程，既不在本批 diff 里，也不该由本会话回填。
    if external > 0 {
        out.push_str(&format!(
            "\n[workspace] 另有 {external} 个变更来自其他会话或进程（未展开、未回填）：spy action=journal\n"
        ));
    }
    if !hints.is_empty() {
        out.push_str("\n可执行回滚（journal 工具）:\n");
        // 行数上限在 backfill 侧（MAX_HINTS），这里是字节闸；两道谁先到算谁的。
        // 预留 160 B 给收尾计数行，保证"还有多少没列出"这个事实总能说出来。
        let mut used = 0usize;
        let mut shown = 0usize;
        for hint in hints {
            let line = format!("  {hint}\n");
            if used + line.len() > HINT_BUDGET_BYTES {
                break;
            }
            used += line.len();
            shown += 1;
            out.push_str(&line);
        }
        if shown < hints.len() {
            out.push_str(&format!(
                "  …另有 {} 条：spy action=journal\n",
                hints.len() - shown
            ));
        }
    }
    out
}

/// 整条注入消息的最后一道闸：超出总预算就按 char 边界截断并留显式标记。
/// 分层预算本已让这里几乎不该触发；一旦触发说明某个分段的预留算漏了——
/// 宁可截在边界上并明说，也绝不能让一条 `role=user` 消息打爆上下文。
/// 总预算由 [`BatchAudit::injection_bytes`] 给出，与报告段上限取小，
/// 所以调大任一个 env 都撑不开这里。
fn clamp_injection(mut text: String, max_bytes: usize) -> String {
    if text.len() <= max_bytes {
        return text;
    }
    let mut cut = max_bytes.saturating_sub(TRUNCATION_NOTE.len());
    while cut > 0 && !text.is_char_boundary(cut) {
        cut -= 1;
    }
    text.truncate(cut);
    text.push_str(TRUNCATION_NOTE);
    text
}

/// 归因口径：扫描测的是**工作区状态转移**，不是系统调用级归因。并行工具、
/// 后台进程写入与用户/其他进程的并发编辑都会被计入同一批。措辞必须诚实，
/// 否则模型会把别人的改动当成自己脚本的后果，甚至去"修复"它们。
const ATTRIBUTION_NOTE: &str = concat!(
    "（按工作区状态扫描测得，覆盖本批全部工具的净效果；并行工具、脚本派生的后台",
    "写入与用户/其他进程的并发编辑同样计入，不等同于单个调用的系统调用归因。",
    "与本批工具语义明显无关的变更可能是用户或其他程序所为：不要清理、删除或",
    "擅自修改这些文件。）\n",
);

#[cfg(test)]
mod tests {
    use super::*;

    fn injection(external: usize, hints: &[String]) -> String {
        injection_with(external, 0, hints)
    }

    fn injection_with(external: usize, drift_files: usize, hints: &[String]) -> String {
        render_injection(
            &ScanId("s0000000000000001_0001".to_string()),
            &["call-1".to_string()],
            "turn-a",
            3,
            InjectionBody {
                report: "[workspace] 自标记以来 1 个文件变更\n─── mine.rs (修改, 1KB → 2KB)\n@@\n-a\n+b\n",
                hints,
                external,
                drift_files,
            },
        )
    }

    /// PR-D：恢复摘要只在真漂移时出现，且只有计数、没有路径。
    #[test]
    fn drift_summary_is_count_only_one_liner() {
        let body = injection_with(0, 42, &[]);
        assert!(
            body.contains("另有 42 个文件发生变化"),
            "漂移计数必须说出来：{body}"
        );
        let quiet = injection_with(0, 0, &[]);
        assert!(
            !quiet.contains("另有 0 个文件"),
            "零漂移不占上下文：{quiet}"
        );
    }

    /// PR-C：外部变更只留一行计数——正文里既不该出现它的路径，也不该为它产 hints。
    #[test]
    fn external_changes_are_counted_not_expanded() {
        let body = injection(7, &[]);
        assert!(
            body.contains("另有 7 个变更来自其他会话或进程"),
            "计数行必须说出来：{body}"
        );
        assert!(body.contains("mine.rs"), "自己的变更照常展开：{body}");
        assert!(
            !body.contains("theirs.rs") && !body.contains("可执行回滚"),
            "外部变更不得进入正文或回滚提示：{body}"
        );
    }

    /// 字节闸：提示区超出 `HINT_BUDGET_BYTES` 就折叠，且必须说出还剩多少条。
    #[test]
    fn hint_region_is_byte_capped_and_says_so() {
        let wide = vec!["x".repeat(300); 20];
        let body = injection(0, &wide);
        // 303 B/行 → 预算 1024 B 只容得下 3 行，其余折叠。
        assert!(body.contains("…另有 17 条"), "折叠计数必须说出来：{body}");
        assert!(
            body.matches(&"x".repeat(300)).count() <= 3,
            "提示区不得越预算"
        );
    }

    /// 总闸门：短文本原样放行；超限文本按 char 边界截断并留显式标记——
    /// 中文正文被切在字符中间会产出非法 UTF-8，那比超长更糟。
    /// 用"报告段本身超长"来触发：分层预算只要算漏一处，这道闸必须兜住。
    #[test]
    fn clamp_bounds_total_size_without_splitting_chars() {
        let short = injection(0, &["journal action=replay file=a.rs at=0 out=a.rs".into()]);
        assert_eq!(clamp_injection(short.clone(), 64 * 1024), short);

        let mut long = injection(0, &[]);
        long.push_str(&"很长".repeat(2000));
        let clamped = clamp_injection(long, 700);
        assert!(clamped.len() <= 700, "实得 {} 字节", clamped.len());
        assert!(clamped.ends_with(TRUNCATION_NOTE), "必须留下截断标记");
        assert!(
            std::str::from_utf8(clamped.as_bytes()).is_ok(),
            "不得切在多字节字符中间"
        );
    }
}
