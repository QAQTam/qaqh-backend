use std::path::PathBuf;

/// Cross-platform home directory.
/// - Windows: `USERPROFILE`
/// - Unix: `HOME`
pub fn home_dir() -> PathBuf {
    if cfg!(target_os = "windows") {
        std::env::var("USERPROFILE").map(PathBuf::from).unwrap_or_default()
    } else {
        std::env::var("HOME").map(PathBuf::from).unwrap_or_default()
    }
}

/// DSX data directory (config, sessions, plans, skills).
/// - Windows: `%USERPROFILE%\.dsx`
/// - Unix: `$XDG_CONFIG_HOME/dsx` or `$HOME/.config/dsx`
pub fn data_dir() -> PathBuf {
    if cfg!(target_os = "windows") {
        home_dir().join(".dsx")
    } else {
        std::env::var("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home_dir().join(".config"))
            .join("dsx")
    }
}

/// DSX config file path.
pub fn config_path() -> PathBuf {
    data_dir().join("config.json")
}

/// DSX HP port file path.
pub fn hp_port_path() -> PathBuf {
    home_dir().join(".dsx").join("hp.port")
}

/// DSX workspace file path.
pub fn workspace_path() -> PathBuf {
    home_dir().join(".dsx").join("workspace.txt")
}

/// DSX sessions directory.
pub fn sessions_dir() -> PathBuf {
    data_dir().join("sessions")
}

/// DSX plans directory.
pub fn plans_dir() -> PathBuf {
    data_dir().join("plans")
}

/// DSX skills directory.
pub fn skills_dir() -> PathBuf {
    data_dir().join("skills")
}

/// Temporary directory (cross-platform).
pub fn temp_dir() -> PathBuf {
    std::env::temp_dir()
}

/// Kill a process by PID (cross-platform).
/// - Windows: `taskkill /F /PID`
/// - Unix: `kill -9`
pub fn kill_process(pid: u32) {
    if cfg!(target_os = "windows") {
        let _ = std::process::Command::new("taskkill")
            .args(["/F", "/PID", &pid.to_string()])
            .output();
    } else {
        let _ = std::process::Command::new("kill")
            .args(["-9", &pid.to_string()])
            .output();
    }
}

/// Terminate a process gracefully by PID (cross-platform).
/// - Windows: `taskkill /PID` (no /F)
/// - Unix: `kill`
pub fn terminate_process(pid: u32) {
    if cfg!(target_os = "windows") {
        let _ = std::process::Command::new("taskkill")
            .args(["/PID", &pid.to_string()])
            .output();
    } else {
        let _ = std::process::Command::new("kill")
            .args([&pid.to_string()])
            .output();
    }
}
