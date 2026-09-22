use std::path::Path;

fn main() {
    println!("cargo:rerun-if-env-changed=QAQH_BUILD_ID");
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/index");
    println!("cargo:rerun-if-changed=../../webui/out/renderer");

    let build_id = std::env::var("QAQH_BUILD_ID")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(git_commit)
        .unwrap_or_else(|| {
            println!(
                "cargo:warning=QAQH_BUILD_ID fell back to CARGO_PKG_VERSION ({}) because the git commit could not be resolved",
                env!("CARGO_PKG_VERSION")
            );
            env!("CARGO_PKG_VERSION").to_string()
        });
    println!("cargo:rustc-env=QAQH_BUILD_ID={build_id}");

    ensure_webui_assets();
}

fn ensure_webui_assets() {
    let root = Path::new("../../webui/out/renderer");
    if root.join("index.html").exists() {
        return;
    }
    if std::env::var("PROFILE").as_deref() == Ok("release") {
        panic!(
            "webui/out/renderer/index.html is missing; run `bun run build` in webui/ before building the release gateway"
        );
    }
    let _ = std::fs::create_dir_all(root);
    let _ = std::fs::create_dir_all(root.join("assets"));
    let placeholder = r#"<!doctype html><html lang="zh-CN"><head><meta charset="utf-8"><title>QAQ Harness WebUI</title></head><body><h1>WebUI assets not built</h1><p>Run <code>bun run build</code> in <code>webui/</code> and rebuild <code>qaqh-webui-gateway</code>.</p></body></html>"#;
    let _ = std::fs::write(root.join("index.html"), placeholder);
    let _ = std::fs::write(root.join("assets/placeholder.js"), "export {};\n");
    println!(
        "cargo:warning=webui/out/renderer missing — generated a placeholder; run `bun run build` in webui/ for the real UI"
    );
}

fn git_commit() -> Option<String> {
    let output = std::process::Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}
