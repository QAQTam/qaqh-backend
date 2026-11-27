//! 强类型 ID newtype（`ADR-0042` 铁律 1）。
//!
//! ## 为什么值得为"一个字符串"造七个类型
//!
//! V2 时代所有 id 都是 `String`：`session_id` 与 `turn_id` 可以互换写进任何字段，
//! 编译器一句话不说，只有线上跑飞了才发现把 turn 的 id 塞进了 `exec_session_id`。
//! 七个 newtype 把这类错误从运行期挪到编译期；线缆形态仍是裸字符串
//! （`serde(transparent)`），前端零适配。
//!
//! ## 构造即校验（Fail-Closed）
//!
//! id 同时是**线缆键**与**磁盘路径组件**（`sessions/<session_id>/exec/<id>.log`），
//! 所以校验必须挡住路径穿越与不可见字符，而不是"看起来不对就算了"：
//! 空串、超长、控制字符、路径分隔符、`.` / `..` 一律 [`IdError`]。
//! 校验只做**结构**判断，不发明语法（不做形如 `SES-` 前缀的臆断——那是各 id
//! 生成器的自由）。

use std::fmt;

use serde::{Deserialize, Serialize};

/// id 长度上限（线缆 + 路径双重考虑：主流文件系统单段上限 255 字节）。
pub const MAX_ID_LEN: usize = 128;

/// id 构造失败原因。
#[derive(Debug, Clone, PartialEq, Eq, thiserror::Error)]
pub enum IdError {
    #[error("{kind} 不能为空")]
    Empty { kind: &'static str },
    #[error("{kind} 超过 id 长度上限（实际 {len} 字节）")]
    TooLong { kind: &'static str, len: usize },
    #[error("{kind} 含控制字符（U+{codepoint:04X}）")]
    ControlChar { kind: &'static str, codepoint: u32 },
    #[error("{kind} 不得含路径分隔符")]
    PathSeparator { kind: &'static str },
    #[error("{kind} 不得为 `.` 或 `..`")]
    PathComponent { kind: &'static str },
    #[error("ContentHash 必须是小写 hex 的 64 个字符（实际 {len}）")]
    ContentHashLength { len: usize },
    #[error("ContentHash 含非 hex 字符")]
    ContentHashChar,
}

fn validate_id(kind: &'static str, raw: &str) -> Result<(), IdError> {
    if raw.is_empty() {
        return Err(IdError::Empty { kind });
    }
    if raw.len() > MAX_ID_LEN {
        return Err(IdError::TooLong {
            kind,
            len: raw.len(),
        });
    }
    if raw == "." || raw == ".." {
        return Err(IdError::PathComponent { kind });
    }
    if raw.contains('/') || raw.contains('\\') {
        return Err(IdError::PathSeparator { kind });
    }
    for character in raw.chars() {
        if character.is_control() {
            return Err(IdError::ControlChar {
                kind,
                codepoint: character as u32,
            });
        }
    }
    Ok(())
}

macro_rules! string_id {
    ($(#[$meta:meta])* $name:ident, $kind:literal) => {
        $(#[$meta])*
        #[derive(Clone, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize)]
        #[serde(transparent)]
        pub struct $name(String);

        impl<'de> Deserialize<'de> for $name {
            fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
            where
                D: serde::Deserializer<'de>,
            {
                let raw = String::deserialize(deserializer)?;
                Self::new(raw).map_err(serde::de::Error::custom)
            }
        }

        impl $name {
            /// 构造并校验（结构校验，见模块文档）。
            pub fn new(raw: impl Into<String>) -> Result<Self, IdError> {
                let raw = raw.into();
                validate_id($kind, &raw)?;
                Ok(Self(raw))
            }

            /// 扁平字符串视图。
            pub fn as_str(&self) -> &str {
                &self.0
            }
        }

        impl fmt::Display for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                formatter.write_str(&self.0)
            }
        }

        impl fmt::Debug for $name {
            fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(formatter, concat!($kind, "({})"), self.0)
            }
        }
    };
}

string_id!(
    /// 会话 id（根会话与同事子会话同一命名空间）。
    SessionId,
    "SessionId"
);
string_id!(
    /// 回合 id。
    TurnId,
    "TurnId"
);
string_id!(
    /// 时间线块 id。
    BlockId,
    "BlockId"
);
string_id!(
    /// 工具调用 id（恰好一次语义的定序键）。
    ToolCallId,
    "ToolCallId"
);
string_id!(
    /// 用户输入 id。
    InputId,
    "InputId"
);
string_id!(
    /// 交互（审批 / Ask）id。
    InteractionId,
    "InteractionId"
);
string_id!(
    /// 执行会话 id（`execs_*`；同时是权威日志文件名）。
    ExecSessionId,
    "ExecSessionId"
);
string_id!(
    /// 工作区 id（会话分组根键）。
    WorkspaceId,
    "WorkspaceId"
);

/// 内容哈希（sha256，线上 32 字节裸字节，不是 64 字符 hex）。
///
/// V2 把哈希当 hex 字符串在 wire 上搬，一字节的摘要要花 64 字节；V3 是二进制
/// 协议，直接付 32 字节。hex 只在**人类面**（日志/DB/HTTP 兼容读）出现，
/// 由 [`ContentHash::from_hex`] / [`ContentHash::to_hex`] 显式转换。
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(transparent)]
pub struct ContentHash([u8; 32]);

impl ContentHash {
    /// 从 32 字节摘要构造。
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Self(bytes)
    }

    /// 裸字节视图。
    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }

    /// 解析小写或大写 hex（恰好 64 个字符，大小写混写也接受——同一摘要不因
    /// 书写风格分家；长度或字符不合法一律 [`IdError`]，不做修补）。
    pub fn from_hex(raw: &str) -> Result<Self, IdError> {
        let bytes = hex::decode(raw).map_err(|error| match error {
            hex::FromHexError::InvalidStringLength | hex::FromHexError::OddLength => {
                IdError::ContentHashLength { len: raw.len() }
            }
            _ => IdError::ContentHashChar,
        })?;
        let array: [u8; 32] = bytes
            .try_into()
            .map_err(|_| IdError::ContentHashLength { len: raw.len() })?;
        Ok(Self(array))
    }

    /// 小写 hex 视图。
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }
}

