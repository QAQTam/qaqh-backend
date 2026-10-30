//! PTC 的共享词汇：绑定、请求、已解析输入、结果与失败分类。

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// 程序求值形态（与源码是否保留状态无关）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PtcProgramFormat {
    /// 异步函数体：支持顶层 `await` 与 `return`，完成值即返回值。
    FunctionBody,
    /// REPL 语句：支持顶层声明与 `await`，末尾表达式为完成值。
    Repl,
    /// 脚本语句：支持顶层声明与 `await`，末尾表达式为完成值。
    Script,
}

impl PtcProgramFormat {
    /// wire 名称，用于日志、诊断与后端协议。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::FunctionBody => "function-body",
            Self::Repl => "repl",
            Self::Script => "script",
        }
    }
}

/// 运行时语言；决定 SDK 声明渲染器与 `run_code` 的 schema 文案。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PtcLanguage {
    /// TypeScript（可擦除语法子集）。
    TypeScript,
    /// Python。
    Python,
}

impl PtcLanguage {
    /// wire 名称。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::TypeScript => "typescript",
            Self::Python => "python",
        }
    }
}

/// 取消句柄：与 `qaqh-tool-core` 的 `CancellationToken` 共享同一 `Arc<AtomicBool>`。
///
/// 只读方（后端）轮询 [`PtcCancel::is_cancelled`]；置位方（宿主）调用
/// [`PtcCancel::cancel`]。两侧永不互相阻塞。
#[derive(Debug, Clone, Default)]
pub struct PtcCancel {
    inner: Arc<AtomicBool>,
}

impl PtcCancel {
    /// 新建未取消的句柄。
    pub fn new() -> Self {
        Self::default()
    }

    /// 复用已有取消标志（桥接 runtime 的取消树到工具上下文）。
    pub fn from_shared_flag(flag: Arc<AtomicBool>) -> Self {
        Self { inner: flag }
    }

    /// 置位取消（宿主调用）。
    pub fn cancel(&self) {
        self.inner.store(true, Ordering::SeqCst);
    }

    /// 是否已取消（后端在 IO 边界轮询）。
    pub fn is_cancelled(&self) -> bool {
        self.inner.load(Ordering::SeqCst)
    }

    /// 共享标志句柄。
    pub fn shared_flag(&self) -> Arc<AtomicBool> {
        self.inner.clone()
    }
}

/// 一个绑定成员：宿主侧异步可调用函数，参数与返回值必须是无损 JSON。
///
/// 拒绝（`Err`）在程序侧表现为该次调用的 rejection，因此字符串应面向模型可读。
pub type PtcBindingFn = dyn Fn(Value) -> Result<Value, String> + Send + Sync;

/// 命名空间内的一个具名成员。
#[derive(Clone)]
pub struct PtcBindingMember {
    /// 程序调用时使用的名字（可以是任意字符串，包括 `__proto__`）。
    pub name: String,
    /// 宿主实现。
    pub call: Arc<PtcBindingFn>,
}

impl fmt::Debug for PtcBindingMember {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PtcBindingMember")
            .field("name", &self.name)
            .finish_non_exhaustive()
    }
}

impl PtcBindingMember {
    /// 以名字与实现构造一个成员。
    pub fn new(name: impl Into<String>, call: Arc<PtcBindingFn>) -> Self {
        Self {
            name: name.into(),
            call,
        }
    }
}

/// 程序可见的类型化拒绝构造器（如 `ToolCallError`）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PtcBindingErrorClass {
    /// 构造器全局名，同时作为 `error.name`。
    pub name: String,
    /// 保存失败成员名的自有属性名（如 `toolName`）。
    pub member_name_property: String,
}

/// 一个命名空间：程序里的一个全局对象（如 `tools`）。
#[derive(Clone)]
pub struct PtcBindingNamespace {
    /// 程序可见的全局名，必须是可移植标识符。
    pub global: String,
    /// 可调用成员。
    pub functions: Vec<PtcBindingMember>,
    /// 可选类型化拒绝契约。
    pub error_class: Option<PtcBindingErrorClass>,
}

