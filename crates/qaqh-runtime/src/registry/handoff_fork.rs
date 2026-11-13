//! `AgentRegistry::handoff_fork` —— handoff 内核 fork 引擎 + 兜底扫描
//! （ADR-0016 切片 1/3）。
//!
//! 封箱续推的所有权在内核（ADR-0016 §决策 1/2）：封箱会话的 `sessions` 行就是
//! fork 的全部继承源（cwd / model_selection / tool_mode / custom_tools 全在行上），
//! 内核读行回放建后继，交接文档经 canonical `send_input` 投成后继第一回合。
//! 延时调度在 service 层的终态路由挂钩（`FORK_DELAY_MS` 阅读窗，默认口径 10s）；
//! daemon 60s sweep（§切片 3）兜底超窗 / 失败 / 重启丢定时器。
//!
//! 幂等与并发：建行、血缘写入与「链头必须仍停在封箱会话」的原子闸在同一写锁
//! 闭包内（`store::create_handoff_successor`），定时任务与 sweep 撞车也只会开出
//! 一条后继。fork 失败若发生在建行之后（spawn 起不来），删行回滚——链头退回
//! 封箱会话，sweep 稍后重试；文档投递失败仅记日志（行已在、链已接，B 会话
//! 可被用户手动继续）。

use std::time::{SystemTime, UNIX_EPOCH};

use qaqh_ringing::ConversationSendMessage;
use qaqh_ringing_contract::events::SessionEvent;
use qaqh_ringing_contract::fact::InputPurpose;
use qaqh_ringing_contract::ulid::generate_ulid;
use qaqh_session::db::store::{self, NewSession};

use super::AgentRegistry;

/// 封箱 → fork 的阅读窗（ADR-0016 §决策 3，默认口径 10s）。常量起步不进
/// ConfigDto：尚无第二消费者，加配置段属于为想象中的需求扩面（宪法 1）。
pub(crate) const FORK_DELAY_MS: u64 = 10_000;

/// 后继会话标题上限（与前端 `handoffTitle` 同口径：摘要优先、首行回退、72 字截断）。
const SUCCESSOR_TITLE_CHARS: usize = 72;

/// fork 的全部输入：封箱行回放 + 封箱调用的交接文档。
struct ForkSnapshot {
    /// 链根（事件与 goal 挂靠点；`handoff_from` 写的是直接前驱 `sealed`）。
    root: String,
    /// 封箱会话（直接前驱，后继行的 `handoff_from_session_id`）。
    sealed: String,
    /// 封箱调用所在回合（文档投递的 message_id 溯源用）。
    turn_id: String,
    /// 交接文档正文（模型亲手写的 `handoff_doc`）。
    doc: String,
    /// 一句话工作简述（`work_summary`，可为空）。
    work_summary: String,
    /// 封箱行回放的继承四项。
    cwd: Option<String>,
    model_selection: Option<qaqh_types::ModelSelection>,
    tool_mode: Option<String>,
    custom_tools: Option<Vec<String>>,
}

fn now_unix_ms() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_millis() as i64)
        .unwrap_or(0)
}

/// 后继标题：摘要优先，空则回退文档首个非空行；72 字截断加省略号。
fn successor_title(work_summary: &str, doc: &str) -> String {
    let summary = work_summary.split_whitespace().collect::<Vec<_>>().join(" ");
    let base = if summary.is_empty() {
        doc.lines()
            .map(str::trim)
            .find(|line| !line.is_empty())
            .unwrap_or("")
            .to_string()
    } else {
        summary
    };
    let mut title: String = base.chars().take(SUCCESSOR_TITLE_CHARS).collect();
    if base.chars().count() > SUCCESSOR_TITLE_CHARS {
        title.push('…');
    }
    title
}

struct SealingCall {
    turn_id: String,
    doc: String,
    work_summary: String,
}

