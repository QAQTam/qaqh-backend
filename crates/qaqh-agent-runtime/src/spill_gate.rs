//! `ADR-0035`：exec 洪峰的**模型消费面泄洪闸**。
//!
//! [ADR-0034](../docs/adr/0034-exec-flood-memory-surface.md) 把传输面（reader 合批、
//! 同 run 合帧、`Arc<str>` 水位共享、settle 不整读）钳住了，保护的是 **daemon 存活**；
//! 但合帧只是**限幅**——限幅后的帧仍旧一条不少地流经**模型上下文**。一个持续打日志的
//! 合法高产进程（长构建、日志转储、`yes` 型循环）照样能把上下文窗口淹掉。
//!
//! 本模块补上限幅与熔断之间的中间档：**引用转发**。
//! - 闸门装在模型消费面（本模块由 `tool_job` 的 exec 宿主调用），capture/泵面不设闸；
//! - 单 exec 会话模型面累计直灌超 [`MODEL_FACE_BUDGET_BYTES`] 即**开闸**：本帧不再直灌，
//!   投递一条合成通告帧（异常流量说明 + 权威日志绝对路径 + tail 预览）；
//! - **开闸不恢复**：本会话余下输出一律不再直灌，只回轻量水位与权威日志引用；
//! - 阈值**硬编码**，不经 `SessionSettingsPatch` 暴露（所有者 2026-11-22 裁定②）；
//! - 客户端事件流（SSE / 会话事件泵）**不设闸**：泄洪只削模型上下文，不削人看的界面。
//!
//! 泄洪区本就已经存在——[ADR-0034](../docs/adr/0034-exec-flood-memory-surface.md) 决策 3
//! 把 append-only 日志文件定为全文权威源。本模块补的只是**闸门与告示牌**：不新增存储路径，
//! 不丢信息（全文在日志里），不杀进程（进程该跑就跑）。

use std::collections::HashMap;
use std::io::{Read, Seek, SeekFrom};
use std::path::Path;
use std::sync::Mutex;

use qaqh_process_tools::exec_session::ExecSessionResult;
use qaqh_ringing_contract::fact::ToolCallOutcome;
use qaqh_session::owner::ToolLifecycle;

/// 模型面单 exec 会话累计直灌预算（字节，**硬编码**）。
///
/// 取值与 capture 侧 `MODEL_PENDING_BYTES`（256 KiB）同界：一整份"未消费模型待读缓冲"
/// 就是单会话模型面预算。超过它的量已经不可能被一次工具调用读完，再往上直灌就是把
/// 上下文当日志垃圾桶。取整为常量；ADR-0035 决策 3 明确不暴露给用户配置。
pub const MODEL_FACE_BUDGET_BYTES: u64 = 256 * 1024;

/// 通告帧尾部预览上限（字符）。
///
/// 通告要能让人/模型一眼看出"泄的是什么"，又不能自己变成新的洪峰：2 Ki 字符约
/// 一个屏幕的尾巴，足够认出构建失败栈或日志格式。
pub const NOTICE_TAIL_CHARS: usize = 2 * 1024;

/// 通告帧标记（首行前缀）。测试与前端按此锚定，改字即破坏可观测性。
const NOTICE_MARKER: &str = "[exec 模型面泄洪 · ADR-0035]";

/// 抑制帧标记（首行前缀）。
const SUPPRESS_MARKER: &str = "[exec 模型面抑制 · ADR-0035]";

/// 单帧模型面判定结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SpillVerdict {
    /// 未开闸：本帧原样直灌。
    Direct,
    /// 本帧触发开闸：本帧内容不进模型上下文，被一条合成通告取代。
    Spilled {
        /// 开闸前已直灌模型上下文的累计字节。
        delivered_bytes: u64,
        /// 本会话模型面累计产生的字节（含本帧）。
        produced_bytes: u64,
        /// 本帧被扣下的字节。
        withheld_bytes: u64,
    },
    /// 已开闸：本帧不再进模型上下文，只回轻量水位。
    Suppressed {
        /// 开闸前已直灌模型上下文的累计字节。
        delivered_bytes: u64,
        /// 本会话模型面累计产生的字节（含本帧）。
        produced_bytes: u64,
        /// 本帧被扣下的字节。
        withheld_bytes: u64,
    },
}

