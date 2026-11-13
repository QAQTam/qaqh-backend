//! `AgentRegistry::goal` —— goal 续跑引擎(ADR-0015 切片 C)。
//!
//! 只借用父模块既有定义与状态,不新增字段。机构全部复用 ADR-0014 遗产:
//! 续跑 = canonical `ConversationSendMessage`(`as_system` + `TriggerTurn`,
//! exec-exit 唤醒同款先例);记账读 `turns.outcome_json` 权威终态;
//! 空回复审计靠输入块的 `<qaqh_goal_continuation>` 标记识别自动续跑轮。
//! goal 状态与计数的唯一权威是 `session_goals` 表(跨 idle 卸载/重启存活)。

use super::*;

use qaqh_agent_runtime::session_loop::TurnTerminalNotice;
use qaqh_ringing::ConversationSendMessage;
use qaqh_ringing_contract::fact::{InputPurpose, TurnTerminal};
use qaqh_ringing_contract::state::{GoalStatus, TurnOutcome};
use qaqh_session::db::goals::{self, GoalRow, GoalUpdate};

/// 续跑消息的特征标记:输入块落库后,终态审计据此识别自动续跑轮。
pub(crate) const GOAL_CONTINUATION_MARKER: &str = "<qaqh_goal_continuation";

const CONTINUATION_TEMPLATE: &str = include_str!("../../templates/goal_continuation.md");

fn escape_xml(text: &str) -> String {
    text.replace('&', "&amp;")
        .replace('<', "&lt;")
        .replace('>', "&gt;")
}

/// 渲染续跑提示词(Codex continuation 语义,本仓工具名;无模板引擎依赖)。
fn continuation_prompt(goal: &GoalRow) -> String {
    let budget = goal
        .token_budget
        .map(|v| v.to_string())
        .unwrap_or_else(|| "none".to_string());
    let remaining = goal
        .token_budget
        .map(|v| v.saturating_sub(goal.tokens_used).to_string())
        .unwrap_or_else(|| "unbounded".to_string());
    CONTINUATION_TEMPLATE
        .replace("{{goal_id}}", &escape_xml(&goal.goal_id))
        .replace("{{objective}}", &escape_xml(&goal.objective))
        .replace("{{tokens_used}}", &goal.tokens_used.to_string())
        .replace("{{token_budget}}", &budget)
        .replace("{{remaining_tokens}}", &remaining)
}

fn goals_enabled() -> bool {
    qaqh_config::watch::authoritative()
        .map(|cfg| cfg.goals.enabled)
        .unwrap_or(true)
}

impl AgentRegistry {
    /// 沿 handoff 工作链解析 goal(切片F):goal 行留在链根,链上任何会话
    /// 的终态都记到同一个 goal 上。
    fn read_goal(&self, session_id: &str) -> Option<GoalRow> {
        let query_id = session_id.to_string();
        self.owners
            .db()
            .read_blocking(move |conn| goals::resolve_goal(conn, &query_id))
            .unwrap_or_else(|error| {
                log::warn!("[goal] read goal for {session_id} failed: {error}");
                None
            })
    }

    fn publish_goal(&self, session_id: &str, goal: Option<goals::GoalRow>) {
        let view = goal.as_ref().map(goals::goal_view);
        self.owners
            .get_or_create(session_id)
            .publish(qaqh_ringing_contract::events::SessionEvent::GoalUpdated { goal: view });
    }

    /// 续跑投递(切片F):目标是**链头**(最新后继会话),不是 goal 所在的根。
    /// 链头已封箱(handoff 成功、后继未建)→ 跳过,等周期扫描下一轮再探。
    fn deliver_continuation(&mut self, goal: &GoalRow) {
        let root = goal.session_id.clone();
        let head = self
            .owners
            .db()
            .read_blocking(move |conn| {
                let head = qaqh_session::db::store::handoff_chain_head(conn, &root)?;
                let sealed = qaqh_session::db::store::session_handed_off(conn, &head)?;
                Ok::<_, qaqh_session::db::DbError>((head, sealed))
            });
        // 解析失败不降级猜测(宪法 5):跳过本轮,周期扫描下轮再探。
        let (head, sealed) = match head {
            Ok(pair) => pair,
            Err(error) => {
                log::warn!("[goal] chain head resolve for {} failed: {error}", goal.session_id);
                return;
            }
        };
        if sealed {
            log::info!(
                "[goal] chain head {head} is sealed (handoff); awaiting successor session"
            );
            return;
        }
        if let Err(error) = self.ensure_loaded_for_command(&head) {
            log::warn!("[goal] continuation load for {head} failed: {error}");
            return;
        }
        let Some(instance) = self.instances.get(&head) else {
            return;
        };
        let input = ConversationSendMessage {
            text: continuation_prompt(goal),
            images: vec![],
            attachments: None,
            message_id: Some(format!(
                "goal-cont:{}:{}",
                goal.goal_id,
                qaqh_ringing_contract::ulid::generate_ulid()
            )),
            input_purpose: InputPurpose::TriggerTurn,
            as_system: true,
            inter_agent: None,
            subagent_terminal: None,
            widget_context: None,
        };
        match instance.loop_.send_input(input) {
            Ok(()) => log::info!(
                "[goal] continuation delivered to {head} (goal={}, chain root={})",
                goal.goal_id,
                goal.session_id
            ),
            Err(error) => log::warn!("[goal] continuation to {head} failed: {error}"),
        }
    }

