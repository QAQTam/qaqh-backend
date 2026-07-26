// DeepX 安装程序 — egui/eframe
// macOS 风格 UI：左侧步骤导航 + 右侧内容区 + 底部按钮栏

#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

use eframe::egui::*;
use std::sync::mpsc;
use std::thread;

mod install;
mod win_process;

// ============================================================
// 配色常量
// ============================================================

mod colors {
    use egui::Color32;
    pub const ACCENT: Color32 = Color32::from_rgb(0, 122, 255);      // macOS 蓝
    pub const SIDEBAR_BG: Color32 = Color32::from_rgb(245, 245, 247); // 浅灰侧边栏
    pub const SIDEBAR_TEXT: Color32 = Color32::from_rgb(50, 50, 55);  // 深色文字
    pub const SIDEBAR_ACTIVE: Color32 = Color32::from_rgb(0, 0, 0);
    pub const CONTENT_BG: Color32 = Color32::from_rgb(255, 255, 255);
    pub const SUCCESS: Color32 = Color32::from_rgb(52, 199, 89);     // 绿色
    pub const DANGER: Color32 = Color32::from_rgb(255, 59, 48);      // 红色
    pub const BORDER: Color32 = Color32::from_rgb(200, 200, 205);
    pub const TEXT_SECONDARY: Color32 = Color32::from_rgb(90, 90, 95); // 次级文字
    pub const TEXT_MUTED: Color32 = Color32::from_rgb(140, 140, 145);  // 更浅（禁用态等）
    pub const STEP_DOT_SIZE: f32 = 28.0;
}

// ============================================================
// 入口
// ============================================================

fn main() -> Result<(), eframe::Error> {
    let args: Vec<String> = std::env::args().collect();

    // ── Headless patch mode: --patch <source_payload> <target_dir> ──
    if args.len() >= 4 && args[1] == "--patch" {
        let source = &args[2];
        let target = &args[3];
        match install::run_patch(source, target) {
            Ok(()) => std::process::exit(0),
            Err(e) => {
                eprintln!("patch failed: {e}");
                std::process::exit(1);
            }
        }
    }

    // ── Normal GUI installer ──
    let options = eframe::NativeOptions {
        viewport: ViewportBuilder::default()
            .with_inner_size([740.0, 500.0])
            .with_resizable(false)
            .with_title("DeepX 安装程序"),
        ..Default::default()
    };

    eframe::run_native(
        "DeepXInstaller",
        options,
        Box::new(|cc| {
            setup_chinese_fonts(&cc.egui_ctx);
            setup_style(&cc.egui_ctx);
            Ok(Box::new(App::default()))
        }),
    )
}

fn setup_chinese_fonts(ctx: &Context) {
    let mut fonts = FontDefinitions::default();
    let font_paths = [
        r"C:\Windows\Fonts\Deng.ttf",
        r"C:\Windows\Fonts\Dengb.ttf",
        r"C:\Windows\Fonts\simfang.ttf",
        r"C:\Windows\Fonts\simkai.ttf",
        r"C:\Windows\Fonts\simhei.ttf",
    ];
    for path in &font_paths {
        if let Ok(bytes) = std::fs::read(path) {
            fonts.font_data.insert("chinese_font".to_owned(), FontData::from_owned(bytes));
            for family in [FontFamily::Proportional, FontFamily::Monospace] {
                fonts.families.entry(family).or_default().insert(0, "chinese_font".to_owned());
            }
            break;
        }
    }
    ctx.set_fonts(fonts);
}

fn setup_style(ctx: &Context) {
    ctx.style_mut(|style| {
        // 强制亮色模式
        style.visuals.dark_mode = false;
        style.visuals.panel_fill = colors::CONTENT_BG;
        style.visuals.window_fill = colors::CONTENT_BG;
        // widget 背景
        style.visuals.widgets.noninteractive.bg_fill = Color32::TRANSPARENT;
        style.visuals.widgets.inactive.bg_fill = Color32::TRANSPARENT;
        style.visuals.widgets.hovered.bg_fill = Color32::from_rgba_premultiplied(0, 122, 255, 20);
        style.visuals.widgets.active.bg_fill = Color32::from_rgba_premultiplied(0, 122, 255, 40);
        // widget 文字色
        style.visuals.widgets.noninteractive.fg_stroke.color = colors::SIDEBAR_TEXT;
        style.visuals.widgets.inactive.fg_stroke.color = colors::SIDEBAR_TEXT;
        style.visuals.widgets.active.fg_stroke.color = colors::SIDEBAR_ACTIVE;
        // 选择态
        style.visuals.selection.bg_fill = colors::ACCENT;
        // 圆角
        style.visuals.widgets.inactive.rounding = Rounding::same(6.0);
        style.visuals.widgets.hovered.rounding = Rounding::same(6.0);
        style.visuals.widgets.active.rounding = Rounding::same(6.0);
        // 无阴影
        style.visuals.window_shadow = egui::epaint::Shadow::NONE;
    });
}

