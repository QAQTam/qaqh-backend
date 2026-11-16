//! 子进程 env 注入的进程内覆盖层（config `[exec.env]` 驱动）。
//!
//! 进程环境块是创建时的快照，OS 不会向已运行进程推送变更通知——daemon 侧
//! 「用户改了环境变量自动生效」的唯一可行形态：daemon 重读配置源（qaqh-runtime
//! 热重载订阅，文件轮询 1.5s + 单写口双路汇入）→ 写入本覆盖层 → 各 spawn
//! 组装点消费。**仅影响新 spawn 的子进程**；已在跑的长驻进程（exec_session、
//! MCP server）不回填。
//!
//! 组装层序（审计 M1 白名单之上的用户授权层，见各组装点）：
//! [`super::direct::minimal_child_env`] < 本覆盖层 < 模型显式 env <
//! [`super::direct::forced_child_env`]。沙箱路径例外：scratch 重指
//! （TMP/TEMP/HOME/USERPROFILE）是 harness 沙箱权威，压过本覆盖层
//! （`sbx_bypass::child_env`）。
//!
//! 覆盖层保存 runtime 层已解析的值（`${secret:name}` 已替换）——本 crate 不
//! 依赖 qaqh-config，不做插值；空 map 即清空。
use std::collections::BTreeMap;
use std::sync::{OnceLock, RwLock};

fn slot() -> &'static RwLock<BTreeMap<String, String>> {
    static OVERLAY: OnceLock<RwLock<BTreeMap<String, String>>> = OnceLock::new();
    OVERLAY.get_or_init(|| RwLock::new(BTreeMap::new()))
}

/// 整体替换覆盖层（空 map = 清空）。由 runtime 的启动铺底 + 热重载订阅者调用；
/// 全量替换（非增量合并）保证「用户删掉一个键 → 下次 spawn 就看不到它」。
pub fn apply(env: &BTreeMap<String, String>) {
    let mut slot = slot().write().unwrap_or_else(|e| e.into_inner());
    *slot = env.clone();
}

/// 当前覆盖层快照（BTreeMap 序 = 按键名排序；spawn 组装点消费，短临界区）。
pub fn snapshot() -> Vec<(String, String)> {
    slot()
        .read()
        .unwrap_or_else(|e| e.into_inner())
        .iter()
        .map(|(key, value)| (key.clone(), value.clone()))
        .collect()
}

/// 测试串行化：覆盖层是进程级全局，单测/集成测试共用一把锁防止并行用例
/// 互相看见对方注入的键（poisoned 锁照常进入，测试 panic 不拖垮其余用例）。
#[cfg(test)]
pub(crate) fn test_lock() -> &'static std::sync::Mutex<()> {
    static LOCK: OnceLock<std::sync::Mutex<()>> = OnceLock::new();
    LOCK.get_or_init(|| std::sync::Mutex::new(()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_replaces_whole_map_and_snapshot_is_sorted() {
        let _guard = test_lock().lock().unwrap_or_else(|e| e.into_inner());
        apply(&BTreeMap::from([
            ("B".to_string(), "2".to_string()),
            ("A".to_string(), "1".to_string()),
        ]));
        assert_eq!(
            snapshot(),
            vec![
                ("A".to_string(), "1".to_string()),
                ("B".to_string(), "2".to_string())
            ]
        );
        // 全量替换语义：新 map 不含的键消失；空 map 清空。
        apply(&BTreeMap::from([("C".to_string(), "3".to_string())]));
        assert_eq!(snapshot(), vec![("C".to_string(), "3".to_string())]);
        apply(&BTreeMap::new());
        assert!(snapshot().is_empty());
    }
}