    /// goal.set 成 Active / daemon 周期扫描的续跑入口:
    /// 空闲开回合、忙则入队(回合边界合流)、冷则先加载——全是 send_input 既有语义。
    pub fn continue_goal_if_idle(&mut self, session_id: &str) {
        if self.shutting_down || !goals_enabled() {
            return;
        }
        let Some(goal) = self.read_goal(session_id) else {
            return;
        };
        if goal.status != GoalStatus::Active {
            return;
        }
        self.deliver_continuation(&goal);
    }

    /// 回合终态的 goal 路由:记账 + 三道闸 + 续跑。由 `route_turn_terminal`
    /// 在 inter-agent 判定之前调用(用户触发的回合也要记账)。
    pub(crate) fn route_goal_terminal(&mut self, notice: &TurnTerminalNotice) {
        if self.shutting_down || !goals_enabled() {
            return;
        }
        // 子会话是派生瞬时体:goal 语义只挂根会话(ADR-0015 §决策 5)。
        if self
            .owners
            .session_parent(&notice.session_id)
            .ok()
            .flatten()
            .is_some()
        {
            return;
        }
        let Some(goal) = self.read_goal(&notice.session_id) else {
            return;
        };
        if goal.status != GoalStatus::Active {
            return;
        }

        // 回合权威事实:终态 usage + 墙钟 + 工具活动 + 终答 + 输入标记。
        let session_id = notice.session_id.clone();
        let turn_id = notice.turn_id.clone();
        let facts = self.owners.db().read_blocking(move |conn| {
            let outcome = qaqh_session::db::store::turn_outcome_and_elapsed(
                conn,
                &session_id,
                &turn_id,
            )?;
            let tool_count = qaqh_session::db::store::turn_tool_count(conn, &turn_id)?;
            let answer = qaqh_session::db::view::turn_answer_text(conn, &session_id, &turn_id)?;
            let input = qaqh_session::db::view::turn_input_text(conn, &session_id, &turn_id)?;
            Ok::<_, qaqh_session::db::DbError>((outcome, tool_count, answer, input))
        });
        let Ok((outcome, tool_count, answer, input)) = facts else {
            log::warn!(
                "[goal] turn facts read failed for {} turn {}: {:?}",
                notice.session_id,
                notice.turn_id,
                facts.err()
            );
            return;
        };

        // 闸 1:回合失败(重试耗尽/不可重试)→ Blocked,防续跑死循环烧 token。
        if matches!(notice.terminal, TurnTerminal::Failed) {
            self.set_goal_status(&goal, GoalStatus::Blocked);
            log::warn!(
                "[goal] turn failed; goal {} blocked (session={})",
                goal.goal_id,
                notice.session_id
            );
            return;
        }

        // 记账:token 取终态 usage.total_tokens(持久化权威值),墙钟取回合时长。
        // 切片F:goal 行在链根——记账/审计一律按 goal.session_id 定位,
        // 不按触发回合的会话(它可能是 handoff 后继)。
        let tokens = match &outcome {
            Some((TurnOutcome::Finished { usage, .. }, _)) => {
                usage.as_ref().map(|u| u64::from(u.total_tokens)).unwrap_or(0)
            }
            _ => 0,
        };
        let elapsed_ms = outcome.as_ref().map(|(_, ms)| (*ms).max(0) as u64).unwrap_or(0);
        let accounted = self.owners.db().write_blocking({
            let session_id = goal.session_id.clone();
            let goal_id = goal.goal_id.clone();
            move |conn| goals::account_usage(conn, &session_id, &goal_id, tokens, elapsed_ms)
        });
        match accounted {
            Ok(Some(row)) if row.status == GoalStatus::BudgetLimited => {
                // 闸 2:预算熔断。回合已在边界收尾,不再续跑;状态经事件告知客户端。
                self.publish_goal(&notice.session_id, Some(row.clone()));
                let root = row.session_id.clone();
                if notice.session_id != root {
                    self.publish_goal(&root, Some(row));
                }
                log::info!(
                    "[goal] budget exhausted; goal {} budget_limited (session={})",
                    goal.goal_id,
                    notice.session_id
                );
                return;
            }
            Ok(Some(row)) => {
                self.publish_goal(&notice.session_id, Some(row.clone()));
                let root = row.session_id.clone();
                if notice.session_id != root {
                    self.publish_goal(&root, Some(row));
                }
            }
            Ok(None) => return, // goal 已被替换/清除
            Err(error) => {
                log::warn!("[goal] accounting failed for {}: {error}", notice.session_id);
                return;
            }
        }

        // 闸 3:空回复审计——只对自动续跑轮计数(Codex 同律);用户回合重置计数。
        let automatic = input.contains(GOAL_CONTINUATION_MARKER);
        let empty = answer.trim().is_empty() && tool_count == 0;
        let audited = self.owners.db().write_blocking({
            let session_id = goal.session_id.clone();
            let goal_id = goal.goal_id.clone();
            move |conn| goals::note_empty_turn(conn, &session_id, &goal_id, automatic && empty)
        });
        match audited {
            Ok(Some(row)) if row.status == GoalStatus::Blocked => {
                self.publish_goal(&notice.session_id, Some(row.clone()));
                let root = row.session_id.clone();
                if notice.session_id != root {
                    self.publish_goal(&root, Some(row));
                }
                log::warn!(
                    "[goal] 3 consecutive empty continuations; goal {} blocked (session={})",
                    goal.goal_id,
                    notice.session_id
                );
                return;
            }
            Ok(Some(row)) => {
                if row.status != GoalStatus::Active {
                    return;
                }
            }
            Ok(None) => return,
            Err(error) => {
                log::warn!("[goal] empty audit failed for {}: {error}", notice.session_id);
                return;
            }
        }

        // 仍 Active → 续跑(Cancelled 也续跑:用户取消不暂停,所有者拍板 Codex 原样)。
        self.continue_goal_if_idle(&notice.session_id);
    }

