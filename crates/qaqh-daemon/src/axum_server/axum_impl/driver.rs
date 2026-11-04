//! Driver 席位(daemon 内存,per-session 单席位)。
//!
//! 席位语义归属 daemon:holder 是 daemon 认证的 lease(`client_session_id`),
//! lease 不跨重启 → 席位不落库、不跨重启是正确语义;重启后新 server_epoch,
//! 客户端经 bootstrap 重新对齐。SessionState.driver 由 bootstrap handler 从
//! 这里 overlay 填充,claim/release 生效即经 owner 发 `DriverChanged`。

use std::collections::HashMap;
use std::sync::Mutex;

use qaqh_ringing_contract::state::DriverState;

/// 一次 claim/release 的裁定结果,直接映射 `RingingV2DriverClaim/ReleaseResponse`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DriverVerdict {
    pub accepted: bool,
    pub holder: Option<String>,
    pub driver_epoch: u64,
    pub reason: &'static str,
}

#[derive(Debug, Clone, PartialEq, Eq)]
struct Seat {
    holder: String,
    epoch: u64,
}

/// daemon 全局一份;`holder` 为 lease 的 client_session_id,epoch 每 session
/// 内单调递增(抢占/释放都推进,重连方按 epoch 检测换代)。
#[derive(Default)]
pub struct DriverSeats {
    seats: Mutex<HashMap<String, Seat>>,
}

impl DriverSeats {
    pub fn new() -> Self {
        Self::default()
    }

    /// 当前席位(bootstrap overlay 用);空位 = None。
    pub fn get(&self, session_id: &str) -> Option<DriverState> {
        self.seats
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner())
            .get(session_id)
            .map(|seat| DriverState {
                holder: Some(seat.holder.clone()),
                epoch: seat.epoch,
            })
    }

    /// claim:空位或 `holder == caller`(幂等重入)或调用方断言的
    /// `stale_holder` 与当前 holder 一致(接管僵死席位)→ 接受;否则拒绝并
    /// 返回当前 holder/epoch 供仲裁。接受即推进 epoch。
    pub fn claim(
        &self,
        session_id: &str,
        caller: &str,
        stale_holder: Option<&str>,
    ) -> DriverVerdict {
        let mut seats = self.seats.lock().unwrap_or_else(|e| e.into_inner());
        match seats.get(session_id) {
            Some(seat) if seat.holder == caller => DriverVerdict {
                accepted: true,
                holder: Some(seat.holder.clone()),
                driver_epoch: seat.epoch,
                reason: "already_holder",
            },
            Some(seat) if stale_holder == Some(seat.holder.as_str()) => {
                let epoch = seat.epoch + 1;
                seats.insert(
                    session_id.to_string(),
                    Seat {
                        holder: caller.to_string(),
                        epoch,
                    },
                );
                DriverVerdict {
                    accepted: true,
                    holder: Some(caller.to_string()),
                    driver_epoch: epoch,
                    reason: "stale_holder_replaced",
                }
            }
            Some(seat) => DriverVerdict {
                accepted: false,
                holder: Some(seat.holder.clone()),
                driver_epoch: seat.epoch,
                reason: "busy",
            },
            None => {
                let epoch = 1;
                seats.insert(
                    session_id.to_string(),
                    Seat {
                        holder: caller.to_string(),
                        epoch,
                    },
                );
                DriverVerdict {
                    accepted: true,
                    holder: Some(caller.to_string()),
                    driver_epoch: epoch,
                    reason: "claimed",
                }
            }
        }
    }

    /// release:`holder == caller` 或 `expected_epoch` 命中当前 epoch → 释放;
    /// 否则拒绝。空位释放幂等接受。
    pub fn release(
        &self,
        session_id: &str,
        caller: &str,
        expected_epoch: Option<u64>,
    ) -> DriverVerdict {
        let mut seats = self.seats.lock().unwrap_or_else(|e| e.into_inner());
        match seats.get(session_id) {
            Some(seat) if seat.holder == caller || expected_epoch == Some(seat.epoch) => {
                let epoch = seat.epoch + 1;
                seats.remove(session_id);
                DriverVerdict {
                    accepted: true,
                    holder: None,
                    driver_epoch: epoch,
                    reason: "released",
                }
            }
            Some(seat) => DriverVerdict {
                accepted: false,
                holder: Some(seat.holder.clone()),
                driver_epoch: seat.epoch,
                reason: "not_holder",
            },
            None => DriverVerdict {
                accepted: true,
                holder: None,
                driver_epoch: 0,
                reason: "not_held",
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn claim_is_exclusive_and_stale_holder_takes_over() {
        let seats = DriverSeats::new();
        let first = seats.claim("s1", "lease-a", None);
        assert!(first.accepted);
        assert_eq!(first.holder.as_deref(), Some("lease-a"));
        assert_eq!(first.driver_epoch, 1);

        // 他人无 stale 断言 → busy,回当前 holder/epoch。
        let busy = seats.claim("s1", "lease-b", None);
        assert!(!busy.accepted);
        assert_eq!(busy.reason, "busy");
        assert_eq!(busy.holder.as_deref(), Some("lease-a"));
        assert_eq!(busy.driver_epoch, 1);

        // 错误 stale 断言 → busy。
        let wrong_stale = seats.claim("s1", "lease-b", Some("lease-c"));
        assert!(!wrong_stale.accepted);

        // stale 断言命中 → 接管,epoch 推进。
        let takeover = seats.claim("s1", "lease-b", Some("lease-a"));
        assert!(takeover.accepted);
        assert_eq!(takeover.driver_epoch, 2);

        // 幂等重入:同 holder 不推进 epoch。
        let reentry = seats.claim("s1", "lease-b", None);
        assert!(reentry.accepted);
        assert_eq!(reentry.reason, "already_holder");
        assert_eq!(reentry.driver_epoch, 2);
    }

    #[test]
    fn release_requires_holder_or_matching_epoch() {
        let seats = DriverSeats::new();
        seats.claim("s1", "lease-a", None);

        let stranger = seats.release("s1", "lease-b", None);
        assert!(!stranger.accepted);
        assert_eq!(stranger.reason, "not_holder");

        let by_epoch = seats.release("s1", "lease-b", Some(1));
        assert!(by_epoch.accepted, "expected_epoch 命中即可释放");
        assert_eq!(by_epoch.driver_epoch, 2);

        seats.claim("s1", "lease-a", None);
        let by_holder = seats.release("s1", "lease-a", None);
        assert!(by_holder.accepted);
        assert_eq!(by_holder.holder, None);

        // 空位释放幂等。
        let idle = seats.release("s1", "lease-a", None);
        assert!(idle.accepted);
        assert_eq!(idle.reason, "not_held");
    }
}
