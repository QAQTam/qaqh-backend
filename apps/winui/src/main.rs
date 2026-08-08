//! DeepX WinUI desktop shell — 原生 XAML 视图族。
//!
//! Mica 窗口承载全原生视图族（sidebar/header/composer/chat/interaction/
//! home/skills/settings），`bridge.rs` 通过 `deepx-client` 直连 daemon
//! （Ringing 协议：三 SSE 频道事件解析 + 命令/查询直发）。

#![windows_subsystem = "windows"]

mod bridge;
mod chat_adapter;
mod chat_view;
mod composer_bar;
mod fonts;
mod header;
mod home_view;
mod info_panel;
mod interaction_overlay;
mod shell;
mod shell_store;
mod session_tabs;
mod settings_view;
mod sidebar;
mod skills_view;

use std::time::{Duration, Instant};

use windows_reactor::*;

/// 开屏覆盖层最长显示时间：超过后切换为失败文案并露出错误详情。
const SPLASH_TIMEOUT: Duration = Duration::from_secs(60);

fn app(cx: &mut RenderCx) -> Element {
    log_diag("render");
    let bridge = bridge::Bridge::shared();
    let timer = cx.use_ref::<Option<DispatcherTimer>>(None);
    // 侧栏宽度：可拖拽（splitter），双击抓握条恢复默认。
    let (sidebar_width, set_sidebar_width) =
        cx.use_state::<f64>(sidebar::SIDEBAR_DEFAULT_WIDTH);

    cx.use_effect((), {
        let bridge = bridge.clone();
        let timer = timer.clone();
        move || {
            if timer.borrow().is_none() {
                match DispatcherTimer::new(Duration::from_millis(50), {
                    let bridge = bridge.clone();
                    move || bridge.pump()
                }) {
                    Ok(t) => {
                        log_diag("timer created");
                        *timer.borrow_mut() = Some(t);
                    }
                    Err(e) => log_diag(&format!("timer failed: {e}")),
                }
            }
        }
    });


    // Step 1: 内容区元素——左 XAML 侧栏（可拖拽宽度）+ 右区。
    // 右区 = 内层 Grid 多行（WORKFLOW §8 壳主导视图族）：
    //   - row0 = chat 区（原生 ChatView + Composer）——view=chat 时 STAR；
    //   - row1 = XAML 技能页——view=skills 时 STAR；
    //   - row2 = XAML 首页（P1）——view=home 时 STAR；
    //   - row3 = XAML 设置页（P2）——view=settings 时 STAR。
    // 非当前视图的行高 0：XAML 页零命中零渲染；无 opacity/命中测试依赖。
    let nav: Element =
        sidebar::sidebar(cx, bridge.clone(), sidebar_width, set_sidebar_width).into();
    let (view, set_view) = cx.use_state::<String>("home".to_string());
    let (info_open, set_info_open) = cx.use_state::<bool>(false);
    let view_timer = cx.use_ref::<Option<DispatcherTimer>>(None);
    let last_view = cx.use_ref::<String>("home".to_string());
    let last_info_open = cx.use_ref::<bool>(false);
    cx.use_effect((), {
        let bridge = bridge.clone();
        let set_view = set_view.clone();
        let set_info_open = set_info_open.clone();
        let view_timer = view_timer.clone();
        let last_view = last_view.clone();
        let last_info_open = last_info_open.clone();
        move || {
            if view_timer.borrow().is_none() {
                match DispatcherTimer::new(Duration::from_millis(250), {
                    let bridge = bridge.clone();
                    let set_view = set_view.clone();
                    let set_info_open = set_info_open.clone();
                    let last_view = last_view.clone();
                    let last_info_open = last_info_open.clone();
                    move || {
                        let v = bridge.core().current_view();
                        if v != *last_view.borrow() {
                            *last_view.borrow_mut() = v.clone();
                            set_view.call(v);
                        }
                        // Info 面板开关（Web shell.setHeader 投影的 info_open）。
                        let o = bridge.core().header_snapshot().0.info_open;
                        if o != *last_info_open.borrow() {
                            *last_info_open.borrow_mut() = o;
                            log_diag(&format!("main: info_open -> {o}"));
                            set_info_open.call(o);
                        }
                    }
                }) {
                    Ok(t) => {
                        *view_timer.borrow_mut() = Some(t);
                    }
                    Err(e) => log_diag(&format!("view timer failed: {e}")),
                }
            }
        }
    });

    // ── 字体：settings 快照到达/变化时全局应用（FontFamily 为继承属性，
    // 设置内容根一次即全树生效；空 = 恢复系统默认）。常驻轮询保证
    // 启动后（不打开设置页）也能应用上次保存的字体。──────────────
    let font_timer = cx.use_ref::<Option<DispatcherTimer>>(None);
    let last_font = cx.use_ref::<String>(String::new());
    cx.use_effect((), {
        let bridge = bridge.clone();
        let font_timer = font_timer.clone();
        let last_font = last_font.clone();
        move || {
            if font_timer.borrow().is_none() {
                match DispatcherTimer::new(Duration::from_millis(500), {
                    let bridge = bridge.clone();
                    let last_font = last_font.clone();
                    move || {
                        let (snap, _) = bridge.core().settings_snapshot();
                        if let Some(snap) = snap {
                            let font = snap.font_family;
                            if font != *last_font.borrow() {
                                *last_font.borrow_mut() = font.clone();
                                if font.is_empty() {
                                    windows_reactor::set_font_family(None);
                                } else {
                                    windows_reactor::set_font_family(Some(&font));
                                }
                            }
                        }
                    }
                }) {
                    Ok(t) => *font_timer.borrow_mut() = Some(t),
                    Err(e) => log_diag(&format!("font timer failed: {e}")),
                }
            }
        }
    });
    let skills: Element = skills_view::skills_view(cx, bridge.clone())
        .grid_row(1)
        .grid_column(0)
        .into();
    let home: Element = home_view::home_view(cx, bridge.clone())
        .grid_row(2)
        .grid_column(0)
        .into();
    let settings: Element = settings_view::settings_view(cx, bridge.clone())
        .grid_row(3)
        .grid_column(0)
        .into();
    // 内容区四行视图族（WORKFLOW §8 壳主导）：row0=chat 区（原生 ChatView
    // + Composer 底部栏，view=chat 时 STAR）、row1=skills、row2=home、
    // row3=settings；非当前视图行高 0（零渲染零命中）。
    let composer: Element = composer_bar::composer_bar(cx, bridge.clone())
        .grid_row(1)
        .grid_column(0)
        .into();
    let native_chat: Element = chat_view::chat_view(cx, bridge.clone())
        .grid_row(0)
        .grid_column(0)
        .into();
    let chat_area: Element = grid((native_chat, composer))
        .rows([GridLength::STAR, GridLength::Auto])
        .into();
    let right_content: Element = grid((chat_area, skills, home, settings))
        .rows([
            if view == "skills" || view == "home" || view == "settings" {
                GridLength::Pixel(0.0)
            } else {
                GridLength::STAR
            },
            if view == "skills" {
                GridLength::STAR
            } else {
                GridLength::Pixel(0.0)
            },
            if view == "home" {
                GridLength::STAR
            } else {
                GridLength::Pixel(0.0)
            },
            if view == "settings" {
                GridLength::STAR
            } else {
                GridLength::Pixel(0.0)
            },
        ])
        .into();
    // Step 1b: Info 面板右列（P4a）——chat 视图且 info_open 时 320px，
    // 否则 0（面板内容组件自管刷新时机：打开瞬间拉取 bootstrap 用量）。
    let info_el: Element = info_panel::info_panel(cx, bridge.clone())
        .grid_row(0)
        .grid_column(1)
        .into();
    let info_width = if info_open && view == "chat" {
        GridLength::Pixel(info_panel::PANEL_WIDTH)
    } else {
        GridLength::Pixel(0.0)
    };
    let right: Element = grid((right_content, info_el))
        .columns([GridLength::STAR, info_width])
        .grid_row(0)
        .grid_column(1)
        .into();

    // ── 开屏覆盖层（P-6 同 cell 重叠预留的首次应用）────────────────
    // daemon 冷启动可达数十秒（加载历史会话）；覆盖层用原生 ProgressRing
    // 动画覆盖启动期，桥连上 daemon 即移除。
    // 顺序语义：connected 分支优先于 timeout 分支（超时瞬间后端恰好连上
    // 时覆盖层正常消失，不卡失败态）。超时（[`SPLASH_TIMEOUT`]）后释放
    // 覆盖层，露出壳界面（含标题栏）——覆盖层使命仅为启动期动画。
    let (splash_visible, set_splash_visible) = cx.use_state::<bool>(true);
    let splash_timer = cx.use_ref::<Option<DispatcherTimer>>(None);
    let splash_started = cx.use_ref::<Option<Instant>>(None);
    let splash_done = cx.use_ref::<bool>(false);
    cx.use_effect((), {
        let bridge = bridge.clone();
        let splash_timer = splash_timer.clone();
        let splash_started = splash_started.clone();
        let splash_done = splash_done.clone();
        let set_splash_visible = set_splash_visible.clone();
        move || {
            if splash_timer.borrow().is_none() {
                match DispatcherTimer::new(Duration::from_millis(250), move || {
                    if *splash_done.borrow() {
                        return;
                    }
                    if splash_started.borrow().is_none() {
                        *splash_started.borrow_mut() = Some(Instant::now());
                    }
                    if bridge.core().backend_connected() {
                        *splash_done.borrow_mut() = true;
                        set_splash_visible.call(false);
                        log_diag("backend connected; splash dismissed");
                    } else if splash_started
                        .borrow()
                        .as_ref()
                        .is_some_and(|t| t.elapsed() >= SPLASH_TIMEOUT)
                    {
                        *splash_done.borrow_mut() = true;
                        set_splash_visible.call(false);
                        log_diag("backend connection timed out; splash released");
                    }
                }) {
                    Ok(t) => *splash_timer.borrow_mut() = Some(t),
                    Err(e) => log_diag(&format!("splash timer failed: {e}")),
                }
            }
        }
    });

    // Step 2: Grid 两行——row0 = XAML 标题栏（48px，SetTitleBar 拖拽区，
    // host 自动接线 host.rs:277-288）；row1 = 内容区（侧栏 | 右区）。
    let titlebar: Element = header::header(cx, bridge.clone())
        .grid_row(0)
        .grid_column(0)
        .into();
    // ── 内容区（row 1）：row0 = 顶部会话标签条（跨侧栏全宽）；
    // row1 = 基础层（侧栏 | 右区）────────────────────────────
    // P-6 覆盖层预留（WORKFLOW §6.1）：未来 XAML 面板/对话框
    // （P1 Flyout anchor / P2 ContentDialog phantom child）作为覆盖层
    // 元素追加进本 Grid（同 cell 重叠渲染），零布局改动。
    let tabs: Element = session_tabs::session_tabs(cx, bridge.clone())
        .grid_row(0)
        .grid_column(0)
        .into();
    let content: Element = grid((
        tabs,
        grid((nav.grid_column(0), right))
            .columns([GridLength::Pixel(sidebar_width), GridLength::STAR])
            .grid_row(1)
            .grid_column(0),
    ))
    .rows([GridLength::Pixel(session_tabs::TAB_STRIP_HEIGHT), GridLength::STAR])
    .grid_row(1)
    .grid_column(0)
    .into();
    let base: Element = grid((titlebar, content))
        .rows([GridLength::Pixel(header::HEADER_HEIGHT), GridLength::STAR])
        .into();
    // 覆盖层与基础层同 cell 重叠渲染（P-6 预留模式），盖住 titlebar + 内容区。
    // 注意：`splash_visible=false` 时的空 `grid(())` 依赖 WinUI"无背景元素不参与
    // 命中测试"的平台行为实现点击穿透——切勿给空 grid 添加背景。
    let splash: Element = if splash_visible {
        grid((
            ProgressRing::default().width(48.0).height(48.0),
            text_block("正在连接 DeepX…")
                .font_size(14.0)
                .foreground(ThemeRef::SecondaryText),
        ))
        .rows([GridLength::Pixel(64.0), GridLength::Auto])
        .background(ThemeRef::LayerFill)
        .into()
    } else {
        grid(()).into()
    };
    // 交互模态覆盖层（P-6 同模式）：kind="none" 时内部空 grid 穿透；
    // 有交互时半透明遮罩 + 卡片（permission/ask 模板）。置于最上层。
    let interaction: Element =
        interaction_overlay::interaction_overlay(cx, bridge.clone()).into();
    grid((base, splash, interaction)).into()
}

