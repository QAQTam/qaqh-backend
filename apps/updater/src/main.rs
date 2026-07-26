use std::env;
use std::fs;
use std::io;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::Duration;

mod maintenance;

use deepx_update::{
    DirectoryUpdateSource, StagedArtifact, StagedOperation, UpdateSource, apply_bundle_zip,
    installation_id_for_path, load_installed_state, plan_update, read_bundle_manifest_zip,
    rollback_bundle_zip, safe_join_under_root, sha256_reader, verify_install_root,
    write_installed_state,
};

fn main() {
    if let Err(error) = run() {
        eprintln!("deepx-updater: {error}");
        std::process::exit(1);
    }
}

fn run() -> Result<(), Box<dyn std::error::Error>> {
    let arguments = env::args().skip(1).collect::<Vec<_>>();
    if let Some(command) = arguments.first().map(String::as_str) {
        match command {
            "maintain" => {
                let options = MaintenanceOptions::parse(&arguments[1..])?;
                return run_maintenance_ui(options.target()?, false);
            }
            "uninstall" => {
                let options = MaintenanceOptions::parse(&arguments[1..])?;
                let target = options.target()?;
                if options.quiet {
                    maintenance::handoff_uninstall(&target, options.delete_user_data, false)?;
                    return Ok(());
                }
                return run_maintenance_ui(target, true);
            }
            "uninstall-worker" => {
                let options = MaintenanceOptions::parse(&arguments[1..])?;
                let wait_pid = options
                    .wait_pid
                    .ok_or("uninstall-worker requires --wait-pid")?;
                return maintenance::uninstall_worker(
                    &options.target()?,
                    wait_pid,
                    options.delete_user_data,
                    options.notify,
                );
            }
            _ => {}
        }
    }
    match arguments.as_slice() {
        [command, operation, target, wait_pid, relaunch] if command == "handoff" => {
            handoff(operation, target, wait_pid, relaunch)
        }
        [
            command,
            operation,
            target,
            wait_flag,
            wait_pid,
            relaunch_flag,
            relaunch,
        ] if command == "apply-staged"
            && wait_flag == "--wait-pid"
            && relaunch_flag == "--relaunch" =>
        {
            apply_staged(operation, target, Some(wait_pid.parse()?), Some(relaunch))
        }
        [command, operation, target] if command == "apply-staged" => {
            apply_staged(operation, target, None, None)
        }
        [command, operation, target] if command == "rollback-staged" => {
            rollback_staged(operation, target)
        }
        [command, source, target] if command == "stage" => stage(source, target),
        [command, source, target] if command == "plan" => plan(source, target),
        [command, source] if command == "inspect" => inspect(source),
        _ => {
            print_usage();
            Err("invalid arguments".into())
        }
    }
}

#[derive(Default)]
struct MaintenanceOptions {
    install_dir: Option<PathBuf>,
    wait_pid: Option<u32>,
    delete_user_data: bool,
    notify: bool,
    quiet: bool,
}

impl MaintenanceOptions {
    fn parse(arguments: &[String]) -> Result<Self, Box<dyn std::error::Error>> {
        let mut options = Self::default();
        let mut index = 0;
        while index < arguments.len() {
            match arguments[index].as_str() {
                "--install-dir" => {
                    index += 1;
                    options.install_dir = Some(
                        arguments
                            .get(index)
                            .ok_or("--install-dir requires a value")?
                            .into(),
                    );
                }
                "--wait-pid" => {
                    index += 1;
                    options.wait_pid = Some(
                        arguments
                            .get(index)
                            .ok_or("--wait-pid requires a value")?
                            .parse()?,
                    );
                }
                "--delete-user-data" => options.delete_user_data = true,
                "--notify" | "--interactive" => options.notify = true,
                "--quiet" => options.quiet = true,
                unknown => return Err(format!("unknown maintenance option: {unknown}").into()),
            }
            index += 1;
        }
        Ok(options)
    }

