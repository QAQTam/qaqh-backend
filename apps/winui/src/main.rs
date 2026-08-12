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
mod diagnostics;
mod diff_drawer;
mod fonts;
mod header;
mod home_view;
mod info_panel;
mod interaction_overlay;
mod oobe_view;
mod session_tabs;
mod settings_view;
mod shell;
mod shell_store;
mod sidebar;
mod skills_view;

use std::time::{Duration, Instant};

use windows_reactor::*;

use deepx_fluent::motion;

/// 开屏覆盖层最长显示时间：超过后切换为失败文案并露出错误详情。
const SPLASH_TIMEOUT: Duration = Duration::from_secs(60);

fn header_component(_: &(), cx: &mut RenderCx) -> Element {
    header::header(cx, bridge::Bridge::shared())
}

fn oobe_component(props: &SetState<bool>, cx: &mut RenderCx) -> Element {
    oobe_view::oobe_view(cx, props.clone())
}

fn sidebar_component(props: &(f64, SetState<f64>), cx: &mut RenderCx) -> Element {
    sidebar::sidebar(cx, bridge::Bridge::shared(), props.0, props.1.clone()).into()
}

fn session_tabs_component(_: &(), cx: &mut RenderCx) -> Element {
    session_tabs::session_tabs(cx, bridge::Bridge::shared())
}

fn chat_component(_: &(), cx: &mut RenderCx) -> Element {
    chat_view::chat_view(cx, bridge::Bridge::shared())
}

fn composer_component(_: &(), cx: &mut RenderCx) -> Element {
    composer_bar::composer_bar(cx, bridge::Bridge::shared())
}

fn info_panel_component(_: &(), cx: &mut RenderCx) -> Element {
    info_panel::info_panel(cx, bridge::Bridge::shared())
}

fn home_component(_: &(), cx: &mut RenderCx) -> Element {
    home_view::home_view(cx, bridge::Bridge::shared())
}

fn skills_component(_: &(), cx: &mut RenderCx) -> Element {
    skills_view::skills_view(cx, bridge::Bridge::shared())
}

fn settings_component(_: &(), cx: &mut RenderCx) -> Element {
    settings_view::settings_view(cx, bridge::Bridge::shared())
}

fn interaction_component(_: &(), cx: &mut RenderCx) -> Element {
    interaction_overlay::interaction_overlay(cx, bridge::Bridge::shared()).into()
}

fn diagram_zoom_component(_: &(), cx: &mut RenderCx) -> Element {
    chat_view::diagram_zoom_overlay(cx).into()
}

fn diff_drawer_component(_: &(), cx: &mut RenderCx) -> Element {
    diff_drawer::diff_drawer_overlay(cx).into()
}

