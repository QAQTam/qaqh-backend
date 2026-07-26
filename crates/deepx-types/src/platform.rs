use std::path::PathBuf;

/// Cross-platform home directory.
/// - Windows: `USERPROFILE`
/// - Unix: `HOME`
pub fn home_dir() -> PathBuf {
    if cfg!(target_os = "windows") {
        std::env::var("USERPROFILE")
            .map(PathBuf::from)
            .unwrap_or_default()
    } else {
        std::env::var("HOME").map(PathBuf::from).unwrap_or_default()
    }
}

/// deepx data directory (config, sessions, plans).
/// - Windows: `%USERPROFILE%\.deepx`
/// - Unix: `$XDG_CONFIG_HOME/deepx` or `$HOME/.config/deepx`
pub fn data_dir() -> PathBuf {
    if cfg!(target_os = "windows") {
        home_dir().join(".deepx")
    } else {
        std::env::var("XDG_CONFIG_HOME")
            .map(PathBuf::from)
            .unwrap_or_else(|_| home_dir().join(".config"))
            .join("deepx")
    }
}

/// deepx config file path.
pub fn config_path() -> PathBuf {
    data_dir().join("config.toml")
}

/// deepx HP port file path.
pub fn hp_port_path() -> PathBuf {
    data_dir().join("hp.port")
}

pub fn daemon_discovery_path() -> PathBuf {
    data_dir().join("daemon.json")
}

pub fn daemon_lock_path() -> PathBuf {
    data_dir().join("daemon.lock")
}

/// deepx sessions directory.
pub fn sessions_dir() -> PathBuf {
    data_dir().join("sessions")
}

/// deepx plans directory.
pub fn plans_dir() -> PathBuf {
    data_dir().join("plans")
}

/// deepx workspace path file.
pub fn workspace_path() -> PathBuf {
    data_dir().join("workspace.txt")
}

/// Kill a process by PID (cross-platform).
/// - Windows: `taskkill /F /PID`
/// - Unix: `kill -9`
pub fn kill_process(pid: u32) {
    if cfg!(target_os = "windows") {
        let mut command = background_command("taskkill");
        drop(command.args(["/F", "/PID", &pid.to_string()]).output());
    } else {
        drop(
            std::process::Command::new("kill")
                .args(["-9", &pid.to_string()])
                .output(),
        );
    }
}

/// Return whether a process id currently exists without mutating it.
pub fn process_is_running(pid: u32) -> bool {
    if pid == 0 {
        return false;
    }
    if cfg!(target_os = "windows") {
        background_command("tasklist")
            .args(["/FI", &format!("PID eq {pid}"), "/FO", "CSV", "/NH"])
            .output()
            .ok()
            .filter(|output| output.status.success())
            .is_some_and(|output| {
                String::from_utf8_lossy(&output.stdout).lines().any(|line| {
                    line.split(',')
                        .nth(1)
                        .is_some_and(|field| field.trim_matches('"').trim() == pid.to_string())
                })
            })
    } else {
        std::process::Command::new("kill")
            .args(["-0", &pid.to_string()])
            .status()
            .is_ok_and(|status| status.success())
    }
}

fn background_command(program: &str) -> std::process::Command {
    let mut command = std::process::Command::new(program);
    #[cfg(target_os = "windows")]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        command.creation_flags(CREATE_NO_WINDOW);
    }
    command
}

/// Convert days since epoch 0000-01-01 to (year, month, day).
/// Algorithm from Howard Hinnant's civil_from_days.
pub fn civil_from_days(days: i64) -> (i64, u32, u32) {
    let z = days + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = (z - era * 146097) as u32;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe as i64 + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    (y, m, d)
}
