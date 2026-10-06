//! Canonical 参数字段名与共享参数类型（09-19 补充稿 §5）。
//!
//! schema、typed 反序列化与展示投影共用同一拼写；禁止各工具自造同义字段名
//! （对齐 grok `tool_taxonomy::field`）。

use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

/// Canonical 字段名常量：emit canonical keys through these。
pub mod field {
    /// 文件路径。
    pub const PATH: &str = "path";
    /// 分页偏移。
    pub const OFFSET: &str = "offset";
    /// 分页上限。
    pub const LIMIT: &str = "limit";
    /// 命令文本。
    pub const COMMAND: &str = "command";
    /// 工作目录。
    pub const CWD: &str = "cwd";
    /// 匹配模式。
    pub const PATTERN: &str = "pattern";
    /// 人类可读描述。
    pub const DESCRIPTION: &str = "description";
    /// 超时（秒）。
    pub const TIMEOUT_SECS: &str = "timeout_secs";
}

/// workspace 相对路径参数（规范化由执行器完成）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct PathArg(pub String);

/// 分页窗口（read / grep 等共享）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize, JsonSchema)]
pub struct OffsetLimit {
    /// 起始偏移（缺省 = 0）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub offset: Option<u64>,
    /// 返回上限。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<u64>,
}

/// 命令执行参数（exec / process 共享）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
pub struct CommandArgs {
    /// 命令文本。
    pub command: String,
    /// 工作目录（缺省 = workspace_root）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cwd: Option<String>,
    /// 超时（秒；缺省 = descriptor.default_timeout）。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub timeout_secs: Option<u64>,
}

/// 匹配模式参数（glob / grep 共享）。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize, JsonSchema)]
#[serde(transparent)]
pub struct PatternArg(pub String);

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn field_constants_are_canonical() {
        assert_eq!(field::PATH, "path");
        assert_eq!(field::TIMEOUT_SECS, "timeout_secs");
    }

    #[test]
    fn shared_args_roundtrip_and_generate_schema() {
        let args = CommandArgs {
            command: "ls".to_owned(),
            cwd: None,
            timeout_secs: Some(30),
        };
        let json = serde_json::to_value(&args).expect("序列化");
        assert_eq!(json["command"], "ls");
        assert_eq!(json["timeout_secs"], 30);
        assert!(json.get("cwd").is_none(), "None 字段不序列化");

        let schema = serde_json::to_value(schemars::schema_for!(CommandArgs)).expect("schema");
        assert_eq!(schema["type"], "object");
        assert!(schema["properties"]["command"].is_object());
    }

    #[test]
    fn path_and_pattern_are_transparent() {
        let path: PathArg = serde_json::from_value(serde_json::json!("src/main.rs")).expect("解析");
        assert_eq!(path.0, "src/main.rs");
        let pattern: PatternArg = serde_json::from_value(serde_json::json!("*.rs")).expect("解析");
        assert_eq!(pattern.0, "*.rs");
    }
}
