//! 控制帧定义（NDJSON，一行一帧）。
//!
//! 宿主 → 子进程：`run` / `result` / `shutdown`。
//! 子进程 → 宿主：`ready` / `log` / `call` / `done` / `limit`。
//!
//! 帧是**不可信输入**：子进程里的程序可以伪造任意行，因此解析失败、字段缺失或
//! 超长都必须变成结构化失败，而不是 panic。

use serde::Deserialize;
use serde::Serialize;
use serde_json::Value;

/// 宿主发给子进程的帧。
#[derive(Debug, Serialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub(crate) enum HostFrame {
    /// 一次执行：程序源码 + 注入的命名空间声明。
    Run {
        /// 程序源码（异步函数体）。
        code: String,
        /// 注入的命名空间声明。
        namespaces: Vec<NamespaceDecl>,
        /// 合并输出上限，子进程据此提前停止捕获。
        #[serde(rename = "maxOutputBytes")]
        max_output_bytes: usize,
    },
    /// 一次绑定调用的结果。
    Result {
        /// 对应 `call` 帧的 id。
        id: u64,
        /// 是否成功。
        ok: bool,
        /// 成功时的无损 JSON 值。
        #[serde(skip_serializing_if = "Option::is_none")]
        value: Option<Value>,
        /// 失败时的面向模型文本。
        #[serde(skip_serializing_if = "Option::is_none")]
        error: Option<String>,
    },
}

/// 一个命名空间声明。
#[derive(Debug, Serialize)]
pub(crate) struct NamespaceDecl {
    /// 程序可见的全局名。
    pub global: String,
    /// 可调用成员名（任意字符串，程序用引号访问）。
    pub names: Vec<String>,
    /// 可选的类型化拒绝构造器。
    #[serde(rename = "errorClass", skip_serializing_if = "Option::is_none")]
    pub error_class: Option<ErrorClassDecl>,
}

/// 类型化拒绝构造器声明。
#[derive(Debug, Serialize)]
pub(crate) struct ErrorClassDecl {
    /// 构造器全局名。
    pub name: String,
    /// 保存失败成员名的属性名。
    #[serde(rename = "memberNameProperty")]
    pub member_name_property: String,
}

/// 子进程发来的帧。
#[derive(Debug, Deserialize)]
#[serde(tag = "t", rename_all = "snake_case")]
pub(crate) enum ChildFrame {
    /// runner 已装好输出补丁并开始读 stdin。
    Ready,
    /// 捕获的一段程序输出。
    Log {
        /// 文本。
        text: String,
    },
    /// 一次宿主绑定调用。
    Call {
        /// 调用 id，回复时原样带回。
        id: u64,
        /// 命名空间全局名。
        #[serde(default)]
        global: String,
        /// 成员名。
        name: String,
        /// 无损 JSON 参数。
        #[serde(default)]
        args: Value,
    },
    /// 程序结算。
    Done {
        /// 完成值；`undefined` 与 `null` 必须区分，因此保留「字段存在」语义。
        #[serde(default, deserialize_with = "some_if_present")]
        value: Option<Value>,
        /// 程序级失败。
        #[serde(default)]
        error: Option<ChildError>,
    },
    /// 子进程侧输出预算耗尽。
    Limit,
}

/// 子进程报告的程序级失败。
#[derive(Debug, Deserialize, Clone)]
pub(crate) struct ChildError {
    /// `exception` / `invalid-output` / `output-limit` / `protocol`。
    pub kind: String,
    /// 可读细节。
    pub message: String,
}

/// 区分「字段缺失」与「字段是 null」：`undefined` 不携带 value，而 `null` 是合法完成值。
fn some_if_present<'de, D>(deserializer: D) -> Result<Option<Value>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Value::deserialize(deserializer).map(Some)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run_frame_uses_camel_case_wire_fields() {
        let frame = HostFrame::Run {
            code: "return 1".to_owned(),
            namespaces: vec![NamespaceDecl {
                global: "tools".to_owned(),
                names: vec!["read".to_owned()],
                error_class: Some(ErrorClassDecl {
                    name: "ToolCallError".to_owned(),
                    member_name_property: "toolName".to_owned(),
                }),
            }],
            max_output_bytes: 1024,
        };
        let json = serde_json::to_value(&frame).expect("serialize");
        assert_eq!(json["t"], "run");
        assert_eq!(json["maxOutputBytes"], 1024);
        assert_eq!(
            json["namespaces"][0]["errorClass"]["memberNameProperty"],
            "toolName"
        );
    }

    #[test]
    fn done_frame_distinguishes_absent_value_from_null() {
        let absent: ChildFrame = serde_json::from_str(r#"{"t":"done"}"#).expect("absent value");
        let null: ChildFrame =
            serde_json::from_str(r#"{"t":"done","value":null}"#).expect("null value");
        match (absent, null) {
            (
                ChildFrame::Done {
                    value: None,
                    error: None,
                },
                ChildFrame::Done {
                    value: Some(Value::Null),
                    error: None,
                },
            ) => {}
            other => panic!("unexpected frames: {other:?}"),
        }
    }

    #[test]
    fn unknown_frames_and_missing_fields_are_rejected_not_panicking() {
        assert!(serde_json::from_str::<ChildFrame>(r#"{"t":"nope"}"#).is_err());
        assert!(serde_json::from_str::<ChildFrame>(r#"{"t":"call"}"#).is_err());
    }
}
