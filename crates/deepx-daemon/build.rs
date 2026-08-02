use std::process::Command;

fn main() {
    println!("cargo:rerun-if-env-changed=DEEPX_BUILD_ID");
    println!("cargo:rerun-if-env-changed=DEEPX_CHANNEL");
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/index");

    let build_id = std::env::var("DEEPX_BUILD_ID")
        .ok()
        .filter(|value| !value.trim().is_empty())
        .or_else(git_commit)
        .unwrap_or_else(|| {
            println!(
                "cargo:warning=DEEPX_BUILD_ID fell back to CARGO_PKG_VERSION ({}) because the git commit could not be resolved; packaged daemon will fail the desktop identity check unless the manifest uses the same value",
                env!("CARGO_PKG_VERSION")
            );
            env!("CARGO_PKG_VERSION").to_string()
        });
    println!("cargo:rustc-env=DEEPX_BUILD_ID={build_id}");
}

fn git_commit() -> Option<String> {
    let output = Command::new("git")
        .args(["rev-parse", "HEAD"])
        .output()
        .ok()?;
    output
        .status
        .success()
        .then(|| String::from_utf8_lossy(&output.stdout).trim().to_string())
}