    fn target(&self) -> Result<PathBuf, Box<dyn std::error::Error>> {
        self.install_dir
            .clone()
            .map(Ok)
            .unwrap_or_else(maintenance::default_install_dir)
    }
}

struct MaintenanceApp {
    target: PathBuf,
    source: String,
    status: String,
    confirm_uninstall: bool,
    delete_user_data: bool,
}

impl MaintenanceApp {
    fn new(target: PathBuf, confirm_uninstall: bool) -> Self {
        let status = installation_summary(&target);
        Self {
            target,
            source: String::new(),
            status,
            confirm_uninstall,
            delete_user_data: false,
        }
    }
}

impl eframe::App for MaintenanceApp {
    fn update(&mut self, context: &egui::Context, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default().show(context, |ui| {
            ui.heading("DeepX 维护");
            ui.add_space(8.0);
            ui.label(format!("安装位置：{}", self.target.display()));
            ui.label(&self.status);
            ui.separator();

            ui.heading("修改或修复");
            ui.label("选择由 DeepX Installer 生成的本地 update-source 目录：");
            ui.text_edit_singleline(&mut self.source);
            if ui.button("验证并暂存更新").clicked() {
                let target = self.target.to_string_lossy().into_owned();
                self.status = if self.source.trim().is_empty() {
                    "请先输入 update-source 目录。".to_string()
                } else {
                    match stage(self.source.trim(), &target) {
                        Ok(()) => "更新已暂存；启动或重启 DeepX 后完成应用。".to_string(),
                        Err(error) => format!("更新暂存失败：{error}"),
                    }
                };
            }

            ui.add_space(16.0);
            ui.separator();
            ui.heading("删除");
            ui.checkbox(
                &mut self.delete_user_data,
                format!(
                    "同时删除用户数据（{}）",
                    deepx_types::platform::data_dir().display()
                ),
            );
            if !self.confirm_uninstall {
                if ui.button("卸载 DeepX…").clicked() {
                    self.confirm_uninstall = true;
                }
            } else {
                ui.colored_label(
                    egui::Color32::from_rgb(190, 40, 40),
                    "卸载将删除程序文件、快捷方式和 Windows 注册信息。",
                );
                ui.horizontal(|ui| {
                    if ui.button("确认卸载").clicked() {
                        match maintenance::handoff_uninstall(
                            &self.target,
                            self.delete_user_data,
                            true,
                        ) {
                            Ok(_) => context.send_viewport_cmd(egui::ViewportCommand::Close),
                            Err(error) => self.status = format!("无法启动卸载：{error}"),
                        }
                    }
                    if ui.button("取消").clicked() {
                        self.confirm_uninstall = false;
                    }
                });
            }
        });
    }
}

fn run_maintenance_ui(
    target: PathBuf,
    confirm_uninstall: bool,
) -> Result<(), Box<dyn std::error::Error>> {
    let target = maintenance::validate_install_dir(&target)?;
    let options = eframe::NativeOptions {
        viewport: egui::ViewportBuilder::default()
            .with_title("DeepX 维护")
            .with_inner_size([620.0, 430.0])
            .with_resizable(false),
        ..Default::default()
    };
    eframe::run_native(
        "DeepXMaintenance",
        options,
        Box::new(move |creation| {
            setup_fonts(&creation.egui_ctx);
            Ok(Box::new(MaintenanceApp::new(target, confirm_uninstall)))
        }),
    )?;
    Ok(())
}

fn installation_summary(target: &std::path::Path) -> String {
    let state_path = target.join("install-state.json");
    match fs::read(&state_path)
        .ok()
        .and_then(|bytes| serde_json::from_slice::<serde_json::Value>(&bytes).ok())
    {
        Some(state) => {
            let release = state
                .get("releaseId")
                .and_then(|value| value.as_str())
                .unwrap_or("未知");
            let component_count = state
                .get("components")
                .and_then(|value| value.as_object())
                .map_or(0, serde_json::Map::len);
            format!("当前版本：{release}；已安装组件：{component_count}")
        }
        None => "未能读取 install-state.json。".to_string(),
    }
}

