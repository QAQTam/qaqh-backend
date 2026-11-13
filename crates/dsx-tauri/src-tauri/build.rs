fn main() {
  // Copy pre-built dsx binary to resources/ so tauri_build can validate & bundle it.
  // dsx is built in beforeDevCommand / beforeBuildCommand (to avoid nested cargo deadlocks).
  let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap();
  let profile = std::env::var("PROFILE").unwrap_or_default();
  let m = std::path::Path::new(&manifest);
  let ws_root = m.parent().unwrap().parent().unwrap().parent().unwrap();
  let src = ws_root.join("target").join(&profile).join("dsx.exe");
  if src.exists() {
    let dest = m.join("resources").join("dsx.exe");
    let _ = std::fs::create_dir_all(dest.parent().unwrap());
    let _ = std::fs::copy(&src, &dest);
  }

  tauri_build::build()
}
