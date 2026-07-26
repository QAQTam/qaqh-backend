// DeepX 卸载器 — egui
//   注册表读安装路径 → 提权 → TEMP 副本 → 安全删除 → 零孤儿
//   线��：确认 → 后台线程删除 → 完成

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui::*;
use std::fs;
use std::path::Path;
use std::sync::mpsc;
use std::thread;

mod color {
    use egui::Color32;
    pub const ACCENT: Color32          = Color32::from_rgb(0, 122, 255);
    pub const DANGER: Color32          = Color32::from_rgb(255, 59, 48);
    pub const SUCCESS: Color32         = Color32::from_rgb(52, 199, 89);
    pub const TEXT_PRIMARY: Color32    = Color32::from_rgb(50, 50, 55);
    pub const TEXT_SECONDARY: Color32  = Color32::from_rgb(90, 90, 95);
    pub const TEXT_MUTED: Color32      = Color32::from_rgb(140, 140, 145);
    pub const BG: Color32              = Color32::from_rgb(255, 255, 255);
}

// ============================================================
// CLI 参数
// ============================================================
struct AppArgs {
    install_dir: Option<String>,
    from_temp: bool,
    delete_config: bool,
}

fn parse_args() -> AppArgs {
    let raw: Vec<String> = std::env::args().collect();
    let mut a = AppArgs { install_dir: None, from_temp: false, delete_config: false };
    let mut i = 1;
    while i < raw.len() {
        match raw[i].as_str() {
            "--install-dir" => { i += 1; if i < raw.len() { a.install_dir = Some(raw[i].clone()); } }
            "--from-temp" => a.from_temp = true,
            "--delete-config" => a.delete_config = true,
            _ => {}
        }
        i += 1;
    }
    a
}

// ============================================================
// 入口 — 阶段机
// ============================================================

fn main() -> Result<(), eframe::Error> {
    let mut args = parse_args();

    // --- 阶段 0: 提权（只要不是 elevated 就提） ---
    if !already_elevated() {
        elevate_self(&args);
        return Ok(());
    }

    // --- 阶段 1: 确定安装目录 ---
    if args.install_dir.is_none() {
        args.install_dir = read_install_dir_from_registry();
    }
    if args.install_dir.is_none() {
        if let Ok(exe) = std::env::current_exe() {
            if let Some(p) = exe.parent() {
                args.install_dir = Some(p.to_string_lossy().to_string());
            }
        }
    }
    let install_dir = args.install_dir.clone().unwrap_or_default();

    // --- 阶段 2: TEMP 副本（加一次就够了） ---
    if !args.from_temp {
        if let Some(temp_exe) = copy_to_temp() {
            launch_from_temp(&temp_exe, &args);
            return Ok(());
        }
    }

    // --- 阶段 3: 启动 egui UI ---
    let options = eframe::NativeOptions {
        viewport: ViewportBuilder::default()
            .with_inner_size([520.0, 380.0])
            .with_resizable(false)
            .with_title("DeepX 卸载"),
        ..Default::default()
    };

    eframe::run_native(
        "DeepXUninstaller",
        options,
        Box::new(move |cc| {
            setup_chinese_fonts(&cc.egui_ctx);
            setup_style(&cc.egui_ctx);
            Ok(Box::new(UninstallApp {
                install_dir,
                delete_config: args.delete_config,
                screen: Screen::Confirm,
                progress: 0.0_f32,
                current_item: String::new(),
                error: None,
                receiver: None,
            }))
        }),
    )
}

// ============================================================
// 后台线程消息
// ============================================================
enum UninstallMsg {
    Tick { item: String, pct: f32 },
    Done(Result<(), String>),
}

// ============================================================
// UI 状态
// ============================================================
#[derive(PartialEq)]
enum Screen { Confirm, Progress, Done }

struct UninstallApp {
    install_dir: String,
    delete_config: bool,
    screen: Screen,
    progress: f32,
    current_item: String,
    error: Option<String>,
    receiver: Option<mpsc::Receiver<UninstallMsg>>,
}