fn setup_fonts(context: &egui::Context) {
    let Some(windows_dir) = env::var_os("WINDIR") else {
        return;
    };
    let fonts_dir = PathBuf::from(windows_dir).join("Fonts");
    for name in ["Deng.ttf", "msyh.ttc", "simhei.ttf"] {
        let path = fonts_dir.join(name);
        let Ok(bytes) = fs::read(path) else {
            continue;
        };
        let mut fonts = egui::FontDefinitions::default();
        fonts
            .font_data
            .insert("deepx-cjk".to_string(), egui::FontData::from_owned(bytes));
        for family in [egui::FontFamily::Proportional, egui::FontFamily::Monospace] {
            fonts
                .families
                .entry(family)
                .or_default()
                .insert(0, "deepx-cjk".to_string());
        }
        context.set_fonts(fonts);
        return;
    }
}

fn handoff(
    operation: &str,
    target: &str,
    wait_pid: &str,
    relaunch: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let _: u32 = wait_pid.parse()?;
    let target = verify_install_root(&absolute_path(target)?)?;
    let operation = verify_staged_operation_path(Path::new(operation), &target)?;
    let runner_dir = operation
        .parent()
        .ok_or("operation.json has no parent directory")?
        .join("runner");
    fs::create_dir_all(&runner_dir)?;
    let runner = runner_dir.join("deepx-updater.exe");
    fs::copy(env::current_exe()?, &runner)?;

    let mut command = Command::new(&runner);
    command
        .arg("apply-staged")
        .arg(&operation)
        .arg(&target)
        .arg("--wait-pid")
        .arg(wait_pid)
        .arg("--relaunch")
        .arg(relaunch)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        command.creation_flags(0x08000000);
    }
    command.spawn()?;
    println!("{}", runner.display());
    Ok(())
}

fn apply_staged(
    operation: &str,
    target: &str,
    wait_pid: Option<u32>,
    relaunch: Option<&str>,
) -> Result<(), Box<dyn std::error::Error>> {
    if let Some(pid) = wait_pid {
        wait_for_process_exit(pid)?;
    }
    let target = verify_install_root(&PathBuf::from(target))?;
    let operation_path = verify_staged_operation_path(Path::new(operation), &target)?;
    let operation: StagedOperation = serde_json::from_slice(&fs::read(&operation_path)?)?;
    let operation_id = operation.operation_id.clone();
    let planned = &operation.plan.artifacts;
    let staged_ids = operation
        .artifacts
        .iter()
        .map(|artifact| &artifact.id)
        .collect::<Vec<_>>();
    if planned.len() != staged_ids.len()
        || planned
            .iter()
            .zip(&staged_ids)
            .any(|(planned, staged)| planned != *staged)
    {
        return Err("staged artifacts do not match the recorded update plan".into());
    }

    for (index, artifact) in operation.artifacts.iter().enumerate() {
        let path = PathBuf::from(&artifact.path);
        let (size, sha256) = sha256_reader(fs::File::open(&path)?)?;
        if size != artifact.size || !sha256.eq_ignore_ascii_case(&artifact.sha256) {
            return Err(format!("staged artifact verification failed: {}", artifact.id).into());
        }
        if let Err(error) = apply_bundle_zip(&path, &target, &operation.operation_id) {
            for applied in operation.artifacts[..=index].iter().rev() {
                let _ = rollback_bundle_zip(&PathBuf::from(&applied.path), &target);
            }
            if let Some(previous) = &operation.previous_state {
                let _ = write_installed_state(&target.join("install-state.json"), previous);
            }
            return Err(error.into());
        }
    }

    let state_path = target.join("install-state.json");
    let installation_id = installation_id_for_path(&target);
    let mut state = load_installed_state(&state_path, &installation_id)?
        .ok_or("bundle apply completed without writing install-state.json")?;
    state.release_id = operation.release_id.clone();
    state.last_committed_operation = Some(operation_id.clone());
    write_installed_state(&state_path, &state)?;
    let pending_path = safe_join_under_root(&target, ".deepx-update/pending.json")?;
    if let Ok(value) = fs::read(&pending_path).and_then(|bytes| {
        serde_json::from_slice::<serde_json::Value>(&bytes)
            .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))
    }) && value.get("operationId").and_then(|value| value.as_str())
        == state.last_committed_operation.as_deref()
    {
        let _ = fs::remove_file(pending_path);
    }
    println!("{}", serde_json::to_string_pretty(&state)?);
    if let Some(executable) = relaunch.filter(|value| *value != "-") {
        relaunch_and_verify(executable, &target, &operation, &operation_id)?;
    }
    Ok(())
}

