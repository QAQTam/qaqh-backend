use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

use deepx_update::{is_reparse_or_symlink, verify_install_root};

type AnyError = Box<dyn std::error::Error>;

const DEEPX_PROCESS_NAMES: &[&str] = &[
    "DeepX.exe",
    "deepx-daemon.exe",
    "deepx-updater.exe",
    "deepx-companion.exe",
];
const MAX_UNINSTALL_ENTRIES: u64 = 100_000;
const MAX_UNINSTALL_BYTES: u64 = 20 * 1024 * 1024 * 1024;

pub fn default_install_dir() -> Result<PathBuf, AnyError> {
    let executable = std::env::current_exe()?;
    executable
        .parent()
        .map(Path::to_path_buf)
        .ok_or_else(|| "updater executable has no parent directory".into())
}

pub fn handoff_uninstall(
    target: &Path,
    delete_user_data: bool,
    notify: bool,
) -> Result<PathBuf, AnyError> {
    let target = validate_install_dir(target)?;
    if delete_user_data {
        let data = deepx_types::platform::data_dir();
        if data.exists() {
            VerifiedDataRoot::new(&data)?;
        }
    }
    let operation_id = format!(
        "{}-{}",
        std::process::id(),
        SystemTime::now().duration_since(UNIX_EPOCH)?.as_millis()
    );
    let runner_dir = std::env::temp_dir()
        .join("DeepX")
        .join("maintenance")
        .join(operation_id);
    fs::create_dir_all(&runner_dir)?;
    let runner = runner_dir.join(if cfg!(windows) {
        "deepx-updater.exe"
    } else {
        "deepx-updater"
    });
    fs::copy(std::env::current_exe()?, &runner)?;

    let mut command = Command::new(&runner);
    command
        .arg("uninstall-worker")
        .arg("--install-dir")
        .arg(&target)
        .arg("--wait-pid")
        .arg(std::process::id().to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    if delete_user_data {
        command.arg("--delete-user-data");
    }
    if notify {
        command.arg("--notify");
    }
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    command.spawn()?;
    Ok(runner)
}

pub fn uninstall_worker(
    target: &Path,
    wait_pid: u32,
    delete_user_data: bool,
    notify: bool,
) -> Result<(), AnyError> {
    wait_for_pid(wait_pid, Duration::from_secs(60))?;
    let result = uninstall(target, delete_user_data);
    if notify {
        match &result {
            Ok(()) => show_message("DeepX 卸载", "DeepX 已从此计算机中移除。", false),
            Err(error) => show_message("DeepX 卸载失败", &error.to_string(), true),
        }
    }
    schedule_self_delete();
    result
}

fn uninstall(target: &Path, delete_user_data: bool) -> Result<(), AnyError> {
    let target = VerifiedInstallRoot::new(target)?;
    let user_data = if delete_user_data {
        let data = deepx_types::platform::data_dir();
        data.exists()
            .then(|| VerifiedDataRoot::new(&data))
            .transpose()?
    } else {
        None
    };
    if let Some(data) = &user_data {
        audit_deletion_tree(data.path())?;
    }
    audit_deletion_tree(target.path())?;
    stop_running_processes(target.path())?;
    target.revalidate()?;
    audit_deletion_tree(target.path())?;
    clear_readonly_tree(target.path())?;
    remove_verified_install_root(&target)?;
    delete_shortcuts()?;
    delete_uninstall_registration()?;
    if let Some(data) = user_data {
        data.revalidate()?;
        audit_deletion_tree(data.path())?;
        clear_readonly_tree(data.path())?;
        remove_verified_data_root(&data)?;
    }
    Ok(())
}

#[derive(Default)]
struct DeletionAudit {
    entries: u64,
    bytes: u64,
}

fn audit_deletion_tree(root: &Path) -> Result<DeletionAudit, AnyError> {
    if is_reparse_or_symlink(root)? {
        return Err(format!(
            "refusing linked or redirected deletion root '{}'",
            root.display()
        )
        .into());
    }
    let mut audit = DeletionAudit::default();
    audit_deletion_directory(root, &mut audit)?;
    Ok(audit)
}

fn audit_deletion_directory(path: &Path, audit: &mut DeletionAudit) -> Result<(), AnyError> {
    for entry in fs::read_dir(path)? {
        let entry = entry?;
        let child = entry.path();
        let metadata = fs::symlink_metadata(&child)?;
        if is_reparse_or_symlink(&child)? {
            return Err(format!(
                "refusing deletion tree containing link or reparse point '{}'",
                child.display()
            )
            .into());
        }
        audit.entries = audit.entries.saturating_add(1);
        audit.bytes = audit.bytes.saturating_add(metadata.len());
        if audit.entries > MAX_UNINSTALL_ENTRIES || audit.bytes > MAX_UNINSTALL_BYTES {
            return Err(format!(
                "deletion safety budget exceeded at '{}': {} entries, {} bytes",
                child.display(),
                audit.entries,
                audit.bytes
            )
            .into());
        }
        if metadata.is_dir() {
            audit_deletion_directory(&child, audit)?;
        }
    }
    Ok(())
}

pub fn validate_install_dir(target: &Path) -> Result<PathBuf, AnyError> {
    VerifiedInstallRoot::new(target).map(|verified| verified.path)
}

struct VerifiedInstallRoot {
    path: PathBuf,
}

struct VerifiedDataRoot {
    path: PathBuf,
}

impl VerifiedDataRoot {
    fn new(path: &Path) -> Result<Self, AnyError> {
        let path = deepx_types::platform::verify_data_root(path)
            .map_err(|error| format!("unverified DeepX user data root: {error}"))?;
        Ok(Self { path })
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn revalidate(&self) -> Result<(), AnyError> {
        let current = deepx_types::platform::verify_data_root(&self.path)
            .map_err(|error| format!("user data root changed before deletion: {error}"))?;
        if !same_path(&current, &self.path) {
            return Err("user data root identity changed before deletion".into());
        }
        Ok(())
    }
}

impl VerifiedInstallRoot {
    fn new(target: &Path) -> Result<Self, AnyError> {
        let target = verify_install_root(target)
            .map_err(|error| format!("unverified DeepX install root: {error}"))?;
        reject_dangerous_root(&target)?;

        let has_application = target.join("DeepX.exe").is_file();
        let has_state = target.join("install-state.json").is_file();
        let has_updater = target.join("deepx-updater.exe").is_file();
        if !has_application || !has_state || !has_updater {
            return Err(format!(
                "'{}' is missing required DeepX installation files",
                target.display()
            )
            .into());
        }
        Ok(Self { path: target })
    }

    fn path(&self) -> &Path {
        &self.path
    }

    fn revalidate(&self) -> Result<(), AnyError> {
        let current = verify_install_root(&self.path)
            .map_err(|error| format!("install root changed before deletion: {error}"))?;
        if !same_path(&current, &self.path) {
            return Err("install root identity changed before deletion".into());
        }
        reject_dangerous_root(&current)
    }
}

fn reject_dangerous_root(target: &Path) -> Result<(), AnyError> {
    let mut dangerous = vec![
        dirs::home_dir(),
        dirs::data_local_dir(),
        Some(std::env::temp_dir()),
    ];
    for variable in [
        "SystemRoot",
        "ProgramData",
        "ProgramFiles",
        "ProgramFiles(x86)",
    ] {
        dangerous.push(std::env::var_os(variable).map(PathBuf::from));
    }
    if dangerous
        .into_iter()
        .flatten()
        .filter_map(|path| fs::canonicalize(path).ok())
        .any(|path| same_path(&path, target) || path_is_within(&path, target))
    {
        return Err(format!("refusing dangerous install root '{}'", target.display()).into());
    }
    Ok(())
}

fn path_is_within(candidate: &Path, root: &Path) -> bool {
    if cfg!(windows) {
        windows_path_key(candidate).starts_with(&(windows_path_key(root) + "\\"))
    } else {
        candidate.starts_with(root)
    }
}

fn same_path(left: &Path, right: &Path) -> bool {
    if cfg!(windows) {
        windows_path_key(left) == windows_path_key(right)
    } else {
        left == right
    }
}

fn windows_path_key(path: &Path) -> String {
    let value = path.to_string_lossy().replace('/', "\\");
    let value = if let Some(rest) = value.strip_prefix(r"\\?\UNC\") {
        format!(r"\\{rest}")
    } else {
        value
            .strip_prefix(r"\\?\")
            .map(str::to_owned)
            .unwrap_or(value)
    };
    value.trim_end_matches('\\').to_ascii_lowercase()
}

fn stop_running_processes(target: &Path) -> Result<(), AnyError> {
    stop_daemon_via_http();
    thread::sleep(Duration::from_secs(2));

    let current_pid = std::process::id();
    for pid in find_deepx_processes()
        .into_iter()
        .filter(|pid| *pid != current_pid && process_belongs_to(pid.to_owned(), target))
    {
        let output = Command::new("taskkill")
            .args(["/f", "/t", "/pid", &pid.to_string()])
            .output()
            .map_err(|error| format!("failed to run taskkill for PID {pid}: {error}"))?;
        if !output.status.success() && is_process_running(pid) {
            return Err(format!(
                "failed to terminate DeepX process {pid}: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )
            .into());
        }
    }

    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let remaining = find_deepx_processes()
            .into_iter()
            .filter(|pid| {
                *pid != current_pid
                    && process_belongs_to(pid.to_owned(), target)
                    && is_process_running(*pid)
            })
            .collect::<Vec<_>>();
        if remaining.is_empty() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(format!("DeepX processes are still running: {remaining:?}").into());
        }
        thread::sleep(Duration::from_millis(250));
    }
}

#[cfg(windows)]
fn process_belongs_to(pid: u32, target: &Path) -> bool {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION,
        QueryFullProcessImageNameW,
    };
    use windows::core::PWSTR;

    let Ok(process) = (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) })
    else {
        return false;
    };
    let mut buffer = vec![0_u16; 32_768];
    let mut length = buffer.len() as u32;
    let result = unsafe {
        QueryFullProcessImageNameW(
            process,
            PROCESS_NAME_WIN32,
            PWSTR::from_raw(buffer.as_mut_ptr()),
            &mut length,
        )
    };
    let _ = unsafe { CloseHandle(process) };
    if result.is_err() {
        return false;
    }
    let executable = PathBuf::from(String::from_utf16_lossy(&buffer[..length as usize]));
    let target_prefix = windows_path_key(target) + "\\";
    windows_path_key(&executable).starts_with(&target_prefix)
}

