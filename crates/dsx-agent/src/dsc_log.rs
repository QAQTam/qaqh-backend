use simplelog::*;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::OnceLock;

static LOGGER_INIT: OnceLock<bool> = OnceLock::new();
static HAS_SESSION: AtomicBool = AtomicBool::new(false);
static LOG_PATH: OnceLock<std::sync::Mutex<PathBuf>> = OnceLock::new();

fn level() -> LevelFilter {
    match std::env::var("DSC_LOG").as_deref() {
        Ok("trace") => LevelFilter::Trace,
        Ok("debug") => LevelFilter::Debug,
        Ok("info") => LevelFilter::Info,
        Ok("warn") => LevelFilter::Warn,
        Ok("error") => LevelFilter::Error,
        _ => LevelFilter::Info,
    }
}

fn log_dir() -> PathBuf {
    let mut p = config_dir();
    p.push("logs");
    let _ = std::fs::create_dir_all(&p);
    p
}

fn config_dir() -> PathBuf {
    if let Ok(d) = std::env::var("DSC_CONFIG_DIR") {
        return PathBuf::from(d);
    }
    dsx_types::platform::home_dir().join(".config").join("dsc")
}

/// Initialize file logging. Call once at startup.
/// Logs to ~/.config/dsc/logs/dsc.log (overwritten each run).
/// Set DSC_LOG=debug|trace|info|warn|error to control verbosity.
/// Set runtime log level (for /dev command).
pub fn set_level(lvl: &str) {
    let filter = match lvl {
        "trace" => LevelFilter::Trace,
        "debug" => LevelFilter::Debug,
        "info" => LevelFilter::Info,
        "warn" => LevelFilter::Warn,
        "error" => LevelFilter::Error,
        _ => return,
    };
    log::set_max_level(filter);
    log::info!("Log level set to {}", lvl);
}

pub fn init() {
    LOGGER_INIT.get_or_init(|| {
        let lvl = level();
        let mut path = log_dir();
        path.push("dsc.pending.log");

        let config = ConfigBuilder::new()
            .set_max_level(lvl)
            .set_time_format_rfc3339()
            .set_time_offset_to_local()
            .unwrap_or_else(|c| c)
            .build();

        let _ = LOG_PATH.set(std::sync::Mutex::new(path.clone()));

        match WriteLogger::init(lvl, config, std::fs::File::create(&path).unwrap()) {
            Ok(()) => log::info!("Log started: {}", path.display()),
            Err(e) => eprintln!("[dsc] log: {}", e),
        }
        true
    });
}

/// Rename log to {seed}.{date}.log once session seed is known.
pub fn set_session(seed: &str) {
    if HAS_SESSION.swap(true, Ordering::Relaxed) { return; }
    let Some(lock) = LOG_PATH.get() else { return };
    let Ok(mut path) = lock.lock() else { return };
    let date = date_tag();
    let new_name = format!("{}.{}.log", seed, date);
    path.set_file_name(&new_name);
    let old_path = path.with_file_name("dsc.pending.log");
    let _ = std::fs::rename(&old_path, &*path);
    log::info!("log: {} → {}", old_path.display(), path.display());
}

fn date_tag() -> String {
    let secs = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH).unwrap_or_default().as_secs();
    let days = secs / 86400;
    let y = 1970 + (days as f64 / 365.25) as u64;
    let rem = days - ((y as f64 - 1970.0) * 365.25) as u64;
    format!("{:04}-{:02}-{:02}",
        y.min(9999), (1 + rem / 30).min(12), (1 + rem % 30).min(31))
}
