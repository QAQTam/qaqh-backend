//! `goal.*` 服务面(ADR-0015):会话持久目标的可写入口。
//!
//! 与 truncate 的「先拉起再执行」不同:goal 状态不依赖在飞回合,**冷会话可用**
//! ——DB 是唯一权威,事件经 owner 泵同流定序(driver 先例),续跑由 registry
//! 在 set 成 Active 后接管(空闲开回合/忙则入队/冷则先加载)。

use serde_json::{Value, json};

use qaqh_ringing::{CommandRejection, CommandTarget, RingingCommand};
use qaqh_ringing_contract::state::{GoalStatus, GoalView};
use qaqh_session::db::goals::{self, GoalUpdate};

use crate::error::{QaqhError, QaqhResult};
use crate::service::QaqhService;

pub(crate) fn handle_ringing(
    service: &QaqhService,
    target: &CommandTarget,
    command: &RingingCommand,
) -> QaqhResult<Value> {
    let CommandTarget::Session { session_id } = target else {
        return Err(QaqhError::invalid(format!(
            "{} requires a session target",
            command.name()
        )));
    };
    match command {
        RingingCommand::GoalSet {
            objective,
            status,
            token_budget,
        } => set_goal(
            service,
            session_id,
            objective.as_deref(),
            *status,
            *token_budget,
        ),
        RingingCommand::GoalGet {} => get_goal(service, session_id),
        RingingCommand::GoalClear {} => clear_goal(service, session_id),
        other => Err(QaqhError::invalid(format!(
            "{} is not a goal mutation",
            other.name()
        ))),
    }
}

/// 当前 goals 配置(单一真相源 = config,与 subagent 同纪律)。
pub(crate) fn goals_config() -> qaqh_config::config::GoalsConfig {
    qaqh_config::watch::authoritative()
        .map(|cfg| cfg.goals.clone())
        .unwrap_or_default()
}

pub(crate) fn publish_goal(service: &QaqhService, session_id: &str, goal: Option<GoalView>) {
    service
        .owners()
        .get_or_create(session_id)
        .publish(qaqh_ringing_contract::events::SessionEvent::GoalUpdated { goal });
}

fn db_error(operation: &'static str, error: qaqh_session::db::DbError) -> QaqhError {
    match error {
        qaqh_session::db::DbError::GoalExists { session_id } => QaqhError::rejected(
            CommandRejection::GoalExists,
            format!("session {session_id} already has an unfinished goal; complete or clear it first"),
        ),
        qaqh_session::db::DbError::NoGoal { session_id } => QaqhError::rejected(
            CommandRejection::NoGoal,
            format!("session {session_id} has no goal"),
        ),
        other => QaqhError::context(operation, other),
    }
}

fn set_goal(
    service: &QaqhService,
    session_id: &str,
    objective: Option<&str>,
    status: Option<GoalStatus>,
    token_budget: Option<Option<u64>>,
) -> QaqhResult<Value> {
    let config = goals_config();
    if !config.enabled {
        return Err(QaqhError::invalid("goals feature is disabled"));
    }
    let objective = objective.map(str::trim).filter(|text| !text.is_empty());
    if let Some(Some(budget)) = token_budget {
        if budget == 0 {
            return Err(QaqhError::invalid("goal token budget must be positive"));
        }
        if let Some(max) = config.max_token_budget
            && budget > max
        {
            return Err(QaqhError::invalid(format!(
                "goal token budget {budget} exceeds the configured maximum {max}"
            )));
        }
    }

    let db = service.owners().db();
    // 切片F：沿 handoff 工作链解析——后继会话看到的/改的是链根的同一个 goal。
    let existing = db
        .read_blocking({
            let session_id = session_id.to_string();
            move |conn| goals::resolve_goal(conn, &session_id)
        })
        .map_err(|error| db_error("goal.set", error))?;

    let row = match objective {
        Some(objective) => {
            // 创建锚定链根(与模型工具同律);改目标按 goal 行所在会话定位。
            let session = match &existing {
                Some(row) => row.session_id.clone(),
                None => db
                    .read_blocking({
                        let session_id = session_id.to_string();
                        move |conn| {
                            qaqh_session::db::store::handoff_chain_root(conn, &session_id)
                        }
                    })
                    .map_err(|error| db_error("goal.set", error))?,
            };
            let objective = objective.to_string();
            // 创建时缺省预算 = config 上限(Codex create_goal 同律);改目标时
            // 未显式给预算则保持原值。
            let create_budget = token_budget
                .flatten()
                .or(config.max_token_budget);
            let update = GoalUpdate {
                objective: Some(objective.clone()),
                status,
                token_budget,
            };
            let expected = existing.as_ref().map(|row| row.goal_id.clone());
            let outcome = db
                .write_blocking(move |conn| {
                    if expected.is_none() {
                        let goal_id = qaqh_ringing_contract::ulid::generate_ulid();
                        goals::create_goal(conn, &session, &goal_id, &objective, create_budget)
                            .map(Some)
                    } else {
                        goals::update_goal(conn, &session, &update, expected.as_deref())
                    }
                })
                .map_err(|error| db_error("goal.set", error))?;
            outcome.ok_or_else(|| {
                QaqhError::rejected(
                    CommandRejection::NoGoal,
                    format!("session {session_id} goal changed concurrently; re-read and retry"),
                )
            })?
        }
        None => {
            let Some(existing) = existing else {
                return Err(QaqhError::rejected(
                    CommandRejection::NoGoal,
                    format!("session {session_id} has no goal"),
                ));
            };
            let session = existing.session_id.clone();
            let expected = existing.goal_id.clone();
            let update = GoalUpdate {
                objective: None,
                status,
                token_budget,
            };
            db.write_blocking(move |conn| {
                goals::update_goal(conn, &session, &update, Some(&expected))
            })
            .map_err(|error| db_error("goal.set", error))?
            .ok_or_else(|| {
                QaqhError::rejected(
                    CommandRejection::NoGoal,
                    format!("session {session_id} goal changed concurrently; re-read and retry"),
                )
            })?
        }
    };

    let view = goals::goal_view(&row);
    publish_goal(service, session_id, Some(view.clone()));
    // Active → 续跑引擎接管(空闲开回合/忙则入队/冷则先加载)。
    if row.status == GoalStatus::Active {
        service
            .registry()
            .map_err(QaqhError::message)?
            .continue_goal_if_idle(session_id);
    }
    QaqhError::to_value("goal.set", json!({ "goal": view }))
}

fn get_goal(service: &QaqhService, session_id: &str) -> QaqhResult<Value> {
    let row = service
        .owners()
        .db()
        .read_blocking({
            let session_id = session_id.to_string();
            move |conn| goals::resolve_goal(conn, &session_id)
        })
        .map_err(|error| db_error("goal.get", error))?;
    match row {
        Some(row) => QaqhError::to_value("goal.get", json!({ "goal": goals::goal_view(&row) })),
        None => Ok(json!({ "goal": Value::Null })),
    }
}

fn clear_goal(service: &QaqhService, session_id: &str) -> QaqhResult<Value> {
    // 切片F：清除按链根定位——后继会话清的是整条链的 goal。
    let row = service
        .owners()
        .db()
        .write_blocking({
            let session_id = session_id.to_string();
            move |conn| {
                let root = qaqh_session::db::store::handoff_chain_root(conn, &session_id)?;
                goals::delete_goal(conn, &root)
            }
        })
        .map_err(|error| db_error("goal.clear", error))?;
    let cleared = row.is_some();
    if cleared {
        publish_goal(service, session_id, None);
    }
    Ok(json!({ "cleared": cleared }))
}
