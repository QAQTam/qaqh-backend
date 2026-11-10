//! `handoff`：模型主动「把工作推到新会话」的控制面工具。
//!
//! 这不是传统的上下文压缩：本会话的 loop 到此终止，不再回到模型一轮。交接文档
//! 由**模型自己**书写（它最知道什么重要），落盘 + 入库后，由持有 driver 的客户端
//! 用 `session.create` + `conversation.send_message` 开一个新会话并把文档投进
//! 「原本第一条 user 消息」的位置，从那里继续推进。
//!
//! 契约面：零新增。声明走 `tool_declared{tool_name:"handoff", args}`，结果走
//! `tool_finished{output}`，报错走 `TurnError{code,message,retryable}`——都是
//! RingingV2 已有的字段。`TurnTerminal` 是冻结的三值，所以终态仍报 `completed`，
//! 交接事实由这条工具调用自身在 timeline 里承载（前端按工具名识别，不按终态）。

use std::path::PathBuf;

use qaqh_types::{ContextUsage, ToolDef, ToolFunction, UsageInfo};

/// 模型面工具名。客户端也按这个名字识别「该 fork 了」。
pub const TOOL_NAME: &str = "handoff";
/// 交接文档正文字段（Markdown）。
pub const FIELD_DOC: &str = "handoff_doc";
/// 一句话工作简述字段（新会话标题/首行摘要用）。
pub const FIELD_SUMMARY: &str = "work_summary";

/// 一条「本会话改过的文件」事实，来源是 `tool_calls.code_delta`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FileNote {
    pub path: String,
    pub added: usize,
    pub removed: usize,
}

/// 交接文档字段。缺任一字段都是模型的错，直接报回去，不做兜底文案。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Doc {
    pub handoff_doc: String,
    pub work_summary: String,
}

fn tool_description() -> String {
    "把工作交接给一个全新的会话并在那里继续（上下文接近上限时**必须**用，工具结果尾部的 \
     `[#ctx 61% · 98k/160k]` 就是实时占用）。\n\
     调用后依次发生：1) 你的交接文档被写入 `sessions/<id>/handoff/handoff-*.md` 并入库存档；\
     2) **本会话的 loop 立即终止**，不会再有下一次模型请求；3) 客户端拿这份文档开新会话，\
     把它作为新会话的第一条输入并继续推进。\n\
     新会话看不到本会话的工具调用、思考过程和 system 环境块——它只能读到你写的这份文档，\
     所以文档必须自足。必写三段：\n\
     ①当前任务与完成判据（怎样算做完）。\n\
     ②已完成：逐条给可验证事实（文件路径、命令、测试名、结论）。\n\
     ③待决与坑：**必须逐条继承上游所有未闭环项**，包括用户明确定过的口径、被否决过的方案、\
     踩过的坑——链只靠这份文档延续，你没写就等于没发生。\n\
     禁止声称验证过没做的事；不确定就写「未验证」。已修改文件清单由系统自动附带，不必复述。"
        .to_string()
}

/// 交给模型的声明。
#[must_use]
pub fn tool_def() -> ToolDef {
    ToolDef {
        call_type: "function".to_string(),
        function: ToolFunction {
            name: TOOL_NAME.to_string(),
            description: tool_description(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    FIELD_DOC: {
                        "type": "string",
                        "description": "交接文档正文（Markdown）：当前任务与判据 / 已完成 / 待决与坑，必须自足并继承上游未闭环项"
                    },
                    FIELD_SUMMARY: {
                        "type": "string",
                        "description": "一句话工作简述：新会话的标题与首行摘要用"
                    }
                },
                "required": [FIELD_DOC, FIELD_SUMMARY]
            }),
        },
    }
}

