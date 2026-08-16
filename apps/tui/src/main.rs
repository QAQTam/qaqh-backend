//! qaqh-tui — temporary ratatui shell for QAQ-Harness.
//!
//! Two entry modes:
//!   qaqh-tui --start     create a fresh session (locked to minimal:dsh)
//!   qaqh-tui --resume    pick an existing session from the list (default)
//!
//! The tool mode is pinned to `minimal:dsh` (bash + str_replace_editor) by
//! construction: creation pre-persists it before the worker spawns, resume
//! re-issues `session.set_tool_mode`, and the UI exposes no mode switch.

mod app;
mod backend;
mod transcript;
mod ui;

use app::{App, StartMode};

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut start = false;
    let mut resume = false;
    for arg in &args {
        match arg.as_str() {
            "--start" | "-s" => start = true,
            "--resume" | "-r" => resume = true,
            "--help" | "-h" => {
                print_usage();
                return;
            }
            other => {
                eprintln!("qaqh-tui: unknown argument '{other}'");
                print_usage();
                std::process::exit(2);
            }
        }
    }
    if start && resume {
        eprintln!("qaqh-tui: --start and --resume are mutually exclusive");
        std::process::exit(2);
    }
    let mode = if start { StartMode::Start } else { StartMode::Resume };

    let runtime = match tokio::runtime::Builder::new_multi_thread().enable_all().build() {
        Ok(runtime) => runtime,
        Err(error) => {
            eprintln!("qaqh-tui: tokio runtime failed: {error}");
            std::process::exit(1);
        }
    };

    runtime.block_on(async {
        if let Err(error) = App::run(mode).await {
            ratatui::restore();
            eprintln!("qaqh-tui: {error}");
            std::process::exit(1);
        }
    });
}

fn print_usage() {
    println!(
        "qaqh-tui — temporary QAQ-Harness terminal shell (tool mode locked to minimal:dsh)\n\
         \n\
         USAGE:\n\
         \x20 qaqh-tui [--start | --resume]\n\
         \n\
         OPTIONS:\n\
         \x20 --start    create a new session and attach immediately\n\
         \x20 --resume   open the session selector (default when no flag is given)\n\
         \n\
         KEYS:\n\
         \x20 Enter        send message / open selected session\n\
         \x20 Esc          clear input; on empty input quit\n\
         \x20 Ctrl+C       cancel running turn; when idle quit\n\
         \x20 Ctrl+D       quit\n\
         \x20 ↑/↓ PageUp/PageDown   scroll transcript\n\
         \x20 End          jump to the bottom of the transcript"
    );
}
