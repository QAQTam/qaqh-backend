//! DeepX WinUI desktop shell — minimal viable prototype.
//!
//! A Mica window hosting the existing SolidJS renderer through WebView2,
//! pointed at the daemon's `/debug/` endpoint, with the full `window.deepx`
//! bridge (see `bridge.rs`) forwarding to `deepx-client`.
//!
//! Frontend resolution order:
//!   1. `DEEPX_DEBUG_URL` env var (any URL, e.g. a Vite dev server)
//!   2. daemon discovery → `http://<host>/debug/`
//!   3. `DEEPX_UI_DIR` env var → `file://<dir>/index.html`

#![windows_subsystem = "windows"]

mod bridge;
mod header;
mod shell;
mod shell_store;
mod sidebar;
mod skills_view;

use std::cell::RefCell;
use std::rc::Rc;
use std::time::{Duration, Instant};

use deepx_client::discovery;
use windows_reactor::*;
use windows_webview::{WebView, webview};

/// 初始导航状态机。
///
/// daemon 冷启动可达 40s+（加载历史会话），壳的固定等待窗口
/// （ensure 8s + /debug/ 就绪 6s）在其首次被拉起时必然不够；WebView2
/// 导航到未就绪端口会显示错误页且不自动重试。此状态机在导航失败或
/// URL 未就绪时每 [`NAV_RETRY_INTERVAL`] 重新解析并导航，直到初始导航
/// 成功（成功后不再干预，页面内后续导航不受影响）。
#[derive(Default)]
struct NavState {
    /// 当前目标 URL；`None` = 尚未拿到可用 URL（daemon 未就绪）。
    url: Option<String>,
    /// 初始导航是否已成功；成功后不再自动重试。
    succeeded: bool,
    /// 重试定时器（排定中为 `Some`，避免重复排定）。
    timer: Option<DispatcherTimer>,
    /// NavigationCompleted 注册（保持 revoker 存活）。
    completed: Option<windows_webview::EventRegistration>,
    /// WebView 句柄（重试导航用；UI 线程独占，不 Send）。
    webview: Option<WebView>,
}

const NAV_RETRY_INTERVAL: Duration = Duration::from_secs(3);

/// 开屏覆盖层最长显示时间：超过后切换为失败文案并露出 renderer 的错误详情。
const SPLASH_TIMEOUT: Duration = Duration::from_secs(60);

/// 排定一次导航重试（幂等：已有排定中的 timer 则跳过）。
fn schedule_retry(state: &Rc<RefCell<NavState>>) {
    if state.borrow().timer.is_some() {
        return;
    }
    let retry_state = state.clone();
    match DispatcherTimer::new(NAV_RETRY_INTERVAL, move || {
        // 重新解析前端 URL（内部 ensure daemon + /debug/ 就绪轮询）。
        // 此回调在 UI 线程同步执行，最坏阻塞约 14s（daemon 未就绪时），
        // 仅发生在初始导航失败后的启动窗口内，可接受。
        let url = resolve_frontend_url();
        let mut s = retry_state.borrow_mut();
        s.timer = None;
        if url == "about:blank" {
            drop(s);
            log_diag("frontend url still not ready; will retry");
            schedule_retry(&retry_state);
            return;
        }
        s.url = Some(url.clone());
        log_diag(&format!("retrying navigation to {url}"));
        if let Some(webview) = s.webview.as_ref()
            && let Err(e) = webview.navigate(&url)
        {
            log_diag(&format!("retry navigate failed: {e}"));
            // navigate 同步失败不会触发 NavigationCompleted，手动重排定。
            drop(s);
            schedule_retry(&retry_state);
        }
    }) {
        Ok(t) => state.borrow_mut().timer = Some(t),
        Err(e) => log_diag(&format!("retry timer failed: {e}")),
    }
}

/// The daemon's debug endpoint, resolved before the message loop starts.
static FRONTEND_URL: std::sync::OnceLock<String> = std::sync::OnceLock::new();

/// 本地 renderer 的虚拟主机名（WebView2 `set_virtual_host_name_to_folder_mapping`）。
/// 页面以 `https://appassets.local/` 真实 https origin 加载——ES module、
/// 字体、fetch 均正常（区别于 file:// 的 CORS/module 限制）。
const APPASSETS_HOST: &str = "appassets.local";