impl fmt::Debug for PtcBindingNamespace {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("PtcBindingNamespace")
            .field("global", &self.global)
            .field(
                "functions",
                &self
                    .functions
                    .iter()
                    .map(|m| m.name.as_str())
                    .collect::<Vec<_>>(),
            )
            .field("error_class", &self.error_class)
            .finish()
    }
}

impl PtcBindingNamespace {
    /// 以全局名开始构造。
    pub fn new(global: impl Into<String>) -> Self {
        Self {
            global: global.into(),
            functions: Vec::new(),
            error_class: None,
        }
    }

    /// 追加一个成员。
    #[must_use]
    pub fn with_member(mut self, name: impl Into<String>, call: Arc<PtcBindingFn>) -> Self {
        self.functions.push(PtcBindingMember::new(name, call));
        self
    }

    /// 声明程序可见的类型化拒绝构造器。
    #[must_use]
    pub fn with_error_class(
        mut self,
        name: impl Into<String>,
        member_name_property: impl Into<String>,
    ) -> Self {
        self.error_class = Some(PtcBindingErrorClass {
            name: name.into(),
            member_name_property: member_name_property.into(),
        });
        self
    }
}

/// 调用方输入的一次程序执行。
#[derive(Debug, Clone)]
pub struct PtcRunRequest {
    /// 程序源码。
    pub program: String,
    /// 程序可见的命名空间。
    pub bindings: Vec<PtcBindingNamespace>,
    /// 执行世界中的工作目录；缺省为 runtime 的默认目录。
    pub cwd: Option<PathBuf>,
    /// 经过时间预算；`None` 用后端默认值。
    pub timeout: Option<Duration>,
    /// 日志与完成值的合并字节上限；`None` 用后端默认值。
    pub max_output_bytes: Option<usize>,
    /// 取消句柄。
    pub cancel: PtcCancel,
}

/// 已解析执行输入：目录、预算都已定稿，后端不再补默认值。
#[derive(Debug, Clone)]
pub struct PtcRunSpec {
    /// 程序源码。
    pub program: String,
    /// 程序可见的命名空间。
    pub bindings: Vec<PtcBindingNamespace>,
    /// 绝对工作目录。
    pub cwd: PathBuf,
    /// 正数经过时间预算。
    pub timeout: Duration,
    /// 合并输出上限。
    pub max_output_bytes: usize,
    /// 取消句柄。
    pub cancel: PtcCancel,
}

/// 失败分类；彼此正交，独立报告。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PtcFailureKind {
    /// 程序抛出或解析/转换失败。
    Exception,
    /// 经过时间预算耗尽。
    Timeout,
    /// 调用方取消。
    Abort,
    /// 执行基底（子进程）未结算即死亡。
    WorkerExit,
    /// 完成值不是无损 JSON。
    InvalidOutput,
    /// 日志/完成值的合并载荷超过上限。
    OutputLimit,
    /// 程序侧发来了非法控制帧或超限流量。
    Protocol,
}

impl PtcFailureKind {
    /// wire 名称。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Exception => "exception",
            Self::Timeout => "timeout",
            Self::Abort => "abort",
            Self::WorkerExit => "worker-exit",
            Self::InvalidOutput => "invalid-output",
            Self::OutputLimit => "output-limit",
            Self::Protocol => "protocol",
        }
    }
}

/// 一次失败：分类 + 面向模型可读的细节。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PtcRunFailure {
    /// 分类。
    pub kind: PtcFailureKind,
    /// 细节文本，适合回灌模型自纠。
    pub message: String,
}

impl PtcRunFailure {
    /// 构造一次失败。
    pub fn new(kind: PtcFailureKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            message: message.into(),
        }
    }
}