#[derive(Debug, Default, Clone, Copy)]
struct SessionBudget {
    /// 已直灌模型上下文的累计字节（开闸后冻结）。
    delivered_bytes: u64,
    /// 模型面累计产生的字节（开闸后继续累计，用于水位报告）。
    produced_bytes: u64,
    /// 是否已开闸（**不恢复**）。
    spilled: bool,
}

/// 会话内的 exec 模型面泄洪闸（一个会话一个）。
///
/// 记账键是 exec 会话 id；exec 会话终结（非 `Yielded` 终态）即释放该项，避免长会话
/// 反复 `exec_command` 把表撑大——终结后不可能再有输出来"恢复直灌"。
#[derive(Debug, Default)]
pub struct ExecSpillGate {
    sessions: Mutex<HashMap<String, SessionBudget>>,
}

impl ExecSpillGate {
    pub fn new() -> Self {
        Self::default()
    }

    /// 模型消费面**唯一**入口：判定 + 就地改写模型可见文本。
    ///
    /// `log_path` 是该 exec 会话权威日志的绝对路径（`qaqh_session::exec_log` 单源规则
    /// 派生）。`None` 只可能发生在业务库路径无父目录的退化装置里——此时通告照发，
    /// 并**如实**写明"权威日志路径不可用"，绝不假装引用存在（宪规 5）。
    pub fn apply(
        &self,
        exec_session_id: &str,
        result: &mut ExecSessionResult,
        log_path: Option<&Path>,
    ) -> SpillVerdict {
        let frame_bytes = result.output.len() as u64;
        let verdict = self.admit(exec_session_id, frame_bytes);
        match verdict {
            SpillVerdict::Direct => {}
            SpillVerdict::Spilled {
                delivered_bytes,
                produced_bytes,
                withheld_bytes,
            } => {
                let tail = log_path.map(|path| read_log_tail(path, NOTICE_TAIL_CHARS));
                result.output = spill_notice(SpillNotice {
                    exec_session_id,
                    log_path,
                    delivered_bytes,
                    produced_bytes,
                    withheld_bytes,
                    tail: tail.as_ref(),
                });
                result.output_truncated = true;
                result.omitted_bytes = result.omitted_bytes.saturating_add(withheld_bytes);
            }
            SpillVerdict::Suppressed {
                delivered_bytes,
                produced_bytes,
                withheld_bytes,
            } => {
                result.output = suppressed_status(SuppressedStatus {
                    log_path,
                    delivered_bytes,
                    produced_bytes,
                    withheld_bytes,
                });
                result.output_truncated = true;
                result.omitted_bytes = result.omitted_bytes.saturating_add(withheld_bytes);
            }
        }
        if outcome_is_terminal(&result.outcome) {
            self.release(exec_session_id);
        }
        verdict
    }

    /// 记账 + 判定。空帧不建账（进程安静时不该凭空生成水位行）。
    fn admit(&self, exec_session_id: &str, frame_bytes: u64) -> SpillVerdict {
        if frame_bytes == 0 {
            return SpillVerdict::Direct;
        }
        let mut sessions = self
            .sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        let budget = sessions.entry(exec_session_id.to_string()).or_default();
        budget.produced_bytes = budget.produced_bytes.saturating_add(frame_bytes);
        if budget.spilled {
            return SpillVerdict::Suppressed {
                delivered_bytes: budget.delivered_bytes,
                produced_bytes: budget.produced_bytes,
                withheld_bytes: frame_bytes,
            };
        }
        if budget.delivered_bytes.saturating_add(frame_bytes) > MODEL_FACE_BUDGET_BYTES {
            budget.spilled = true;
            return SpillVerdict::Spilled {
                delivered_bytes: budget.delivered_bytes,
                produced_bytes: budget.produced_bytes,
                withheld_bytes: frame_bytes,
            };
        }
        budget.delivered_bytes = budget.delivered_bytes.saturating_add(frame_bytes);
        SpillVerdict::Direct
    }