/// 本地 renderer 产物目录（虚拟主机映射用），`main` 里解析一次。
/// 存在则页面来源为 `https://appassets.local/`（秒开，不依赖 daemon）；
/// 不存在则回退 daemon 的 `/debug/`。
static RENDERER_DIR: std::sync::OnceLock<Option<std::path::PathBuf>> =
    std::sync::OnceLock::new();

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

    let webview = webview({
        let bridge = bridge.clone();
        move |ready: WebView| {
            log_diag("on_ready");
            bridge.attach_webview(ready.clone());
            let handler = {
                let bridge = bridge.clone();
                move |args: windows_webview::WebMessageReceivedArgs| {
                    let raw = args.web_message_as_json();
                    log_diag(&format!("msg in: {raw}"));
                    bridge.handle_message(raw);
                }
            };
            match ready.on_web_message_received(handler) {
                Ok(reg) => {
                    log_diag("message handler registered");
                    bridge.attach_registration(reg);
                }
                Err(e) => log_diag(&format!("message handler failed: {e}")),
            }
            // ── 页面来源 + 初始导航（失败自动重试）────────────────
            // 优先级：DEEPX_DEBUG_URL（dev Vite server）→ 本地 renderer 目录
            // （WebView2 虚拟主机映射 https://appassets.local/，页面秒开，
            // 不依赖 daemon 就绪）→ daemon 的 /debug/（浏览器调试入口/兜底）。
            // 先注册 NavigationCompleted 再导航：本地回环加载极快时，
            // 首个完成事件可能在注册前触发而丢失。
            let debug_url = std::env::var("DEEPX_DEBUG_URL").ok().filter(|u| !u.is_empty());
            let initial_url: Option<String> = if let Some(u) = debug_url {
                Some(u)
            } else if let Some(dir) = RENDERER_DIR.get().and_then(|d| d.as_ref()) {
                match ready.set_virtual_host_name_to_folder_mapping(
                    APPASSETS_HOST,
                    &dir.to_string_lossy(),
                    windows_webview::HostResourceAccessKind::DenyCors,
                ) {
                    Ok(()) => {
                        let url = format!("https://{APPASSETS_HOST}/index.html");
                        log_diag(&format!(
                            "serving local renderer at {url} ({})",
                            dir.display()
                        ));
                        Some(url)
                    }
                    Err(e) => {
                        log_diag(&format!("virtual host mapping failed: {e}; falling back"));
                        FRONTEND_URL.get().cloned()
                    }
                }
            } else {
                FRONTEND_URL.get().cloned()
            };
            let state = Rc::new(RefCell::new(NavState::default()));
            let nav_state = state.clone();
            match ready.on_navigation_completed(move |args| {
                let mut s = nav_state.borrow_mut();
                if s.succeeded {
                    return;
                }
                let is_real = s.url.as_deref().is_some_and(|u| u != "about:blank");
                if args.is_success() && is_real {
                    log_diag("initial navigation succeeded");
                    s.succeeded = true;
                    return;
                }
                log_diag("initial navigation failed; scheduling retry");
                drop(s);
                schedule_retry(&nav_state);
            }) {
                Ok(reg) => state.borrow_mut().completed = Some(reg),
                Err(e) => log_diag(&format!("navigation-completed handler failed: {e}")),
            }
            state.borrow_mut().webview = Some(ready.clone());
            if let Some(url) = initial_url {
                log_diag(&format!("navigating to {url}"));
                if url != "about:blank" {
                    state.borrow_mut().url = Some(url.clone());
                    if let Err(e) = ready.navigate(&url) {
                        log_diag(&format!("initial navigate failed: {e}"));
                        // 同步失败不会触发 NavigationCompleted，直接进入重试。
                        schedule_retry(&state);
                    }
                }
            }
            if state.borrow().url.is_none() {
                // 首次解析失败：立即进入重试循环（resolve 会重新 ensure daemon）。
                log_diag("frontend url not ready; scheduling initial retry");
                schedule_retry(&state);
            }
        }
    });

    // Step 1: 内容区元素——左 XAML 侧栏（可拖拽宽度）+ 右区。
    // 右区 = 内层 Grid 两行（WORKFLOW §8 壳主导视图族）：
    //   - row0 = WebView2（renderer）——view≠skills 时 STAR（常驻，不销毁）；
    //   - row1 = XAML 技能页——view=skills 时 STAR。
    // 非当前视图的行高 0：WebView2 尺寸 0 保留导航状态（不销毁不重建），
    // XAML 技能页零命中零渲染；无 opacity/命中测试依赖。
    let nav: Element =
        sidebar::sidebar(cx, bridge.clone(), sidebar_width, set_sidebar_width).into();
    let (view, set_view) = cx.use_state::<String>("home".to_string());
    let view_timer = cx.use_ref::<Option<DispatcherTimer>>(None);
    let last_view = cx.use_ref::<String>("home".to_string());
    cx.use_effect((), {
        let bridge = bridge.clone();
        let set_view = set_view.clone();
        let view_timer = view_timer.clone();
        let last_view = last_view.clone();
        move || {
            if view_timer.borrow().is_none() {
                match DispatcherTimer::new(Duration::from_millis(250), {
                    let bridge = bridge.clone();
                    let set_view = set_view.clone();
                    let last_view = last_view.clone();
                    move || {
                        let v = bridge.core().current_view();
                        if v != *last_view.borrow() {
                            *last_view.borrow_mut() = v.clone();
                            set_view.call(v);
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
    let webview: Element = webview
        .horizontal_alignment(HorizontalAlignment::Stretch)
        .vertical_alignment(VerticalAlignment::Stretch)
        .grid_row(0)
        .grid_column(0)
        .into();
    let skills: Element = skills_view::skills_view(cx, bridge.clone())
        .grid_row(1)
        .grid_column(0)
        .into();
    let right: Element = grid((webview, skills))
        .rows([
            if view == "skills" {
                GridLength::Pixel(0.0)
            } else {
                GridLength::STAR
            },
            if view == "skills" {
                GridLength::STAR
            } else {
                GridLength::Pixel(0.0)
            },
        ])
        .grid_row(0)
        .grid_column(1)
        .into();

    // ── 开屏覆盖层（P-6 同 cell 重叠预留的首次应用）────────────────
    // 页面（WebView2）本地映射秒开，但 daemon 冷启动可达数十秒；等待期内
    // renderer 会显示 "Backend disconnected" 错误横幅——覆盖层用原生
    // ProgressRing 动画替代它，桥连上 daemon 即移除。
    // 顺序语义：connected 分支优先于 timeout 分支（超时瞬间后端恰好连上
    // 时覆盖层正常消失，不卡失败态）。超时（[`SPLASH_TIMEOUT`]）后释放
    // 覆盖层，露出 renderer 的错误详情与可交互界面（含标题栏）——覆盖层
    // 使命仅为启动期动画，不做错误拦截。
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
    // WebView2 从 row 1 开始，与拖拽区无输入区域重叠。
    let titlebar: Element = header::header(cx, bridge.clone())
        .grid_row(0)
        .grid_column(0)
        .into();
    // ── 内容区（row 1）：基础层（侧栏 | 右区）────────────────
    // P-6 覆盖层预留（WORKFLOW §6.1）：未来 XAML 面板/对话框
    // （P1 Flyout anchor / P2 ContentDialog phantom child）作为覆盖层
    // 元素追加进本 Grid（同 cell 重叠渲染），零布局改动。
    let content: Element = grid((
        nav.grid_row(0).grid_column(0),
        right,
    ))
    .columns([GridLength::Pixel(sidebar_width), GridLength::STAR])
    .grid_row(1)
    .grid_column(0)
    .into();
    let base: Element = grid((titlebar, content))
        .rows([GridLength::Pixel(header::HEADER_HEIGHT), GridLength::STAR])
        .into();
    // 覆盖层与基础层同 cell 重叠渲染（P-6 预留模式），盖住 titlebar + 内容区。
    // 注意：`splash_visible=false` 时的空 `grid(())` 依赖 WinUI"无背景元素不参与
    // 命中测试"的平台行为实现点击穿透——切勿给空 grid 添加背景，否则会无声
    // 拦截下方 WebView2 的输入。
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
    grid((base, splash)).into()
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
    let renderer_dir = resolve_local_renderer_dir();
    if let Some(dir) = &renderer_dir {
        log_diag(&format!("local renderer dir: {}", dir.display()));
    }
    let _ = RENDERER_DIR.set(renderer_dir);
    // 页面来源选择：
    // - DEEPX_DEBUG_URL（dev Vite server）→ 直接用它；
    // - 本地 renderer 可用 → 不阻塞等待 daemon（页面秒开，daemon 由桥
    //   后台连接），FRONTEND_URL 仅作兜底占位；
    // - 否则 → 解析 daemon /debug/（阻塞等待，与旧行为一致）。
    let has_debug_url = std::env::var("DEEPX_DEBUG_URL").is_ok_and(|u| !u.is_empty());
    let url = if has_debug_url || RENDERER_DIR.get().is_none_or(|d| d.is_none()) {
        resolve_frontend_url()
    } else {
        "about:blank".to_string()
    };
    let _ = FRONTEND_URL.set(url);

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

fn resolve_frontend_url() -> String {
    // 1. Explicit debug URL (e.g. Vite dev server).
    if let Ok(url) = std::env::var("DEEPX_DEBUG_URL") {
        if !url.is_empty() {
            return url;
        }
    }

    // 2. Daemon debug endpoint: ensures the daemon is running, then uses
    //    its discovery to build `http://<host>/debug/`.
    //    `ensure_daemon_running` 只保证 pid 存活（进程已启动），不保证
    //    HTTP 监听就绪（daemon 启动/重启窗口）——先健康检查再导航，
    //    避免 WebView2 导航到未就绪端口（错误页无自动重试）。
    //    若已有 daemon 在跑（含 release 安装版），discovery pid 存活即
    //    直接复用，不 spawn 新实例。
    match discovery::ensure_daemon_running(std::time::Duration::from_secs(8)) {
        Ok(discovery) => {
            if let Ok(base) = discovery.base_url() {
                if let Some(url) = wait_for_frontend_ready(&base, std::time::Duration::from_secs(6))
                {
                    return url;
                }
                eprintln!("[winui] /debug/ not ready at {base}");
            }
            eprintln!("[winui] cannot derive base url from discovery");
        }
        Err(err) => eprintln!("[winui] daemon unavailable: {err}"),
    }

    // 3. 本地 renderer 目录由虚拟主机映射处理（resolve_local_renderer_dir），
    //    不再生成 file:// 路径——WebView2（标准 Chromium）下 file:// 加载
    //    ES module 产物会被 CORS 拦截。

    // Fallback: daemon may already be reachable even if discovery raced.
    "about:blank".to_string()
}

/// 本地 renderer 产物目录（虚拟主机映射用）。定位优先级：
/// 1. `DEEPX_UI_DIR`（显式覆盖；旧语义是 file:// 路径，现已废弃——
///    WebView2 下 file:// 会拦 ES module，统一改为映射为虚拟主机）；
/// 2. exe 旁 `resources/out/renderer`（安装布局）；
/// 3. exe 旁 `out/renderer`（dev 布局）。
fn resolve_local_renderer_dir() -> Option<std::path::PathBuf> {
    if let Ok(dir) = std::env::var("DEEPX_UI_DIR") {
        if !dir.is_empty() {
            // 相对路径按当前工作目录绝对化：存在性检查与 WebView2 映射
            // （相对路径解释为相对 exe 目录）的基准保持一致。
            let p = std::path::PathBuf::from(&dir);
            let p = if p.is_absolute() {
                p
            } else {
                std::env::current_dir().unwrap_or_default().join(p)
            };
            if p.join("index.html").is_file() {
                return Some(p);
            }
        }
    }
    let exe_dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    for base in [
        exe_dir.join("resources").join("out").join("renderer"),
        exe_dir.join("out").join("renderer"),
    ] {
        if base.join("index.html").is_file() {
            return Some(base);
        }
    }
    None
}

/// 等待 daemon 的 `/debug/` 端点就绪（HTTP 200），返回完整前端 URL。
///
/// daemon 启动/重启窗口内 discovery 已发布但监听未就绪——WebView2 导航
/// 到未就绪端口会显示错误页且不自动重试；此处同步轮询（300ms 间隔），
/// 就绪后才让 WebView2 导航。
fn wait_for_frontend_ready(base: &str, timeout: std::time::Duration) -> Option<String> {
    let url = format!("{base}/debug/");
    let deadline = std::time::Instant::now() + timeout;
    loop {
        if debug_endpoint_ready(&url) {
            return Some(url);
        }
        if std::time::Instant::now() >= deadline {
            return None;
        }
        std::thread::sleep(std::time::Duration::from_millis(300));
    }
}

/// 轻量 HTTP GET（同步 TcpStream，不依赖 tokio）：200 即就绪。
fn debug_endpoint_ready(url: &str) -> bool {
    use std::io::{Read, Write};
    use std::net::TcpStream;

    let Some(rest) = url
        .strip_prefix("http://")
        .or_else(|| url.strip_prefix("https://"))
    else {
        return false;
    };
    let host_port = rest.split('/').next().unwrap_or("");
    if host_port.is_empty() {
        return false;
    }
    let Ok(mut stream) = TcpStream::connect(host_port) else {
        return false;
    };
    let _ = stream.set_read_timeout(Some(std::time::Duration::from_millis(1500)));
    let _ = stream.set_write_timeout(Some(std::time::Duration::from_millis(1500)));
    let req = format!("GET /debug/ HTTP/1.0\r\nHost: {host_port}\r\nConnection: close\r\n\r\n");
    if stream.write_all(req.as_bytes()).is_err() {
        return false;
    }
    let mut buf = [0u8; 64];
    let Ok(n) = stream.read(&mut buf) else {
        return false;
    };
    let head = String::from_utf8_lossy(&buf[..n]);
    head.starts_with("HTTP/1.0 200") || head.starts_with("HTTP/1.1 200")
}