impl fmt::Display for ContentHash {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(&hex::encode(self.0))
    }
}

impl fmt::Debug for ContentHash {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(formatter, "ContentHash({})", hex::encode(self.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn well_formed_ids_round_trip_through_the_string_view() {
        let id = SessionId::new("SES-01M8CJZ6RV4A70EV0V0ABFXV2V").expect("valid");
        assert_eq!(id.as_str(), "SES-01M8CJZ6RV4A70EV0V0ABFXV2V");
        assert_eq!(id.to_string(), "SES-01M8CJZ6RV4A70EV0V0ABFXV2V");
        assert_eq!(format!("{id:?}"), "SessionId(SES-01M8CJZ6RV4A70EV0V0ABFXV2V)");
    }

    #[test]
    fn ids_are_distinct_types_not_interchangeable_strings() {
        // 编译期反证：把 TurnId 塞进 SessionId 参数位必须编译失败，所以这里
        // 只能"同值不同型"地验证语义边界确实存在。
        let raw = "01H8ZZZZZZZZZZZZZZZZZZZZZZ";
        let turn = TurnId::new(raw).expect("valid");
        let session = SessionId::new(raw).expect("valid");
        assert_eq!(turn.as_str(), session.as_str());
        assert_eq!(format!("{turn:?}"), "TurnId(01H8ZZZZZZZZZZZZZZZZZZZZZZ)");
    }

    #[test]
    fn structural_validation_blocks_path_traversal_and_invisible_junk() {
        assert_eq!(SessionId::new(""), Err(IdError::Empty { kind: "SessionId" }));
        assert_eq!(
            SessionId::new(".."),
            Err(IdError::PathComponent { kind: "SessionId" })
        );
        assert_eq!(
            SessionId::new("a/b"),
            Err(IdError::PathSeparator { kind: "SessionId" })
        );
        assert_eq!(
            SessionId::new("a\\b"),
            Err(IdError::PathSeparator { kind: "SessionId" })
        );
        assert_eq!(
            SessionId::new("a\u{0}b"),
            Err(IdError::ControlChar {
                kind: "SessionId",
                codepoint: 0
            })
        );
        assert_eq!(
            ExecSessionId::new("x".repeat(MAX_ID_LEN + 1)),
            Err(IdError::TooLong {
                kind: "ExecSessionId",
                len: MAX_ID_LEN + 1
            })
        );
        assert!(ExecSessionId::new("x".repeat(MAX_ID_LEN)).is_ok());
    }

    #[test]
    fn deserializing_malformed_id_fails_closed() {
        use serde::de::IntoDeserializer;
        use serde::de::value::Error as DeError;

        let ok: Result<SessionId, DeError> =
            SessionId::deserialize("SES-01M8CJZ6RV4A70EV0V0ABFXV2V".into_deserializer());
        assert_eq!(
            ok.expect("合法 id 反序列化必须通过").as_str(),
            "SES-01M8CJZ6RV4A70EV0V0ABFXV2V"
        );

        let too_long = "x".repeat(MAX_ID_LEN + 1);
        for bad in [
            "",
            ".",
            "..",
            "../etc/passwd",
            "a/b",
            "a\\b",
            "a\u{0}b",
            too_long.as_str(),
        ] {
            let err: Result<SessionId, DeError> = SessionId::deserialize(bad.into_deserializer());
            assert!(err.is_err(), "反序列化非法 id `{bad:?}` 必须被拒");
            let exec_err: Result<ExecSessionId, DeError> =
                ExecSessionId::deserialize(bad.into_deserializer());
            assert!(exec_err.is_err(), "反序列化非法 ExecSessionId `{bad:?}` 必须被拒");
        }
    }

    #[test]
    fn content_hash_round_trips_between_bytes_and_hex() {
        let hex_text = "0123456789abcdef0123456789abcdef0123456789abcdef0123456789abcdef";
        let hash = ContentHash::from_hex(hex_text).expect("valid hex");
        assert_eq!(hash.to_hex(), hex_text);
        assert_eq!(ContentHash::from_bytes(*hash.as_bytes()), hash);
        // 大小写混写视为同一摘要。
        let upper = hex_text.to_ascii_uppercase();
        assert_eq!(ContentHash::from_hex(&upper).expect("valid hex"), hash);
        assert_eq!(
            ContentHash::from_hex("abc"),
            Err(IdError::ContentHashLength { len: 3 })
        );
        assert_eq!(
            ContentHash::from_hex(&"z".repeat(64)),
            Err(IdError::ContentHashChar)
        );
    }
}