// ============================================================
// 枚举
// ============================================================

#[derive(Default, PartialEq, Clone, Copy)]
enum Screen {
    #[default]
    Welcome,
    License,
    Location,
    Components,
    CloseProcesses,
    Progress,
    Finish,
}

impl Screen {
    fn all() -> &'static [Screen] {
        &[Screen::Welcome, Screen::License, Screen::Location, Screen::Components]
    }

    fn title(&self) -> &'static str {
        match self {
            Screen::Welcome => "欢迎",
            Screen::License => "许可协议",
            Screen::Location => "安装位置",
            Screen::Components => "安装组件",
            Screen::CloseProcesses => "关闭进程",
            Screen::Progress => "正在安装",
            Screen::Finish => "完成",
        }
    }

    fn subtitle(&self) -> &'static str {
        match self {
            Screen::Welcome => "本向导将引导您完成 DeepX 的安装配置。",
            Screen::License => "��阅读并接受许可协议以继续。",
            Screen::Location => "选择 DeepX 的安装目录。",
            Screen::Components => "选择要安装的组件与快捷方式。",
            Screen::CloseProcesses => "检测到 DeepX 正在运行，请先关闭以继续安装。",
            Screen::Progress => "正在将文件复制到您的计算机...",
            Screen::Finish => "",
        }
    }

    fn step_index(&self) -> usize {
        match self {
            Screen::Welcome => 0,
            Screen::License => 1,
            Screen::Location => 2,
            Screen::Components => 3,
            Screen::CloseProcesses | Screen::Progress | Screen::Finish => 4,
        }
    }
}

enum InstallMsg {
    Progress(install::InstallerConfig),
    Done(Result<(), String>),
}

// ============================================================
// 主应用状态
// ============================================================

struct App {
    screen: Screen,
    config: install::InstallerConfig,
    license_agreed: bool,
    license_text: String,
    install_result: Option<Result<(), String>>,
    install_receiver: Option<mpsc::Receiver<InstallMsg>>,
    location_input: String,
    /// 检测到的运行中 DeepX 进程
    running_procs: Vec<win_process::ProcInfo>,
    /// 是否已尝试关闭进程
    close_attempted: bool,
}

impl Default for App {
    fn default() -> Self {
        Self {
            screen: Screen::Welcome,
            config: install::InstallerConfig {
                target_path: install::InstallerConfig::default_path(),
                install_desktop_app: true,
                create_start_menu: true,
                create_desktop_shortcut: true,
                ..Default::default()
            },
            license_agreed: false,
            license_text: LICENSE_TEXT.to_string(),
            install_result: None,
            install_receiver: None,
            location_input: install::InstallerConfig::default_path(),
            running_procs: win_process::find_deepx_processes(),
            close_attempted: false,
        }
    }
}

impl eframe::App for App {
    fn update(&mut self, ctx: &Context, _frame: &mut eframe::Frame) {
        self.poll_install(ctx);
        let is_install_phase = matches!(self.screen, Screen::CloseProcesses | Screen::Progress | Screen::Finish);

        // 左侧步骤导航
        if !is_install_phase {
            SidePanel::left("steps")
                .resizable(false)
                .default_width(170.0)
                .show_separator_line(false)
                .frame(Frame::none().fill(colors::SIDEBAR_BG))
                .show(ctx, |ui| {
                    self.render_sidebar(ui);
                });
        }

        // 导航栏（底部）
        TopBottomPanel::bottom("nav")
            .resizable(false)
            .min_height(if is_install_phase { 0.0 } else { 52.0 })
            .show_separator_line(!is_install_phase)
            .frame(Frame::none().fill(colors::CONTENT_BG).inner_margin(Margin::symmetric(16.0, 10.0)))
            .show(ctx, |ui| {
                if !is_install_phase {
                    self.render_nav_bar(ui);
                }
            });

        // 主内容区
        CentralPanel::default()
            .frame(Frame::none().fill(colors::CONTENT_BG).inner_margin(Margin::symmetric(32.0, 20.0)))
            .show(ctx, |ui| {
                match self.screen {
                    Screen::Welcome => self.render_welcome(ui),
                    Screen::License => self.render_license(ui),
                    Screen::Location => self.render_location(ui),
                    Screen::Components => self.render_components(ui),
                    Screen::CloseProcesses => self.render_close_processes(ui),
                    Screen::Progress => self.render_progress(ui),
                    Screen::Finish => self.render_finish(ui),
                }
            });
    }
}