impl eframe::App for UninstallApp {
    fn update(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        // 轮询后台线程消息
        let mut msgs: Vec<UninstallMsg> = Vec::new();
        if let Some(ref rx) = self.receiver {
            while let Ok(msg) = rx.try_recv() {
                msgs.push(msg);
            }
        }
        for msg in msgs {
            match msg {
                UninstallMsg::Tick { item, pct } => {
                    self.current_item = item;
                    self.progress = pct;
                }
                UninstallMsg::Done(result) => {
                    self.error = result.err();
                    self.receiver = None;
                    self.screen = Screen::Done;
                }
            }
        }

        if self.screen == Screen::Progress {
            ctx.request_repaint();
        }

        CentralPanel::default()
            .frame(Frame::none().inner_margin(Margin::symmetric(32.0, 24.0)))
            .show(ctx, |ui| match self.screen {
                Screen::Confirm => self.ui_confirm(ui),
                Screen::Progress => self.ui_progress(ui),
                Screen::Done => self.ui_done(ui),
            });
    }
}

// ============================================================
// 确认页
// ============================================================
impl UninstallApp {
    fn ui_confirm(&mut self, ui: &mut Ui) {
        ui.vertical_centered(|ui| {
            ui.add_space(30.0);
            ui.label(RichText::new("卸载 DeepX").size(22.0).strong());
            ui.add_space(12.0);
            ui.label(RichText::new("确定要从此计算机中移除 DeepX 吗？").size(14.0).color(color::TEXT_SECONDARY));
            ui.add_space(20.0);

            Frame::none()
                .fill(Color32::from_rgb(248, 248, 250))
                .rounding(Rounding::same(8.0))
                .inner_margin(Margin::same(12.0))
                .show(ui, |ui| {
                    ui.set_width(380.0);
                    ui.label(RichText::new("安装路径:").size(12.0).color(color::TEXT_SECONDARY));
                    ui.label(RichText::new(&self.install_dir).size(13.0));
                });
        });

        let home = dirs::home_dir()
            .map(|p| p.join(".deepx").to_string_lossy().to_string())
            .unwrap_or_else(|| "%USERPROFILE%\\.deepx".to_string());

        ui.add_space(16.0);
        ui.checkbox(
            &mut self.delete_config,
            &format!("同时删除用户配置文件 ({})", home),
        );

        ui.add_space(24.0);
        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            let btn = Button::new(RichText::new("卸载").color(Color32::WHITE).size(14.0))
                .fill(color::DANGER)
                .rounding(Rounding::same(6.0))
                .min_size(Vec2::new(100.0, 34.0));
            if ui.add(btn).clicked() {
                self.start_uninstall();
            }
            if ui.add(Button::new(RichText::new("取消").size(14.0)).fill(Color32::TRANSPARENT).min_size(Vec2::new(80.0, 34.0))).clicked() {
                std::process::exit(0);
            }
        });
    }

    fn start_uninstall(&mut self) {
        let install_dir = self.install_dir.clone();
        let delete_config = self.delete_config;
        let (tx, rx) = mpsc::channel();
        self.receiver = Some(rx);
        self.screen = Screen::Progress;

        thread::spawn(move || {
            let steps: &[(&str, fn(&str, bool) -> Result<(), String>)] = &[
                ("关闭运行进程", stop_running_processes),
                ("删除快捷方式", delete_shortcuts),
                ("删除注册表信息", |_, _| delete_registry_key()),
                ("清理用户配置", |_, del| if del { delete_user_config() } else { Ok(()) }),
                ("删除程序文件", |dir, _| delete_install_dir(dir)),
                ("清理残留", |_, _| schedule_temp_self_delete()),
            ];

            for (i, (name, f)) in steps.iter().enumerate() {
                tx.send(UninstallMsg::Tick { item: name.to_string(), pct: i as f32 / steps.len() as f32 }).ok();
                if let Err(e) = f(&install_dir, delete_config) {
                    tx.send(UninstallMsg::Done(Err(e))).ok();
                    return;
                }
            }
            tx.send(UninstallMsg::Done(Ok(()))).ok();
        });
    }
}

// ============================================================
// 进度页
// ============================================================
impl UninstallApp {
    fn ui_progress(&self, ui: &mut Ui) {
        ui.vertical_centered(|ui| {
            ui.add_space(50.0);
            ui.label(RichText::new("正在卸载 DeepX...").size(20.0).strong());
            ui.add_space(20.0);
            ui.add(ProgressBar::new(self.progress).desired_width(360.0).text(format!("{:.0}%", self.progress * 100.0)));
            ui.add_space(12.0);
            if !self.current_item.is_empty() {
                ui.label(RichText::new(&self.current_item).size(12.0).color(color::TEXT_SECONDARY));
            }
            if let Some(ref e) = self.error {
                ui.add_space(8.0);
                ui.colored_label(color::DANGER, e);
            }
        });
    }
}