fn app(cx: &mut RenderCx) -> Element {
    let bridge = bridge::Bridge::shared();
    let timer = cx.use_ref::<Option<DispatcherTimer>>(None);
    // 侧栏宽度：可拖拽（splitter），双击抓握条恢复默认。
    let (sidebar_width, set_sidebar_width) = cx.use_state::<f64>(sidebar::SIDEBAR_DEFAULT_WIDTH);

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
    // 非当前视图不仅行高 0，还不声明对应 Component：子树直接卸载，
    // use_effect cleanup/drop 停止 timer/on_frame。
    let nav: Element = component(
        sidebar_component,
        (sidebar_width, set_sidebar_width.clone()),
    )
    .with_key("shell-sidebar");
    let (view, set_view) = cx.use_state::<String>("home".to_string());
    let (_, set_info_open) = cx.use_state::<bool>(false);
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
                            set_view.call(v.clone());
                        }
                        // Info 面板开关（Web shell.setHeader 投影的 info_open；
                        // 面板已常驻（2026-08 决策：省动画），此处仅同步 flag
                        // 供 header 按钮状态使用，不再驱动列宽。
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
                            let font = fonts::effective_ui_font(&snap.font_family).to_string();
                            if font != *last_font.borrow() {
                                *last_font.borrow_mut() = font.clone();
                                windows_reactor::set_font_family(Some(&font));
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

    // ── OOBE：首次启动引导（P-6 覆盖层最上层，盖住整个壳）────────────
    // 无完成标志 → 显示引导；config 快照到达且已配置 key → 自动收尾
    // （老用户/标志丢失场景，一闪而过）；设置页「重新运行引导」→ 强制
    // 重开且关闭自动收尾（否则刚打开就被"已配置"判定关掉）。
    let (oobe_visible, set_oobe_visible) = cx.use_state::<bool>(!oobe_view::oobe_done());
    let oobe_auto = cx.use_ref::<bool>(!oobe_view::oobe_done());
    let oobe_timer = cx.use_ref::<Option<DispatcherTimer>>(None);
    cx.use_effect((), {
        let bridge = bridge.clone();
        let set_oobe_visible = set_oobe_visible.clone();
        let oobe_auto = oobe_auto.clone();
        let oobe_timer = oobe_timer.clone();
        move || {
            if oobe_timer.borrow().is_none() {
                match DispatcherTimer::new(Duration::from_millis(500), {
                    let core = bridge.core();
                    let set_oobe_visible = set_oobe_visible.clone();
                    let oobe_auto = oobe_auto.clone();
                    move || {
                        // 设置页「重新运行引导」：强制重开，本次不自动收尾。
                        if oobe_view::take_rerun_request() {
                            *oobe_auto.borrow_mut() = false;
                            set_oobe_visible.call(true);
                            log_diag("oobe: rerun requested");
                            return;
                        }
                        // 首次路径：daemon 已配置主 key（老用户/标志丢失）→ 自动完成。
                        if *oobe_auto.borrow() {
                            let configured = core
                                .settings_snapshot()
                                .0
                                .is_some_and(|s| s.api_key_configured);
                            if configured {
                                oobe_view::mark_oobe_done();
                                *oobe_auto.borrow_mut() = false;
                                set_oobe_visible.call(false);
                                log_diag("oobe: config present, auto-dismissed");
                            }
                        }
                    }
                }) {
                    Ok(t) => *oobe_timer.borrow_mut() = Some(t),
                    Err(e) => log_diag(&format!("oobe timer failed: {e}")),
                }
            }
        }
    });

    let skills: Element = if view == "skills" {
        grid((component(skills_component, ()).with_key("shell-skills"),))
            .with_key("shell-skills-wrap")
            .rows([GridLength::STAR])
            .columns([GridLength::STAR])
            .grid_row(1)
            .grid_column(0)
            .transition(motion::navigation_enter(), motion::content_exit())
            .into()
    } else {
        Element::Empty
    };
    let home: Element = if view == "home" {
        grid((component(home_component, ()).with_key("shell-home"),))
            .with_key("shell-home-wrap")
            .rows([GridLength::STAR])
            .columns([GridLength::STAR])
            .grid_row(2)
            .grid_column(0)
            .transition(motion::navigation_enter(), motion::content_exit())
            .into()
    } else {
        Element::Empty
    };
    let settings: Element = if view == "settings" {
        grid((component(settings_component, ()).with_key("shell-settings"),))
            .with_key("shell-settings-wrap")
            .rows([GridLength::STAR])
            .columns([GridLength::STAR])
            .grid_row(3)
            .grid_column(0)
            .transition(motion::navigation_enter(), motion::content_exit())
            .into()
    } else {
        Element::Empty
    };
    // 内容区四行视图族（WORKFLOW §8 壳主导）：row0=chat 区（原生 ChatView
    // + Composer 底部栏，view=chat 时 STAR）、row1=skills、row2=home、
    // row3=settings；非当前视图行高 0 且不挂组件，停止后台轮询。
    let chat_visible = view == "chat";
    // reactor 限制：Element::Component 无 modifiers（fork element.rs 注释：
    // Component/Empty 链式修饰静默忽略）——component 上的 grid_row/grid_column
    // 不生效（解绑回归：info 面板落到 chat 区、composer 被覆盖）。
    // 改用普通 grid 包裹承载定位，组件填满 wrapper。
    let composer: Element = if chat_visible {
        grid((component(composer_component, ()).with_key("shell-composer"),))
            .with_key("shell-composer-wrap")
            .rows([GridLength::STAR])
            .columns([GridLength::STAR])
            .grid_row(1)
            .grid_column(0)
            .into()
    } else {
        Element::Empty
    };
    let native_chat: Element = if chat_visible {
        component(chat_component, ())
            .with_key("shell-chat")
            .grid_row(0)
            .grid_column(0)
    } else {
        Element::Empty
    };
    // chat_area 必须显式 STAR 列：缺列定义时 WinUI 按隐式 Auto 测量，
    // chat_view（Star 列 ListView）与 composer 宽度由内容决定而非可用空间
    // （长行不换行撑爆列宽 / 窗口缩放不跟随）——L376-378 覆盖层同款实证。
    let chat_area: Element = if chat_visible {
        grid((native_chat, composer))
            .with_key("shell-chat-area")
            .rows([GridLength::STAR, GridLength::Auto])
            .columns([GridLength::STAR])
            .transition(motion::navigation_enter(), motion::content_exit())
            .into()
    } else {
        Element::Empty
    };
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
        .columns([GridLength::STAR])
        .into();
    // Step 1b: Info 面板右列（P4a）——chat 视图恒显；离开 chat 时
    // 不挂组件以卸载内部轮询，列宽同时归零。
    let info_el: Element = if chat_visible {
        grid((component(info_panel_component, ()).with_key("shell-info"),))
            .with_key("shell-info-wrap")
            .rows([GridLength::STAR])
            .columns([GridLength::STAR])
            .grid_row(0)
            .grid_column(1)
            .into()
    } else {
        Element::Empty
    };
    let info_width = if view == "chat" {
        GridLength::Pixel(info_panel::PANEL_WIDTH)
    } else {
        GridLength::Pixel(0.0)
    };
    // right_body 必须显式 STAR 行：缺行定义时隐式 Auto 行高 = 内容高，
    // chat 记录超高时整行溢出裁剪，composer 被推出可视区、info panel 的
    // scroll_viewer 视口 = 内容高导致滚动失效。
    let right_body: Element = grid((right_content, info_el))
        .columns([GridLength::STAR, info_width])
        .rows([GridLength::STAR])
        .grid_row(1)
        .grid_column(0)
        .into();
    // Session tabs belong to the document workspace, not to the navigation
    // rail. Keeping both in one right-side grid aligns their coordinate space
    // while the transcript retains its narrower centered reading measure.
    let tabs: Element = component(session_tabs_component, ())
        .with_key("shell-tabs")
        .grid_row(0)
        .grid_column(0);
    let right: Element = grid((tabs, right_body))
        .rows([
            GridLength::Pixel(session_tabs::TAB_STRIP_HEIGHT),
            GridLength::STAR,
        ])
        .columns([GridLength::STAR])
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
    let titlebar: Element = component(header_component, ())
        .with_key("shell-header")
        .grid_row(0)
        .grid_column(0);
    // ── 内容区（row 1）：侧栏 | 文档工作区（标签条 + 视图）──────
    // P-6 覆盖层预留（WORKFLOW §6.1）：未来 XAML 面板/对话框
    // （P1 Flyout anchor / P2 ContentDialog phantom child）作为覆盖层
    // 元素追加进本 Grid（同 cell 重叠渲染），零布局改动。
    let content: Element = grid((nav.grid_column(0), right))
        .columns([GridLength::Pixel(sidebar_width), GridLength::STAR])
        .rows([GridLength::STAR])
        .grid_row(1)
        .grid_column(0)
        .into();
    let base: Element = grid((titlebar, content))
        .rows([GridLength::Pixel(header::HEADER_HEIGHT), GridLength::STAR])
        .columns([GridLength::STAR])
        .into();
    // 覆盖层与基础层同 cell 重叠渲染（P-6 预留模式），盖住 titlebar + 内容区。
    // 注意：`splash_visible=false` 时的空 `grid(())` 依赖 WinUI"无背景元素不参与
    // 命中测试"的平台行为实现点击穿透——切勿给空 grid 添加背景。
    let splash: Element = if splash_visible {
        grid((
            ProgressRing::default().width(48.0).height(48.0),
            text_block("正在连接 DeepX…")
                .font_size(14.0)
                .foreground(ThemeRef::SecondaryText)
                .grid_row(1),
        ))
        .rows([GridLength::Pixel(64.0), GridLength::Auto])
        .background(ThemeRef::LayerFill)
        .into()
    } else {
        grid(()).into()
    };
    // 交互模态覆盖层（P-6 同模式）：kind="none" 时内部空 grid 穿透；
    // 有交互时半透明遮罩 + 卡片（permission/ask 模板）。置于最上层。
    let interaction: Element = component(interaction_component, ()).with_key("shell-interaction");
    // 图表放大覆盖层（P-6 同模式）：chat_view 点击写入 DIAGRAM_ZOOM，
    // 本组件轮询弹开全窗大图。无请求时空 grid 穿透。
    let diagram_zoom: Element = component(diagram_zoom_component, ()).with_key("shell-diagram-zoom");
    // diff 抽屉覆盖层（V4）：turn 末尾「查看详情」→ 右侧滑入面板。
    // 无请求时空 grid 穿透（同 interaction 模式）。
    let diff_drawer: Element = component(diff_drawer_component, ()).with_key("shell-diff-drawer");
    // 首次启动引导（P-6 同模式）：无完成标志时盖住整个壳（含 titlebar，
    // 内部自带全屏 LayerFill 背景）；完成/跳过写标志后组件卸载，空 grid
    // 穿透（同 splash 注释：切勿给空 grid 添加背景）。
    let oobe: Element = if oobe_visible {
        component(oobe_component, set_oobe_visible.clone()).with_key("shell-oobe")
    } else {
        grid(()).into()
    };
    // 覆盖层 grid 必须显式 STAR 行/列：Grid 默认 Auto 会让 Stretch 子元素只
    // 覆盖内容大小（scrim 不全窗，遮罩不生效）——demo 已实证，同 interaction
    // 内部 grid 的修复。五个子元素重叠同一 cell（P-6 覆盖层模式）。
    grid((base, splash, interaction, diagram_zoom, diff_drawer, oobe))
        .rows([GridLength::STAR])
        .columns([GridLength::STAR])
        .into()
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
    // 壳侧性能诊断：恢复上次采集模式（默认 ZDR）；必须在 render 前注册
    // render observer（thread_local，UI 线程 = main 线程）。
    diagnostics::init();
    windows_reactor::set_render_observer(Some(Box::new(|info| {
        diagnostics::record_render(
            info.tree_build_ms,
            info.reconcile_ms,
            info.effects_ms,
            info.elements_diffed,
            info.elements_skipped,
            info.elements_created,
        );
    })));
    App::new()
        .title("DeepX")
        .inner_size(1200.0, 800.0)
        .backdrop(Backdrop::Mica)
        // 退出诊断（reactor #4787 on_exit）：窗口全关后、进程退出前执行。
        // 日志里出现此行 = 正常退出路径；闪退（崩溃/强杀）不会执行到这里，
        // 用于区分「正常关闭」与「异常终止」，辅助闪退调查。
        .on_exit(|| log_diag("app exit: all windows closed (normal path)"))
        // panic 诊断（reactor #4829 起 fatal）：on_fault API 已删除，callback panic
        // 不再转发给应用；reactor 会在 abort 前自行 emit
        // 「windows_reactor: {context} panicked: {msg}; aborting」，
        // 闪退调查可从该行（OutputDebugString/stderr）取源头证据。
        .render(app)
}
