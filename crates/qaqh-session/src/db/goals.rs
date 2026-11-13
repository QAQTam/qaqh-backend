//! `session_goals` 直查函数(ADR-0015):会话持久目标的唯一业务权威。
//!
//! 与 [`super::store`] 同纪律:全部接收 `&Connection`/`&mut Connection`,由调用方
//! 在 `Db::write`/`Db::read` 闭包内组合短事务;不做 Repository 抽象。
//! 状态与用量以库内行为准;坏 status 字符串读侧 fail-closed(`Corrupt`),不静默降级。

use rusqlite::{Connection, OptionalExtension, params};

use qaqh_ringing_contract::state::{GoalStatus, GoalView};

use super::DbError;
use super::store::now_ms;

/// 库内 goal 行(含续跑审计计数;计数不出 wire,只在投影时丢弃)。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GoalRow {
    pub goal_id: String,
    pub session_id: String,
    pub objective: String,
    pub status: GoalStatus,
    pub token_budget: Option<u64>,
    pub tokens_used: u64,
    pub time_used_ms: u64,
    /// 连续空续跑回合计数(ADR-0015 §决策 4):≥3 → Blocked。
    pub empty_turns: u32,
    pub created_at_ms: i64,
    pub updated_at_ms: i64,
}

/// goal 可变字段补丁(`None` = 保持原值)。`token_budget` 双层 Option:
/// 外层 = 是否改预算,内层 = 改成什么(`None` = 取消预算)。
#[derive(Debug, Clone, Default)]
pub struct GoalUpdate {
    pub objective: Option<String>,
    pub status: Option<GoalStatus>,
    pub token_budget: Option<Option<u64>>,
}

fn status_to_str(status: GoalStatus) -> &'static str {
    match status {
        GoalStatus::Active => "active",
        GoalStatus::Paused => "paused",
        GoalStatus::Blocked => "blocked",
        GoalStatus::UsageLimited => "usage_limited",
        GoalStatus::BudgetLimited => "budget_limited",
        GoalStatus::Complete => "complete",
    }
}

fn status_from_str(text: &str) -> Result<GoalStatus, DbError> {
    match text {
        "active" => Ok(GoalStatus::Active),
        "paused" => Ok(GoalStatus::Paused),
        "blocked" => Ok(GoalStatus::Blocked),
        "usage_limited" => Ok(GoalStatus::UsageLimited),
        "budget_limited" => Ok(GoalStatus::BudgetLimited),
        "complete" => Ok(GoalStatus::Complete),
        other => Err(DbError::Corrupt(format!("session_goals.status: {other:?}"))),
    }
}

const SELECT_COLS: &str = "goal_id, session_id, objective, status, token_budget, tokens_used, \
                             time_used_ms, empty_turns, created_at_ms, updated_at_ms";

fn row_to_goal(row: &rusqlite::Row<'_>) -> rusqlite::Result<GoalRow> {
    let status_text: String = row.get(3)?;
    let status = status_from_str(&status_text).map_err(|error| {
        rusqlite::Error::FromSqlConversionFailure(
            3,
            rusqlite::types::Type::Text,
            Box::new(error),
        )
    })?;
    Ok(GoalRow {
        goal_id: row.get(0)?,
        session_id: row.get(1)?,
        objective: row.get(2)?,
        status,
        token_budget: row.get::<_, Option<i64>>(4)?.map(|v| v.max(0) as u64),
        tokens_used: row.get::<_, i64>(5)?.max(0) as u64,
        time_used_ms: row.get::<_, i64>(6)?.max(0) as u64,
        empty_turns: row.get::<_, i64>(7)?.max(0) as u32,
        created_at_ms: row.get(8)?,
        updated_at_ms: row.get(9)?,
    })
}

/// 投影为 wire 读模型(审计计数不出 wire)。
pub fn goal_view(row: &GoalRow) -> GoalView {
    GoalView {
        goal_id: row.goal_id.clone(),
        session_id: row.session_id.clone(),
        objective: row.objective.clone(),
        status: row.status,
        token_budget: row.token_budget,
        tokens_used: row.tokens_used,
        time_used_ms: row.time_used_ms,
        created_at_ms: row.created_at_ms,
        updated_at_ms: row.updated_at_ms,
    }
}

pub fn get_goal(conn: &Connection, session_id: &str) -> Result<Option<GoalRow>, DbError> {
    conn.query_row(
        &format!("SELECT {SELECT_COLS} FROM session_goals WHERE session_id = ?1"),
        [session_id],
        row_to_goal,
    )
    .optional()
    .map_err(DbError::from)
}

