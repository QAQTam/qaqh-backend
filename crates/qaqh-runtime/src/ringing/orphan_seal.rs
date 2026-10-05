//! ringing::orphan_seal — 孤儿收尾（running turn seal + 全量 seal）。
//!
//! 由 `hub.rs` 拆分（Phase 2-6）：`impl RingingHub` 跨文件块，对外 API 不变。

use super::hub::RingingHub;
use qaqh_domain::{TimelineBlockState, TimelineFailure, TimelineTurnState};

impl RingingHub {
    /// 收尾孤儿 running turn。daemon 重启或 worker 重新 spawn 后，timeline 中
    /// 任何未 seal 的 turn 都没有存活生产者（典型场景：工具调用未返回 result
    /// 时进程被杀）。若不 seal，前端会永远把它投影为 running，stop/send 按钮
    /// 卡死在 stop 且新消息无法发送——这是"重启 daemon/前端都无法再发送新
    /// 消息"的根因。
    ///
    /// seal 顺序遵循 TimelineAppender 契约：先 seal 全部 open block，再 seal
    /// 全部未 seal round（is_final=true，这是该 turn 的最后一轮），最后将 turn
    /// seal 为 Cancelled。幂等：已 seal 的 turn 直接跳过。返回是否有变更。
    pub fn seal_orphan_running_turns(&self, session_id: &str) -> bool {
        let mut appender = self.timeline.lock().unwrap_or_else(|e| e.into_inner());
        let Some(snapshot) = appender.snapshot(session_id) else {
            return false;
        };
        let mut changed = false;
        for turn in snapshot.turns.iter().filter(|turn| !turn.sealed) {
            for round in &turn.rounds {
                for block in &round.blocks {
                    if block.state == TimelineBlockState::Sealed {
                        continue;
                    }
                    match appender.seal_block(
                        session_id,
                        &turn.turn_id,
                        round.round_num,
                        &block.block_id,
                    ) {
                        Ok(_) => changed = true,
                        Err(error) => log::warn!(
                            "[timeline] orphan seal block failed for {session_id} {}: {error}",
                            block.block_id
                        ),
                    }
                }
            }
            for round in &turn.rounds {
                if round.sealed {
                    continue;
                }
                match appender.seal_round(session_id, &turn.turn_id, round.round_num, true) {
                    Ok(_) => changed = true,
                    Err(error) => log::warn!(
                        "[timeline] orphan seal round failed for {session_id} {}/{}: {error}",
                        turn.turn_id,
                        round.round_num
                    ),
                }
            }
            match appender.seal_turn_with_state(
                session_id,
                &turn.turn_id,
                TimelineTurnState::Cancelled,
                Some(TimelineFailure {
                    code: "daemon_restart_interrupted".into(),
                    message:
                        "Daemon restarted while this turn was running; the turn was interrupted and the session is ready for new input."
                            .into(),
                }),
            ) {
                Ok(_) => changed = true,
                Err(error) => log::warn!(
                    "[timeline] orphan seal turn failed for {session_id} {}: {error}",
                    turn.turn_id
                ),
            }
        }
        drop(appender);
        self.offload_all_sealed_turns(session_id);
        changed
    }


    /// 优雅关闭收尾：对所有已知 timeline seed 执行孤儿收尾（timeline）。
    /// 正常路径下 worker 已优雅退出并自行 seal（terminal intent 同步落盘），
    /// 此处兜底 worker 超时被杀 / 未收尾的场景——退出时不留孤儿，安装器
    /// 更新后重启不再出现 daemon_restart_interrupted。
    ///
    /// 只覆盖已加载 seed 的当前状态：磁盘上未加载的 seed 由下次
    /// `ensure_timeline_loaded` 启动时收尾（懒加载路径自带孤儿 seal）。
    pub fn seal_all_orphans(&self) {
        let sessions: Vec<String> = self
            .disk_timeline_sessions
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .iter()
            .cloned()
            .collect();
        for session_id in &sessions {
            if self.seal_orphan_running_turns(session_id) {
                log::info!("[timeline] sealed orphan turn(s) for {session_id} at shutdown");
            }
        }
    }
}
