//! ADR-0016 切片 1：封箱 → 内核 fork 的延时调度（service 层挂钩）。
//!
//! 终态路由闭包（service.rs init）在每个回合终态都会进 [`schedule_after_terminal`]：
//! 会话已封箱且仍是链头时，起一个 tokio 任务在 `FORK_DELAY_MS` 阅读窗后执行
//! fork。fork 本体在 [`crate::registry::handoff_fork`]，幂等闸在建行闭包里——
//! 本模块只负责「什么时候去试」，不是「要不要 fork」的权威；调度与 sweep 撞车
//! 也不会开出两条后继。

use std::time::Duration;

use crate::registry::handoff_fork::FORK_DELAY_MS;
use crate::service::QaqhService;

pub(crate) fn schedule_after_terminal(service: &QaqhService, session_id: &str) {
    let eligible = service
        .owners
        .db()
        .read_blocking({
            let session_id = session_id.to_string();
            move |conn| {
                let sealed = qaqh_session::db::store::session_handed_off(conn, &session_id)?;
                let root = qaqh_session::db::store::handoff_chain_root(conn, &session_id)?;
                let head = qaqh_session::db::store::handoff_chain_head(conn, &root)?;
                Ok::<_, qaqh_session::db::DbError>(sealed && head == session_id)
            }
        })
        .unwrap_or_else(|error| {
            log::warn!("[handoff-fork] eligibility check for {session_id} failed: {error}");
            false
        });
    if !eligible {
        return;
    }
    if tokio::runtime::Handle::try_current().is_err() {
        // 没有 runtime 上下文就起不了延时任务：兜底扫描（切片 3）会补上。
        log::warn!("[handoff-fork] no tokio runtime; fork for {session_id} deferred to sweep");
        return;
    }
    let service = service.clone();
    let sealed = session_id.to_string();
    tokio::spawn(async move {
        tokio::time::sleep(Duration::from_millis(FORK_DELAY_MS)).await;
        let outcome = match service.registry() {
            Ok(mut registry) => registry.fork_handoff_successor(&sealed),
            Err(error) => Err(error),
        };
        match outcome {
            Ok(Some(successor)) => log::info!("[handoff-fork] {sealed} → {successor}"),
            Ok(None) => {}
            Err(error) => {
                log::warn!("[handoff-fork] fork for {sealed} failed (sweep will retry): {error}")
            }
        }
    });
}
