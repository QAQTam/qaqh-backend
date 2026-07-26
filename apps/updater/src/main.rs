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

mod colors {
    use egui::Color32;

    pub const ACCENT: Color32 = Color32::from_rgb(0, 122, 255);
    pub const ACCENT_SOFT: Color32 = Color32::from_rgb(235, 244, 255);
    pub const BACKGROUND: Color32 = Color32::from_rgb(247, 248, 250);
    pub const CARD: Color32 = Color32::WHITE;
    pub const BORDER: Color32 = Color32::from_rgb(222, 225, 230);
    pub const TEXT: Color32 = Color32::from_rgb(35, 37, 42);
    pub const TEXT_SECONDARY: Color32 = Color32::from_rgb(102, 106, 115);
    pub const DANGER: Color32 = Color32::from_rgb(215, 53, 47);
    pub const DANGER_SOFT: Color32 = Color32::from_rgb(255, 244, 243);
}

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
    installation: String,
    status: String,
    confirm_uninstall: bool,
    delete_user_data: bool,
}

impl MaintenanceApp {
    fn new(target: PathBuf, confirm_uninstall: bool) -> Self {
        let installation = installation_summary(&target);
        Self {
            target,
            source: String::new(),
            installation,
            status: "维护程序已就绪。".to_string(),
            confirm_uninstall,
            delete_user_data: false,
        }
    }
}

impl eframe::App for MaintenanceApp {
    fn update(&mut self, context: &egui::Context, _frame: &mut eframe::Frame) {
        egui::CentralPanel::default()
            .frame(
                egui::Frame::none()
                    .fill(colors::BACKGROUND)
                    .inner_margin(egui::Margin::symmetric(30.0, 24.0)),
            )
            .show(context, |ui| {
                ui.horizontal(|ui| {
                    let (icon_rect, _) =
                        ui.allocate_exact_size(egui::vec2(44.0, 44.0), egui::Sense::hover());
                    ui.painter().rect_filled(
                        icon_rect,
                        egui::Rounding::same(12.0),
                        colors::ACCENT_SOFT,
                    );
                    ui.painter().text(
                        icon_rect.center(),
                        egui::Align2::CENTER_CENTER,
                        "DX",
                        egui::FontId::proportional(17.0),
                        colors::ACCENT,
                    );
                    ui.add_space(10.0);
                    ui.vertical(|ui| {
                        ui.label(
                            egui::RichText::new("DeepX 维护")
                                .size(24.0)
                                .strong()
                                .color(colors::TEXT),
                        );
                        ui.label(
                            egui::RichText::new("修改、修复或安全卸载 DeepX")
                                .size(12.0)
                                .color(colors::TEXT_SECONDARY),
                        );
                    });
                });
                ui.add_space(20.0);

                Self::card().show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(
                        egui::RichText::new("当前安装")
                            .size(12.0)
                            .strong()
                            .color(colors::TEXT_SECONDARY),
                    );
                    ui.add_space(6.0);
                    ui.label(
                        egui::RichText::new(self.target.display().to_string())
                            .size(13.0)
                            .color(colors::TEXT),
                    );
                    ui.label(
                        egui::RichText::new(&self.installation)
                            .size(12.0)
                            .color(colors::TEXT_SECONDARY),
                    );
                });
                ui.add_space(14.0);

                Self::card().show(ui, |ui| {
                    ui.set_width(ui.available_width());
                    ui.label(
                        egui::RichText::new("修改或修复")
                            .size(17.0)
                            .strong()
                            .color(colors::TEXT),
                    );
                    ui.add_space(4.0);
                    ui.label(
                        egui::RichText::new("选择 DeepX Installer 生成的本地 update-source 目录。")
                            .size(12.0)
                            .color(colors::TEXT_SECONDARY),
                    );
                    ui.add_space(10.0);
                    ui.add(
                        egui::TextEdit::singleline(&mut self.source)
                            .hint_text("例如：D:\\DeepX\\packages\\update-source")
                            .desired_width(f32::INFINITY),
                    );
                    ui.add_space(10.0);
                    if ui
                        .add(
                            egui::Button::new(
                                egui::RichText::new("验证并暂存更新").color(egui::Color32::WHITE),
                            )
                            .fill(colors::ACCENT)
                            .rounding(egui::Rounding::same(7.0))
                            .min_size(egui::vec2(132.0, 34.0)),
                        )
                        .clicked()
                    {
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
                });
                ui.add_space(14.0);

                egui::Frame::none()
                    .fill(colors::DANGER_SOFT)
                    .rounding(egui::Rounding::same(10.0))
                    .stroke(egui::Stroke::new(
                        1.0_f32,
                        egui::Color32::from_rgb(242, 205, 202),
                    ))
                    .inner_margin(egui::Margin::same(16.0))
                    .show(ui, |ui| {
                        ui.set_width(ui.available_width());
                        ui.label(
                            egui::RichText::new(if self.confirm_uninstall {
                                "确认卸载 DeepX"
                            } else {
                                "卸载 DeepX"
                            })
                            .size(17.0)
                            .strong()
                            .color(colors::DANGER),
                        );
                        ui.add_space(4.0);
                        ui.label(
                            egui::RichText::new("将删除程序文件、快捷方式和 Windows 注册信息。")
                                .size(12.0)
                                .color(colors::TEXT_SECONDARY),
                        );
                        ui.add_space(10.0);
                        ui.checkbox(
                            &mut self.delete_user_data,
                            format!(
                                "同时删除全局用户数据：{}",
                                deepx_types::platform::data_dir().display()
                            ),
                        );
                        ui.label(
                            egui::RichText::new("工作区内的 .deepx 数据不会自动删除。")
                                .size(11.0)
                                .color(colors::TEXT_SECONDARY),
                        );
                        ui.add_space(12.0);

                        if !self.confirm_uninstall {
                            if ui
                                .add(
                                    egui::Button::new(
                                        egui::RichText::new("卸载 DeepX").color(colors::DANGER),
                                    )
                                    .fill(egui::Color32::WHITE)
                                    .stroke(egui::Stroke::new(1.0_f32, colors::DANGER))
                                    .rounding(egui::Rounding::same(7.0))
                                    .min_size(egui::vec2(112.0, 34.0)),
                                )
                                .clicked()
                            {
                                self.confirm_uninstall = true;
                            }
                        } else {
                            ui.label(
                                egui::RichText::new("此操作无法撤销，请确认后继续。")
                                    .size(12.0)
                                    .strong()
                                    .color(colors::DANGER),
                            );
                            ui.add_space(8.0);
                            ui.horizontal(|ui| {
                                if ui
                                    .add(
                                        egui::Button::new(
                                            egui::RichText::new("确认卸载")
                                                .color(egui::Color32::WHITE),
                                        )
                                        .fill(colors::DANGER)
                                        .rounding(egui::Rounding::same(7.0))
                                        .min_size(egui::vec2(112.0, 34.0)),
                                    )
                                    .clicked()
                                {
                                    match maintenance::handoff_uninstall(
                                        &self.target,
                                        self.delete_user_data,
                                        true,
                                    ) {
                                        Ok(_) => {
                                            context.send_viewport_cmd(egui::ViewportCommand::Close)
                                        }
                                        Err(error) => {
                                            self.status = format!("无法启动卸载：{error}")
                                        }
                                    }
                                }
                                if ui
                                    .add(
                                        egui::Button::new("取消")
                                            .fill(egui::Color32::WHITE)
                                            .stroke(egui::Stroke::new(1.0_f32, colors::BORDER))
                                            .rounding(egui::Rounding::same(7.0))
                                            .min_size(egui::vec2(80.0, 34.0)),
                                    )
                                    .clicked()
                                {
                                    self.confirm_uninstall = false;
                                }
                            });
                        }
                    });

                ui.add_space(14.0);
                let status_color = if self.status.contains("失败")
                    || self.status.contains("无法")
                    || self.status.contains("请先")
                {
                    colors::DANGER
                } else {
                    colors::TEXT_SECONDARY
                };
                ui.label(
                    egui::RichText::new(&self.status)
                        .size(12.0)
                        .color(status_color),
                );
            });
    }
}

