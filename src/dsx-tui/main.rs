use clap::Parser;

mod prompt;
mod tui_text;
mod config;
mod types;
mod api;
mod tools;
mod context;
mod tokenizer;
mod memory;
mod session;
mod skills;
mod router;
mod dsc_log;
mod health;
mod conversation;
mod orchestrator;
mod tui;

#[derive(Parser)]
#[command(name = "dsx", about = "DeepSeek X — self-evolving AI coding agent")]
struct Cli {
    /// Resume a previous session (TUI session picker)
    #[arg(long)]
    resume: bool,

    /// List all saved sessions
    #[arg(long)]
    list_sessions: bool,

    /// List all installed skills
    #[arg(long)]
    list_skills: bool,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    #[cfg(debug_assertions)]
    std::env::set_var("DSC_LOG", "debug");
    crate::dsc_log::init();
    let cli = Cli::parse();
    log::info!("dsx v{} starting", env!("CARGO_PKG_VERSION"));
    let config = config::Config::load()?;

    if cli.list_sessions {
        let metas = session::load_index();
        if metas.is_empty() {
            println!("No saved sessions.");
        } else {
            for m in &metas {
                let date = if m.updated_at > 0 {
                    let secs = m.updated_at;
                    let days = secs / 86400;
                    let hrs = (secs % 86400) / 3600;
                    let mins = (secs % 3600) / 60;
                    format!("{}d{}h{}m", days, hrs, mins)
                } else {
                    "-".into()
                };
                println!("  {}  {}  {:22}  {}", m.seed, date, m.model, m.last_summary);
            }
        }
        return Ok(());
    }

    if cli.list_skills {
        let skills = skills::global_skills();
        let all = skills.all();
        if all.is_empty() {
            println!("No skills installed.");
            println!("Add skills to ~/.config/dsx/skills/<name>/SKILL.md");
        } else {
            println!("Installed skills ({}):\n", all.len());
            for s in all {
                let preview = if s.description.len() > 80 {
                    s.description.chars().take(60).collect::<String>() + "…"
                } else {
                    s.description.clone()
                };
                println!("  {}  —  {}", s.name, preview);
            }
            println!("\nUse list_skills tool in dsx to see full descriptions.");
        }
        return Ok(());
    }

    tui::run(config, cli.resume).await
}
