//! `GoalHost` 的进程内实现(ADR-0015 切片 D)。
//!
//! 与 `host_impl`(SubagentHost)同款先例:模型工具 handler 经宿主句柄直达
//! 进程内 DB + owner 泵 + registry,不建立 HTTP 回环。用户命令面
//! (`goal.set/get/clear`)在 `service::handlers::goal`;本模块是模型面
//! (create 只创建、update 只迁状态),两者共用 DB 权威与事件/续跑出口。

use qaqh_ringing_contract::state::{GoalStatus, GoalView};
use qaqh_session::db::goals::{self, GoalUpdate};

use crate::service::handlers::goal::{goals_config, publish_goal};
use crate::service::QaqhService;

impl qaqh_goal::GoalHost for QaqhService {
    fn create_goal(
        &self,
        session_id: &str,
        objective: &str,
        token_budget: Option<u64>,
    ) -> Result<GoalView, String> {
        let config = goals_config();
        if !config.enabled {
            return Err("goals feature is disabled".to_string());
        }
        let objective = objective.trim();
        if objective.is_empty() {
            return Err("objective is required".to_string());
        }
        if let Some(budget) = token_budget {
            if budget == 0 {
                return Err("goal token budget must be positive".to_string());
            }
            if let Some(max) = config.max_token_budget
                && budget > max
            {
                return Err(format!(
                    "goal token budget {budget} exceeds the configured maximum {max}"
                ));
            }
        }
        // 缺省预算 = config 上限(Codex create_goal 同律)。
        let budget = token_budget.or(config.max_token_budget);
        let goal_id = qaqh_ringing_contract::ulid::generate_ulid();
        // 切片F：goal 锚定链根——从后继会话创建也落在根上,后续 fork 不断链。
        let session = self
            .owners()
            .db()
            .read_blocking({
                let session_id = session_id.to_string();
                move |conn| qaqh_session::db::store::handoff_chain_root(conn, &session_id)
            })
            .map_err(|error| error.to_string())?;
        let objective = objective.to_string();
        let row = self
            .owners()
            .db()
            .write_blocking(move |conn| {
                goals::create_goal(conn, &session, &goal_id, &objective, budget)
            })
            .map_err(|error| error.to_string())?;
        let view = goals::goal_view(&row);
        publish_goal(self, session_id, Some(view.clone()));
        self.registry()
            .map_err(|error| error.to_string())?
            .continue_goal_if_idle(session_id);
        Ok(view)
    }

    fn update_goal_status(&self, session_id: &str, status: GoalStatus) -> Result<GoalView, String> {
        let existing = self
            .owners()
            .db()
            .read_blocking({
                let session_id = session_id.to_string();
                move |conn| goals::resolve_goal(conn, &session_id)
            })
            .map_err(|error| error.to_string())?
            .ok_or_else(|| format!("session {session_id} has no goal"))?;
        // 切片F：写口按 goal 行所在会话(链根)定位。
        let session = existing.session_id.clone();
        let expected = existing.goal_id.clone();
        let update = GoalUpdate {
            status: Some(status),
            ..Default::default()
        };
        let row = self
            .owners()
            .db()
            .write_blocking(move |conn| goals::update_goal(conn, &session, &update, Some(&expected)))
            .map_err(|error| error.to_string())?
            .ok_or_else(|| format!("session {session_id} goal changed concurrently"))?;
        let view = goals::goal_view(&row);
        publish_goal(self, session_id, Some(view.clone()));
        if row.status == GoalStatus::Active {
            self.registry()
                .map_err(|error| error.to_string())?
                .continue_goal_if_idle(session_id);
        }
        Ok(view)
    }

    fn get_goal(&self, session_id: &str) -> Result<Option<GoalView>, String> {
        let row = self
            .owners()
            .db()
            .read_blocking({
                let session_id = session_id.to_string();
                move |conn| goals::resolve_goal(conn, &session_id)
            })
            .map_err(|error| error.to_string())?;
        Ok(row.as_ref().map(goals::goal_view))
    }
}