impl MaintenanceApp {
    fn card() -> egui::Frame {
        egui::Frame::none()
            .fill(colors::CARD)
            .rounding(egui::Rounding::same(10.0))
            .stroke(egui::Stroke::new(1.0_f32, colors::BORDER))
            .inner_margin(egui::Margin::same(16.0))
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
            .with_inner_size([680.0, 620.0])
            .with_resizable(false),
        ..Default::default()
    };
    eframe::run_native(
        "DeepXMaintenance",
        options,
        Box::new(move |creation| {
            setup_fonts(&creation.egui_ctx);
            setup_style(&creation.egui_ctx);
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

fn setup_style(context: &egui::Context) {
    context.style_mut(|style| {
        style.spacing.item_spacing = egui::vec2(8.0, 8.0);
        style.spacing.button_padding = egui::vec2(14.0, 7.0);
        style.visuals.dark_mode = false;
        style.visuals.panel_fill = colors::BACKGROUND;
        style.visuals.window_fill = colors::BACKGROUND;
        style.visuals.extreme_bg_color = egui::Color32::WHITE;
        style.visuals.faint_bg_color = colors::BACKGROUND;
        style.visuals.selection.bg_fill = colors::ACCENT;
        style.visuals.widgets.inactive.rounding = egui::Rounding::same(7.0);
        style.visuals.widgets.hovered.rounding = egui::Rounding::same(7.0);
        style.visuals.widgets.active.rounding = egui::Rounding::same(7.0);
        style.visuals.widgets.noninteractive.fg_stroke.color = colors::TEXT;
        style.visuals.widgets.inactive.fg_stroke.color = colors::TEXT;
        style.visuals.window_shadow = egui::epaint::Shadow::NONE;
    });
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