/// 从最近的已结算 handoff 调用里挑出封箱那条。优先 `outcome.kind ==
/// "completed"`；封箱写库先于工具结算（切片 0 的回执顺序）留下的 running 行
/// 在 declare 时已带完整 `args_json`，照样可取——封箱事实只能来自成功路径，
/// failed 参数不拦这条回退。
fn pick_sealing_call(calls: &[store::HandoffCallTrace]) -> Option<SealingCall> {
    let parse = |trace: &store::HandoffCallTrace| -> Option<(String, String, String, bool)> {
        let args: serde_json::Value =
            serde_json::from_str(trace.args_json.as_deref().unwrap_or("")).ok()?;
        let doc = args.get("handoff_doc")?.as_str()?.trim().to_string();
        if doc.is_empty() {
            return None;
        }
        let work_summary = args
            .get("work_summary")
            .and_then(|value| value.as_str())
            .unwrap_or("")
            .to_string();
        let completed = serde_json::from_str::<serde_json::Value>(
            trace.state_json.as_deref().unwrap_or(""),
        )
        .ok()
        .and_then(|state| state["outcome"]["kind"].as_str().map(str::to_string))
        .is_some_and(|kind| kind == "completed");
        Some((trace.turn_id.clone(), doc, work_summary, completed))
    };
    let mut fallback = None;
    for trace in calls {
        let Some((turn_id, doc, work_summary, completed)) = parse(trace) else {
            continue;
        };
        if completed {
            return Some(SealingCall {
                turn_id,
                doc,
                work_summary,
            });
        }
        if fallback.is_none() {
            fallback = Some(SealingCall {
                turn_id,
                doc,
                work_summary,
            });
        }
    }
    fallback
}

impl AgentRegistry {
    /// ADR-0016 切片 1：把封箱会话 fork 出后继并投递交接文档。幂等：链上已有
    /// 后继（链头已前进）→ `Ok(None)`；未封箱或找不到封箱调用 → `Ok(None)` /
    /// `Err`。成功返回后继会话 id。
    pub fn fork_handoff_successor(
        &mut self,
        sealed_session_id: &str,
    ) -> Result<Option<String>, String> {
        let Some(snapshot) = self.fork_snapshot(sealed_session_id)? else {
            return Ok(None);
        };
        // 建行 + 血缘 + 幂等闸（一个写锁闭包，见 store::create_handoff_successor）。
        let successor_id = format!("ses_{}", generate_ulid());
        let title = successor_title(&snapshot.work_summary, &snapshot.doc);
        let row = NewSession {
            session_id: successor_id.clone(),
            parent_session_id: None,
            title,
            // 内核 fork 是机器命名：`auto` 保住 B 的自动重命名权（前端路径写
            // `user` 会把 `session_title` 工具永久锁死，ADR-0016 §决策 2）。
            title_source: Some("auto".to_string()),
            cwd: snapshot.cwd.clone(),
            model: snapshot
                .model_selection
                .as_ref()
                .map(|selection| selection.model_id.clone()),
            tool_mode: snapshot.tool_mode.clone(),
            model_selection: snapshot.model_selection.clone(),
            custom_tools: snapshot.custom_tools.clone(),
            frozen_annotation: None,
            ephemeral: false,
        };
        let created = self
            .owners
            .db()
            .write_blocking({
                let from_session = snapshot.sealed.clone();
                let chain_root = snapshot.root.clone();
                move |conn| {
                    store::create_handoff_successor(conn, &row, &from_session, &chain_root)
                }
            })
            .map_err(|error| error.to_string())?;
        if !created {
            // 链头已前进（定时任务与 sweep 撞车，或用户已在后继上续推）。
            return Ok(None);
        }
        // 装载后继；起不来就删行回滚——链头退回封箱会话，sweep 稍后重试。
        if let Err(error) = self.spawn_new(&successor_id) {
            let rolled_back = self
                .owners
                .db()
                .write_blocking({
                    let successor = successor_id.clone();
                    move |conn| {
                        store::delete_session_tree(conn, &successor)?;
                        Ok::<_, qaqh_session::db::DbError>(())
                    }
                })
                .is_ok();
            return Err(format!(
                "handoff fork spawn failed for {successor_id} (row rolled back: {rolled_back}): {error}"
            ));
        }
        // 文档先行（ADR-0016 §被否 3：goal 续跑只能排队跟在交接文档之后）。
        let input = ConversationSendMessage {
            text: snapshot.doc,
            images: vec![],
            attachments: None,
            message_id: Some(format!(
                "handoff-doc:{}:{}",
                snapshot.sealed, snapshot.turn_id
            )),
            input_purpose: InputPurpose::TriggerTurn,
            as_system: true,
            inter_agent: None,
            subagent_terminal: None,
            widget_context: None,
        };
        if let Err(error) = self.deliver_input(&successor_id, input) {
            log::error!(
                "[handoff-fork] doc delivery to {successor_id} failed (chain live, session awaits manual continue): {error}"
            );
        }
        // 权威广播：链根（封箱线）与后继线各一条。
        let chain_root = snapshot.root.clone();
        let event = SessionEvent::HandoffSealed {
            sealed_session_id: snapshot.sealed.clone(),
            chain_root,
            successor_session_id: successor_id.clone(),
            turn_id: qaqh_ringing_contract::fact::TurnId::new(snapshot.turn_id),
            work_summary: snapshot.work_summary,
            at_ms: now_unix_ms().max(0) as u64,
        };
        self.owners
            .get_or_create(&snapshot.sealed)
            .publish(event.clone());
        self.owners.get_or_create(&successor_id).publish(event);
        log::info!(
            "[handoff-fork] {} forked successor {} (chain root {})",
            snapshot.sealed,
            successor_id,
            snapshot.root
        );
        Ok(Some(successor_id))
    }

