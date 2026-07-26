use log::{LevelFilter, Log, Metadata, Record};
use std::path::Path;

struct FileLogSink {
    path: std::path::PathBuf,
}

impl Log for FileLogSink {
    fn enabled(&self, _: &Metadata) -> bool { true }
    fn log(&self, record: &Record) {
        if self.enabled(record.metadata()) {
            let level = record.level();
            let target = record.target();
            let msg = record.args();
            if let Ok(mut file) = std::fs::OpenOptions::new()
                .create(true)
                .append(true)
                .open(&self.path)
            {
                use std::io::Write;
                let _ = writeln!(file, "[{level:5}] {target} | {msg}");
            }
        }
    }
    fn flush(&self) {}
}

/// Install a simple file-based logger that writes to `<data>/agent.log`.
/// This is the runtime process-level log; the agent loop itself does not
/// depend on any particular logger.
pub fn init_agent_logger(data_dir: &Path) {
    let path = data_dir.join("agent.log");
    if let Some(parent) = path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let sink = FileLogSink { path };
    let _ = log::set_boxed_logger(Box::new(sink));
    log::set_max_level(LevelFilter::Debug);
}