impl fmt::Display for PtcRunFailure {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.kind.as_str(), self.message)
    }
}

/// 文件策略模式（沙箱事实）。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum PtcSandboxMode {
    /// 只读。
    ReadOnly,
    /// 工作区可写。
    WorkspaceWrite,
    /// 完全访问。
    DangerFullAccess,
}

/// 一次执行适用的沙箱事实；与实际结果独立报告。
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct PtcRunSandbox {
    /// 生效模式。
    pub mode: PtcSandboxMode,
    /// 是否观察到拒绝（不是强制证明）。
    pub denied: bool,
}

/// 一次执行的结果：日志、完成值、失败与沙箱事实。
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PtcRunResult {
    /// 捕获的文本输出，保持发出顺序。
    pub logs: Vec<String>,
    /// 完成值（无损 JSON）；失败或无值时为 `None`。
    pub value: Option<Value>,
    /// 仅在失败时存在。
    pub failure: Option<PtcRunFailure>,
    /// 沙箱事实；后端不支持时为 `None`。
    pub sandbox: Option<PtcRunSandbox>,
}

impl PtcRunResult {
    /// 成功结果。
    pub fn success(logs: Vec<String>, value: Option<Value>) -> Self {
        Self {
            logs,
            value,
            failure: None,
            sandbox: None,
        }
    }

    /// 失败结果。
    pub fn failed(logs: Vec<String>, failure: PtcRunFailure) -> Self {
        Self {
            logs,
            value: None,
            failure: Some(failure),
            sandbox: None,
        }
    }

    /// 是否成功（无失败字段）。
    pub fn is_ok(&self) -> bool {
        self.failure.is_none()
    }
}

/// 调用方误用或后端无法启动；程序级失败走 [`PtcRunResult::failure`]。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum PtcError {
    /// 请求无法解析（相对目录、非法绑定名、非正数预算等）。
    Invalid(String),
    /// 后端不可用（可执行文件缺失、spawn 失败、bootstrap 无法落盘）。
    Unavailable(String),
}

impl fmt::Display for PtcError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Invalid(message) => write!(f, "invalid PTC request: {message}"),
            Self::Unavailable(message) => write!(f, "PTC runtime unavailable: {message}"),
        }
    }
}

impl std::error::Error for PtcError {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn failure_kinds_round_trip_their_wire_names() {
        for kind in [
            PtcFailureKind::Exception,
            PtcFailureKind::Timeout,
            PtcFailureKind::Abort,
            PtcFailureKind::WorkerExit,
            PtcFailureKind::InvalidOutput,
            PtcFailureKind::OutputLimit,
            PtcFailureKind::Protocol,
        ] {
            let json = serde_json::to_string(&kind).expect("serialize");
            assert_eq!(json, format!("\"{}\"", kind.as_str()));
            let back: PtcFailureKind = serde_json::from_str(&json).expect("deserialize");
            assert_eq!(back, kind);
        }
    }

    #[test]
    fn cancel_token_shares_one_flag() {
        let token = PtcCancel::new();
        let clone = token.clone();
        assert!(!clone.is_cancelled());
        token.cancel();
        assert!(clone.is_cancelled());
    }

    #[test]
    fn namespace_builder_keeps_member_order_and_error_class() {
        let namespace = PtcBindingNamespace::new("tools")
            .with_member("read", Arc::new(|_args| Ok(Value::Null)))
            .with_member("grep", Arc::new(|_args| Ok(Value::Null)))
            .with_error_class("ToolCallError", "toolName");
        assert_eq!(namespace.global, "tools");
        assert_eq!(
            namespace
                .functions
                .iter()
                .map(|member| member.name.as_str())
                .collect::<Vec<_>>(),
            vec!["read", "grep"]
        );
        assert_eq!(
            namespace.error_class,
            Some(PtcBindingErrorClass {
                name: "ToolCallError".to_owned(),
                member_name_property: "toolName".to_owned(),
            })
        );
    }
}