// ============================================================
// 左侧步骤导航
// ============================================================

impl App {
    fn render_sidebar(&self, ui: &mut Ui) {
        ui.add_space(28.0);
        ui.label(RichText::new("安装步骤").size(13.0).color(colors::TEXT_SECONDARY).strong());
        ui.add_space(20.0);

        let current = self.screen.step_index();

        for (i, step) in Screen::all().iter().enumerate() {
            let (dot_color, text_color, dot_text) = if i < current {
                // 已完成
                (colors::SUCCESS, colors::SIDEBAR_TEXT, "✓".to_string())
            } else if i == current {
                // 当前
                (colors::ACCENT, colors::SIDEBAR_ACTIVE, (i + 1).to_string())
            } else {
                // 待完成
                (colors::BORDER, colors::TEXT_SECONDARY, (i + 1).to_string())
            };

            ui.horizontal(|ui| {
                // 圆点
                let dot_rect = Rect::from_min_size(
                    ui.next_widget_position(),
                    Vec2::splat(colors::STEP_DOT_SIZE),
                );
                ui.painter().circle_filled(
                    dot_rect.center(),
                    colors::STEP_DOT_SIZE / 2.0,
                    dot_color,
                );
                ui.painter().text(
                    dot_rect.center(),
                    Align2::CENTER_CENTER,
                    dot_text,
                    FontId::proportional(13.0),
                    Color32::WHITE,
                );
                // 推进光标，否则下一个 widget 会和圆点重叠
                ui.advance_cursor_after_rect(dot_rect);

                ui.add_space(10.0);

                // 步骤名
                let label = RichText::new(step.title()).size(13.0).color(text_color);
                let label = if i == current { label.strong() } else { label };
                ui.label(label);
            });

            ui.add_space(16.0);

            // 连接线（简化：用 spacing 替代）
            if i < Screen::all().len() - 1 {}
        }

        // 右下角版本号
        ui.with_layout(Layout::bottom_up(Align::LEFT), |ui| {
            ui.add_space(8.0);
            ui.label(
                RichText::new(format!("v{}", env!("CARGO_PKG_VERSION")))
                    .size(11.0)
                    .color(colors::TEXT_SECONDARY),
            );
        });
    }
}

// ============================================================
// 底部导航栏
// ============================================================