#[cfg(not(windows))]
fn process_belongs_to(_pid: u32, _target: &Path) -> bool {
    false
}

#[cfg(windows)]
fn find_deepx_processes() -> Vec<u32> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::{
        CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
        TH32CS_SNAPPROCESS,
    };

    let Ok(snapshot) = (unsafe { CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) }) else {
        return Vec::new();
    };
    let mut entry = PROCESSENTRY32W {
        dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
        ..Default::default()
    };
    let mut processes = Vec::new();
    if unsafe { Process32FirstW(snapshot, &mut entry) }.is_ok() {
        loop {
            let end = entry
                .szExeFile
                .iter()
                .position(|value| *value == 0)
                .unwrap_or(entry.szExeFile.len());
            let name = String::from_utf16_lossy(&entry.szExeFile[..end]);
            if DEEPX_PROCESS_NAMES
                .iter()
                .any(|expected| expected.eq_ignore_ascii_case(&name))
            {
                processes.push(entry.th32ProcessID);
            }
            if unsafe { Process32NextW(snapshot, &mut entry) }.is_err() {
                break;
            }
        }
    }
    let _ = unsafe { CloseHandle(snapshot) };
    processes
}

#[cfg(not(windows))]
fn find_deepx_processes() -> Vec<u32> {
    Vec::new()
}

