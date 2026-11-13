//! dsx-sudo: standalone privilege-elevation helper for DSX.
//!
//! Zero external dependencies — std only. Tiny binary, safe for setuid.
//!
//! USAGE
//!   dsx-sudo <command> [args...]
//!
//! OUTPUT (JSON to stdout)
//!   {"ok":true,"exit":0,"stdout":"...","stderr":"..."}
//!   {"ok":false,"error":"reason"}
//!
//! SECURITY
//!   • setuid root: chown root:root && chmod u+s /path/to/dsx-sudo
//!   • Whitelist-only — only predefined safe commands allowed.
//!   • Arg sanitization — all args must match [a-zA-Z0-9_./@:~-+=]
//!   • No shell expansion — uses Command::new().args() (execvp style).
//!   • Exits immediately — privilege auto-revoked, no lingering session.

use std::io::{self, Write};
use std::process::Command;

struct Rule {
    cmd: &'static str,
    arg_ok: &'static [&'static str],
    max_args: usize,
}

const RULES: &[Rule] = &[
    Rule { cmd: "apt",          arg_ok: &["install", "update", "upgrade", "remove", "autoremove", "purge", "list"], max_args: 12 },
    Rule { cmd: "apt-get",      arg_ok: &["install", "update", "upgrade", "remove", "autoremove", "purge"], max_args: 10 },
    Rule { cmd: "dnf",          arg_ok: &["install", "update", "upgrade", "remove", "autoremove", "list"], max_args: 10 },
    Rule { cmd: "yum",          arg_ok: &["install", "update", "upgrade", "remove", "list"], max_args: 10 },
    Rule { cmd: "zypper",       arg_ok: &["install", "update", "upgrade", "remove", "list"], max_args: 10 },
    Rule { cmd: "pacman",       arg_ok: &["-S", "-R", "-U", "-Sy", "-Syu", "-Qs"], max_args: 10 },
    Rule { cmd: "pkg",          arg_ok: &["install", "update", "upgrade", "remove", "list", "audit"], max_args: 10 },
    Rule { cmd: "brew",         arg_ok: &["install", "update", "upgrade", "remove", "list", "services"], max_args: 10 },
    Rule { cmd: "snap",         arg_ok: &["install", "remove", "list", "refresh", "revert"], max_args: 6 },
    Rule { cmd: "systemctl",    arg_ok: &["start", "stop", "restart", "reload", "status", "enable", "disable", "is-active", "is-enabled", "daemon-reload", "list-units", "show", "cat", "help"], max_args: 6 },
    Rule { cmd: "journalctl",   arg_ok: &["-u", "--unit", "-n", "--since", "--until", "-f", "-e", "-x", "--no-pager", "-p", "--list-boots", "--disk-usage", "--vacuum"], max_args: 8 },
    Rule { cmd: "service",      arg_ok: &["start", "stop", "restart", "status", "reload"], max_args: 5 },
    Rule { cmd: "sysctl",       arg_ok: &["-w", "-p", "-a", "-n"], max_args: 5 },
    Rule { cmd: "mkdir",        arg_ok: &["-p"], max_args: 5 },
    Rule { cmd: "cp",           arg_ok: &["-r", "-rf", "-f", "-n", "-u", "--preserve", "--parents"], max_args: 6 },
    Rule { cmd: "mv",           arg_ok: &["-f", "-i", "-n", "-u"], max_args: 6 },
    Rule { cmd: "rm",           arg_ok: &["-rf", "-r", "-f", "-I"], max_args: 5 },
    Rule { cmd: "ln",           arg_ok: &["-s", "-sf", "-f", "-n"], max_args: 5 },
    Rule { cmd: "chmod",        arg_ok: &["a+x", "+x", "u+x", "go+r", "go-w", "g+", "o-", "u+", "a+", "=", "755", "644", "777", "600", "400", "500", "700", "750", "664"], max_args: 5 },
    Rule { cmd: "chown",        arg_ok: &[], max_args: 0 },
    Rule { cmd: "df",           arg_ok: &["-h", "-H", "-T", "-i"], max_args: 3 },
    Rule { cmd: "du",           arg_ok: &["-sh", "-h", "-cha", "--max-depth", "-d"], max_args: 5 },
    Rule { cmd: "free",         arg_ok: &["-h", "-m", "-g", "-w"], max_args: 3 },
    Rule { cmd: "lsblk",        arg_ok: &["-f", "-o", "-J", "-P", "-l", "-d", "-n"], max_args: 5 },
    Rule { cmd: "blkid",        arg_ok: &[], max_args: 3 },
    Rule { cmd: "fdisk",        arg_ok: &["-l"], max_args: 2 },
    Rule { cmd: "lsof",         arg_ok: &["-i", "-P", "-n", "-p", "-u", "-c"], max_args: 6 },
    Rule { cmd: "ss",           arg_ok: &["-tlnp", "-ulnp", "-tunap", "-t", "-u", "-l", "-n", "-p"], max_args: 6 },
    Rule { cmd: "ip",           arg_ok: &["addr", "link", "route", "neigh", "netns", "-s", "-br", "a", "l", "r", "n"], max_args: 8 },
    Rule { cmd: "ping",         arg_ok: &["-c", "-i", "-W", "-4", "-6"], max_args: 5 },
    Rule { cmd: "traceroute",   arg_ok: &["-n", "-m", "-w", "-q", "-4", "-6", "-I"], max_args: 5 },
    Rule { cmd: "sshd",         arg_ok: &["-T", "-t"], max_args: 3 },
    Rule { cmd: "ufw",          arg_ok: &["status", "enable", "disable", "allow", "deny", "reload"], max_args: 5 },
];

