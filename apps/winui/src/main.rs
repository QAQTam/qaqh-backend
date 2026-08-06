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

    // Step 1: 内容区元素——左 XAML 侧栏（可拖拽宽度）+ 右 WebView2。
    let nav: Element =
        sidebar::sidebar(cx, bridge.clone(), sidebar_width, set_sidebar_width).into();
    let webview: Element = webview
        .horizontal_alignment(HorizontalAlignment::Stretch)
        .vertical_alignment(VerticalAlignment::Stretch)
        .into();

    // Step 2: Grid 两行——row0 = XAML 标题栏（48px，SetTitleBar 拖拽区，
    // host 自动接线 host.rs:277-288）；row1 = 内容区（侧栏 | WebView2）。
    // WebView2 从 row 1 开始，与拖拽区无输入区域重叠。
    let titlebar: Element = header::header(cx, bridge.clone())
        .grid_row(0)
        .grid_column(0)
        .into();
    // ── 内容区（row 1）：基础层（侧栏 | WebView2）────────────────
    // P-6 覆盖层预留（WORKFLOW §6.1）：未来 XAML 面板/对话框
    // （P1 Flyout anchor / P2 ContentDialog phantom child）作为覆盖层
    // 元素追加进本 Grid（同 cell 重叠渲染），零布局改动。
    let content: Element = grid((
        nav.grid_row(0).grid_column(0),
        webview.grid_row(0).grid_column(1),
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
    match discovery::ensure_daemon_running(std::time::Duration::from_secs(8)) {
        Ok(discovery) => {
            if let Ok(base) = discovery.base_url() {
                return format!("{base}/debug/");
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