// ============================================================
// 完成页
// ============================================================
impl UninstallApp {
    fn ui_done(&self, ui: &mut Ui) {
        ui.vertical_centered(|ui| {
            ui.add_space(50.0);
            let ok = self.error.is_none();
            let (icon, clr) = if ok { ("✓", color::SUCCESS) } else { ("✗", color::DANGER) };
            let rect = Rect::from_min_size(ui.next_widget_position(), Vec2::splat(56.0));
            ui.painter().circle_filled(rect.center(), 28.0, clr);
            ui.painter().text(rect.center(), Align2::CENTER_CENTER, icon, FontId::proportional(26.0), Color32::WHITE);
            ui.advance_cursor_after_rect(rect);
            ui.add_space(12.0);

            if ok {
                ui.label(RichText::new("���载完成").size(20.0).strong());
                ui.add_space(8.0);
                ui.label("DeepX 已从您的计算机中移除。");
            } else {
                ui.label(RichText::new("卸载出错").size(20.0).strong().color(color::DANGER));
                if let Some(ref e) = self.error {
                    ui.add_space(8.0);
                    ui.label(e);
                }
            }
            ui.add_space(24.0);
            if ui.add(Button::new(RichText::new("关闭").color(Color32::WHITE)).fill(color::ACCENT).rounding(Rounding::same(6.0)).min_size(Vec2::new(100.0, 34.0))).clicked() {
                std::process::exit(0);
            }
        });
    }
}

// ============================================================
// 实际删除操作（在后台线程中执行）
// ============================================================

/// 关闭运行中的 DeepX 进程（先 HTTP 优雅停止 daemon，再强杀残留）
fn stop_running_processes(_dir: &str, _cfg: bool) -> Result<(), String> {
    // 第一步：通过 daemon HTTP 端点优雅停止
    stop_daemon_via_http();

    // 第二步：强制终止任何剩余的 DeepX/daemon 进程
    let procs = ["DeepX.exe", "deepx-daemon.exe"];
    for name in &procs {
        // 用 tasklist 查找 PID
        if let Ok(out) = std::process::Command::new("tasklist")
            .args(["/fo", "csv", "/nh", "/fi", &format!("imagename eq {}", name)])
            .output()
        {
            let text = String::from_utf8_lossy(&out.stdout);
            for line in text.lines() {
                let parts: Vec<&str> = line.split(',').map(|s| s.trim_matches('"').trim()).collect();
                if parts.len() >= 2 {
                    if let Ok(pid) = parts[1].parse::<u32>() {
                        // 强制终止 + 子进程树
                        let _ = std::process::Command::new("taskkill")
                            .args(["/f", "/t", "/pid", &pid.to_string()])
                            .output();
                    }
                }
            }
        }
    }
    Ok(())
}

/// 读取 discovery 文件并发送 HTTP stop 请求
fn stop_daemon_via_http() {
    use std::io::Write;

    let home = std::env::var("USERPROFILE").unwrap_or_default();
    let discovery_path = std::path::PathBuf::from(&home).join(".deepx").join("daemon.json");

    let content = match std::fs::read_to_string(&discovery_path) {
        Ok(c) => c,
        Err(_) => return,
    };

    let discovery: serde_json::Value = match serde_json::from_str(&content) {
        Ok(v) => v,
        Err(_) => return,
    };

    let endpoint = discovery.get("endpoint").and_then(|v| v.as_str()).unwrap_or("");
    let token = discovery.get("token").and_then(|v| v.as_str()).unwrap_or("");
    if endpoint.is_empty() || token.is_empty() { return; }

    let addr = endpoint
        .trim_start_matches("ws://")
        .split('/')
        .next()
        .unwrap_or("");

    let Ok(socket_addr) = addr.parse::<std::net::SocketAddr>() else { return; };

    let Ok(mut stream) =
        std::net::TcpStream::connect_timeout(&socket_addr, std::time::Duration::from_secs(2))
    else {
        return;
    };

    let request = format!(
        "POST /control/v1/stop HTTP/1.1\r\nHost: {}\r\nAuthorization: Bearer {}\r\nContent-Length: 0\r\nConnection: close\r\n\r\n",
        addr, token
    );

    let _ = stream.write_all(request.as_bytes());
}

