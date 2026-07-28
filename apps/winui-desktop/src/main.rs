//! DeepX WinUI Desktop — minimal prototype.
//! Mica window with WebView2 loading existing SolidJS frontend.
//!
//! Start Vite dev server in `apps/desktop` first, then run this binary.
//! Or set `DEEPX_UI_DIR` to a built renderer output directory.

#![windows_subsystem = "windows"]

use windows_reactor::*;
use windows_webview::{WebView, webview};

const DEV_URL: &str = "http://localhost:5173";

fn app(_cx: &mut RenderCx) -> Element {
    webview(|ready: WebView| {
        let url = frontend_url();
        let _ = ready.navigate(&url);
    })
    .into()
}

fn frontend_url() -> String {
    if let Ok(dir) = std::env::var("DEEPX_UI_DIR") {
        let path = std::path::Path::new(&dir)
            .join("index.html")
            .to_string_lossy()
            .replace('\\', "/");
        format!("file:///{path}")
    } else {
        DEV_URL.to_string()
    }
}

fn main() -> Result<()> {
    App::new()
        .title("DeepX")
        .backdrop(Backdrop::Mica)
        .render(app)
}