/// 无条件把 `handoff` 追加进模型面工具清单（幂等）。
///
/// 它必须在 allowlist 之外：`filtered_defs()` 按会话/全局白名单一刀切，minimal 档
/// 连 `ask` 都不带，若 handoff 也被过滤掉，模型就没有逃生阀，只会在撞墙时 400。
pub fn ensure_declared(tools: &mut Option<Vec<ToolDef>>) {
    match tools {
        Some(list) => {
            if !list.iter().any(|tool| tool.function.name == TOOL_NAME) {
                list.push(tool_def());
            }
        }
        None => *tools = Some(vec![tool_def()]),
    }
}

fn field<'a>(input: &'a serde_json::Value, key: &str) -> Result<&'a str, String> {
    match input.get(key) {
        None => Err(format!("handoff 缺少 `{key}` 字段")),
        Some(serde_json::Value::String(text)) if !text.trim().is_empty() => Ok(text),
        Some(serde_json::Value::String(_)) => Err(format!("handoff 的 `{key}` 不能为空")),
        Some(_) => Err(format!("handoff 的 `{key}` 必须是字符串")),
    }
}

/// 解析模型提交的交接文档。
///
/// # Errors
/// 字段缺失、类型不对或为空串时报错——让模型重写，而不是后端替它编一份。
pub fn parse_doc(input: &serde_json::Value) -> Result<Doc, String> {
    Ok(Doc {
        handoff_doc: field(input, FIELD_DOC)?.to_string(),
        work_summary: field(input, FIELD_SUMMARY)?.to_string(),
    })
}

/// 交接文档镜像目录：`{data_dir}/sessions/{session_id}/handoff`。
///
/// 走 `platform::data_dir()` 而不是自拼路径——`QAQH_DATA_DIR` 多实例与测试隔离
/// 全靠它，且 `qaqh-session` 的清理逻辑只认这两个每会话目录。
#[must_use]
pub fn mirror_dir(session_id: &str) -> PathBuf {
    qaqh_types::platform::data_dir()
        .join("sessions")
        .join(session_id)
        .join("handoff")
}

/// `MM-DD-HHMMSS`（UTC+8，与 `platform::now_utc8` 同一口径）。
#[must_use]
pub fn stamp() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs()
        + 8 * 3600;
    let day_secs = secs % 86_400;
    let (_, m, d) = qaqh_types::platform::civil_from_days((secs / 86_400) as i64);
    format!(
        "{m:02}-{d:02}-{:02}{:02}{:02}",
        day_secs / 3600,
        (day_secs % 3600) / 60,
        day_secs % 60
    )
}

/// 落盘交接文档镜像，返回写入路径。
///
/// 权威永远是 SQLite（`tool_calls.args_json` 里存着模型亲手写的原文，`blocks` 里
/// 存着这条 tool_result）；这份 `.md` 只是给人和 CLI 读的派生物，删了可以重导。
///
/// # Errors
/// 建目录或写字节失败一律外报，绝不「写盘失败也算交接成功」。
pub fn write_mirror(
    session_id: &str,
    turn_id: &str,
    doc: &Doc,
    files: &[FileNote],
    context: Option<ContextUsage>,
) -> Result<PathBuf, String> {
    let dir = mirror_dir(session_id);
    let path = dir.join(format!("handoff-{}.md", stamp()));
    let body = render_markdown(session_id, turn_id, doc, files, context);
    std::fs::create_dir_all(&dir)
        .map_err(|error| format!("建交接目录失败 {}: {error}", dir.display()))?;
    std::fs::write(&path, body)
        .map_err(|error| format!("写交接文档失败 {}: {error}", path.display()))?;
    Ok(path)
}