    /// 状态迁移(系统侧:Blocked/BudgetLimited 等)+ 事件。按 goal 行所在会话定位。
    fn set_goal_status(&mut self, goal: &GoalRow, status: GoalStatus) {
        let session = goal.session_id.clone();
        let expected = goal.goal_id.clone();
        let update = GoalUpdate {
            status: Some(status),
            ..Default::default()
        };
        let outcome = self
            .owners
            .db()
            .write_blocking(move |conn| goals::update_goal(conn, &session, &update, Some(&expected)));
        match outcome {
            Ok(Some(row)) => self.publish_goal(&row.session_id.clone(), Some(row)),
            Ok(None) => {}
            Err(error) => log::warn!("[goal] status update failed for {}: {error}", goal.session_id),
        }
    }

    /// daemon 周期扫描(ADR-0015 切片 E/F):DB 中 Active goal 跨 idle 卸载/重启
    /// 存活。续跑目标是链头;链头已加载且忙(或有排队输入)跳过——终态钩子会续跑。
    /// 链头封箱(handoff 后继未建)时 deliver_continuation 自行跳过,下轮再探。
    pub fn sweep_active_goals(&mut self) -> Vec<String> {
        if self.shutting_down || !goals_enabled() {
            return Vec::new();
        }
        let active = self
            .owners
            .db()
            .read_blocking(|conn| goals::active_goals(conn))
            .unwrap_or_else(|error| {
                log::warn!("[goal] active goal scan failed: {error}");
                Vec::new()
            });
        let mut continued = Vec::new();
        for goal in active {
            let head = self
                .owners
                .db()
                .read_blocking({
                    let root = goal.session_id.clone();
                    move |conn| qaqh_session::db::store::handoff_chain_head(conn, &root)
                })
                .unwrap_or_else(|_| goal.session_id.clone());
            if let Some(instance) = self.instances.get(&head) {
                let busy = instance.loop_.activity_state()
                    != qaqh_ringing_contract::fact::ActivityState::Idle
                    || instance.loop_.pending_inputs() > 0;
                if busy {
                    continue;
                }
            }
            self.deliver_continuation(&goal);
            continued.push(goal.session_id.clone());
        }
        continued
    }
}
