//! 常驻用例:同一会话并发 read-modify-write 不得丢更新(P1-3 原子性判据)。
//!
//! 旧形态靠全局 `TODO_LOCK` 串行化;文件路径收编进 SQLite 后,原子性判据
//! 换成「写执行器串行 + 单事务 RMW」。本用例钉住 [`mutate_todo`] 的该承诺:
//! 8 线程 × 各追加 10 条,终态必须 80 行、无缺失、无重复——若 mutate 被拆成
//! 独立读事务 + 写事务(即回归),这里必然丢更新。

use std::sync::Arc;
use std::thread;

use qaqh_session::db::{Db, todos};

#[test]
fn concurrent_mutate_todo_never_loses_updates() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Arc::new(Db::open(dir.path().join("qaqh.db")).expect("open db"));

    let workers: Vec<_> = (0..8)
        .map(|worker| {
            let db = Arc::clone(&db);
            thread::spawn(move || {
                for seq in 0..10 {
                    todos::mutate_todo(&db, "S1", move |items, _state| {
                        items.push(todos::TodoRow {
                            todo_id: format!("w{worker}t{seq}"),
                            title: format!("w{worker} task {seq}"),
                            description: String::new(),
                            status: "pending".to_string(),
                            evidence: None,
                        });
                        Ok(())
                    })
                    .expect("mutate");
                }
            })
        })
        .collect();
    for worker in workers {
        worker.join().expect("worker");
    }

    let (items, _state) = todos::load_todo(&db, "S1").expect("load").expect("row exists");
    assert_eq!(items.len(), 80, "lost updates detected");
    let mut ids: Vec<_> = items.iter().map(|item| item.todo_id.clone()).collect();
    ids.sort();
    ids.dedup();
    assert_eq!(ids.len(), 80, "duplicate writes detected");
}

#[test]
fn failing_closure_rolls_back_without_touching_stored_list() {
    let dir = tempfile::tempdir().expect("tempdir");
    let db = Db::open(dir.path().join("qaqh.db")).expect("open db");
    todos::mutate_todo(&db, "S2", |items, _state| {
        items.push(todos::TodoRow {
            todo_id: "keep".to_string(),
            title: "keep".to_string(),
            description: String::new(),
            status: "pending".to_string(),
            evidence: None,
        });
        Ok(())
    })
    .expect("first mutate");

    let error = todos::mutate_todo::<(), _>(&db, "S2", |items, _state| {
        items.push(todos::TodoRow {
            todo_id: "ghost".to_string(),
            title: "ghost".to_string(),
            description: String::new(),
            status: "pending".to_string(),
            evidence: None,
        });
        Err("模型侧错误".to_string())
    })
    .expect_err("failing mutate must not commit");
    assert!(error.to_string().contains("模型侧错误"));
    let (items, _state) = todos::load_todo(&db, "S2").expect("load").expect("row exists");
    assert_eq!(items.len(), 1, "rollback lost or kept the wrong rows");
    assert_eq!(items[0].todo_id, "keep");
}
