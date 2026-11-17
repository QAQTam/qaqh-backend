//! 用户 shell 终端的 ws 数据面帧型（ADR-0028 §D3 裁定 = 候选 B）。
//!
//! # 词表钉死在这里：4 个控制帧名，没有第五个
//!
//! | 方向 | 帧 | 载荷 |
//! |---|---|---|
//! | 上行 | `attach` | `from_offset?` |
//! | 上行 | `stdin` | `bytes`（base64 的原始 PTY 输入） |
//! | 上行 | `resize` | `cols`, `rows` |
//! | 下行 | `closed` | `reason` |
//!
//! 除这些 text 帧之外，下行只有一种东西：**裸字节**（PTY 输出，不套封包）。
//! 刻意**没有** `seq`、没有 ack 帧、没有 `resync_required`：这条流的恢复语义与
//! 会话事件流是**两套**（会话那套是 `epoch:seq` + 回放尾 + resync），shell 不存
//! 历史，断线即重新 `attach`，取 `ShellRegistry` 的有界环（256KiB）—— 这正是
//! ADR-0017 §被否选项第 4 条警告「第二套游标/去重/重连语义 = 平行协议」的解法：
//! 不发明第二套游标，只把 offset 这一个数交给客户端自己留着。
//!
//! # 为什么上行没有「裸字节 = 打字」这条路
//!
//! 一条流上同一个动作有两种写法（text `stdin` 或 binary 裸字节）就是两份事实源，
//! 前端与后端会各自实现一种。上行只有 text 一种形状，`bytes` 用 base64 承载任意
//! 字节（粘贴里的 `\x1b[200~` 之类不会被 UTF-8 假设吃掉）。

use serde::{Deserialize, Serialize};
#[cfg(feature = "ts")]
use ts_rs::TS;

/// `WS /ringing/v2/shells/{shell_id}/stream` 的路径（供客户端/探针单点取用）。
pub fn shell_stream_path(shell_id: &str) -> String {
    format!("/ringing/v2/shells/{shell_id}/stream")
}

/// 上行控制帧。**新增变体 = 扩这条流的词表**，必须先过 ADR-0028 §D3 的硬约束
/// （不许引入第二套游标/回执/重连语义），不是随手加一个 if 分支。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub enum ShellStreamControl {
    /// 重新订阅：回放 `from_offset` 之后的有界缓冲尾，然后续实时流。
    /// 缺省 = 从头给（整段缓冲尾，上限 `SHELL_REPLAY_CAP`）。
    Attach {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        #[cfg_attr(feature = "ts", ts(optional))]
        from_offset: Option<u64>,
    },
    /// 原始 PTY 输入字节，base64。粘贴是一次大帧，keystroke 是一帧几个字节。
    Stdin { bytes: String },
    /// UI 测量的几何。真值不在这条流上回执（没有 ack 帧）：读 `shell.list`。
    Resize { cols: u16, rows: u16 },
}

/// 下行控制帧：只有「这条会话已经终止」这一种事实，原因就是
/// `qaqh_runtime::ShellStopReason::as_str()` 的口径（单点生成，两处不许各写一份）。
///
/// 终止之后服务端关闭连接。**断联本身不是原因**（ADR-0028 §D5：断联不 kill）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
#[cfg_attr(feature = "ts", derive(TS), ts(export, export_to = "qaqh/"))]
pub enum ShellStreamEvent {
    Closed { reason: String },
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 词表闸：这条流上只有 4 个控制帧名。多一个名字就必须同时改这张表，
    /// 而改这张表是 ADR-0028 §D3 的裁定事项，不是实现细节。
    const CONTROL_VOCABULARY: [&str; 4] = ["attach", "stdin", "resize", "closed"];

    fn type_name(raw: &str) -> String {
        serde_json::from_str::<serde_json::Value>(raw)
            .expect("sample frame must be valid JSON")
            .get("type")
            .and_then(serde_json::Value::as_str)
            .unwrap_or_default()
            .to_owned()
    }

    #[test]
    fn control_frames_parse_only_the_pinned_vocabulary() {
        let attach: ShellStreamControl = serde_json::from_str(r#"{"type":"attach"}"#).unwrap();
        assert_eq!(attach, ShellStreamControl::Attach { from_offset: None });
        let attach_offset: ShellStreamControl =
            serde_json::from_str(r#"{"type":"attach","from_offset":4096}"#).unwrap();
        assert_eq!(attach_offset, ShellStreamControl::Attach { from_offset: Some(4096) });
        let stdin: ShellStreamControl =
            serde_json::from_str(r#"{"type":"stdin","bytes":"ZXhpdA0K"}"#).unwrap();
        assert_eq!(stdin, ShellStreamControl::Stdin { bytes: "ZXhpdA0K".into() });
        let resize: ShellStreamControl =
            serde_json::from_str(r#"{"type":"resize","cols":132,"rows":44}"#).unwrap();
        assert_eq!(resize, ShellStreamControl::Resize { cols: 132, rows: 44 });

        // 5 年内会长成平行协议的那些名字，当场拒：回执、游标、resync、心跳、二进制封包。
        for ghost in [
            r#"{"type":"ack","seq":1}"#,
            r#"{"type":"resync_required"}"#,
            r#"{"type":"cursor","seq":7}"#,
            r#"{"type":"ping"}"#,
            r#"{"type":"frame","bytes":"AAA"}"#,
        ] {
            let error = serde_json::from_str::<ShellStreamControl>(ghost)
                .expect_err("非词表内的控制帧必须被拒: {ghost}");
            // 拒绝理由要把词表报出来：客户端猜错帧名时，错误文案就是它的说明书。
            let message = error.to_string();
            for expected in ["attach", "stdin", "resize"] {
                assert!(
                    message.contains(expected),
                    "未知帧 {ghost} 的拒绝理由没报出词表里的 {expected}: {message}"
                );
            }
        }
    }

    #[test]
    fn downlink_control_is_only_the_terminal_fact() {
        let raw = serde_json::to_string(&ShellStreamEvent::Closed {
            reason: "child_exited".to_owned(),
        })
        .unwrap();
        assert_eq!(raw, r#"{"type":"closed","reason":"child_exited"}"#);
        assert!(serde_json::from_str::<ShellStreamEvent>(r#"{"type":"output","bytes":"AA"}"#).is_err());
    }

    #[test]
    fn vocabulary_table_matches_the_types_that_parse() {
        let names = [
            type_name(r#"{"type":"attach"}"#),
            type_name(r#"{"type":"stdin","bytes":"AA"}"#),
            type_name(r#"{"type":"resize","cols":80,"rows":24}"#),
            type_name(r#"{"type":"closed","reason":"closed"}"#),
        ];
        assert_eq!(names, CONTROL_VOCABULARY, "帧词表漂移：ADR-0028 §D3 只批了这 4 个名字");
    }

    #[test]
    fn stream_path_is_the_single_source_of_the_route_shape() {
        assert_eq!(shell_stream_path("shell_01"), "/ringing/v2/shells/shell_01/stream");
    }
}
