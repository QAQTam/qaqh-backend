//! 纯净域枚举（`ADR-0042` 铁律 1）：`ActivityState` / `TurnTerminal` / `UsageInfo` /
//! `ContentRef`。
//!
//! "纯净"指**没有任何协议外衣**：不藏 `serde_json::Value`、不带通道/序号/连接
//! 元数据、不区分"哪个客户端看的版本"。这些是业务事实本身，`postcard` 直接编码。
//!
//! **与 V2 的关系**：V2 的真源仍是 `qaqh-ringing-contract`（冰封中，
//! `1C′` 纪律），本模块**不**提供 V2 ↔ V3 转换层——那正是宪法 #5 判死的懒政。
//! V3 的投影由 `v3_server` 在后端单向构造（`v4.6 P3-4`），V2 在 `v4.9` 物理处决。

use serde::{Deserialize, Serialize};

use crate::ids::ContentHash;

/// 会话活动状态（决定前端"正在想 / 等你 / 正在停"的转动指示）。
///
/// 变体集合刻意与 V2 事实层同构（`Idle` / `Running` / `WaitingUser` / `Cancelling`）：
/// 状态机没变，只是表达换成了二进制强类型。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum ActivityState {
    /// 无回合在跑。
    #[default]
    Idle,
    /// 回合执行中（模型在产 token 或工具在跑）。
    Running,
    /// 卡在等用户（审批 / Ask / 用户输入）。
    WaitingUser,
    /// 正在取消，尚未收口。
    Cancelling,
}

/// 回合终态（**终态一旦成立不可回退**；失败携带可诊断的错误码与文案）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum TurnTerminal {
    /// 正常完成。
    Completed,
    /// 失败；`code` 是稳定机读码，`message` 是给人看的文案。
    Failed { code: String, message: String },
    /// 被取消（用户 / 条件），细分原因走 `code`。
    Cancelled { code: String },
    /// 被中断（守护进程重启 / 执行丢失），`code` 标出是哪一类。
    Interrupted { code: String },
}

impl TurnTerminal {
    /// 是否成功终态（唯一"没出事"的终态）。
    pub fn is_success(&self) -> bool {
        matches!(self, Self::Completed)
    }
}

/// token 用量（`None` = 上游没给这份数据，**不是 0**）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub struct UsageInfo {
    /// 输入 token。
    pub input_tokens: u64,
    /// 输出 token。
    pub output_tokens: u64,
    /// 命中前缀缓存的输入 token（上游未报告时 `None`）。
    pub cached_input_tokens: Option<u64>,
    /// 推理 token（上游未报告时 `None`）。
    pub reasoning_tokens: Option<u64>,
}

/// 内容引用：**小内容内联，大内容外置**（`ADR-0042` 铁律"保护前端帧率与内存预算"）。
///
/// 前端只做薄投影：拿到 `Inline` 直接渲染，拿到 `Blob` 去引用的位置按需拉取。
/// 这与 V2 的 `ContentRef` 同精神但不共享类型——V3 的 blob 以 [`ContentHash`]
/// 定址，天然内容寻址、天然去重。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum ContentRef {
    /// 内联全文（小内容；上限由投影方按帧预算把关）。
    Inline { text: String },
    /// 有界尾窗 + 偏移（权威全文另有出处，如 exec 的 append-only 日志）。
    Windowed {
        preview: String,
        start_offset: u64,
        total_bytes: u64,
    },
    /// 内容寻址外置（大输出走 blob 通道，`len` 用于前端预留进度）。
    Blob { hash: ContentHash, len: u64 },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn activity_state_defaults_to_idle() {
        assert_eq!(ActivityState::default(), ActivityState::Idle);
    }

    #[test]
    fn only_completed_counts_as_success() {
        assert!(TurnTerminal::Completed.is_success());
        assert!(
            !TurnTerminal::Failed {
                code: "exit_nonzero".into(),
                message: "boom".into()
            }
            .is_success()
        );
        assert!(
            !TurnTerminal::Cancelled {
                code: "user".into()
            }
            .is_success()
        );
        assert!(
            !TurnTerminal::Interrupted {
                code: "daemon_restart".into()
            }
            .is_success()
        );
    }

    #[test]
    fn absent_usage_stays_absent_instead_of_zero() {
        let usage = UsageInfo {
            input_tokens: 12,
            output_tokens: 34,
            ..UsageInfo::default()
        };
        assert_eq!(usage.cached_input_tokens, None);
        assert_eq!(usage.reasoning_tokens, None);
    }

    #[test]
    fn content_ref_keeps_big_payloads_out_of_the_frame() {
        let ref_ = ContentRef::Blob {
            hash: ContentHash::from_bytes([7u8; 32]),
            len: 200 * 1024 * 1024,
        };
        match ref_ {
            ContentRef::Blob { len, .. } => assert_eq!(len, 200 * 1024 * 1024),
            _ => panic!("必须是外置引用"),
        }
    }
}
