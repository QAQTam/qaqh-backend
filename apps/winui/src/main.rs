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

use std::time::Duration;

use deepx_client::discovery;
use windows_reactor::*;
use windows_webview::{WebView, webview};

/// The daemon's debug endpoint, resolved before the message loop starts.
static FRONTEND_URL: std::sync::OnceLock<String> = std::sync::OnceLock::new();

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
            if let Some(url) = FRONTEND_URL.get() {
                log_diag(&format!("navigating to {url}"));
                let _ = ready.navigate(url);
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
    grid((titlebar, content))
        .rows([GridLength::Pixel(header::HEADER_HEIGHT), GridLength::STAR])
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
    let url = resolve_frontend_url();
    let _ = FRONTEND_URL.set(url);

    App::new()
        .title("DeepX")
        .inner_size(1200.0, 800.0)
        .backdrop(Backdrop::Mica)
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

    // 3. Static renderer output directory.
    if let Ok(dir) = std::env::var("DEEPX_UI_DIR") {
        if !dir.is_empty() {
            let path = std::path::Path::new(&dir)
                .join("index.html")
                .to_string_lossy()
                .replace('\\', "/");
            return format!("file:///{path}");
        }
    }

    // Fallback: daemon may already be reachable even if discovery raced.
    "about:blank".to_string()
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
