//! Stages the Windows App SDK runtime and DeepX content assets next to the
//! built executable.
//!
//! Self-contained mode downloads `Microsoft.WindowsAppSDK.Runtime` +
//! `Microsoft.Web.WebView2` (Core.dll) from NuGet on first build, so the app
//! runs without a system-installed Windows App SDK runtime.

use std::path::{Path, PathBuf};

fn copy_dir(source: &Path, destination: &Path) {
    std::fs::create_dir_all(destination).expect("create staged asset directory");
    for entry in std::fs::read_dir(source).expect("read asset directory") {
        let entry = entry.expect("read asset entry");
        let source_path = entry.path();
        let destination_path = destination.join(entry.file_name());
        if source_path.is_dir() {
            copy_dir(&source_path, &destination_path);
        } else {
            std::fs::copy(&source_path, &destination_path).expect("stage app asset");
        }
    }
}

fn stage_app_assets() {
    let manifest_dir = PathBuf::from(std::env::var_os("CARGO_MANIFEST_DIR").unwrap());
    let source = manifest_dir.join("assets");
    println!("cargo:rerun-if-changed={}", source.display());

    // OUT_DIR = target/<profile>/build/<package-hash>/out. The executable
    // resolves ms-appx:/// content relative to target/<profile> in unpackaged
    // development runs, so keep the same Assets layout used by the release
    // assembler.
    let out_dir = PathBuf::from(std::env::var_os("OUT_DIR").unwrap());
    let profile_dir = out_dir
        .ancestors()
        .nth(3)
        .expect("resolve Cargo profile directory");
    copy_dir(&source, &profile_dir.join("Assets"));
}

fn main() {
    windows_reactor_setup::as_self_contained();
    stage_app_assets();
}
