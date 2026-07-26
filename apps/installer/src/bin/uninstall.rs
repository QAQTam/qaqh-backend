//! Compatibility launcher for installations whose Windows uninstall entry still
//! points at `uninstall.exe`. The lifecycle implementation lives in
//! `deepx-updater.exe`.

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use std::process::{Command, Stdio};

fn main() {
    if let Err(error) = forward_to_updater() {
        show_error(&error.to_string());
        std::process::exit(1);
    }
}

fn forward_to_updater() -> Result<(), Box<dyn std::error::Error>> {
    let executable = std::env::current_exe()?;
    let install_dir = executable
        .parent()
        .ok_or("uninstaller executable has no parent directory")?;
    let updater = install_dir.join(if cfg!(windows) {
        "deepx-updater.exe"
    } else {
        "deepx-updater"
    });
    if !updater.is_file() {
        return Err(format!(
            "DeepX maintenance program is missing: {}",
            updater.display()
        )
        .into());
    }

    let mut command = Command::new(updater);
    command
        .arg("uninstall")
        .arg("--interactive")
        .arg("--install-dir")
        .arg(install_dir)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    command.spawn()?;
    Ok(())
}

#[cfg(windows)]
fn show_error(message: &str) {
    use windows::core::PCWSTR;
    use windows::Win32::UI::WindowsAndMessaging::{MessageBoxW, MB_ICONERROR, MB_OK};

    let title = wide("DeepX 卸载");
    let message = wide(message);
    let _ = unsafe {
        MessageBoxW(
            None,
            PCWSTR::from_raw(message.as_ptr()),
            PCWSTR::from_raw(title.as_ptr()),
            MB_OK | MB_ICONERROR,
        )
    };
}

#[cfg(not(windows))]
fn show_error(message: &str) {
    eprintln!("{message}");
}

#[cfg(windows)]
fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}
