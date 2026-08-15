//! File-based agent logger.
//!
//! Writes to `<data>/agent.log` (the runtime process-level log). The default
//! level is `info`; set `DEEPX_LOG` (or `RUST_LOG`) to one of
//! `off|error|warn|info|debug|trace` to change it. Third-party crates'
//! `debug`/`trace` records (e.g. `ureq::*`, `ureq_proto::*`, `rustls`) are
//! always dropped so HTTP clients cannot flood the log with per-request
//! internals; only DeepX targets keep their `debug`/`trace` output.

use chrono::Local;
use log::{Level, LevelFilter, Log, Metadata, Record};
use std::fs::{File, OpenOptions};
use std::io::Write;
use std::path::Path;
use std::sync::Mutex;

/// 默认日志级别：info（不写第三方 debug 噪音）。
const DEFAULT_LEVEL: LevelFilter = LevelFilter::Info;

/// Debug/Trace 级别下允许写文件的 target 前缀（DeepX 自身 crate）。
const DEBUG_TARGET_PREFIXES: &[&str] = &["deepx"];

struct FileLogSink {
    file: Mutex<File>,
    max_level: LevelFilter,
}

impl FileLogSink {
    fn should_log(&self, metadata: &Metadata) -> bool {
        let level = metadata.level();
        if level.to_level_filter() > self.max_level {
            return false;
        }
        // Debug/Trace 只放行 DeepX 自身 target；第三方库（ureq、rustls、
        // h2、hyper 等）的 DEBUG 一律不落盘，避免每个 HTTP 请求刷十几行。
        if level <= Level::Debug {
            let target = metadata.target();
            return DEBUG_TARGET_PREFIXES.iter().any(|p| target.starts_with(p));
        }
        true
    }
}

impl Log for FileLogSink {
    fn enabled(&self, metadata: &Metadata) -> bool {
        self.should_log(metadata)
    }

    fn log(&self, record: &Record) {
        if !self.should_log(record.metadata()) {
            return;
        }
        let ts = Local::now().format("%Y-%m-%d %H:%M:%S%.3f");
        // 整行拼好后再一次性写入，避免 writeln! 多次 write 导致并发行交错。
        let line = format!(
            "[{ts}] [{:5}] {} | {}\n",
            record.level(),
            record.target(),
            record.args()
        );
        if let Ok(mut file) = self.file.lock() {
            let _ = file.write_all(line.as_bytes());
        }
    }

    fn flush(&self) {
        if let Ok(mut file) = self.file.lock() {
            let _ = file.flush();
        }
    }
}

/// 解析环境变量里的日志级别（大小写不敏感；仅接受纯级别名）。
fn parse_level(value: &str) -> Option<LevelFilter> {
    match value.trim().to_ascii_lowercase().as_str() {
        "off" => Some(LevelFilter::Off),
        "error" => Some(LevelFilter::Error),
        "warn" => Some(LevelFilter::Warn),
        "info" => Some(LevelFilter::Info),
        "debug" => Some(LevelFilter::Debug),
        "trace" => Some(LevelFilter::Trace),
        _ => None,
    }
}

/// Install a simple file-based logger that writes to `<data>/agent.log`.
/// This is the runtime process-level log; the agent loop itself does not
/// depend on any particular logger.
///
/// 级别优先级：`DEEPX_LOG` > `RUST_LOG` > 默认 `info`。
pub fn init_agent_logger(data_dir: &Path) {
    let path = data_dir.join("agent.log");
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let Ok(file) = OpenOptions::new().create(true).append(true).open(&path) else {
        return;
    };
    let max_level = std::env::var("DEEPX_LOG")
        .ok()
        .and_then(|v| parse_level(&v))
        .or_else(|| std::env::var("RUST_LOG").ok().and_then(|v| parse_level(&v)))
        .unwrap_or(DEFAULT_LEVEL);
    let sink = FileLogSink {
        file: Mutex::new(file),
        max_level,
    };
    let _ = log::set_boxed_logger(Box::new(sink));
    log::set_max_level(max_level);
}