impl App {
    fn render_nav_bar(&mut self, ui: &mut Ui) {
        let can_back = self.screen != Screen::Welcome && self.screen != Screen::CloseProcesses;
        let can_next = match self.screen {
            Screen::Welcome => true,
            Screen::License => self.license_agreed,
            Screen::Location => !self.location_input.trim().is_empty(),
            Screen::Components => self.config.install_desktop_app,
            Screen::CloseProcesses => false, // 按钮在内容区自己控制
            _ => true,
        };

        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            // 取消按钮
            let cancel = Button::new(RichText::new("取消").color(colors::TEXT_SECONDARY))
                .fill(Color32::TRANSPARENT)
                .min_size(Vec2::new(80.0, 30.0));
            if ui.add(cancel).clicked() {
                std::process::exit(0);
            }

            // 下一步 / 安装按钮
            let next_label = match self.screen {
                Screen::Components => "安装",
                _ => "继续",
            };
            let next_btn = if can_next {
                Button::new(RichText::new(next_label).color(Color32::WHITE).size(13.0))
                    .fill(colors::ACCENT)
                    .rounding(Rounding::same(6.0))
                    .min_size(Vec2::new(90.0, 30.0))
            } else {
                Button::new(RichText::new(next_label).color(Color32::from_rgb(130, 130, 135)).size(13.0))
                    .fill(Color32::from_rgb(230, 230, 235))
                    .rounding(Rounding::same(6.0))
                    .min_size(Vec2::new(90.0, 30.0))
            };

            if ui.add_enabled(can_next, next_btn).clicked() {
                self.go_next();
            }

            // 上一步按钮
            if can_back {
                let back = Button::new(RichText::new("← 上一步").size(13.0))
                    .fill(Color32::TRANSPARENT)
                    .min_size(Vec2::new(90.0, 30.0));
                if ui.add(back).clicked() {
                    self.go_back();
                }
            }
        });
    }

    fn go_next(&mut self) {
        match self.screen {
            Screen::Welcome => self.screen = Screen::License,
            Screen::License => self.screen = Screen::Location,
            Screen::Location => {
                self.config.target_path = self.location_input.trim().to_string();
                self.screen = Screen::Components;
            }
            Screen::Components => {
                // 检查是否有运行中的进程
                self.running_procs = win_process::find_deepx_processes();
                if !self.running_procs.is_empty() {
                    self.screen = Screen::CloseProcesses;
                    self.close_attempted = false;
                } else {
                    self.screen = Screen::Progress;
                    self.start_install();
                }
            }
            Screen::CloseProcesses | Screen::Progress | Screen::Finish => {}
        }
    }

    fn go_back(&mut self) {
        match self.screen {
            Screen::Welcome => {}
            Screen::License => self.screen = Screen::Welcome,
            Screen::Location => self.screen = Screen::License,
            Screen::Components => self.screen = Screen::Location,
            Screen::CloseProcesses => self.screen = Screen::Components,
            Screen::Progress | Screen::Finish => {}
        }
    }
}

// ============================================================
// 内容页面
// ============================================================

impl App {
    /// 统一的页面标题
    fn page_header(ui: &mut Ui, screen: Screen) {
        ui.add_space(12.0);
        ui.label(RichText::new(screen.title()).size(22.0).strong());
        ui.add_space(4.0);
        ui.label(RichText::new(screen.subtitle()).size(13.0).color(colors::TEXT_SECONDARY));
        ui.add_space(20.0);
    }

    // ---- 欢迎 ----
    fn render_welcome(&self, ui: &mut Ui) {
        ui.vertical_centered(|ui| {
            ui.add_space(40.0);

            // 图标占位
            let icon_rect = Rect::from_min_size(ui.next_widget_position(), Vec2::splat(72.0));
            ui.painter().rect_filled(
                icon_rect,
                Rounding::same(16.0),
                Color32::from_rgb(230, 240, 255),
            );
            ui.painter().text(
                icon_rect.center(),
                Align2::CENTER_CENTER,
                "DX",
                FontId::proportional(28.0),
                colors::ACCENT,
            );
            ui.advance_cursor_after_rect(icon_rect);
            ui.add_space(24.0);

            ui.label(RichText::new("DeepX").size(32.0).strong());
            ui.add_space(4.0);
            ui.label(RichText::new("本地优先的桌面效率工具集").size(14.0).color(colors::TEXT_SECONDARY));
            ui.add_space(36.0);

            // 特性列表
            Frame::none()
                .fill(Color32::from_rgb(248, 248, 250))
                .rounding(Rounding::same(10.0))
                .inner_margin(Margin::same(20.0))
                .show(ui, |ui| {
                    ui.set_width(360.0);
                    let items = [
                        ("◆", "智能桌面应用 (Electron)"),
                        ("◆", "本地守护进程 (Rust 后端)"),
                        ("◆", "高效、安全、本地优先"),
                    ];
                    for (icon, text) in &items {
                        ui.horizontal(|ui| {
                            ui.label(RichText::new(*icon).color(colors::ACCENT));
                            ui.label(*text);
                        });
                        ui.add_space(4.0);
                    }
                });
        });
    }

    // ---- 许可协议 ----
    fn render_license(&mut self, ui: &mut Ui) {
        Self::page_header(ui, Screen::License);

        Frame::none()
            .fill(Color32::from_rgb(248, 248, 250))
            .rounding(Rounding::same(8.0))
            .stroke(Stroke::new(1.0_f32, colors::BORDER))
            .inner_margin(Margin::same(14.0))
            .show(ui, |ui| {
                ScrollArea::vertical()
                    .max_height(200.0)
                    .show(ui, |ui| {
                        ui.add(
                            TextEdit::multiline(&mut self.license_text)
                                .font(TextStyle::Body)
                                .interactive(false)
                                .desired_width(f32::INFINITY)
                                .desired_rows(12),
                        );
                    });
            });

        ui.add_space(14.0);
        ui.checkbox(&mut self.license_agreed, "我接受许可协议中的条款");

        if !self.license_agreed {
            ui.add_space(4.0);
            ui.label(
                RichText::new("请先接受许可协议后再继续。")
                    .size(12.0)
                    .color(colors::DANGER),
            );
        }
    }