    fn release(&self, exec_session_id: &str) {
        self.sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .remove(exec_session_id);
    }

    /// 可观测性：该 exec 会话当前是否处于泄洪态。
    pub fn is_spilled(&self, exec_session_id: &str) -> bool {
        self.sessions
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(exec_session_id)
            .is_some_and(|budget| budget.spilled)
    }
}

/// 模型消费面接线：判定 + 就地改写（exec 宿主每个出口调**一次**，一行）。
///
/// 权威日志路径由 [`ToolLifecycle`] 按 `qaqh_session::exec_log` 单源规则派生，
/// 与 owner 泵写入口同一条规则——通告指向的文件必然就是泵正在写的那个。
pub fn admit_model_face(
    gate: &ExecSpillGate,
    lifecycle: &ToolLifecycle,
    result: &mut ExecSessionResult,
) {
    let Some(exec_session_id) = result.exec_session_id.clone() else {
        return;
    };
    let log_path = lifecycle.exec_output_log_path(exec_session_id.as_str());
    gate.apply(exec_session_id.as_str(), result, log_path.as_deref());
}

fn outcome_is_terminal(outcome: &ToolCallOutcome) -> bool {
    !matches!(outcome, ToolCallOutcome::Yielded { .. })
}

struct SpillNotice<'a> {
    exec_session_id: &'a str,
    log_path: Option<&'a Path>,
    delivered_bytes: u64,
    produced_bytes: u64,
    withheld_bytes: u64,
    tail: Option<&'a std::io::Result<Option<String>>>,
}

/// 合成通告帧：异常流量说明 + 权威日志路径 + tail 预览（ADR-0035 决策 4）。
fn spill_notice(notice: SpillNotice<'_>) -> String {
    let SpillNotice {
        exec_session_id,
        log_path,
        delivered_bytes,
        produced_bytes,
        withheld_bytes,
        tail,
    } = notice;
    let mut text = String::new();
    text.push_str(NOTICE_MARKER);
    text.push('\n');
    text.push_str(&format!(
        "本 exec 会话（{exec_session_id}）的模型面输出累计已达 {produced_bytes} 字节，越过硬编码预算 \
         {MODEL_FACE_BUDGET_BYTES} 字节。继续把增量直灌模型上下文只会把上下文淹掉，因此从本帧起\
         **停止直灌，且本会话余下输出永不恢复直灌**（ADR-0035 决策 2）。\n"
    ));
    text.push_str(&format!(
        "本帧被扣下 {withheld_bytes} 字节；开闸前已直灌 {delivered_bytes} 字节。信息没有丢——全文在权威日志里。\n"
    ));
    text.push_str("权威日志（append-only 全文，按 offset 分段读 / 按需 grep，tail 优先）：\n");
    push_log_locator(&mut text, log_path);
    text.push_str(
        "后续行为：`write_stdin` 轮询仍然可用，只返回轻量水位与上面的日志引用，不再重放全量输出；\
         客户端界面不受影响（泄洪只削模型上下文，不削 SSE 事件流）。进程没有被杀，让它继续跑完。\n",
    );
    if let Some(tail) = tail {
        match tail {
            Ok(Some(preview)) if !preview.is_empty() => {
                text.push_str(&format!(
                    "尾部预览（最后 ≤{NOTICE_TAIL_CHARS} 字符，来自上面的权威日志）：\n"
                ));
                text.push_str("----- 8< -----\n");
                text.push_str(preview);
                if !preview.ends_with('\n') {
                    text.push('\n');
                }
                text.push_str("----->8 -----\n");
            }
            Ok(_) => {
                text.push_str("尾部预览：权威日志此刻尚未写入任何字节。\n");
            }
            Err(error) => {
                text.push_str(&format!(
                    "尾部预览：读取权威日志失败（{error}）——按上面路径自行读取时请留意同一故障。\n"
                ));
            }
        }
    } else {
        text.push_str("尾部预览：权威日志路径不可用（数据目录无法从业务库路径派生），只能靠 `write_stdin` 水位判断进度。\n");
    }
    text
}