fn render_markdown(
    session_id: &str,
    turn_id: &str,
    doc: &Doc,
    files: &[FileNote],
    context: Option<ContextUsage>,
) -> String {
    let mut out = String::from("# 交接文档\n\n");
    out.push_str(&format!(
        "- 来源会话：`{session_id}`\n- 来源回合：`{turn_id}`\n"
    ));
    out.push_str(&format!("- 时间：{}\n", qaqh_types::platform::now_utc8()));
    if let Some(usage) = context {
        out.push_str(&format!(
            "- 交接时上下文占用：{} / {} tokens（{}%）\n",
            usage.used_tokens,
            usage.window_tokens,
            usage.used_pct()
        ));
    }
    out.push_str(&format!("\n## 工作简述\n\n{}\n", doc.work_summary));
    out.push_str("\n## 交接文档\n\n");
    out.push_str(&doc.handoff_doc);
    if !out.ends_with('\n') {
        out.push('\n');
    }
    out.push_str("\n## 本会话已修改文件（系统实测，非模型复述）\n\n");
    if files.is_empty() {
        out.push_str("（无记录）\n");
    } else {
        for file in files {
            out.push_str(&format!(
                "- `{}` +{}/-{}\n",
                file.path, file.added, file.removed
            ));
        }
    }
    out.push_str(
        "\n## 给下一个会话的话\n\n\
         这份文档是你唯一的上下文来源：工具调用轨迹、思考和 system 环境块都没有跟过来。\
         待决清单里没写的事，等于没交代。\n",
    );
    out
}

/// 回给前端的工具输出：前半句人话 + 尾部 fenced JSON 机器面。
///
/// 机器面同时是落库的 tool_result 文本，所以关键 id 绝不只写在「俏皮话」里。
#[must_use]
pub fn success_output(
    session_id: &str,
    turn_id: &str,
    doc: &Doc,
    mirror: &std::path::Path,
    files: &[FileNote],
    context: Option<ContextUsage>,
) -> String {
    let report = serde_json::json!({
        "handed_off": true,
        "from_session_id": session_id,
        "turn_id": turn_id,
        "work_summary": doc.work_summary,
        "handoff_doc": doc.handoff_doc,
        "handoff_path": mirror.display().to_string(),
        "modified_files": files
            .iter()
            .map(|file| file.path.clone())
            .collect::<Vec<_>>(),
        "context_used_tokens": context.map(|usage| usage.used_tokens),
        "context_window_tokens": context.map(|usage| usage.window_tokens),
        "next": "客户端用 session.create 建新会话（继承 cwd/model_selection/tool_mode/custom_tools），\
                 再以 conversation.send_message 把 handoff_doc 作为新会话第一条输入投进去",
        "loop": "本会话 loop 已终止；不会再有下一次模型请求",
    });
    format!("已封箱交接。本会话到此为止，余下的路由到新开的那条线去谈。\n\n```json\n{report}\n```")
}

/// 同回合第二次调用：不再交接，也不报错掩盖——明说这是重复调用。
#[must_use]
pub fn duplicate_output(session_id: &str) -> String {
    format!("这一回合已经交接过了（{session_id}），不再重复封箱。")
}

/// 模型可见的上下文占用页脚。窗口未知（未声明/为 0）时不编百分比，返回空串。
#[must_use]
pub fn ctx_footer(used_tokens: u32, window_tokens: u32) -> String {
    match ContextUsage::new(used_tokens, window_tokens) {
        Some(usage) => format!(
            "\n[#ctx {}% · {}k/{}k · 接近阈值请调用 handoff]",
            usage.used_pct(),
            usage.used_tokens / 1000,
            usage.window_tokens / 1000
        ),
        None => String::new(),
    }
}

/// 软阈值提醒（会作为一条**可见**的 timeline Input 落库，不是暗箱）。
#[must_use]
pub fn reminder(usage: ContextUsage) -> String {
    format!(
        "[系统] 上下文占用 {}%（{} / {} tokens）。本轮必须调用 handoff 写交接文档：\
         超过硬阈值后请求会被拒发，届时没有交接文档就是交接失败，不静默降级。",
        usage.used_pct(),
        usage.used_tokens,
        usage.window_tokens
    )
}

/// 交接失败（模型该调没调）的错误码，走现成 `TurnError{code,message,retryable}`。
pub const ERR_MISSING_DOC: &str = "handoff_missing";