/// ADR-0015 切片F：沿 handoff 工作链解析 goal——goal 行留在链根,
/// 链上任何会话(含 fork 后继)读到的都是同一个 goal。
pub fn resolve_goal(conn: &Connection, session_id: &str) -> Result<Option<GoalRow>, DbError> {
    let root = super::store::handoff_chain_root(conn, session_id)?;
    get_goal(conn, &root)
}

/// 创建 goal。已有**未完成** goal(状态非 complete)→ `GoalExists`;
/// 已 complete 的旧 goal 被替换(新 goal_id,用量清零)。
pub fn create_goal(
    conn: &mut Connection,
    session_id: &str,
    goal_id: &str,
    objective: &str,
    token_budget: Option<u64>,
) -> Result<GoalRow, DbError> {
    let tx = conn.transaction()?;
    if let Some(existing) = get_goal(&tx, session_id)? {
        if existing.status != GoalStatus::Complete {
            return Err(DbError::GoalExists {
                session_id: session_id.to_string(),
            });
        }
        tx.execute(
            "DELETE FROM session_goals WHERE session_id = ?1",
            [session_id],
        )?;
    }
    let now = now_ms();
    tx.execute(
        "INSERT INTO session_goals
            (goal_id, session_id, objective, status, token_budget, tokens_used,
             time_used_ms, empty_turns, created_at_ms, updated_at_ms)
         VALUES (?1, ?2, ?3, 'active', ?4, 0, 0, 0, ?5, ?5)",
        params![
            goal_id,
            session_id,
            objective,
            token_budget.map(|v| v as i64),
            now,
        ],
    )?;
    let row = get_goal(&tx, session_id)?
        .ok_or_else(|| DbError::Corrupt(format!("goal {goal_id} vanished after insert")))?;
    tx.commit()?;
    Ok(row)
}

/// 更新 goal。`expected_goal_id` 给出时做 CAS:不匹配 = 目标已被替换,返回 `None`。
/// 无 goal 返回 `None`(调用方渲染 NoGoal 拒绝码)。
/// 状态迁回 Active(resume)重置空回复审计——Codex「resumed run starts a fresh
/// blocked audit」同律。
pub fn update_goal(
    conn: &mut Connection,
    session_id: &str,
    update: &GoalUpdate,
    expected_goal_id: Option<&str>,
) -> Result<Option<GoalRow>, DbError> {
    let tx = conn.transaction()?;
    let Some(mut row) = get_goal(&tx, session_id)? else {
        return Ok(None);
    };
    if let Some(expected) = expected_goal_id
        && row.goal_id != expected
    {
        return Ok(None);
    }
    if let Some(objective) = &update.objective {
        row.objective = objective.clone();
    }
    if let Some(status) = update.status {
        if status == GoalStatus::Active && row.status != GoalStatus::Active {
            row.empty_turns = 0;
        }
        row.status = status;
    }
    if let Some(budget) = update.token_budget {
        row.token_budget = budget;
    }
    // 预算收紧可能当场触发熔断(与记账同律)。
    if row.status == GoalStatus::Active
        && row.token_budget.is_some_and(|budget| row.tokens_used >= budget)
    {
        row.status = GoalStatus::BudgetLimited;
    }
    let now = now_ms();
    tx.execute(
        "UPDATE session_goals
            SET objective = ?2, status = ?3, token_budget = ?4, empty_turns = ?5,
                updated_at_ms = ?6
          WHERE session_id = ?1",
        params![
            session_id,
            row.objective,
            status_to_str(row.status),
            row.token_budget.map(|v| v as i64),
            i64::from(row.empty_turns),
            now,
        ],
    )?;
    row.updated_at_ms = now;
    tx.commit()?;
    Ok(Some(row))
}

pub fn delete_goal(conn: &mut Connection, session_id: &str) -> Result<Option<GoalRow>, DbError> {
    let tx = conn.transaction()?;
    let row = get_goal(&tx, session_id)?;
    if row.is_some() {
        tx.execute(
            "DELETE FROM session_goals WHERE session_id = ?1",
            [session_id],
        )?;
    }
    tx.commit()?;
    Ok(row)
}