fn relaunch_and_verify(
    executable: &str,
    target: &std::path::Path,
    operation: &StagedOperation,
    operation_id: &str,
) -> Result<(), Box<dyn std::error::Error>> {
    let health_dir = safe_join_under_root(target, ".deepx-update/health")?;
    fs::create_dir_all(&health_dir)?;
    let health = health_dir.join(format!("{operation_id}.ok"));
    let _ = fs::remove_file(&health);
    let mut child = Command::new(executable)
        .arg("--deepx-update-operation")
        .arg(operation_id)
        .current_dir(target)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()?;

    let deadline = std::time::Instant::now() + Duration::from_secs(30);
    while std::time::Instant::now() < deadline {
        if health.is_file() {
            let _ = fs::remove_file(&health);
            return Ok(());
        }
        if child.try_wait()?.is_some() {
            break;
        }
        thread::sleep(Duration::from_millis(200));
    }

    let _ = child.kill();
    let _ = child.wait();
    let rollback_supported = operation
        .artifacts
        .iter()
        .all(|artifact| !matches!(artifact.kind, deepx_update::ArtifactKind::Full));
    if rollback_supported {
        for artifact in operation.artifacts.iter().rev() {
            rollback_bundle_zip(&PathBuf::from(&artifact.path), target)?;
        }
        if let Some(mut previous) = operation.previous_state.clone() {
            previous.last_committed_operation = Some(format!("rollback-{operation_id}"));
            write_installed_state(&target.join("install-state.json"), &previous)?;
        }
        Command::new(executable)
            .arg("--deepx-update-rollback")
            .arg(operation_id)
            .current_dir(target)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()?;
    }
    Err(format!(
        "restarted application did not confirm update health within 30 seconds: {operation_id}"
    )
    .into())
}

fn rollback_staged(operation: &str, target: &str) -> Result<(), Box<dyn std::error::Error>> {
    let target = verify_install_root(&absolute_path(target)?)?;
    let operation_path = verify_staged_operation_path(Path::new(operation), &target)?;
    let operation: StagedOperation = serde_json::from_slice(&fs::read(&operation_path)?)?;
    for artifact in operation.artifacts.iter().rev() {
        rollback_bundle_zip(&PathBuf::from(&artifact.path), &target)?;
    }
    if let Some(mut previous) = operation.previous_state {
        previous.last_committed_operation = Some(format!("rollback-{}", operation.operation_id));
        write_installed_state(&target.join("install-state.json"), &previous)?;
        println!("{}", serde_json::to_string_pretty(&previous)?);
    }
    Ok(())
}

#[cfg(windows)]
fn wait_for_process_exit(pid: u32) -> Result<(), Box<dyn std::error::Error>> {
    use windows::Win32::Foundation::{CloseHandle, WAIT_OBJECT_0, WAIT_TIMEOUT};
    use windows::Win32::System::Threading::{
        OpenProcess, PROCESS_SYNCHRONIZE, WaitForSingleObject,
    };

    let process = match unsafe { OpenProcess(PROCESS_SYNCHRONIZE, false, pid) } {
        Ok(process) => process,
        Err(_) => return Ok(()),
    };
    let result = unsafe { WaitForSingleObject(process, 60_000) };
    let _ = unsafe { CloseHandle(process) };
    if result == WAIT_OBJECT_0 {
        Ok(())
    } else if result == WAIT_TIMEOUT {
        Err(format!("timed out waiting for process {pid} to exit").into())
    } else {
        Err(format!("failed waiting for process {pid}: {result:?}").into())
    }
}