fn delete_shortcuts(_dir: &str, _cfg: bool) -> Result<(), String> {
    if let Some(d) = dirs::desktop_dir() {
        let lnk = d.join("DeepX.lnk");
        let _ = fs::remove_file(&lnk);
    }
    if let Some(d) = dirs::data_dir() {
        let p = d.join(r"Microsoft\Windows\Start Menu\Programs\DeepX");
        let _ = fs::remove_dir_all(&p);
    }
    Ok(())
}

fn delete_user_config() -> Result<(), String> {
    // 后端 deepx-types/platform.rs 定义的唯一数据路径
    if let Some(home) = dirs::home_dir() {
        let dot = home.join(".deepx");
        if dot.exists() {
            fs::remove_dir_all(&dot).map_err(|e| format!("删除用户配置失败: {}", e))?;
        }
    }
    Ok(())
}

fn delete_install_dir(dir: &str) -> Result<(), String> {
    let p = Path::new(dir);
    if p.exists() {
        fs::remove_dir_all(p).map_err(|e| format!("无法删除安装目录: {}", e))?;
    }
    Ok(())
}

// ============================================================
// 注册表
// ============================================================

#[cfg(windows)]
fn read_install_dir_from_registry() -> Option<String> {
    use windows::Win32::System::Registry::{RegGetValueW, HKEY_CURRENT_USER, RRF_RT_REG_SZ};
    let sub: Vec<u16> = "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\DeepX".encode_utf16().chain(std::iter::once(0)).collect();
    let val: Vec<u16> = "InstallLocation".encode_utf16().chain(std::iter::once(0)).collect();
    let mut buf = vec![0u8; 1024];
    let mut len = buf.len() as u32;
    unsafe {
        if RegGetValueW(HKEY_CURRENT_USER, wstr(&sub), wstr(&val), RRF_RT_REG_SZ, None, Some(buf.as_mut_ptr() as *mut _), Some(&mut len)).is_ok() {
            let data = &buf[..len as usize];
            if let Some(end) = data.iter().position(|&b| b == 0) {
                let u16s: Vec<u16> = data[..end].chunks_exact(2).map(|c| u16::from_le_bytes([c[0], c[1]])).collect();
                return String::from_utf16(&u16s).ok();
            }
        }
    }
    None
}

#[cfg(not(windows))]
fn read_install_dir_from_registry() -> Option<String> { None }

#[cfg(windows)]
fn delete_registry_key() -> Result<(), String> {
    use windows::Win32::System::Registry::{RegDeleteTreeW, RegOpenKeyExW, HKEY_CURRENT_USER, KEY_SET_VALUE};
    let parent: Vec<u16> = "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall".encode_utf16().chain(std::iter::once(0)).collect();
    let sub: Vec<u16> = "DeepX".encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let mut h = std::mem::zeroed();
        if RegOpenKeyExW(HKEY_CURRENT_USER, wstr(&parent), 0, KEY_SET_VALUE, &mut h).is_ok() {
            let _ = RegDeleteTreeW(h, wstr(&sub));
            let _ = windows::Win32::System::Registry::RegCloseKey(h);
        }
    }
    Ok(())
}

#[cfg(not(windows))]
fn delete_registry_key() -> Result<(), String> { Ok(()) }

// ============================================================
// 提权
// ============================================================

#[cfg(windows)]
fn already_elevated() -> bool {
    // 简单检测：尝试打开仅 admin 可写的注册表键
    use windows::Win32::System::Registry::{RegOpenKeyExW, HKEY_LOCAL_MACHINE, KEY_WRITE};
    let key: Vec<u16> = "SOFTWARE".encode_utf16().chain(std::iter::once(0)).collect();
    unsafe {
        let mut h = std::mem::zeroed();
        if RegOpenKeyExW(HKEY_LOCAL_MACHINE, wstr(&key), 0, KEY_WRITE, &mut h).is_ok() {
            let _ = windows::Win32::System::Registry::RegCloseKey(h);
            true
        } else {
            false
        }
    }
}

#[cfg(not(windows))]
fn already_elevated() -> bool { true }