#[cfg(windows)]
fn is_process_running(pid: u32) -> bool {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Threading::{
        GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
    };
    let Ok(process) = (unsafe { OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) })
    else {
        return false;
    };
    let mut exit_code = 0_u32;
    let alive = unsafe { GetExitCodeProcess(process, &mut exit_code) }.is_ok() && exit_code == 259;
    let _ = unsafe { CloseHandle(process) };
    alive
}

#[cfg(not(windows))]
fn is_process_running(_pid: u32) -> bool {
    false
}

fn wait_for_pid(pid: u32, timeout: Duration) -> Result<(), AnyError> {
    let deadline = Instant::now() + timeout;
    while Instant::now() < deadline {
        if !is_process_running(pid) {
            return Ok(());
        }
        thread::sleep(Duration::from_millis(200));
    }
    Err(format!("timed out waiting for maintenance process {pid}").into())
}

fn stop_daemon_via_http() {
    let Some(home) = dirs::home_dir() else {
        return;
    };
    let Ok(content) = fs::read_to_string(home.join(".deepx").join("daemon.json")) else {
        return;
    };
    let Ok(discovery) = serde_json::from_str::<serde_json::Value>(&content) else {
        return;
    };
    let Some(endpoint) = discovery.get("endpoint").and_then(|value| value.as_str()) else {
        return;
    };
    let Some(token) = discovery.get("token").and_then(|value| value.as_str()) else {
        return;
    };
    let address = endpoint
        .trim_start_matches("ws://")
        .split('/')
        .next()
        .unwrap_or_default();
    let Ok(socket_address) = address.parse::<std::net::SocketAddr>() else {
        return;
    };
    let Ok(mut stream) =
        std::net::TcpStream::connect_timeout(&socket_address, Duration::from_secs(2))
    else {
        return;
    };
    let request = format!(
        "POST /control/v1/stop HTTP/1.1\r\nHost: {address}\r\nAuthorization: Bearer {token}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n"
    );
    let _ = stream.write_all(request.as_bytes());
}

