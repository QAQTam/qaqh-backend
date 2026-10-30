//! 后端实现的唯一入口。

use crate::types::{
    PtcError, PtcLanguage, PtcProgramFormat, PtcRunRequest, PtcRunResult, PtcRunSpec,
};

/// 一个 PTC 执行后端。
///
/// 实现者把程序放到自己的执行基底上（子进程、解释器、容器），把具名绑定桥接
/// 成异步可调用函数，并返回捕获输出与结果。运行时**不得**解释工具语义，也
/// **不得**在 `run` 内重放失败程序。
pub trait PtcRuntime: Send + Sync {
    /// 程序语言，决定 SDK 声明渲染器。
    fn language(&self) -> PtcLanguage;

    /// 程序求值形态。
    fn program_format(&self) -> PtcProgramFormat;

    /// 由后端拥有的、面向模型的使用说明（拼进 `run_code` 的描述）。
    fn execution_instructions(&self) -> &str {
        ""
    }

    /// 校验并补全执行输入。
    ///
    /// 不支持的选项必须在此显式失败，而不是在 `run` 里静默忽略。
    fn resolve(&self, request: PtcRunRequest) -> Result<PtcRunSpec, PtcError>;

    /// 执行已解析输入。
    ///
    /// 程序级失败通过 [`PtcRunResult::failure`] 返回；只有调用方误用或基底无法
    /// 启动才返回 [`PtcError`]。方法必须是同步的：宿主在阻塞线程上调用它，并
    /// 用取消句柄表达取消。
    fn run(&self, spec: PtcRunSpec) -> Result<PtcRunResult, PtcError>;
}