    // ---- 安装位置 ----
    fn render_location(&mut self, ui: &mut Ui) {
        Self::page_header(ui, Screen::Location);

        ui.label("安装路径:");
        ui.add_space(6.0);

        // 路径输入行
        ui.horizontal(|ui| {
            let _resp = ui.add(
                TextEdit::singleline(&mut self.location_input)
                    .desired_width(360.0)
                    .font(TextStyle::Monospace),
            );
            ui.add_space(8.0);
            if ui.button("浏览...").clicked() {
                if let Some(path) = native_folder_picker() {
                    self.location_input = path;
                }
            }
        });

        ui.add_space(10.0);

        // 空间信息
        let resolved = shellexpand(&self.location_input);
        if let Some(free) = disk_free_space(&resolved) {
            let free_gb = free as f64 / 1_073_741_824.0;
            let (color, icon) = if free < 200_000_000 {
                (colors::DANGER, "⚠")
            } else {
                (colors::TEXT_SECONDARY, "💾")
            };
            ui.label(
                RichText::new(format!("{}  可用空间: {:.1} GB", icon, free_gb))
                    .size(12.0)
                    .color(color),
            );
        }

        if resolved != self.location_input {
            ui.add_space(4.0);
            ui.label(
                RichText::new(format!("解析路径: {}", resolved))
                    .size(11.0)
                    .color(colors::TEXT_SECONDARY),
            );
        }
    }

    // ---- 安装组件 ----
    fn render_components(&mut self, ui: &mut Ui) {
        Self::page_header(ui, Screen::Components);

        Frame::none()
            .fill(Color32::from_rgb(248, 248, 250))
            .rounding(Rounding::same(10.0))
            .inner_margin(Margin::same(18.0))
            .show(ui, |ui| {
                ui.set_width(420.0);

                ui.horizontal(|ui| {
                    ui.checkbox(&mut self.config.install_desktop_app, "");
                    ui.vertical(|ui| {
                        ui.label(RichText::new("DeepX 桌面应用").strong());
                        ui.label(
                            RichText::new("Electron 桌面客户端 + 本地守护进程，提供完整功能。")
                                .size(12.0)
                                .color(colors::TEXT_SECONDARY),
                        );
                    });
                });
                ui.add_space(10.0);
                ui.separator();
                ui.add_space(10.0);

                ui.horizontal(|ui| {
                    ui.checkbox(&mut self.config.create_start_menu, "");
                    ui.vertical(|ui| {
                        ui.label(RichText::new("开始菜单快捷方式").strong());
                        ui.label(
                            RichText::new("在开始菜单中创建 DeepX 程序组。")
                                .size(12.0)
                                .color(colors::TEXT_SECONDARY),
                        );
                    });
                });
                ui.add_space(10.0);
                ui.separator();
                ui.add_space(10.0);

                ui.horizontal(|ui| {
                    ui.checkbox(&mut self.config.create_desktop_shortcut, "");
                    ui.vertical(|ui| {
                        ui.label(RichText::new("桌面快捷方式").strong());
                        ui.label(
                            RichText::new("在桌面上创建 DeepX 快捷方式。")
                                .size(12.0)
                                .color(colors::TEXT_SECONDARY),
                        );
                    });
                });
            });

        ui.add_space(14.0);
        ui.label(
            RichText::new(format!("安装至: {}", self.config.target_path))
                .size(11.0)
                .color(colors::TEXT_SECONDARY),
        );
    }

