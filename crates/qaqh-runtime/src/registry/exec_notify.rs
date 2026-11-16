//! `AgentRegistry::exec_notify` —— exec 订阅终态推送（ADR-0026）。
//!
//! ADR-0017 决策 8 移除了**无条件**的 exec 终态唤醒注入链（重复交付、重复
//! 塞输出、后台服务器超时误唤醒）。本机构是它的**订阅制**定向回调：只有模型
//! 显式 `write_stdin(notify_on_exit=true)` 订阅的会话才推送；交付恰一次
//! （manager 侧 armed/delivered 标志保证）；注入物是不含输出正文的小结构
//! 事实（终态全文在 content store / append-only 日志，按需读取）。
//!
//! 机构全部复用既有遗产：推送 = canonical `ConversationSendMessage`
//! （`as_system` + `TriggerTurn`，goal 续跑同款）；投递目标解析 handoff
//! 链头（`deliver_continuation` 同款）；冷会话经 `ensure_loaded_for_command`
//! 加载；投递失败由 daemon 周期扫描补投（`sweep_active_goals` 同款先例）。
//! armed 状态是内存态：daemon 重启杀全部子进程，无跨重启义务。

use super::*;
use qaqh_agent_runtime::ExecTerminalNotice;
use qaqh_ringing::ConversationSendMessage;
use qaqh_ringing_contract::fact::InputPurpose;
use qaqh_ringing_contract::state::ExecSessionOutcome;

/// 推送消息的特征标记：输入块落库后据此识别 exec 终态推送轮
/// （与 `GOAL_CONTINUATION_MARKER` 同一审计先例）。
pub(crate) const EXEC_TERMINAL_MARKER: &str = "<qaqh_exec_session_terminal";

impl AgentRegistry {
    /// manager finalize 端口回调（watcher 线程 → registry 锁短临界区，
    /// 与 `route_turn_terminal` 的装配方式同构）。返回是否已送达。
    pub(crate) fn route_exec_terminal(&mut self, notice: &ExecTerminalNotice) -> bool {
        if self.shutting_down {
            return false;
        }
        // 链头解析与 goal 续跑同款：armed 会话可能属于已 handoff 的链根。
        // 解析失败不降级猜测：保留 armed 未送达，周期扫描下轮再探。
        let root = notice.owner_session_id.clone();
        let resolved = self.owners.db().read_blocking(move |conn| {
            let head = qaqh_session::db::store::handoff_chain_head(conn, &root)?;
            let sealed = qaqh_session::db::store::session_handed_off(conn, &head)?;
            Ok::<_, qaqh_session::db::DbError>((head, sealed))
        });
        let (head, sealed) = match resolved {
            Ok(pair) => pair,
            Err(error) => {
                log::warn!(
                    "[exec-notify] chain head resolve for {} failed: {error}",
                    notice.owner_session_id
                );
                return false;
            }
        };
        if sealed {
            log::info!(
                "[exec-notify] chain head {head} is sealed (handoff); awaiting successor session"
            );
            return false;
        }
        if let Err(error) = self.ensure_loaded_for_command(&head) {
            log::warn!("[exec-notify] load for {head} failed: {error}");
            return false;
        }
        let Some(instance) = self.instances.get(&head) else {
            return false;
        };
        let input = ConversationSendMessage {
            text: exec_terminal_prompt(notice),
            images: vec![],
            attachments: None,
            message_id: Some(format!("exec-terminal:{}", notice.exec_session_id.as_str())),
            input_purpose: InputPurpose::TriggerTurn,
            as_system: true,
            inter_agent: None,
            subagent_terminal: None,
            widget_context: None,
        };
        match instance.loop_.send_input(input) {
            Ok(()) => {
                let first = self
                    .exec_sessions
                    .mark_notify_delivered(&notice.exec_session_id);
                log::info!(
                    "[exec-notify] terminal notice delivered to {head} (session={}, first={first})",
                    notice.exec_session_id.as_str()
                );
                true
            }
            Err(error) => {
                log::warn!("[exec-notify] delivery to {head} failed: {error}");
                false
            }
        }
    }

    /// daemon 周期扫描兜底（ADR-0026）：armed+terminal+未送达 的残留补投。
    /// 主路径是 finalize 端口回调；这里只覆盖投递失败（链头解析/装载/入队）
    /// 的残留。返回送达成功的 owner 会话 id 列表。
    pub fn sweep_armed_exec_sessions(&mut self) -> Vec<String> {
        if self.shutting_down {
            return Vec::new();
        }
        let mut delivered = Vec::new();
        for notice in self.exec_sessions.armed_undelivered() {
            if self.route_exec_terminal(&notice) {
                delivered.push(notice.owner_session_id);
            }
        }
        delivered
    }
}

/// 渲染终态推送文本（ADR-0026）：小结构事实，不带输出正文——输出经
/// write_stdin（终态后一次收割待读缓冲）或 content store 按需读取。
fn exec_terminal_prompt(notice: &ExecTerminalNotice) -> String {
    let (state, outcome_line) = match &notice.outcome {
        ExecSessionOutcome::Exited { exit_code } => {
            ("exited", format!("exited with code {exit_code}"))
        }
        ExecSessionOutcome::TimedOut { exit_code } => (
            "timed_out",
            format!(
                "timed out and was force-stopped; exit code {}",
                exit_code
                    .map(|code| code.to_string())
                    .unwrap_or_else(|| "unknown".to_owned())
            ),
        ),
        _ => ("terminal", format!("{:?}", notice.outcome)),
    };
    let id = notice.exec_session_id.as_str();
    format!(
        "{EXEC_TERMINAL_MARKER} exec_session_id=\"{id}\" state=\"{state}\">\n\
         A detached exec session reached its terminal state (you subscribed with \
         write_stdin notify_on_exit=true; this is the one-shot terminal delivery).\n\
         - command: {}\n\
         - outcome: {outcome_line}\n\
         - No output body is included. If needed, a single write_stdin(exec_session_id=\"{id}\") \
         now drains the buffered output; the session is terminal and returns it immediately.\n\
         Treat this as the completion event for that session; do not wait on it further.\n\
         </qaqh_exec_session_terminal>",
        notice.command
    )
}