/// 硬阈值（百分比）：到这儿不再赌模型自觉。它只在 `autoCompactThreshold > 0`
/// 时生效——契约里 `0` = 关闭自动压缩，硬收口也一并停手，不替用户做主。
pub const HARD_THRESHOLD_PCT: u16 = 95;

/// 折叠占位文本。同时是幂等标记：已经是这条的结果不再重复计数。
pub const FOLD_NOTE: &str = "[工具结果已折叠以腾出交接空间；完整事实仍在会话存档，无需在此复述]";

/// 折叠那一轮的强制要求（同样落库成可见 Input）。
#[must_use]
pub fn forced_reminder(folded: usize, usage: ContextUsage) -> String {
    format!(
        "[系统] 上下文占用 {}%（{} / {} tokens），已折叠 {folded} 条工具结果腾出这一轮。\
         本轮唯一任务：调用 handoff 写交接文档。不写就没有下一次——请求会被拒发。",
        usage.used_pct(),
        usage.used_tokens,
        usage.window_tokens
    )
}

/// 从 provider 回传的 usage 取「总输入」。
///
/// 优先 `命中 + 未命中`：inclusive 端点上裸 `prompt_tokens` 与缓存桶重叠，直接拿它
/// 当占用会把读数读成 2×（`qaqh_gate::usage_semantics` 模块头成文记录过这个失真）。
/// 两个桶都没数时才退回 `prompt_tokens`。
#[must_use]
pub fn total_input(usage: &UsageInfo) -> u32 {
    let cached = usage
        .prompt_cache_hit_tokens
        .saturating_add(usage.prompt_cache_miss_tokens);
    if cached > 0 {
        cached
    } else {
        usage.prompt_tokens
    }
}

/// 从 provider 回传的 usage + 当前分母取占用快照。
#[must_use]
pub fn occupancy(usage: &UsageInfo, window_tokens: u32) -> Option<ContextUsage> {
    ContextUsage::new(total_input(usage), window_tokens)
}

/// 上游「上下文超限」类错误的识别。
///
/// 各家文案不统一，但都直说长度问题；命中即视为**该窗口不够用**的实测证据。
#[must_use]
pub fn is_context_overflow(text: &str) -> bool {
    let lower = text.to_lowercase();
    [
        "context_length_exceeded",
        "context length",
        "maximum context",
        "too many tokens",
        "input tokens exceed",
        "prompt is too long",
        "context window",
    ]
    .iter()
    .any(|needle| lower.contains(needle))
}

/// 分母的实测权威：`(base_url, model) → 已被上游证实可用的输入量`。
///
/// 只由真实信号驱动：请求被接受且用量更大 → 上抬；被判超限 → 下压。**不写回**
/// `ModelDef`/目录（那是契约类型，且会被模型编辑器的全量覆盖固化），所以前端是否
/// 显式回传 `context_window` 都不影响收敛结果。作用域与 gate 的 `chat_sync` 退化
/// 键一致（`usage_semantics.rs:37`）。
static OBSERVED: std::sync::OnceLock<
    std::sync::Mutex<std::collections::HashMap<(String, String), u32>>,
> = std::sync::OnceLock::new();

fn observed_table()
-> std::sync::MutexGuard<'static, std::collections::HashMap<(String, String), u32>> {
    OBSERVED
        .get_or_init(Default::default)
        .lock()
        .unwrap_or_else(|poisoned| poisoned.into_inner())
}

/// 当前生效的分母：有实测就用实测，否则用声明值。
#[must_use]
pub fn effective_window(base_url: &str, model: &str, declared: u32) -> u32 {
    observed_table()
        .get(&(base_url.to_string(), model.to_string()))
        .copied()
        .unwrap_or(declared)
}

