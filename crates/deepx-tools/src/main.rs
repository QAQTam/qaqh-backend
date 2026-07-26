//! Standalone deepx-tools binary — run individual tools from CLI.
//!
//! Usage:
//!   deepx-tools <tool_name> [json_args]
//!   deepx-tools explore
//!   deepx-tools read_file '{"path":"src/main.rs","start_line":1,"end_line":50}'
//!   deepx-tools list

use std::env;

fn main() {
    deepx_tools::runtime::init_tools("cli", &[], vec![]);
    deepx_tools::runtime::set_context("cli", 4);
    let cwd = env::current_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|_| ".".into());
    deepx_tools::workspace::set_process_workspace(&cwd);

    let args: Vec<String> = env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: deepx-tools <tool> [json_args]");
        eprintln!("       deepx-tools list");
        std::process::exit(1);
    }

    let tool = &args[1];
    if tool == "list" {
        let defs = deepx_tools::runtime::all_tools();
        println!("Available tools:");
        for def in &defs {
            println!("  {} — {}", def.function.name, def.function.description);
        }
        println!("\n{} tools registered", defs.len());
        return;
    }

    let json_args = args.get(2).map(|s| s.as_str()).unwrap_or("{}");
    let parsed_args: serde_json::Value = serde_json::from_str(json_args).unwrap_or_else(|_| {
        eprintln!("Error: invalid JSON args '{}'", json_args);
        std::process::exit(1);
    });

    let r = deepx_tools::execution::execute_with_context(
        tool,
        "",
        &parsed_args.to_string(),
        "cli_0",
        None,
    );

    println!("{}", r.content);
    if !r.success {
        std::process::exit(1);
    }
}
