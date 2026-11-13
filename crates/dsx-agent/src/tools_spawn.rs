use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

use dsx_proto::{self, AgentToTools, ToolsToAgent};
use dsx_types::ToolDef;

use crate::tools;

/// Spawn the dsx-tools subprocess with piped stdin/stdout.
pub fn spawn_process(exe: &std::path::Path) -> (std::process::Child, Box<dyn BufRead + Send>, Box<dyn Write + Send>) {
    let mut tools_cmd = Command::new(
        if std::env::var("DSX_SINGLE_BINARY").is_ok() {
            exe.to_path_buf()
        } else {
            exe.with_file_name("dsx-tools")
        }
    );
    if std::env::var("DSX_SINGLE_BINARY").is_ok() {
        tools_cmd.arg("tools");
    }
    let mut child = tools_cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .spawn()
        .expect("dsx-agent: failed to spawn dsx-tools");

    let reader: Box<dyn BufRead + Send> = Box::new(BufReader::new(child.stdout.take().unwrap()));
    let writer: Box<dyn Write + Send> = Box::new(child.stdin.take().unwrap());
    (child, reader, writer)
}

/// Respawn the tools subprocess. Kills any existing process, spawns new one,
/// completes the Init/Ready handshake, and stores the child.
pub fn respawn(child: &mut Option<std::process::Child>) -> bool {
    if let Some(mut c) = child.take() {
        let _ = c.kill();
        let _ = c.wait();
    }

    let exe = match std::env::current_exe() {
        Ok(e) => e,
        Err(_) => return false,
    };
    let (mut new_child, mut reader, mut writer) = spawn_process(&exe);

    let init = AgentToTools::Init {
        allowed_tools: vec![], session_seed: "pipe".into(), auto_mode: false,
    };
    let _ = dsx_proto::write_frame(&mut writer, &init);
    let ready = dsx_proto::read_frame::<ToolsToAgent>(&mut reader).ok().flatten();

    match ready {
        Some(ToolsToAgent::Ready { tools }) => {
            let defs: Vec<ToolDef> = tools.iter().filter(|t| crate::tools::ESSENTIAL_TOOLS.contains(&t.function.name.as_str())).cloned().collect();
            tools::init_tools_ipc(reader, writer, defs);
            *child = Some(new_child);
            eprintln!("dsx-agent: tools respawned");
            true
        }
        _ => {
            eprintln!("dsx-agent: tools respawn failed (no Ready)");
            let _ = new_child.kill();
            let _ = new_child.wait();
            false
        }
    }
}