struct SuppressedStatus<'a> {
    log_path: Option<&'a Path>,
    delivered_bytes: u64,
    produced_bytes: u64,
    withheld_bytes: u64,
}

/// 泄洪态下的轻量水位行（ADR-0035 落地清单：泄洪后轮询不重放全量）。
fn suppressed_status(status: SuppressedStatus<'_>) -> String {
    let SuppressedStatus {
        log_path,
        delivered_bytes,
        produced_bytes,
        withheld_bytes,
    } = status;
    let mut text = String::new();
    text.push_str(SUPPRESS_MARKER);
    text.push('\n');
    text.push_str(&format!(
        "本帧 {withheld_bytes} 字节未进模型上下文（累计产生 {produced_bytes} 字节，其中直灌 {delivered_bytes} 字节，\
         其余全部扣下）。泄洪不恢复。\n"
    ));
    push_log_locator(&mut text, log_path);
    text.push_str(
        "需要细节请按 offset 分段读该日志，或等进程结束后按需 grep。`write_stdin` 继续可用。\n",
    );
    text
}

fn push_log_locator(text: &mut String, log_path: Option<&Path>) {
    match log_path {
        Some(path) => {
            text.push_str("  路径: ");
            text.push_str(&path.display().to_string());
            text.push('\n');
        }
        None => text.push_str("  路径: 不可用（数据目录无法从业务库路径派生）\n"),
    }
}