/// 用量记账:累加 token 与墙钟增量;Active 且 `tokens_used >= token_budget`
/// 时自动转 BudgetLimited(DB 层熔断,与 Codex `status_after_budget_limit` 同律)。
/// goal 不存在或 goal_id 不匹配(已被替换)返回 `None`,不记到别人头上。
pub fn account_usage(
    conn: &mut Connection,
    session_id: &str,
    goal_id: &str,
    tokens: u64,
    time_ms: u64,
) -> Result<Option<GoalRow>, DbError> {
    if tokens == 0 && time_ms == 0 {
        return get_goal(conn, session_id).map(|row| row.filter(|r| r.goal_id == goal_id));
    }
    let tx = conn.transaction()?;
    let Some(mut row) = get_goal(&tx, session_id)? else {
        return Ok(None);
    };
    if row.goal_id != goal_id {
        return Ok(None);
    }
    row.tokens_used = row.tokens_used.saturating_add(tokens);
    row.time_used_ms = row.time_used_ms.saturating_add(time_ms);
    if row.status == GoalStatus::Active
        && row.token_budget.is_some_and(|budget| row.tokens_used >= budget)
    {
        row.status = GoalStatus::BudgetLimited;
    }
    let now = now_ms();
    tx.execute(
        "UPDATE session_goals
            SET tokens_used = ?2, time_used_ms = ?3, status = ?4, updated_at_ms = ?5
          WHERE session_id = ?1",
        params![
            session_id,
            row.tokens_used as i64,
            row.time_used_ms as i64,
            status_to_str(row.status),
            now,
        ],
    )?;
    row.updated_at_ms = now;
    tx.commit()?;
    Ok(Some(row))
}

/// 续跑空回复审计(ADR-0015 §决策 4):`empty = true` 计数 +1,≥3 → Blocked;
/// `empty = false` 清零。只对 Active goal 生效;goal 不存在/id 不匹配返回 `None`。
pub fn note_empty_turn(
    conn: &mut Connection,
    session_id: &str,
    goal_id: &str,
    empty: bool,
) -> Result<Option<GoalRow>, DbError> {
    let tx = conn.transaction()?;
    let Some(mut row) = get_goal(&tx, session_id)? else {
        return Ok(None);
    };
    if row.goal_id != goal_id || row.status != GoalStatus::Active {
        return Ok(Some(row));
    }
    if empty {
        row.empty_turns = row.empty_turns.saturating_add(1);
        if row.empty_turns >= 3 {
            row.status = GoalStatus::Blocked;
        }
    } else {
        row.empty_turns = 0;
    }
    let now = now_ms();
    tx.execute(
        "UPDATE session_goals
            SET empty_turns = ?2, status = ?3, updated_at_ms = ?4
          WHERE session_id = ?1",
        params![
            session_id,
            i64::from(row.empty_turns),
            status_to_str(row.status),
            now,
        ],
    )?;
    row.updated_at_ms = now;
    tx.commit()?;
    Ok(Some(row))
}