    // ---- 关闭进程 ----
    fn render_close_processes(&mut self, ui: &mut Ui) {
        Self::page_header(ui, Screen::CloseProcesses);

        ui.add_space(8.0);

        // 列出检测到的进程
        Frame::none()
            .fill(Color32::from_rgb(248, 248, 250))
            .rounding(Rounding::same(8.0))
            .inner_margin(Margin::same(14.0))
            .show(ui, |ui| {
                ui.set_width(420.0);
                ui.label(RichText::new("检测到以下 DeepX 进程正在运行:").strong());
                ui.add_space(8.0);
                for p in &self.running_procs {
                    let status = if p.closed { "✓ 已关闭" } else { "● 运行中" };
                    ui.label(format!("  {}  (PID: {})  {}", p.name, p.pid, status));
                }
            });

        ui.add_space(16.0);

        if !self.close_attempted {
            ui.label("可以尝试自动关闭这些进程（同用户进程无需管理员权限）。");
            ui.add_space(4.0);
            ui.label(RichText::new("提示：关闭后未保存的数据可能丢失。").size(12.0).color(colors::TEXT_SECONDARY));
        } else {
            // 检查还有哪些在运行
            let still_running: Vec<_> = self.running_procs.iter().filter(|p| !p.closed).collect();
            if still_running.is_empty() {
                ui.label(RichText::new("所有进程已关闭，可以继续安装。").color(colors::SUCCESS));
            } else {
                ui.label(RichText::new("部分进程未能关闭。您可以:").color(colors::DANGER));
                ui.add_space(4.0);
                ui.label("  • 手动关闭后点击「重试」");
                ui.label("  • 点击「强制关闭」强制终止进程");
                ui.label("  • 点击「跳过」忽略（安装可能不完整）");
            }
        }

        ui.add_space(20.0);

        ui.with_layout(Layout::right_to_left(Align::Center), |ui| {
            // 继续安装（进程已关闭时）
            if self.running_procs.iter().all(|p| p.closed) {
                if ui.add(Button::new(RichText::new("继续安装").color(Color32::WHITE)).fill(colors::ACCENT).rounding(Rounding::same(6.0)).min_size(Vec2::new(100.0, 30.0))).clicked() {
                    self.screen = Screen::Progress;
                    self.start_install();
                }
            }

            // 跳过（忽略运行中的进程）
            if self.close_attempted {
                if ui.add(Button::new(RichText::new("跳过（可能不完整）").color(colors::TEXT_SECONDARY).size(12.0)).fill(Color32::TRANSPARENT).min_size(Vec2::new(130.0, 30.0))).clicked() {
                    self.screen = Screen::Progress;
                    self.start_install();
                }
            }

            // 强制关闭
            if self.close_attempted && !self.running_procs.iter().all(|p| p.closed) {
                if ui.add(Button::new(RichText::new("强制关闭").color(Color32::WHITE)).fill(colors::DANGER).rounding(Rounding::same(6.0)).min_size(Vec2::new(90.0, 30.0))).clicked() {
                    for p in &mut self.running_procs {
                        if !p.closed {
                            win_process::force_terminate(p.pid);
                            p.closed = true;
                        }
                    }
                }
            }

            // 重试（重新检测）
            if self.close_attempted {
                if ui.add(Button::new(RichText::new("重试").size(13.0)).fill(Color32::TRANSPARENT).min_size(Vec2::new(70.0, 30.0))).clicked() {
                    self.running_procs = win_process::find_deepx_processes();
                    self.close_attempted = false;
                }
            }

            // 自动关闭（首次）
            if !self.close_attempted {
                if ui.add(Button::new(RichText::new("自动关闭").color(Color32::WHITE)).fill(colors::ACCENT).rounding(Rounding::same(6.0)).min_size(Vec2::new(100.0, 30.0))).clicked() {
                    win_process::graceful_close(&mut self.running_procs);
                    let all_gone = win_process::wait_for_exit(&self.running_procs, 5);
                    if all_gone {
                        for p in &mut self.running_procs {
                            p.closed = true;
                        }
                    } else {
                        // 标记已关闭的
                        for p in &mut self.running_procs {
                            p.closed = !win_process::is_alive(p.pid);
                        }
                    }
                    self.close_attempted = true;
                }
            }
        });

        // 底部说明
        ui.with_layout(Layout::bottom_up(Align::LEFT), |ui| {
            ui.add_space(8.0);
            ui.label(
                RichText::new("自动关闭无需管理员权限（同用户进程），仅关闭 DeepX 自身进程。")
                    .size(11.0)
                    .color(colors::TEXT_SECONDARY),
            );
        });
    }