/// 读权威日志尾部最多 `max_chars` 个字符。
///
/// 语义诚实：`Ok(None)` = 日志尚不存在（还没写出任何字节）；`Err` = 读失败，**不吞**，
/// 由通告原样转发失败原因。起点落在多字节字符中间时跳到下一个前导字节——这是
/// "预览"的定位语义，不是权威全文读取（权威全文读取是 `qaqh_session::owner::
/// read_exec_output_log_range`，它对非法 UTF-8 Fail-Closed）。
fn read_log_tail(path: &Path, max_chars: usize) -> std::io::Result<Option<String>> {
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let len = file.metadata()?.len();
    if len == 0 {
        return Ok(Some(String::new()));
    }
    // 每个字符最多 4 字节 ⇒ 取 `max_chars * 4` 字节窗口一定够装下最后 `max_chars`
    // 个字符；窗口被文件首部截断时也不会少算。
    let window = (max_chars as u64).saturating_mul(4).min(len);
    let start = len - window;
    file.seek(SeekFrom::Start(start))?;
    let mut bytes = vec![0u8; window as usize];
    file.read_exact(&mut bytes)?;
    let mut offset = 0usize;
    while offset < bytes.len() && (bytes[offset] & 0xC0) == 0x80 {
        offset += 1;
    }
    let text = String::from_utf8_lossy(&bytes[offset..]);
    let skip = text.chars().count().saturating_sub(max_chars);
    Ok(Some(text.chars().skip(skip).collect()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use qaqh_ringing_contract::fact::ToolCancellationReason;
    use qaqh_types::ExecSessionId;

    fn frame(text: &str) -> ExecSessionResult {
        ExecSessionResult {
            outcome: ToolCallOutcome::Yielded {
                exec_session_id: ExecSessionId::new("execs_01TEST"),
            },
            exec_session_id: Some(ExecSessionId::new("execs_01TEST")),
            output: text.to_string(),
            output_truncated: false,
            omitted_bytes: 0,
            exit_code: None,
            timed_out: false,
            cancelled: false,
            input_bytes_written: None,
        }
    }

    fn finished(text: &str) -> ExecSessionResult {
        ExecSessionResult {
            outcome: ToolCallOutcome::Completed,
            ..frame(text)
        }
    }

    /// 预算内：帧原样直灌，文本一个字节都不改。
    #[test]
    fn frames_below_budget_pass_through_untouched() {
        let gate = ExecSpillGate::new();
        let mut result = frame("hello\n");
        assert_eq!(
            gate.apply("execs_01A", &mut result, None),
            SpillVerdict::Direct
        );
        assert_eq!(result.output, "hello\n");
        assert!(!result.output_truncated);
        assert_eq!(result.omitted_bytes, 0);
        assert!(!gate.is_spilled("execs_01A"));
    }

    /// 反证锚（红）：**不设闸**时 40 帧 × 32 KiB = 1.25 MiB 全额进模型上下文。
    /// 设闸后同一串帧只给一条通告 + 水位行，模型面字节被钉在预算上。
    #[test]
    fn flooding_frames_collapse_to_one_notice_and_never_recover() {
        let unit = "x".repeat(32 * 1024);
        let frames = 40usize;
        let ungated_bytes = unit.len() as u64 * frames as u64;
        assert!(
            ungated_bytes > MODEL_FACE_BUDGET_BYTES * 4,
            "反证装置失效：不带闸的洪峰必须显著越预算"
        );

        let gate = ExecSpillGate::new();
        let mut spent = 0u64;
        let mut direct_frames = 0usize;
        let mut spilled_frames = 0usize;
        let mut suppressed_frames = 0usize;
        let mut notice = String::new();
        for _ in 0..frames {
            let mut result = frame(&unit);
            match gate.apply("execs_01FLOOD", &mut result, None) {
                SpillVerdict::Direct => {
                    direct_frames += 1;
                    spent += result.output.len() as u64;
                }
                SpillVerdict::Spilled { .. } => {
                    spilled_frames += 1;
                    spent += result.output.len() as u64;
                    notice = result.output.clone();
                    assert!(notice.starts_with(NOTICE_MARKER));
                    assert!(result.omitted_bytes >= unit.len() as u64);
                }
                SpillVerdict::Suppressed { .. } => {
                    suppressed_frames += 1;
                    spent += result.output.len() as u64;
                    assert!(
                        !result.output.contains(&unit[..64]),
                        "泄洪后不得重放原始增量"
                    );
                    assert!(result.output.starts_with(SUPPRESS_MARKER));
                }
            }
        }
        assert_eq!(
            direct_frames + spilled_frames + suppressed_frames,
            frames,
            "每一帧都必须有判定"
        );
        assert_eq!(
            direct_frames as u64 * unit.len() as u64,
            MODEL_FACE_BUDGET_BYTES - MODEL_FACE_BUDGET_BYTES % unit.len() as u64,
            "预算内的帧必须原样直灌（只有刚好填满预算的那些）"
        );
        assert_eq!(spilled_frames, 1, "开闸必须只发生一次");
        assert_eq!(
            suppressed_frames,
            frames - direct_frames - 1,
            "开闸后每一帧都必须被抑制"
        );
        assert!(gate.is_spilled("execs_01FLOOD"), "泄洪不恢复");
        // 模型面总字节被钉在预算 + 通告 + 水位行上，与洪峰总量无关。
        assert!(
            spent < MODEL_FACE_BUDGET_BYTES + 32 * 1024,
            "模型面 {spent} 字节越界（预算 {MODEL_FACE_BUDGET_BYTES}）"
        );
        assert!(
            spent * 4 < ungated_bytes,
            "闸门未生效：{spent} vs {ungated_bytes}"
        );
    }

    /// 通告必须把模型指到**真实存在**的权威日志，并带出尾部署名内容。
    #[test]
    fn notice_points_at_authoritative_log_and_carries_tail_preview() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log_path = dir.path().join("exec").join("execs_01LOG.log");
        std::fs::create_dir_all(log_path.parent().expect("parent")).expect("mkdir");
        std::fs::write(&log_path, "build ok\nERROR: boom at line 42\n").expect("write log");

        let gate = ExecSpillGate::new();
        let mut result = frame(&"y".repeat(MODEL_FACE_BUDGET_BYTES as usize + 1));
        let verdict = gate.apply("execs_01LOG", &mut result, Some(&log_path));
        assert!(matches!(verdict, SpillVerdict::Spilled { .. }));
        let notice = result.output;
        assert!(
            notice.contains(&log_path.display().to_string()),
            "通告必须给出日志绝对路径"
        );
        assert!(
            notice.contains("ERROR: boom at line 42"),
            "通告必须带尾部预览：{notice}"
        );
        assert!(notice.contains("永不恢复直灌"), "通告必须写明不恢复");
        assert!(
            notice.contains("客户端界面不受影响"),
            "通告必须写明客户端面不设闸"
        );
    }

    /// 日志缺席 / 路径不可用时，通告如实登记，不假装引用存在。
    #[test]
    fn missing_log_is_reported_honestly() {
        let dir = tempfile::tempdir().expect("tempdir");
        let absent = dir.path().join("nope.log");
        let gate = ExecSpillGate::new();
        let mut result = frame(&"z".repeat(MODEL_FACE_BUDGET_BYTES as usize + 1));
        gate.apply("execs_01ABSENT", &mut result, Some(&absent));
        assert!(result.output.contains(&absent.display().to_string()));
        assert!(
            result.output.contains("尚未写入任何字节"),
            "{}",
            result.output
        );

        let gate = ExecSpillGate::new();
        let mut result = frame(&"z".repeat(MODEL_FACE_BUDGET_BYTES as usize + 1));
        gate.apply("execs_01NOPATH", &mut result, None);
        assert!(result.output.contains("路径不可用"), "{}", result.output);
    }

    /// 扣下的字节必须出现在 `omitted_bytes` 上——模型被扣了多少要说得清。
    #[test]
    fn withheld_bytes_accumulate_as_omitted() {
        let gate = ExecSpillGate::new();
        let over = MODEL_FACE_BUDGET_BYTES as usize + 1;
        let mut first = frame(&"a".repeat(over));
        gate.apply("execs_01OMIT", &mut first, None);
        assert!(first.output_truncated);
        assert!(first.omitted_bytes >= MODEL_FACE_BUDGET_BYTES + 1);

        let mut next = frame(&"b".repeat(1024));
        gate.apply("execs_01OMIT", &mut next, None);
        assert!(next.output_truncated);
        assert_eq!(next.omitted_bytes, 1024);
    }

    /// 空帧不生成水位噪声；终结后记账释放。
    #[test]
    fn empty_frames_are_silent_and_terminal_frames_release_the_budget() {
        let gate = ExecSpillGate::new();
        let mut empty = frame("");
        assert_eq!(
            gate.apply("execs_01REL", &mut empty, None),
            SpillVerdict::Direct
        );
        assert_eq!(empty.output, "");
        assert!(!gate.is_spilled("execs_01REL"));

        let over = MODEL_FACE_BUDGET_BYTES as usize + 1;
        let mut first = frame(&"a".repeat(over));
        gate.apply("execs_01REL", &mut first, None);
        assert!(gate.is_spilled("execs_01REL"));

        let mut done = finished("");
        gate.apply("execs_01REL", &mut done, None);
        assert!(!gate.is_spilled("execs_01REL"), "终态释放记账");
        // 释放后新会话重新从零记账（新 exec 会话 id 本就是新账本）。
        let mut fresh = frame("small");
        assert_eq!(
            gate.apply("execs_01REL", &mut fresh, None),
            SpillVerdict::Direct
        );
    }

    #[test]
    fn tail_preview_is_utf8_safe_and_char_bounded() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("log.txt");
        let text: String = std::iter::repeat_n('中', 4096).collect();
        std::fs::write(&path, &text).expect("write");
        let tail = read_log_tail(&path, 64).expect("read").expect("exists");
        assert_eq!(tail.chars().count(), 64);
        assert_eq!(tail, "中".repeat(64));

        std::fs::write(&path, "").expect("truncate");
        assert_eq!(read_log_tail(&path, 64).expect("read"), Some(String::new()));
    }

    #[test]
    fn cancelled_outcome_is_terminal_too() {
        assert!(outcome_is_terminal(&ToolCallOutcome::Cancelled {
            reason: ToolCancellationReason::TurnCancel,
        }));
        assert!(!outcome_is_terminal(&ToolCallOutcome::Yielded {
            exec_session_id: ExecSessionId::new("execs_01Y"),
        }));
    }
}