    /// ADR-0016 切片 3：兜底扫描——封箱超窗（`sealed + FORK_DELAY_MS ≤ now`）
    /// 且链头未前进的链立即补 fork。覆盖：定时任务失败 / 丢失、daemon 在阅读
    /// 窗内重启。返回本次 fork 出的后继 id。
    pub fn sweep_handoff_forks(&mut self) -> Vec<String> {
        let now = now_unix_ms();
        let due = self
            .owners
            .db()
            .read_blocking(|conn| {
                Ok::<_, qaqh_session::db::DbError>(
                    store::sealed_handoff_sessions(conn)?
                        .into_iter()
                        .filter(|(_, sealed_at_ms)| sealed_at_ms + FORK_DELAY_MS as i64 <= now)
                        .map(|(session_id, _)| session_id)
                        .collect::<Vec<_>>(),
                )
            })
            .unwrap_or_else(|error| {
                log::warn!("[handoff-fork] sealed scan failed: {error}");
                Vec::new()
            });
        let mut forked = Vec::new();
        for sealed in due {
            match self.fork_handoff_successor(&sealed) {
                Ok(Some(successor)) => {
                    log::info!("[handoff-fork] sweep forked {sealed} → {successor}");
                    forked.push(successor);
                }
                Ok(None) => {}
                Err(error) => {
                    log::warn!("[handoff-fork] sweep fork for {sealed} failed: {error}")
                }
            }
        }
        forked
    }

    /// ADR-0016：向（必要时装载的）会话注入一条 canonical 输入。fork 投递
    /// 交接文档用；与 goal 续跑的投递同款装载语义。
    pub fn deliver_input(
        &mut self,
        session_id: &str,
        input: ConversationSendMessage,
    ) -> Result<(), String> {
        self.ensure_loaded_for_command(session_id)
            .map_err(|error| error.to_string())?;
        let Some(instance) = self.instances.get(session_id) else {
            return Err(format!("session {session_id} not loaded after ensure"));
        };
        instance
            .loop_
            .send_input(input)
            .map_err(|error| error.to_string())
    }