    // ---- 安装进度 ----
    fn render_progress(&mut self, ui: &mut Ui) {
        ui.vertical_centered(|ui| {
            ui.add_space(60.0);

            // 动画圆环占位 — 用简单的文字替代
            ui.label(RichText::new("正在安装 DeepX...").size(20.0).strong());
            ui.add_space(20.0);

            let progress = self.config.progress;
            ui.add(
                ProgressBar::new(progress)
                    .desired_width(360.0)
                    .text(format!("{:.0}%", progress * 100.0)),
            );

            ui.add_space(16.0);

            if !self.config.current_file.is_empty() {
                ui.label(
                    RichText::new(&self.config.current_file)
                        .size(12.0)
                        .color(colors::TEXT_SECONDARY),
                );
            }

            if self.config.total_files > 0 {
                ui.label(
                    RichText::new(format!(
                        "文件 {}/{}",
                        self.config.completed_files, self.config.total_files
                    ))
                    .size(12.0)
                    .color(colors::TEXT_SECONDARY),
                );
            }

            if let Some(ref err) = self.config.error {
                ui.add_space(12.0);
                ui.colored_label(colors::DANGER, format!("错误: {}", err));
            }
        });
    }

    // ---- 完成 ----
    fn render_finish(&self, ui: &mut Ui) {
        ui.vertical_centered(|ui| {
            ui.add_space(50.0);

            let success = self.install_result.as_ref().map(|r| r.is_ok()).unwrap_or(false);

            // 图标
            let icon = if success { "✓" } else { "✗" };
            let icon_color = if success { colors::SUCCESS } else { colors::DANGER };
            let dot_rect = Rect::from_min_size(ui.next_widget_position(), Vec2::splat(64.0));
            ui.painter().circle_filled(dot_rect.center(), 32.0, icon_color);
            ui.painter().text(
                dot_rect.center(),
                Align2::CENTER_CENTER,
                icon,
                FontId::proportional(30.0),
                Color32::WHITE,
            );
            ui.advance_cursor_after_rect(dot_rect);
            ui.add_space(16.0);

            if success {
                ui.label(RichText::new("安装完成").size(22.0).strong());
                ui.add_space(8.0);
                ui.label("DeepX 已成功安装到您的计算机。");
                ui.add_space(12.0);

                Frame::none()
                    .fill(Color32::from_rgb(248, 248, 250))
                    .rounding(Rounding::same(8.0))
                    .inner_margin(Margin::same(14.0))
                    .show(ui, |ui| {
                        ui.set_width(340.0);
                        ui.label(RichText::new("启动方式:").strong());
                        if self.config.create_start_menu {
                            ui.label("  ◆  开始菜单 → DeepX");
                        }
                        if self.config.create_desktop_shortcut {
                            ui.label("  ◆  桌面快捷方式");
                        }
                        ui.label(format!("  ◆  {}\\DeepX.exe", self.config.target_path));
                    });

                ui.add_space(20.0);
            } else {
                ui.label(
                    RichText::new("安装失败").size(22.0).strong().color(colors::DANGER),
                );
                if let Some(Err(ref err)) = self.install_result {
                    ui.add_space(8.0);
                    ui.colored_label(colors::DANGER, err);
                }
                ui.add_space(16.0);
            }

            if ui
                .add(
                    Button::new(RichText::new(if success { "完成" } else { "关闭" }).color(Color32::WHITE))
                        .fill(colors::ACCENT)
                        .rounding(Rounding::same(6.0))
                        .min_size(Vec2::new(120.0, 34.0)),
                )
                .clicked()
            {
                std::process::exit(if success { 0 } else { 1 });
            }
        });
    }
}

// ============================================================
// 安装引擎衔接
// ============================================================

impl App {
    fn start_install(&mut self) {
        let mut config = self.config.clone();
        let (tx, rx) = mpsc::channel();
        self.install_receiver = Some(rx);

        thread::spawn(move || {
            let result = install::run_install(&mut config, |cfg| {
                let _ = tx.send(InstallMsg::Progress(cfg.clone()));
            });
            let _ = tx.send(InstallMsg::Done(result));
        });
    }

    fn poll_install(&mut self, ctx: &Context) {
        if self.screen != Screen::Progress {
            return;
        }
        let mut msgs: Vec<InstallMsg> = Vec::new();
        if let Some(ref rx) = self.install_receiver {
            while let Ok(msg) = rx.try_recv() {
                msgs.push(msg);
            }
        }
        for msg in msgs {
            match msg {
                InstallMsg::Progress(cfg) => self.config = cfg,
                InstallMsg::Done(result) => {
                    self.install_result = Some(result);
                    self.post_install();
                    self.screen = Screen::Finish;
                }
            }
        }
        ctx.request_repaint();
    }