fn main() {
    let args: Vec<String> = std::env::args().collect();
    if args.len() < 2 {
        eprintln!("Usage: dsx-sudo <command> [args...]");
        std::process::exit(1);
    }

    let cmd = &args[1];
    let cmd_args = &args[2..];

    let rule = match RULES.iter().find(|r| r.cmd == cmd.as_str()) {
        Some(r) => r,
        None => json_exit(false, &json_err(&format!("not whitelisted: {cmd}")), 1),
    };

    if rule.max_args > 0 && cmd_args.len() > rule.max_args {
        json_exit(false, &json_err(&format!("too many args for {cmd}: max {}", rule.max_args)), 1);
    }

    if rule.arg_ok.is_empty() {
        json_exit(false, &json_err(&format!("{cmd} is blocked")), 1);
    }

    if !rule.arg_ok.is_empty() {
        let first = cmd_args.first().map(|s| s.as_str()).unwrap_or("");
        let allowed = rule.arg_ok.iter().any(|ok| first.starts_with(ok));
        if !allowed {
            json_exit(false, &json_err(&format!("{cmd}: first arg must be one of: {}", rule.arg_ok.join(", "))), 1);
        }
    }

    for (i, arg) in cmd_args.iter().enumerate() {
        if !arg.chars().all(|c| c.is_alphanumeric() || "./_@:~-+=".contains(c)) {
            json_exit(false, &json_err(&format!("arg {} has invalid chars: {:?}", i + 1, arg)), 1);
        }
    }

    let output = match Command::new(cmd).args(cmd_args).output() {
        Ok(o) => o,
        Err(e) => json_exit(false, &json_err(&format!("exec: {e}")), 1),
    };

    let out = json_result(
        output.status.success(),
        output.status.code().unwrap_or(-1),
        &String::from_utf8_lossy(&output.stdout),
        &String::from_utf8_lossy(&output.stderr),
    );
    let _ = writeln!(io::stdout(), "{out}");
}

// ── Manual JSON construction (zero deps) ──

fn json_escape(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 8);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => {
                out.push_str(&format!("\\u{:04x}", c as u32));
            }
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

fn json_err(msg: &str) -> String {
    format!(r#"{{"ok":false,"error":{}}}"#, json_escape(msg))
}

fn json_result(ok: bool, exit: i32, stdout: &str, stderr: &str) -> String {
    format!(
        r#"{{"ok":{},"exit":{},"stdout":{},"stderr":{}}}"#,
        ok,
        exit,
        json_escape(stdout),
        json_escape(stderr),
    )
}

fn json_exit(ok: bool, json: &str, code: i32) -> ! {
    let _ = writeln!(io::stdout(), "{json}");
    std::process::exit(code);
}