#[cfg_attr(windows, allow(clippy::permissions_set_readonly_false))]
fn clear_readonly_tree(root: &Path) -> Result<(), AnyError> {
    if !root.exists() {
        return Ok(());
    }
    if is_reparse_or_symlink(root)? {
        return Err(format!(
            "refusing to traverse symbolic link or reparse point '{}'",
            root.display()
        )
        .into());
    }
    for entry in fs::read_dir(root)? {
        let path = entry?.path();
        let metadata = fs::symlink_metadata(&path)?;
        if is_reparse_or_symlink(&path)? {
            return Err(format!(
                "refusing to traverse symbolic link or reparse point '{}'",
                path.display()
            )
            .into());
        }
        if metadata.is_dir() {
            clear_readonly_tree(&path)?;
        }
        let mut permissions = metadata.permissions();
        if permissions.readonly() {
            #[cfg(windows)]
            permissions.set_readonly(false);
            #[cfg(unix)]
            {
                use std::os::unix::fs::PermissionsExt;
                permissions.set_mode(permissions.mode() | 0o200);
            }
            fs::set_permissions(&path, permissions)?;
        }
    }
    Ok(())
}

fn remove_verified_install_root(root: &VerifiedInstallRoot) -> Result<(), AnyError> {
    root.revalidate()?;
    remove_dir_all_retry(root.path())
}

fn remove_verified_data_root(root: &VerifiedDataRoot) -> Result<(), AnyError> {
    root.revalidate()?;
    remove_dir_all_retry(root.path())
}

fn remove_dir_all_retry(path: &Path) -> Result<(), AnyError> {
    let mut last_error = None;
    for _ in 0..20 {
        let result = if path.exists() && is_reparse_or_symlink(path)? {
            fs::remove_dir(path)
        } else {
            fs::remove_dir_all(path)
        };
        match result {
            Ok(()) => return Ok(()),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(()),
            Err(error) => {
                last_error = Some(error);
                thread::sleep(Duration::from_millis(250));
            }
        }
    }
    Err(format!(
        "failed to remove '{}': {}",
        path.display(),
        last_error
            .map(|error| error.to_string())
            .unwrap_or_else(|| "unknown error".to_string())
    )
    .into())
}

fn delete_shortcuts() -> Result<(), AnyError> {
    if let Some(desktop) = dirs::desktop_dir() {
        remove_file_if_exists(&desktop.join("DeepX.lnk"))?;
    }
    if let Some(data) = dirs::data_dir() {
        let start_menu = data.join("Microsoft/Windows/Start Menu/Programs/DeepX");
        if start_menu.exists() {
            remove_dir_all_retry(&start_menu)?;
        }
    }
    Ok(())
}

fn remove_file_if_exists(path: &Path) -> Result<(), AnyError> {
    match fs::remove_file(path) {
        Ok(()) => Ok(()),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(()),
        Err(error) => Err(error.into()),
    }
}

