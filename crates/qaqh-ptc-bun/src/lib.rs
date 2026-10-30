//! Bun PTC provider。
//!
//! 每次 [`BunPtcRuntime::run`] 起一个**全新**的 bun 进程，用 NDJSON 控制通道
//! （stdin/stdout）承载：程序输出、宿主绑定调用与完成值。程序的 `console` 与
//! `process.stdout/stderr.write` 都在 runner 里被折叠成 `log` 帧，因此控制通道
//! 不会被程序输出污染。
//!
//! 设计边界：
//!
//! - 只用 bun，不引入 node 兼容层；runner 是纯 TS，不需要构建步骤。
//! - 进程由宿主杀停（超时/取消/输出超限）；runner 不负责自证副作用已停止。
//! - 绑定调用是串行的：一次程序执行最多一个在途绑定（P0 取舍），因此不存在
//!   「子调用并发 + 审批」的歧义。
//!
//! 已知限制（下一轮收敛）：
//!
//! - 控制通道与程序输出共用 stdout，依赖 runner 的输出补丁；绕开补丁的原生写入
//!   会污染通道（Unix 上应升级为独立 fd）。
//! - 沙箱尚未接线：宿主应通过 [`BunPtcRuntime::with_spawn_hook`] 传入
//!   `qaqh_sandbox::wrap_command`。
//! - 无持久状态（每次全新进程），跨 `run_code` 的内存保留属于后续阶段。

#![deny(missing_docs)]

mod protocol;
mod runner;

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use qaqh_ptc_core::{
    PtcError, PtcLanguage, PtcProgramFormat, PtcRunRequest, PtcRunResult, PtcRunSpec, PtcRuntime,
};

pub use runner::{BunConfig, SpawnHook};

/// bun 后端默认的面向模型使用说明。
const EXECUTION_INSTRUCTIONS: &str = "The program is the body of an async TypeScript function executed by bun; \
only erasable syntax is accepted (no enum or namespaces), type annotations are advisory. \
Top-level await and return are supported. Emit output with console.log(...) and/or return a \
lossless JSON value; only printed values and the returned value become program output. Each \
call runs in a fresh process. Direct filesystem and process effects follow the execution's \
sandbox policy.";

/// 一个 bun 执行后端。
///
/// 通过 [`BunPtcRuntime::new`] 构造；服务注册由宿主决定（见 crate 文档）。
pub struct BunPtcRuntime {
    config: BunConfig,
    spawn_hook: Option<Arc<SpawnHook>>,
}

impl BunPtcRuntime {
    /// 以给定配置构造。
    pub fn new(config: BunConfig) -> Self {
        Self {
            config,
            spawn_hook: None,
        }
    }

    /// 注入 spawn 前的命令改写（沙箱包装、环境裁剪、可执行文件重定位）。
    ///
    /// 钩子在 `Command` 已设置 argv/cwd/env 之后、`spawn` 之前调用；返回 `Err`
    /// 表示本次执行无法建立受约束的基底，调用方得到 [`PtcError::Unavailable`]。
    #[must_use]
    pub fn with_spawn_hook(mut self, hook: Arc<SpawnHook>) -> Self {
        self.spawn_hook = Some(hook);
        self
    }

    /// 当前生效配置。
    pub fn config(&self) -> &BunConfig {
        &self.config
    }

    /// bun 可执行文件路径。
    pub fn executable(&self) -> &PathBuf {
        &self.config.executable
    }
}

impl PtcRuntime for BunPtcRuntime {
    fn language(&self) -> PtcLanguage {
        PtcLanguage::TypeScript
    }

    fn program_format(&self) -> PtcProgramFormat {
        PtcProgramFormat::FunctionBody
    }

    fn execution_instructions(&self) -> &str {
        EXECUTION_INSTRUCTIONS
    }

    fn resolve(&self, request: PtcRunRequest) -> Result<PtcRunSpec, PtcError> {
        let cwd = match request.cwd {
            Some(cwd) => cwd,
            None => std::env::current_dir().map_err(|error| {
                PtcError::Invalid(format!("cannot resolve the default cwd: {error}"))
            })?,
        };
        if !cwd.is_absolute() {
            return Err(PtcError::Invalid(format!(
                "cwd must be absolute, got {}",
                cwd.display()
            )));
        }
        if !cwd.is_dir() {
            return Err(PtcError::Invalid(format!(
                "cwd is not a directory: {}",
                cwd.display()
            )));
        }
        let timeout = request.timeout.unwrap_or(self.config.default_timeout);
        if timeout.is_zero() {
            return Err(PtcError::Invalid("timeout must be positive".to_owned()));
        }
        let max_output_bytes = request
            .max_output_bytes
            .unwrap_or(self.config.max_output_bytes);
        if max_output_bytes == 0 {
            return Err(PtcError::Invalid(
                "max_output_bytes must be positive".to_owned(),
            ));
        }
        Ok(PtcRunSpec {
            program: request.program,
            bindings: request.bindings,
            cwd,
            timeout: timeout.min(self.config.max_timeout),
            max_output_bytes: max_output_bytes.min(self.config.max_output_bytes),
            cancel: request.cancel,
        })
    }

    fn run(&self, spec: PtcRunSpec) -> Result<PtcRunResult, PtcError> {
        runner::run(&self.config, self.spawn_hook.as_ref(), spec)
    }
}

/// 默认配置的后端（`bun` 走 PATH）。
impl Default for BunPtcRuntime {
    fn default() -> Self {
        Self::new(BunConfig::default())
    }
}

impl std::fmt::Debug for BunPtcRuntime {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("BunPtcRuntime")
            .field("config", &self.config)
            .field("spawn_hook", &self.spawn_hook.is_some())
            .finish()
    }
}

/// 便于宿主把「一次程序预算」写成常量时使用的默认值。
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(120);
