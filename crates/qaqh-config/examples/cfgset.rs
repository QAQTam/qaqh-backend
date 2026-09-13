//! BUG-2026-09-13-11 并发回归夹具：以**独立进程**调用 `SecretStore` 的
//! read-modify-write，用于验证 `secrets.toml` 跨进程并发写不丢键。
//!
//! 用法（由 `tests/secrets_concurrent_write.rs` 驱动，也可手工压测）：
//!
//! ```text
//! cargo run -p qaqh-config --example cfgset -- set --path <secrets.toml> \
//!     --mcp <name> <value> --repeat 12
//! ```
//!
//! 每个进程持自己的 `SecretStore`（因此持各自的 `secrets.toml.lock`，
//! 语义等价于 daemon 与 CLI 两个独立进程各写一次）——这正是 issue 中
//! "daemon（webUI 保存配置）与 CLI（`qaqh-daemon mcp import --exec`）
//! 并发写"的最小可复现拓扑。

use qaqh_config::secrets::{SecretSlot, SecretStore};

fn usage() -> ! {
    eprintln!(
        "usage: cfgset set --path <secrets.toml> [--mcp <name> <value>] \
         [--main <value>] [--repeat <n>] [--space <ms>]"
    );
    std::process::exit(2);
}

fn main() {
    let args: Vec<String> = std::env::args().skip(1).collect();
    if args.first().map(String::as_str) != Some("set") {
        usage();
    }
    let mut path: Option<String> = None;
    let mut mcp: Option<(String, String)> = None;
    let mut main_value: Option<String> = None;
    let mut repeat = 1usize;
    let mut space_ms = 0u64;

    let mut index = 1;
    while index < args.len() {
        let flag = args[index].as_str();
        let value = args.get(index + 1).cloned().unwrap_or_default();
        match flag {
            "--path" => path = Some(value),
            "--mcp" => {
                let name = value;
                let secret = args.get(index + 2).cloned().unwrap_or_default();
                mcp = Some((name, secret));
                index += 1;
            }
            "--main" => main_value = Some(value),
            "--repeat" => repeat = value.parse().unwrap_or(1).max(1),
            "--space" => space_ms = value.parse().unwrap_or(0),
            _ => usage(),
        }
        index += 2;
    }

    let Some(path) = path else { usage() };
    let store = SecretStore::new(path.into());
    for round in 0..repeat {
        if let Some((name, secret)) = &mcp
            && let Err(error) = store.set_mcp(name, secret)
        {
            eprintln!("cfgset: set_mcp({name}) round {round} failed: {error}");
            std::process::exit(1);
        }
        if let Some(secret) = &main_value
            && let Err(error) = store.set(SecretSlot::Main, secret)
        {
            eprintln!("cfgset: set(main) round {round} failed: {error}");
            std::process::exit(1);
        }
        if space_ms > 0 {
            std::thread::sleep(std::time::Duration::from_millis(space_ms));
        }
    }
}