    fn post_install(&mut self) {
        let app_exe = format!(r"{}\DeepX.exe", self.config.target_path);
        if self.config.create_desktop_shortcut {
            let _ = install::create_desktop_shortcut(&app_exe, "DeepX 桌面应用");
        }
        if self.config.create_start_menu {
            let _ = install::create_start_menu_shortcut(&app_exe, "DeepX 桌面应用");
        }
        let _ = install::write_uninstall_registry(&self.config.target_path, env!("CARGO_PKG_VERSION"));
    }
}

// ============================================================
// 工具函数
// ============================================================

fn shellexpand(path: &str) -> String {
    let re = regex_lite::Regex::new(r"%([^%]+)%").unwrap();
    re.replace_all(path, |caps: &regex_lite::Captures| {
        let var = caps.get(1).unwrap().as_str();
        std::env::var(var).unwrap_or_else(|_| format!("%{}%", var))
    })
    .to_string()
}

fn disk_free_space(path: &str) -> Option<u64> {
    let path = shellexpand(path);
    let p = std::path::Path::new(&path);
    let mut current = if p.is_absolute() { Some(p.to_path_buf()) } else { None };
    while let Some(ref c) = current {
        if c.exists() { break; }
        current = c.parent().map(|p| p.to_path_buf());
    }
    let check = current.unwrap_or_else(|| std::path::PathBuf::from("C:\\"));
    let path_str = check.to_string_lossy();
    let drive = if path_str.len() >= 2 && path_str.as_bytes().get(1) == Some(&b':') {
        Some(&path_str[..2])
    } else {
        None
    }?;

    #[cfg(windows)]
    unsafe {
        use windows::Win32::Storage::FileSystem::GetDiskFreeSpaceExW;
        let drive_wide: Vec<u16> = format!("{}\\", drive).encode_utf16().chain(std::iter::once(0)).collect();
        let mut free: u64 = 0;
        GetDiskFreeSpaceExW(
            windows::core::PCWSTR::from_raw(drive_wide.as_ptr()),
            Some(&mut free), None, None,
        ).ok()?;
        Some(free)
    }

    #[cfg(not(windows))]
    { None }
}

fn native_folder_picker() -> Option<String> {
    #[cfg(windows)]
    unsafe {
        use windows::Win32::UI::Shell::{FileOpenDialog, IFileDialog, FOS_PICKFOLDERS, FOS_PATHMUSTEXIST};
        use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED};
        let _ = CoInitializeEx(None, COINIT_APARTMENTTHREADED);
        let dialog: IFileDialog = CoCreateInstance(&FileOpenDialog, None, CLSCTX_INPROC_SERVER).ok()?;
        dialog.SetOptions(FOS_PICKFOLDERS | FOS_PATHMUSTEXIST).ok()?;
        dialog.Show(None).ok()?;
        let item = dialog.GetResult().ok()?;
        let name = item.GetDisplayName(windows::Win32::UI::Shell::SIGDN_FILESYSPATH).ok()?;
        Some(name.to_string().unwrap_or_default())
    }
    #[cfg(not(windows))]
    { None }
}

// ============================================================
// 许可协议
// ============================================================

const LICENSE_TEXT: &str = r#"DeepX 软件许可协议

版权所有 © 2024-2026 DeepX 开发团队

特此免费授予获得本软件及相关文档文件（以下简称"软件"）的任何人
不受限制地处理本软件的权利，包括但不限于使用、复制、修改、合并、
发布、分发、再许可和/或销售本软件副本的权利，以及允许获得本软件
的人员这样做，但须符合以下条件：

上述版权声明和本许可声明应包含在本软件的所有副本或主要部分中。

本软件按"原样"提供，不提供任何形式的明示或暗示的保证，包括但不
限于对适销性、特定用途的适用性和非侵权性的保证。在任何情况下，
作者或版权所有者均不对因本软件或本软件的使用或其他交易而产生、
引起或与之相关的任何索赔、损害赔偿或其他责任负责，无论是合同诉
讼、侵权行为还是其他行为。

---

第三方开源组件许可

本软件使用了以下开源组件：
- Electron (MIT)
- SolidJS (MIT)
- egui (MIT/Apache-2.0)
"#;