/// daemon 周期扫描用:全部 Active goal(含冷会话)。
pub fn active_goals(conn: &Connection) -> Result<Vec<GoalRow>, DbError> {
    let mut stmt = conn.prepare(&format!(
        "SELECT {SELECT_COLS} FROM session_goals WHERE status = 'active' ORDER BY created_at_ms"
    ))?;
    let rows = stmt
        .query_map([], row_to_goal)?
        .collect::<Result<Vec<_>, _>>()?;
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn open_db() -> Connection {
        let mut conn = Connection::open_in_memory().expect("open");
        // 最小 sessions 行满足外键;schema 全量初始化走 Db::open 路径,
        // 这里只建 goal 相关表做单元级验证。
        conn.execute_batch(
            "CREATE TABLE sessions (
                 session_id TEXT PRIMARY KEY,
                 handoff_from_session_id TEXT,
                 handoff_sealed_at_ms INTEGER,
                 created_at_ms INTEGER NOT NULL DEFAULT 0
             );
             INSERT INTO sessions (session_id) VALUES ('s1');
             CREATE TABLE session_goals (
                 goal_id       TEXT PRIMARY KEY,
                 session_id    TEXT NOT NULL UNIQUE REFERENCES sessions(session_id) ON DELETE CASCADE,
                 objective     TEXT NOT NULL,
                 status        TEXT NOT NULL,
                 token_budget  INTEGER,
                 tokens_used   INTEGER NOT NULL DEFAULT 0,
                 time_used_ms  INTEGER NOT NULL DEFAULT 0,
                 empty_turns   INTEGER NOT NULL DEFAULT 0,
                 created_at_ms INTEGER NOT NULL,
                 updated_at_ms INTEGER NOT NULL
             );",
        )
        .expect("ddl");
        conn
    }

    #[test]
    fn create_rejects_unfinished_and_replaces_complete() {
        let mut conn = open_db();
        let row = create_goal(&mut conn, "s1", "g1", "ship it", Some(100)).expect("create");
        assert_eq!(row.status, GoalStatus::Active);
        assert!(matches!(
            create_goal(&mut conn, "s1", "g2", "another", None),
            Err(DbError::GoalExists { .. })
        ));
        update_goal(
            &mut conn,
            "s1",
            &GoalUpdate {
                status: Some(GoalStatus::Complete),
                ..Default::default()
            },
            Some("g1"),
        )
        .expect("complete");
        let replaced = create_goal(&mut conn, "s1", "g2", "another", None).expect("replace");
        assert_eq!(replaced.goal_id, "g2");
        assert_eq!(replaced.tokens_used, 0);
    }

    #[test]
    fn budget_limit_fires_on_accounting() {
        let mut conn = open_db();
        create_goal(&mut conn, "s1", "g1", "ship it", Some(100)).expect("create");
        let row = account_usage(&mut conn, "s1", "g1", 60, 1000)
            .expect("account")
            .expect("row");
        assert_eq!(row.status, GoalStatus::Active);
        let row = account_usage(&mut conn, "s1", "g1", 40, 0)
            .expect("account")
            .expect("row");
        assert_eq!(row.status, GoalStatus::BudgetLimited);
        assert_eq!(row.tokens_used, 100);
        // 熔断后不再被记账改回 active。
        let row = account_usage(&mut conn, "s1", "g1", 5, 0)
            .expect("account")
            .expect("row");
        assert_eq!(row.status, GoalStatus::BudgetLimited);
    }

    #[test]
    fn stale_goal_id_is_not_accounted() {
        let mut conn = open_db();
        create_goal(&mut conn, "s1", "g1", "ship it", None).expect("create");
        assert!(account_usage(&mut conn, "s1", "stale", 10, 0).expect("account").is_none());
        let row = get_goal(&conn, "s1").expect("get").expect("row");
        assert_eq!(row.tokens_used, 0);
    }

    #[test]
    fn three_empty_turns_block_and_resume_resets_audit() {
        let mut conn = open_db();
        create_goal(&mut conn, "s1", "g1", "ship it", None).expect("create");
        for _ in 0..2 {
            let row = note_empty_turn(&mut conn, "s1", "g1", true)
                .expect("note")
                .expect("row");
            assert_eq!(row.status, GoalStatus::Active);
        }
        let row = note_empty_turn(&mut conn, "s1", "g1", true)
            .expect("note")
            .expect("row");
        assert_eq!(row.status, GoalStatus::Blocked);
        // resume → fresh audit(Codex 同律)。
        let row = update_goal(
            &mut conn,
            "s1",
            &GoalUpdate {
                status: Some(GoalStatus::Active),
                ..Default::default()
            },
            Some("g1"),
        )
        .expect("resume")
        .expect("row");
        assert_eq!(row.empty_turns, 0);
        assert_eq!(row.status, GoalStatus::Active);
    }

    #[test]
    fn cas_mismatch_returns_none() {
        let mut conn = open_db();
        create_goal(&mut conn, "s1", "g1", "ship it", None).expect("create");
        let outcome = update_goal(
            &mut conn,
            "s1",
            &GoalUpdate {
                status: Some(GoalStatus::Paused),
                ..Default::default()
            },
            Some("stale"),
        )
        .expect("update");
        assert!(outcome.is_none());
        assert_eq!(get_goal(&conn, "s1").expect("get").expect("row").status, GoalStatus::Active);
    }

    /// 切片F：goal 行留在链根,后继会话沿链解析到同一个 goal;
    /// 链头下溯取最新后继;封箱标记独立可查。
    #[test]
    fn goal_resolves_along_handoff_chain() {
        let mut conn = open_db();
        create_goal(&mut conn, "s1", "g1", "ship it", Some(100)).expect("create");
        // s2 ← s1, s3 ← s2(两级 handoff)。
        conn.execute_batch(
            "INSERT INTO sessions (session_id, handoff_from_session_id, created_at_ms)
             VALUES ('s2', 's1', 1), ('s3', 's2', 2);",
        )
        .expect("chain");
        let resolved = resolve_goal(&conn, "s3").expect("resolve").expect("goal");
        assert_eq!(resolved.goal_id, "g1");
        assert_eq!(resolved.session_id, "s1");
        assert_eq!(
            crate::db::store::handoff_chain_root(&conn, "s3").expect("root"),
            "s1"
        );
        assert_eq!(
            crate::db::store::handoff_chain_head(&conn, "s1").expect("head"),
            "s3"
        );
        // 封箱前:链头可续跑;封箱后:引擎跳过(判据在此,投递逻辑在 registry)。
        assert!(!crate::db::store::session_handed_off(&conn, "s3").expect("sealed"));
        crate::db::store::mark_handoff_sealed(&mut conn, "s3").expect("seal");
        assert!(crate::db::store::session_handed_off(&conn, "s3").expect("sealed"));
        // 记账落在链根,不按后继会话。
        let row = account_usage(&mut conn, "s1", "g1", 10, 0)
            .expect("account")
            .expect("row");
        assert_eq!(row.tokens_used, 10);
        assert!(account_usage(&mut conn, "s3", "g1", 10, 0)
            .expect("account")
            .is_none());
    }
}