    /// 读封箱行回放 + 挑封箱调用。未封箱 / 行不存在 → `Ok(None)`。
    fn fork_snapshot(&self, sealed_session_id: &str) -> Result<Option<ForkSnapshot>, String> {
        let found = self
            .owners
            .db()
            .read_blocking({
                let sealed = sealed_session_id.to_string();
                move |conn| {
                    let row = store::session_row(conn, &sealed)?;
                    let Some(row) = row else {
                        return Ok(None);
                    };
                    let sealed_flag = store::session_handed_off(conn, &sealed)?;
                    let calls = store::recent_handoff_calls(conn, &sealed, 8)?;
                    let root = store::handoff_chain_root(conn, &sealed)?;
                    Ok::<_, qaqh_session::db::DbError>(Some((row, sealed_flag, calls, root)))
                }
            })
            .map_err(|error| error.to_string())?;
        let Some((row, sealed_flag, calls, root)) = found else {
            return Ok(None);
        };
        if !sealed_flag {
            return Ok(None);
        }
        let Some(sealing) = pick_sealing_call(&calls) else {
            // 封箱但找不到任何带正文的 handoff 调用 = 存量不一致（fail-closed，
            // 宪法 5）：不猜文档，不 fork，sweep 每轮显式告警。
            log::error!(
                "[handoff-fork] sealed session {sealed_session_id} has no handoff call with a doc; fork stalled"
            );
            return Ok(None);
        };
        Ok(Some(ForkSnapshot {
            root,
            sealed: sealed_session_id.to_string(),
            turn_id: sealing.turn_id,
            doc: sealing.doc,
            work_summary: sealing.work_summary,
            cwd: row.cwd,
            model_selection: row.model_selection,
            tool_mode: row.tool_mode,
            custom_tools: Some(row.custom_tools),
        }))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn trace(args: &str, outcome: &str) -> store::HandoffCallTrace {
        store::HandoffCallTrace {
            call_id: "c".into(),
            turn_id: "t".into(),
            args_json: Some(args.into()),
            state_json: Some(
                serde_json::json!({"kind":"finished","outcome":{"kind": outcome}}).to_string(),
            ),
        }
    }

    #[test]
    fn sealing_call_prefers_completed_and_falls_back_to_running() {
        // 崩溃窗（running）+ 更旧的 completed：新→旧扫描里 completed 命中即回。
        let calls = vec![
            trace(r#"{"handoff_doc":"崩溃窗"}"#, "completed"),
            trace(r#"{"handoff_doc":"旧文档"}"#, "completed"),
        ];
        let picked = pick_sealing_call(&calls).expect("picked");
        assert_eq!(picked.doc, "崩溃窗");

        // 只有 failed（新）与 running（旧）：completed 没有就回退到带正文的行。
        let calls = vec![
            trace(r#"{"handoff_doc":"失败参数"}"#, "failed"),
            trace(r#"{"handoff_doc":"running 正文"}"#, "completed"),
        ];
        let picked = pick_sealing_call(&calls).expect("fallback");
        assert_eq!(picked.doc, "running 正文");

        // 无正文/无参数：整条不成立。
        let calls = vec![
            trace(r#"{"work_summary":"只有摘要"}"#, "completed"),
            trace("not-json", "completed"),
        ];
        assert!(pick_sealing_call(&calls).is_none());
    }

    #[test]
    fn successor_title_prefers_summary_and_truncates() {
        assert_eq!(successor_title("  多  空格 摘要  ", "正文"), "多 空格 摘要");
        assert_eq!(successor_title("   ", "首行\n第二行"), "首行");
        let long = "字".repeat(80);
        let title = successor_title(&long, "x");
        assert_eq!(title.chars().count(), SUCCESSOR_TITLE_CHARS + 1);
        assert!(title.ends_with('…'));
    }
}