/// 请求被接受：用量超过当前分母就说明「窗口至少这么大」，抬到实测值。
///
/// 只在真实成功后抬——声明值猜大是事故，猜小只是早交接。
pub fn note_accepted(base_url: &str, model: &str, used: u32, declared: u32) {
    let mut table = observed_table();
    let key = (base_url.to_string(), model.to_string());
    let current = table.get(&key).copied().unwrap_or(declared);
    if used <= current {
        return;
    }
    table.insert(key, used);
    log::warn!("[COMPACT] 窗口实测上抬 {base_url} {model}: {current} → {used}（请求已被接受）");
}

/// 上游判超限：分母立刻收敛为「已知能被接受的最大输入量」。
///
/// 刚被证伪的声明值不再可信；没有实测记录时（第一跳就撞墙）退回声明值的一半，
/// 方向性永远是「宁可早交接」。收敛后占用读数直接顶到 100%，下一轮就是硬压力。
pub fn note_overflow(base_url: &str, model: &str, declared: u32) {
    let mut table = observed_table();
    let key = (base_url.to_string(), model.to_string());
    let current = table.get(&key).copied().unwrap_or(declared);
    let conservative = match table.get(&key).copied() {
        Some(max_accepted) => max_accepted,
        None => declared / 2,
    };
    if conservative >= current {
        // 已是已知最大值，没有更低的真实证据可依据——如实记录，不硬造收敛。
        log::error!(
            "[COMPACT] 判超限但无更低依据 {base_url} {model}: current={current} declared={declared}"
        );
        return;
    }
    table.insert(key, conservative);
    log::warn!(
        "[COMPACT] 窗口实测下压 {base_url} {model}: {current} → {conservative}（上游判超限）"
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_doc_rejects_missing_and_empty() {
        let missing = serde_json::json!({ FIELD_SUMMARY: "x" });
        assert!(parse_doc(&missing).unwrap_err().contains(FIELD_DOC));
        let empty = serde_json::json!({ FIELD_DOC: "  ", FIELD_SUMMARY: "x" });
        assert!(parse_doc(&empty).unwrap_err().contains("不能为空"));
        let ok = serde_json::json!({ FIELD_DOC: "d", FIELD_SUMMARY: "s" });
        assert_eq!(
            parse_doc(&ok).expect("合法入参"),
            Doc {
                handoff_doc: "d".into(),
                work_summary: "s".into()
            }
        );
    }

    #[test]
    fn ensure_declared_is_idempotent_and_survives_empty_catalog() {
        let mut tools: Option<Vec<ToolDef>> = None;
        ensure_declared(&mut tools);
        ensure_declared(&mut tools);
        let list = tools.expect("控制面工具必须无条件存在");
        assert_eq!(
            list.iter()
                .filter(|tool| tool.function.name == TOOL_NAME)
                .count(),
            1
        );
    }

    #[test]
    fn ctx_footer_silent_without_window() {
        assert!(ctx_footer(1000, 0).is_empty());
        assert!(ctx_footer(80_000, 100_000).contains("80%"));
    }

    #[test]
    fn window_converges_only_on_real_signals() {
        // 键是本用例专属的字面量：别的用例各用各的 ephemeral base_url，
        // 共享表里永不碰撞，因此不需要（也不该）清空全局表。
        const RAISE: &str = "https://unit-raise.example";
        const HALF: &str = "https://unit-half.example";
        // 声明 383K、实际被接受到 500K → 抬到实测下限。
        note_accepted(RAISE, "m1", 500_000, 383_000);
        assert_eq!(effective_window(RAISE, "m1", 383_000), 500_000);
        // 判超限但已有实测记录：没有更低依据，绝不硬造一个数。
        note_overflow(RAISE, "m1", 383_000);
        assert_eq!(effective_window(RAISE, "m1", 383_000), 500_000);
        // 第一跳就撞墙、无实测记录 → 保守一半（宁可早交接）。
        note_overflow(HALF, "m2", 200_000);
        assert_eq!(effective_window(HALF, "m2", 200_000), 100_000);
        assert!(is_context_overflow(
            "HTTP 400: This model's maximum context length is 200000 tokens"
        ));
        assert!(!is_context_overflow("HTTP 429: rate limited"));
    }
}