#[cfg(windows)]
fn elevate_self(args: &AppArgs) {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOW;
    let exe = std::env::current_exe().unwrap_or_default();
    let exe_s = exe.to_string_lossy();
    let mut cmd = format!("\"{}\" --elevated", exe_s);
    if let Some(ref d) = args.install_dir { cmd.push_str(&format!(" --install-dir \"{}\"", d)); }
    if args.from_temp { cmd.push_str(" --from-temp"); }
    if args.delete_config { cmd.push_str(" --delete-config"); }

    let op: Vec<u16> = "runas".encode_utf16().chain(std::iter::once(0)).collect();
    let f: Vec<u16> = exe_s.encode_utf16().chain(std::iter::once(0)).collect();
    let p: Vec<u16> = cmd.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe { ShellExecuteW(None, wstr(&op), wstr(&f), wstr(&p), None, SW_SHOW); }
}

#[cfg(not(windows))]
fn elevate_self(_: &AppArgs) {}

// ============================================================
// TEMP 副本
// ============================================================

#[cfg(windows)]
fn copy_to_temp() -> Option<String> {
    let exe = std::env::current_exe().ok()?;
    let temp = std::env::var("TEMP").unwrap_or_else(|_| "C:\\Windows\\Temp".to_string());
    let dest = Path::new(&temp).join("deepx_uninstall.exe");
    fs::copy(&exe, &dest).ok()?;
    Some(dest.to_string_lossy().to_string())
}

#[cfg(not(windows))]
fn copy_to_temp() -> Option<String> { None }

#[cfg(windows)]
fn launch_from_temp(temp_exe: &str, args: &AppArgs) {
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOW;
    let mut cmd = format!("\"{}\" --from-temp --elevated", temp_exe);
    if let Some(ref d) = args.install_dir { cmd.push_str(&format!(" --install-dir \"{}\"", d)); }
    if args.delete_config { cmd.push_str(" --delete-config"); }
    let op: Vec<u16> = "open".encode_utf16().chain(std::iter::once(0)).collect();
    let f: Vec<u16> = temp_exe.encode_utf16().chain(std::iter::once(0)).collect();
    let p: Vec<u16> = cmd.encode_utf16().chain(std::iter::once(0)).collect();
    unsafe { ShellExecuteW(None, wstr(&op), wstr(&f), wstr(&p), None, SW_SHOW); }
}

#[cfg(not(windows))]
fn launch_from_temp(_: &str, _: &AppArgs) {}

fn schedule_temp_self_delete() -> Result<(), String> {
    #[cfg(windows)]
    {
        use windows::Win32::Storage::FileSystem::MoveFileExW;
        let exe = std::env::current_exe().map_err(|e| format!("{}", e))?;
        let s = exe.to_string_lossy();
        let w: Vec<u16> = s.encode_utf16().chain(std::iter::once(0)).collect();
        unsafe { MoveFileExW(wstr(&w), None, windows::Win32::Storage::FileSystem::MOVEFILE_DELAY_UNTIL_REBOOT) }
            .map_err(|e| format!("MoveFileEx 失败: {:?}", e))?;
    }
    Ok(())
}

// ============================================================
// 工具
// ============================================================

fn setup_chinese_fonts(ctx: &Context) {
    let mut fonts = FontDefinitions::default();
    for p in &[r"C:\Windows\Fonts\Deng.ttf", r"C:\Windows\Fonts\simfang.ttf", r"C:\Windows\Fonts\simkai.ttf"] {
        if let Ok(b) = std::fs::read(p) {
            fonts.font_data.insert("cjk".to_owned(), FontData::from_owned(b));
            for f in [FontFamily::Proportional, FontFamily::Monospace] {
                fonts.families.entry(f).or_default().insert(0, "cjk".to_owned());
            }
            break;
        }
    }
    ctx.set_fonts(fonts);
}

fn setup_style(ctx: &Context) {
    ctx.style_mut(|style| {
        style.visuals.dark_mode = false;
        style.visuals.panel_fill = color::BG;
        style.visuals.window_fill = color::BG;
        style.visuals.widgets.noninteractive.fg_stroke.color = color::TEXT_PRIMARY;
        style.visuals.widgets.inactive.fg_stroke.color = color::TEXT_PRIMARY;
        style.visuals.widgets.active.fg_stroke.color = Color32::BLACK;
        style.visuals.widgets.inactive.rounding = Rounding::same(6.0);
        style.visuals.widgets.hovered.rounding = Rounding::same(6.0);
        style.visuals.widgets.active.rounding = Rounding::same(6.0);
        style.visuals.window_shadow = egui::epaint::Shadow::NONE;
    });
}

#[cfg(windows)]
fn wstr(v: &[u16]) -> windows::core::PCWSTR { windows::core::PCWSTR::from_raw(v.as_ptr()) }