#[cfg(not(windows))]
fn wait_for_process_exit(_pid: u32) -> Result<(), Box<dyn std::error::Error>> {
    thread::sleep(Duration::from_millis(1500));
    Ok(())
}

fn stage(source: &str, target: &str) -> Result<(), Box<dyn std::error::Error>> {
    let source = DirectoryUpdateSource::new(source)?;
    let catalog = source.catalog()?;
    let target = verify_install_root(&absolute_path(target)?)?;
    let installation_id = installation_id_for_path(&target);
    let state = load_installed_state(&target.join("install-state.json"), &installation_id)?;
    let plan = plan_update(state.as_ref(), &catalog)?;
    if plan.artifacts.is_empty() {
        println!("{}", serde_json::to_string_pretty(&plan)?);
        return Ok(());
    }

    let stage_root = safe_join_under_root(
        &target,
        &format!(".deepx-update/staging/{}", plan.operation_id),
    )?;
    fs::create_dir_all(&stage_root)?;
    let mut staged = Vec::new();
    for artifact_id in &plan.artifacts {
        let artifact = catalog
            .artifacts
            .iter()
            .find(|artifact| &artifact.id == artifact_id)
            .ok_or_else(|| format!("planned artifact is missing from catalog: {artifact_id}"))?;
        let file_name = PathBuf::from(&artifact.payload.path)
            .file_name()
            .ok_or_else(|| format!("artifact has no file name: {}", artifact.payload.path))?
            .to_owned();
        let destination = stage_root.join(file_name);
        let temporary = destination.with_extension("deepx-part");
        let mut input = source.open_artifact(&artifact.payload.path)?;
        let mut output = fs::File::create(&temporary)?;
        io::copy(&mut input, &mut output)?;
        output.sync_all()?;
        drop(output);

        let (size, sha256) = sha256_reader(fs::File::open(&temporary)?)?;
        if size != artifact.payload.size || !sha256.eq_ignore_ascii_case(&artifact.payload.sha256) {
            let _ = fs::remove_file(&temporary);
            return Err(format!(
                "artifact verification failed for {}: expected {} bytes/{}, got {} bytes/{}",
                artifact.id, artifact.payload.size, artifact.payload.sha256, size, sha256
            )
            .into());
        }
        if destination.exists() {
            fs::remove_file(&destination)?;
        }
        fs::rename(&temporary, &destination)?;
        let manifest = read_bundle_manifest_zip(&destination)?;
        if manifest.kind != artifact.kind.as_str() {
            let _ = fs::remove_file(&destination);
            return Err(format!(
                "artifact {} kind mismatch: catalog={}, bundle={}",
                artifact.id,
                artifact.kind.as_str(),
                manifest.kind
            )
            .into());
        }
        let bundle_targets = manifest
            .components
            .iter()
            .map(|(name, component)| (name.clone(), component.build_id.clone()))
            .collect::<std::collections::BTreeMap<_, _>>();
        if bundle_targets != artifact.targets {
            let _ = fs::remove_file(&destination);
            return Err(format!(
                "artifact {} component targets do not match bundle.json",
                artifact.id
            )
            .into());
        }
        staged.push(StagedArtifact {
            id: artifact.id.clone(),
            kind: artifact.kind,
            path: destination.to_string_lossy().into_owned(),
            size,
            sha256,
        });
    }

    let operation = StagedOperation {
        format_version: 1,
        operation_id: plan.operation_id.clone(),
        release_id: catalog.release_id,
        source: source.describe().into(),
        plan,
        previous_state: state,
        artifacts: staged,
    };
    let operation_path = stage_root.join("operation.json");
    fs::write(&operation_path, serde_json::to_vec_pretty(&operation)?)?;
    let pending_path = safe_join_under_root(&target, ".deepx-update/pending.json")?;
    let pending = serde_json::json!({
        "formatVersion": 1,
        "operationPath": operation_path,
        "operationId": operation.operation_id,
        "releaseId": operation.release_id,
        "mode": operation.plan.mode,
        "artifacts": operation.plan.artifacts,
        "actions": operation.plan.actions,
    });
    fs::write(&pending_path, serde_json::to_vec_pretty(&pending)?)?;
    println!("{}", serde_json::to_string_pretty(&operation)?);
    Ok(())
}