/// Minimal file logger for headless diagnosis (GUI subsystem has no console).
fn log_diag(msg: &str) {
    use std::io::Write;
    if let Ok(mut f) = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(std::env::var("DEEPX_WINUI_LOG").unwrap_or_else(|_| ".deepx-winui.log".into()))
    {
        let _ = writeln!(f, "{}", msg);
    }
}

fn main() -> windows_reactor::Result<()> {

    App::new()
        .title("DeepX")
        .inner_size(1200.0, 800.0)
        .backdrop(Backdrop::Mica)
        // 退出诊断（reactor #4787 on_exit）：窗口全关后、进程退出前执行。
        // 日志里出现此行 = 正常退出路径；闪退（崩溃/强杀）不会执行到这里，
        // 用于区分「正常关闭」与「异常终止」，辅助闪退调查。
        .on_exit(|| log_diag("app exit: all windows closed (normal path)"))
        // panic 诊断：渲染/事件回调/timer 的 panic 被 reactor 捕获后转发到这里
        // （context = 捕获边界，message = panic 消息）。release 下 panic 逃逸到
        // WinUI C++ 帧是 UB——此日志可在崩溃前留下源头证据（闪退调查工具链）。
        .on_fault(|fault| {
            log_diag(&format!(
                "reactor fault [{}]: {}",
                fault.context, fault.message
            ))
        })
        .render(app)
}