#[cfg(windows)]
fn delete_uninstall_registration() -> Result<(), AnyError> {
    use windows::Win32::System::Registry::{
        HKEY_CURRENT_USER, KEY_SET_VALUE, RegCloseKey, RegDeleteTreeW, RegOpenKeyExW,
    };
    use windows::core::PCWSTR;

    let parent = wide("Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall");
    let child = wide("DeepX");
    unsafe {
        let mut key = std::mem::zeroed();
        if RegOpenKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR::from_raw(parent.as_ptr()),
            0,
            KEY_SET_VALUE,
            &mut key,
        )
        .is_ok()
        {
            let result = RegDeleteTreeW(key, PCWSTR::from_raw(child.as_ptr()));
            let _ = RegCloseKey(key);
            if !result.is_ok() {
                return Err(format!("failed to remove uninstall registration: {result:?}").into());
            }
        }
    }
    Ok(())
}

#[cfg(not(windows))]
fn delete_uninstall_registration() -> Result<(), AnyError> {
    Ok(())
}

#[cfg(windows)]
fn schedule_self_delete() {
    use windows::Win32::Storage::FileSystem::{MOVEFILE_DELAY_UNTIL_REBOOT, MoveFileExW};
    use windows::core::PCWSTR;

    if let Ok(executable) = std::env::current_exe() {
        let executable = wide(&executable.to_string_lossy());
        let _ = unsafe {
            MoveFileExW(
                PCWSTR::from_raw(executable.as_ptr()),
                PCWSTR::null(),
                MOVEFILE_DELAY_UNTIL_REBOOT,
            )
        };
    }
}

#[cfg(not(windows))]
fn schedule_self_delete() {}

#[cfg(windows)]
fn show_message(title: &str, message: &str, error: bool) {
    use windows::Win32::UI::WindowsAndMessaging::{
        MB_ICONERROR, MB_ICONINFORMATION, MB_OK, MessageBoxW,
    };
    use windows::core::PCWSTR;

    let title = wide(title);
    let message = wide(message);
    let style = MB_OK
        | if error {
            MB_ICONERROR
        } else {
            MB_ICONINFORMATION
        };
    let _ = unsafe {
        MessageBoxW(
            None,
            PCWSTR::from_raw(message.as_ptr()),
            PCWSTR::from_raw(title.as_ptr()),
            style,
        )
    };
}

#[cfg(not(windows))]
fn show_message(_title: &str, _message: &str, _error: bool) {}

#[cfg(windows)]
fn wide(value: &str) -> Vec<u16> {
    value.encode_utf16().chain(std::iter::once(0)).collect()
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_UNINSTALL_BYTES, audit_deletion_tree, path_is_within, reject_dangerous_root,
        windows_path_key,
    };
    use std::path::Path;
    use std::time::{SystemTime, UNIX_EPOCH};

    #[test]
    fn normalizes_extended_windows_paths_for_process_matching() {
        assert_eq!(
            windows_path_key(Path::new(r"\\?\C:\Users\Test\DeepX")),
            windows_path_key(Path::new(r"c:\users\test\deepx\"))
        );
    }

    #[test]
    fn normalizes_extended_unc_paths() {
        assert_eq!(
            windows_path_key(Path::new(r"\\?\UNC\server\share\DeepX")),
            windows_path_key(Path::new(r"\\server\share\deepx"))
        );
    }

    #[test]
    fn protected_directories_and_their_ancestors_are_never_delete_roots() {
        let temporary = std::fs::canonicalize(std::env::temp_dir()).expect("canonical temp");
        assert!(reject_dangerous_root(&temporary).is_err());
        if let Some(parent) = temporary.parent() {
            assert!(path_is_within(&temporary, parent));
            assert!(reject_dangerous_root(parent).is_err());
        }
    }

    #[test]
    fn deletion_budget_rejects_implausibly_large_tree() {
        let nonce = SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .expect("clock before epoch")
            .as_nanos();
        let root = std::env::temp_dir().join(format!(
            "deepx-deletion-budget-{}-{nonce}",
            std::process::id()
        ));
        std::fs::create_dir_all(&root).expect("create audit root");
        let oversized = root.join("oversized.bin");
        std::fs::File::create(&oversized)
            .and_then(|file| file.set_len(MAX_UNINSTALL_BYTES + 1))
            .expect("create sparse oversized file");
        assert!(audit_deletion_tree(&root).is_err());
        std::fs::remove_file(oversized).expect("remove sparse file");
        std::fs::remove_dir(root).expect("remove audit root");
    }
}