fn absolute_path(path: &str) -> io::Result<PathBuf> {
    let path = PathBuf::from(path);
    if path.is_absolute() {
        Ok(path)
    } else {
        Ok(env::current_dir()?.join(path))
    }
}

fn verify_staged_operation_path(
    operation: &Path,
    target: &Path,
) -> Result<PathBuf, Box<dyn std::error::Error>> {
    let operation = fs::canonicalize(operation)?;
    if operation.file_name().and_then(|name| name.to_str()) != Some("operation.json") {
        return Err("staged operation must be named operation.json".into());
    }
    let staging = safe_join_under_root(target, ".deepx-update/staging")?;
    let staging = fs::canonicalize(&staging)?;
    if !operation.starts_with(&staging) || operation == staging {
        return Err(format!(
            "staged operation is outside the verified installation: {}",
            operation.display()
        )
        .into());
    }
    Ok(operation)
}

fn plan(source: &str, target: &str) -> Result<(), Box<dyn std::error::Error>> {
    let source = DirectoryUpdateSource::new(source)?;
    let catalog = source.catalog()?;
    let target = verify_install_root(&PathBuf::from(target))?;
    let installation_id = installation_id_for_path(&target);
    let state = load_installed_state(&target.join("install-state.json"), &installation_id)?;
    let plan = plan_update(state.as_ref(), &catalog)?;
    println!("{}", serde_json::to_string_pretty(&plan)?);
    Ok(())
}

fn inspect(source: &str) -> Result<(), Box<dyn std::error::Error>> {
    let source = DirectoryUpdateSource::new(source)?;
    let catalog = source.catalog()?;
    println!("{}", serde_json::to_string_pretty(&catalog)?);
    Ok(())
}

fn print_usage() {
    eprintln!(
        "Usage:\n  deepx-updater maintain --interactive [--install-dir <directory>]\n  deepx-updater uninstall [--interactive|--quiet] [--install-dir <directory>] [--delete-user-data]\n  deepx-updater inspect <source-directory>\n  deepx-updater plan <source-directory> <install-directory>\n  deepx-updater stage <source-directory> <install-directory>\n  deepx-updater apply-staged <operation.json> <install-directory>\n  deepx-updater rollback-staged <operation.json> <install-directory>\n  deepx-updater handoff <operation.json> <install-directory> <wait-pid> <relaunch-exe>"
    );
}

#[cfg(test)]
mod tests {
    use super::MaintenanceOptions;
    use std::path::PathBuf;

    #[test]
    fn parses_interactive_maintenance_options() {
        let options = MaintenanceOptions::parse(&[
            "--interactive".to_string(),
            "--install-dir".to_string(),
            "C:/Users/Test/AppData/Local/Programs/DeepX".to_string(),
            "--delete-user-data".to_string(),
        ])
        .expect("maintenance options should parse");

        assert_eq!(
            options.install_dir,
            Some(PathBuf::from("C:/Users/Test/AppData/Local/Programs/DeepX"))
        );
        assert!(options.notify);
        assert!(options.delete_user_data);
        assert!(!options.quiet);
    }

    #[test]
    fn rejects_unknown_maintenance_options() {
        assert!(
            MaintenanceOptions::parse(&["--empty-update-means-uninstall".to_string()]).is_err()
        );
    }
}
