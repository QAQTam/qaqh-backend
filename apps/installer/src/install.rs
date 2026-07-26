// 安装逻辑：文件复制（SFX / 目录模式）、快捷方式、注册表

use std::fs;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::path::{Path, PathBuf};
use std::process::Command;
use std::thread;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use deepx_update::{
    commit_bundle_state, parse_bundle_manifest, safe_join_under_root, write_install_root_marker,
    BundleFile, BundleManifest,
};
use sha2::{Digest, Sha256};

pub fn write_legal_acceptance(
    document_version: &str,
    user_agreement: &str,
    privacy_policy: &str,
) -> Result<(), String> {
    if document_version.trim().is_empty() {
        return Err("legal document version is empty".into());
    }

    let data_dir = deepx_types::platform::data_dir();
    fs::create_dir_all(&data_dir)
        .map_err(|error| format!("create legal acceptance directory: {error}"))?;
    let path = data_dir.join("legal-consent.json");
    let accepted_at_unix = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_err(|error| format!("read system time for legal acceptance: {error}"))?
        .as_secs();
    let record = legal_acceptance_record(
        document_version,
        accepted_at_unix,
        user_agreement,
        privacy_policy,
    );
    let encoded = serde_json::to_vec_pretty(&record)
        .map_err(|error| format!("serialize legal acceptance: {error}"))?;
    let mut file = fs::OpenOptions::new()
        .create(true)
        .truncate(true)
        .write(true)
        .open(&path)
        .map_err(|error| format!("open {}: {error}", path.display()))?;
    file.write_all(&encoded)
        .and_then(|_| file.sync_all())
        .map_err(|error| format!("write {}: {error}", path.display()))
}

fn legal_acceptance_record(
    document_version: &str,
    accepted_at_unix: u64,
    user_agreement: &str,
    privacy_policy: &str,
) -> serde_json::Value {
    serde_json::json!({
        "accepted": true,
        "document_version": document_version,
        "software_version": env!("CARGO_PKG_VERSION"),
        "accepted_at_unix": accepted_at_unix,
        "user_agreement_sha256": sha256_text(user_agreement),
        "privacy_policy_sha256": sha256_text(privacy_policy),
        "source": "installer",
    })
}

fn sha256_text(value: &str) -> String {
    format!("{:x}", Sha256::digest(value.as_bytes()))
}

// ============================================================
// InstallerConfig
// ============================================================

#[derive(Default, Clone)]
pub struct InstallerConfig {
    pub target_path: String,
    pub install_desktop_app: bool,
    pub create_start_menu: bool,
    pub create_desktop_shortcut: bool,
    pub progress: f32,
    pub current_file: String,
    pub total_files: usize,
    pub completed_files: usize,
    pub error: Option<String>,
    pub bundle_kind: String,
    pub bundle_build_id: String,
    pub operation: String,
}

impl InstallerConfig {
    pub fn default_path() -> String {
        dirs::data_local_dir()
            .or_else(|| {
                std::env::var_os("USERPROFILE")
                    .map(PathBuf::from)
                    .map(|path| path.join(r"AppData\Local"))
            })
            .unwrap_or_else(|| PathBuf::from("."))
            .join("Programs")
            .join("DeepX")
            .to_string_lossy()
            .into_owned()
    }
}

pub fn push_update(source: &str, target_dir: &str) -> Result<String, String> {
    let source =
        fs::canonicalize(source).map_err(|error| format!("更新源目录无效 '{source}': {error}"))?;
    let target = PathBuf::from(target_dir);
    let updater = target.join(if cfg!(windows) {
        "deepx-updater.exe"
    } else {
        "deepx-updater"
    });
    if !updater.is_file() {
        return Err(format!(
            "目标安装缺少 updater，请先进行完整安装或升级: {}",
            updater.display()
        ));
    }
    let output = Command::new(&updater)
        .arg("stage")
        .arg(&source)
        .arg(&target)
        .output()
        .map_err(|error| format!("启动 updater '{}' 失败: {error}", updater.display()))?;
    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr).trim().to_owned();
        return Err(if message.is_empty() {
            format!("updater 失败，退出码 {}", output.status)
        } else {
            message
        });
    }
    String::from_utf8(output.stdout).map_err(|error| format!("updater 输出不是 UTF-8: {error}"))
}

// ============================================================
// Manifest
// ============================================================

// ============================================================
// Headless bundle install: --patch <source_payload> <target_dir>
// ============================================================

pub fn run_patch(source: &str, target_dir: &str) -> Result<(), String> {
    let src = Path::new(source);
    let dst = Path::new(target_dir);

    if !src.exists() {
        return Err(format!("source not found: {}", source));
    }

    if src
        .extension()
        .map_or(false, |ext| ext.eq_ignore_ascii_case("zip"))
    {
        return run_patch_zip(src, dst);
    }

    if src.is_dir() {
        return run_patch_dir(src, dst);
    }

    Err(format!(
        "unsupported source (expected .zip or directory): {}",
        source
    ))
}

fn run_patch_dir(src: &Path, dst: &Path) -> Result<(), String> {
    let manifest = read_manifest_file(&src.join("bundle.json"))?;
    validate_bundle(&manifest, dst)?;
    for file in &manifest.files {
        let source_path = safe_join(src, &file.source)?;
        let target_path = safe_join(dst, &file.target)?;
        install_file_from_reader(
            fs::File::open(&source_path)
                .map_err(|e| format!("打开 '{}' 失败: {e}", source_path.display()))?,
            &target_path,
            file,
            manifest.kind != "full",
        )?;
    }
    write_install_state(dst, &manifest)?;
    println!(
        "{} bundle {} installed: {} files in {}",
        manifest.kind,
        manifest.build_id,
        manifest.files.len(),
        dst.display()
    );
    Ok(())
}

fn run_patch_zip(zip_path: &Path, dst: &Path) -> Result<(), String> {
    let file = fs::File::open(zip_path)
        .map_err(|e| format!("打开 ZIP '{}' 失败: {e}", zip_path.display()))?;
    let mut archive = zip::ZipArchive::new(file)
        .map_err(|e| format!("读取 ZIP '{}' 失败: {e}", zip_path.display()))?;
    install_zip_archive(&mut archive, dst, |_, _, _| {}).map(|_| ())
}

fn read_manifest_file(path: &Path) -> Result<BundleManifest, String> {
    let bytes = fs::read(path)
        .map_err(|e| format!("读取 Bundle manifest '{}' 失败: {e}", path.display()))?;
    parse_manifest(&bytes)
}

fn parse_manifest(bytes: &[u8]) -> Result<BundleManifest, String> {
    parse_bundle_manifest(bytes).map_err(|error| format!("解析 bundle.json 失败: {error}"))
}

fn validate_bundle(manifest: &BundleManifest, target: &Path) -> Result<(), String> {
    if manifest.requires_full_install && !target.join("DeepX.exe").is_file() {
        return Err(format!(
            "{} 是组件更新包，但目标目录不是完整的 DeepX 安装目录: {}",
            manifest.kind,
            target.display()
        ));
    }
    Ok(())
}

fn safe_join(root: &Path, relative: &str) -> Result<PathBuf, String> {
    safe_join_under_root(root, relative)
        .map_err(|error| format!("Bundle 包含非法路径 '{relative}': {error}"))
}

pub fn run_install<F>(config: &mut InstallerConfig, on_progress: F) -> Result<(), String>
where
    F: Fn(&InstallerConfig),
{
    if let Ok(zip_offset) = find_zip_in_exe() {
        return run_install_sfx(config, on_progress, zip_offset);
    }
    run_install_from_dir(config, on_progress)
}

fn run_install_from_dir<F>(config: &mut InstallerConfig, on_progress: F) -> Result<(), String>
where
    F: Fn(&InstallerConfig),
{
    let exe_dir = current_exe_dir()?;
    let payload_dir = exe_dir.join("payload");
    if !payload_dir.exists() {
        return Err("未找到安装数据：EXE 内无嵌入包，且 payload/ 目录不存在。".into());
    }

    let manifest = read_manifest_file(&payload_dir.join("bundle.json"))?;
    let target_path = config.target_path.clone();
    let target = Path::new(&target_path);
    validate_bundle(&manifest, target)?;
    initialize_progress(config, &manifest, target);

    for file in &manifest.files {
        let source_path = safe_join(&payload_dir, &file.source)?;
        let target_path = safe_join(target, &file.target)?;
        install_file_from_reader(
            fs::File::open(&source_path)
                .map_err(|e| format!("打开 '{}' 失败: {e}", source_path.display()))?,
            &target_path,
            file,
            manifest.kind != "full",
        )?;
        advance_progress(config, &file.target, &on_progress);
    }

    write_install_state(target, &manifest)?;
    finish_progress(config, &on_progress);
    Ok(())
}

fn initialize_progress(config: &mut InstallerConfig, manifest: &BundleManifest, target: &Path) {
    config.bundle_kind = manifest.kind.clone();
    config.bundle_build_id = manifest.build_id.clone();
    config.operation = if manifest.kind == "full" {
        if target.join("DeepX.exe").is_file() {
            "upgrade"
        } else {
            "install"
        }
    } else {
        "update"
    }
    .to_string();
    config.total_files = manifest.files.len().max(1);
    config.completed_files = 0;
    config.progress = 0.0;
    config.error = None;
}

fn advance_progress<F>(config: &mut InstallerConfig, target: &str, on_progress: &F)
where
    F: Fn(&InstallerConfig),
{
    config.current_file = target.to_string();
    config.completed_files += 1;
    config.progress = config.completed_files as f32 / config.total_files as f32;
    on_progress(config);
}

fn finish_progress<F>(config: &mut InstallerConfig, on_progress: &F)
where
    F: Fn(&InstallerConfig),
{
    config.progress = 1.0;
    on_progress(config);
}

fn current_exe_dir() -> Result<PathBuf, String> {
    std::env::current_exe()
        .map_err(|e| format!("无法获取安装器路径: {}", e))?
        .parent()
        .map(|p| p.to_path_buf())
        .ok_or_else(|| "无法获取安装器目录".into())
}

// ============================================================
// SFX 自解压引擎 — EXE 尾部带 ZIP
// ============================================================

/// ZIP 偏移读取器：对上层透明，让 ZIP 偏移看起来像是从 0 开始的独立文件。
struct OffsetReader<R: Read + Seek> {
    inner: R,
    offset: u64,
}

impl<R: Read + Seek> Read for OffsetReader<R> {
    fn read(&mut self, buf: &mut [u8]) -> io::Result<usize> {
        self.inner.read(buf)
    }
}

impl<R: Read + Seek> Seek for OffsetReader<R> {
    fn seek(&mut self, pos: SeekFrom) -> io::Result<u64> {
        let real = match pos {
            SeekFrom::Start(p) => SeekFrom::Start(self.offset + p),
            SeekFrom::End(p) => SeekFrom::End(p), // 尾部 EOCD 直接用文件尾
            SeekFrom::Current(p) => SeekFrom::Current(p),
        };
        let abs = self.inner.seek(real)?;
        Ok(abs.saturating_sub(self.offset))
    }

    fn stream_position(&mut self) -> io::Result<u64> {
        self.inner
            .stream_position()
            .map(|p| p.saturating_sub(self.offset))
    }
}

/// 在 EXE 尾部扫描 ZIP 的 EOCD 签名，返回 ZIP 起始偏移（失败则说明非 SFX）
fn find_zip_in_exe() -> Result<u64, String> {
    let exe_path = std::env::current_exe().map_err(|e| format!("无法获取自身路径: {}", e))?;
    let mut f = fs::File::open(&exe_path).map_err(|e| format!("打开自身失败: {}", e))?;
    let file_len = f
        .seek(SeekFrom::End(0))
        .map_err(|e| format!("seek 失败: {}", e))?;

    // ZIP EOCD 最小 22 字节，最大 22 + 65535（注释）
    let scan = 65536u64.min(file_len);
    let mut buf = vec![0u8; scan as usize];
    f.seek(SeekFrom::End(-(scan as i64)))
        .map_err(|e| format!("seek 失败: {}", e))?;
    f.read_exact(&mut buf)
        .map_err(|e| format!("读取尾部失败: {}", e))?;

    // 从后往前扫 EOCD 签名 PK\x05\x06
    let sig: [u8; 4] = [0x50, 0x4B, 0x05, 0x06];
    let pos = buf
        .windows(4)
        .rposition(|w| w == sig)
        .ok_or_else(|| "末尾未找到 ZIP 签名".to_string())?;

    // 解析 EOCD 获取 central directory 偏移和大小
    let eocd_file_pos = file_len - scan + pos as u64;
    let cd_size =
        u32::from_le_bytes([buf[pos + 12], buf[pos + 13], buf[pos + 14], buf[pos + 15]]) as u64;
    let cd_offset =
        u32::from_le_bytes([buf[pos + 16], buf[pos + 17], buf[pos + 18], buf[pos + 19]]) as u64;

    // ZIP 起始 = EOCD 文件位置 - central_dir_size - central_dir_offset
    let zip_start = eocd_file_pos
        .checked_sub(cd_size)
        .and_then(|v| v.checked_sub(cd_offset))
        .ok_or("ZIP 偏移计算溢出")?;

    // 验证：读取 central directory 签名 PK\x01\x02
    f.seek(SeekFrom::Start(zip_start + cd_offset))
        .map_err(|e| format!("seek CD 失败: {}", e))?;
    let mut cd_sig = [0u8; 4];
    f.read_exact(&mut cd_sig)
        .map_err(|e| format!("读取 CD 签名失败: {}", e))?;
    if cd_sig != [0x50, 0x4B, 0x01, 0x02] {
        return Err("ZIP central directory 签名验证失败".into());
    }

    Ok(zip_start)
}

fn run_install_sfx<F>(
    config: &mut InstallerConfig,
    on_progress: F,
    zip_offset: u64,
) -> Result<(), String>
where
    F: Fn(&InstallerConfig),
{
    let exe_path = std::env::current_exe().map_err(|e| format!("无法获取自身路径: {}", e))?;
    let file = fs::File::open(&exe_path).map_err(|e| format!("打开自身失败: {}", e))?;
    let reader = OffsetReader {
        inner: file,
        offset: zip_offset,
    };
    let mut archive =
        zip::ZipArchive::new(reader).map_err(|e| format!("读取内嵌 ZIP 失败: {}", e))?;
    let target_path = config.target_path.clone();
    let target = Path::new(&target_path);
    let manifest = read_manifest_from_archive(&mut archive)?;
    validate_bundle(&manifest, target)?;
    initialize_progress(config, &manifest, target);
    install_archive_files(&mut archive, target, &manifest, |target_name, _, _| {
        advance_progress(config, target_name, &on_progress);
    })?;
    write_install_state(target, &manifest)?;
    finish_progress(config, &on_progress);
    Ok(())
}

fn install_zip_archive<R, F>(
    archive: &mut zip::ZipArchive<R>,
    target: &Path,
    on_file: F,
) -> Result<BundleManifest, String>
where
    R: Read + Seek,
    F: FnMut(&str, usize, usize),
{
    let manifest = read_manifest_from_archive(archive)?;
    validate_bundle(&manifest, target)?;
    install_archive_files(archive, target, &manifest, on_file)?;
    write_install_state(target, &manifest)?;
    Ok(manifest)
}

fn read_manifest_from_archive<R: Read + Seek>(
    archive: &mut zip::ZipArchive<R>,
) -> Result<BundleManifest, String> {
    let mut manifest_file = archive
        .by_name("bundle.json")
        .map_err(|e| format!("内嵌包缺少 bundle.json: {e}"))?;
    let mut bytes = Vec::new();
    manifest_file
        .read_to_end(&mut bytes)
        .map_err(|e| format!("读取 bundle.json 失败: {e}"))?;
    parse_manifest(&bytes)
}

fn install_archive_files<R, F>(
    archive: &mut zip::ZipArchive<R>,
    target: &Path,
    manifest: &BundleManifest,
    mut on_file: F,
) -> Result<(), String>
where
    R: Read + Seek,
    F: FnMut(&str, usize, usize),
{
    for (index, file) in manifest.files.iter().enumerate() {
        let mut zip_file = archive
            .by_name(&file.source)
            .map_err(|e| format!("Bundle 缺少清单文件 '{}': {e}", file.source))?;
        if zip_file.is_dir() {
            return Err(format!("清单文件实际是目录: {}", file.source));
        }
        let target_path = safe_join(target, &file.target)?;
        install_file_from_reader(&mut zip_file, &target_path, file, manifest.kind != "full")?;
        on_file(&file.target, index + 1, manifest.files.len());
    }
    Ok(())
}

fn install_file_from_reader<R: Read>(
    mut source: R,
    target: &Path,
    expected: &BundleFile,
    preserve_previous: bool,
) -> Result<(), String> {
    let parent = target
        .parent()
        .ok_or_else(|| format!("目标路径没有父目录: {}", target.display()))?;
    fs::create_dir_all(parent).map_err(|e| format!("创建目录 '{}' 失败: {e}", parent.display()))?;

    let file_name = target
        .file_name()
        .and_then(|v| v.to_str())
        .ok_or_else(|| format!("目标文件名无效: {}", target.display()))?;
    let temp = parent.join(format!(".{file_name}.deepx-new-{}", std::process::id()));
    if temp.exists() {
        retry_io(|| fs::remove_file(&temp))
            .map_err(|e| format!("清理临时文件 '{}' 失败: {e}", temp.display()))?;
    }

    let mut output = fs::File::create(&temp)
        .map_err(|e| format!("创建临时文件 '{}' 失败: {e}", temp.display()))?;
    let mut hasher = Sha256::new();
    let mut copied = 0u64;
    let mut buffer = [0u8; 128 * 1024];
    loop {
        let read = source
            .read(&mut buffer)
            .map_err(|e| format!("读取 '{}' 失败: {e}", expected.source))?;
        if read == 0 {
            break;
        }
        output
            .write_all(&buffer[..read])
            .map_err(|e| format!("写入临时文件 '{}' 失败: {e}", temp.display()))?;
        hasher.update(&buffer[..read]);
        copied += read as u64;
    }
    output
        .sync_all()
        .map_err(|e| format!("同步临时文件 '{}' 失败: {e}", temp.display()))?;
    drop(output);

    let actual_hash = format!("{:x}", hasher.finalize());
    if copied != expected.size || !actual_hash.eq_ignore_ascii_case(&expected.sha256) {
        let _ = fs::remove_file(&temp);
        return Err(format!(
            "文件校验失败: {}（大小 {copied}/{}, SHA-256 {actual_hash}/{}）",
            expected.target, expected.size, expected.sha256
        ));
    }

    let backup = parent.join(format!("{file_name}.previous"));
    if target.exists() {
        clear_readonly(target);
        if preserve_previous {
            if backup.exists() {
                clear_readonly(&backup);
                retry_io(|| fs::remove_file(&backup))
                    .map_err(|e| format!("删除旧备份 '{}' 失败: {e}", backup.display()))?;
            }
            retry_io(|| fs::rename(target, &backup))
                .map_err(|e| format!("备份 '{}' 失败: {e}", target.display()))?;
        } else {
            retry_io(|| fs::remove_file(target))
                .map_err(|e| format!("替换前删除 '{}' 失败: {e}", target.display()))?;
        }
    }

    if let Err(error) = retry_io(|| fs::rename(&temp, target)) {
        if preserve_previous && backup.exists() && !target.exists() {
            let _ = retry_io(|| fs::rename(&backup, target));
        }
        let _ = fs::remove_file(&temp);
        return Err(format!("启用新文件 '{}' 失败: {error}", target.display()));
    }
    Ok(())
}

fn clear_readonly(path: &Path) {
    if let Ok(metadata) = fs::metadata(path) {
        let mut permissions = metadata.permissions();
        if permissions.readonly() {
            permissions.set_readonly(false);
            let _ = fs::set_permissions(path, permissions);
        }
    }
}

fn retry_io<T>(mut operation: impl FnMut() -> io::Result<T>) -> io::Result<T> {
    let mut last_error = None;
    for attempt in 0..20 {
        match operation() {
            Ok(value) => return Ok(value),
            Err(error) => {
                last_error = Some(error);
                if attempt < 19 {
                    thread::sleep(Duration::from_millis(150));
                }
            }
        }
    }
    Err(last_error.expect("retry loop always records an error"))
}

fn write_install_state(target: &Path, manifest: &BundleManifest) -> Result<(), String> {
    commit_bundle_state(
        target,
        manifest,
        &format!("installer-{}", manifest.build_id),
    )
    .map(|_| ())
    .map_err(|error| {
        format!(
            "写入安装状态 '{}' 失败: {error}",
            target.join("install-state.json").display()
        )
    })
}

// ============================================================
// Windows 快捷方式
// ============================================================

#[cfg(windows)]
pub fn create_desktop_shortcut(target_exe: &str, description: &str) -> Result<(), String> {
    let desktop = dirs::desktop_dir().ok_or("无法获取桌面路径")?;
    let lnk_path = desktop.join("DeepX.lnk");
    create_shortcut(target_exe, lnk_path.to_str().unwrap_or(""), description)
}

#[cfg(windows)]
pub fn create_start_menu_shortcut(target_exe: &str, description: &str) -> Result<(), String> {
    let start_menu = dirs::data_dir()
        .map(|p| p.join(r"Microsoft\Windows\Start Menu\Programs\DeepX"))
        .ok_or("无法获取开始菜单路径")?;

    fs::create_dir_all(&start_menu).map_err(|e| format!("创建开始菜单目录失败: {}", e))?;

    create_shortcut(
        target_exe,
        start_menu.join("DeepX.lnk").to_str().unwrap_or(""),
        description,
    )
}

#[cfg(windows)]
fn create_shortcut(target_exe: &str, lnk_path_str: &str, description: &str) -> Result<(), String> {
    use windows::core::Interface;
    use windows::Win32::System::Com::IPersistFile;
    use windows::Win32::System::Com::{
        CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED,
    };
    use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};

    unsafe {
        CoInitializeEx(None, COINIT_APARTMENTTHREADED)
            .ok()
            .map_err(|e| format!("COM 初始化失败: {:?}", e))?;
    }

    let result = unsafe {
        let shell_link: IShellLinkW = CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)
            .map_err(|e| format!("创建 ShellLink 失败: {:?}", e))?;

        let target_wide: Vec<u16> = target_exe
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        shell_link
            .SetPath(windows::core::PCWSTR::from_raw(target_wide.as_ptr()))
            .map_err(|e| format!("SetPath 失败: {:?}", e))?;

        let desc_wide: Vec<u16> = description
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();
        shell_link
            .SetDescription(windows::core::PCWSTR::from_raw(desc_wide.as_ptr()))
            .map_err(|e| format!("SetDescription 失败: {:?}", e))?;

        let persist: IPersistFile = shell_link
            .cast()
            .map_err(|e| format!("cast 失败: {:?}", e))?;

        let path_wide: Vec<u16> = lnk_path_str
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect();

        persist
            .Save(windows::core::PCWSTR::from_raw(path_wide.as_ptr()), true)
            .map_err(|e| format!("保存快捷方式失败: {:?}", e))?;

        Ok::<_, String>(())
    };

    unsafe {
        windows::Win32::System::Com::CoUninitialize();
    }

    result
}

#[cfg(not(windows))]
pub fn create_desktop_shortcut(_: &str, _: &str) -> Result<(), String> {
    Ok(())
}
#[cfg(not(windows))]
pub fn create_start_menu_shortcut(_: &str, _: &str) -> Result<(), String> {
    Ok(())
}

// ============================================================
// 注册表 — 卸载信息
// ============================================================

pub fn write_install_marker(install_path: &str) -> Result<(), String> {
    write_install_root_marker(Path::new(install_path))
        .map_err(|error| format!("写入安装根标记失败: {error}"))
}

#[cfg(windows)]
pub fn write_uninstall_registry(install_path: &str, version: &str) -> Result<(), String> {
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
    use windows::Win32::System::Registry::{
        RegCloseKey, RegCreateKeyExW, RegDeleteValueW, RegSetValueExW, HKEY_CURRENT_USER,
        KEY_CREATE_SUB_KEY, KEY_SET_VALUE, REG_CREATE_KEY_DISPOSITION, REG_DWORD,
        REG_OPTION_NON_VOLATILE, REG_SZ,
    };

    let subkey: Vec<u16> = "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\DeepX"
        .encode_utf16()
        .chain(std::iter::once(0))
        .collect();

    unsafe {
        let mut hkey = std::mem::zeroed();
        let mut disposition: REG_CREATE_KEY_DISPOSITION = std::mem::zeroed();

        RegCreateKeyExW(
            HKEY_CURRENT_USER,
            PCWSTR::from_raw(subkey.as_ptr()),
            0,
            PCWSTR::null(),
            REG_OPTION_NON_VOLATILE,
            KEY_SET_VALUE | KEY_CREATE_SUB_KEY,
            None,
            &mut hkey,
            Some(&mut disposition),
        )
        .ok()
        .map_err(|e| format!("创建注册表键失败: {:?}", e))?;

        let set_value = |name: &str, value: &str| -> Result<(), String> {
            let name_wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
            let value_wide: Vec<u16> = value.encode_utf16().chain(std::iter::once(0)).collect();
            let data =
                std::slice::from_raw_parts(value_wide.as_ptr() as *const u8, value_wide.len() * 2);

            RegSetValueExW(
                hkey,
                PCWSTR::from_raw(name_wide.as_ptr()),
                0,
                REG_SZ,
                Some(data),
            )
            .ok()
            .map_err(|e| format!("设置注册表值失败: {:?}", e))?;

            Ok(())
        };
        let set_dword = |name: &str, value: u32| -> Result<(), String> {
            let name_wide: Vec<u16> = name.encode_utf16().chain(std::iter::once(0)).collect();
            RegSetValueExW(
                hkey,
                PCWSTR::from_raw(name_wide.as_ptr()),
                0,
                REG_DWORD,
                Some(&value.to_le_bytes()),
            )
            .ok()
            .map_err(|e| format!("设置注册表值失败: {:?}", e))
        };

        set_value("DisplayName", "DeepX")?;
        set_value("Publisher", "DeepX Team")?;
        set_value("InstallLocation", install_path)?;
        set_value("DisplayVersion", version)?;
        set_value(
            "UninstallString",
            &format!(
                "\"{}\\deepx-updater.exe\" uninstall --interactive --install-dir \"{}\"",
                install_path, install_path
            ),
        )?;
        set_value(
            "QuietUninstallString",
            &format!(
                "\"{}\\deepx-updater.exe\" uninstall --quiet --install-dir \"{}\"",
                install_path, install_path
            ),
        )?;
        set_value(
            "ModifyPath",
            &format!(
                "\"{}\\deepx-updater.exe\" maintain --interactive --install-dir \"{}\"",
                install_path, install_path
            ),
        )?;
        set_value("DisplayIcon", &format!("{}\\DeepX.exe", install_path))?;
        let no_modify = "NoModify"
            .encode_utf16()
            .chain(std::iter::once(0))
            .collect::<Vec<_>>();
        let delete_result = RegDeleteValueW(hkey, PCWSTR::from_raw(no_modify.as_ptr()));
        if delete_result != ERROR_SUCCESS && delete_result != ERROR_FILE_NOT_FOUND {
            let _ = RegCloseKey(hkey);
            return Err(format!("清除 NoModify 注册表值失败: {delete_result:?}"));
        }
        set_dword("NoRepair", 1)?;

        let _ = RegCloseKey(hkey);
    }

    Ok(())
}

#[cfg(not(windows))]
pub fn write_uninstall_registry(_: &str, _: &str) -> Result<(), String> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::legal_acceptance_record;

    #[test]
    fn legal_acceptance_is_bound_to_version_and_both_documents() {
        let record = legal_acceptance_record("2026-07-27.1", 42, "agreement", "privacy");

        assert_eq!(record["accepted"], true);
        assert_eq!(record["document_version"], "2026-07-27.1");
        assert_eq!(record["accepted_at_unix"], 42);
        assert_eq!(record["source"], "installer");
        assert_eq!(
            record["user_agreement_sha256"]
                .as_str()
                .expect("agreement digest")
                .len(),
            64
        );
        assert_ne!(
            record["user_agreement_sha256"],
            record["privacy_policy_sha256"]
        );
    }

    #[test]
    fn bundled_legal_documents_declare_the_current_document_version() {
        let version = include_str!("../../../docs/legal/version.txt").trim();
        let agreement = include_str!("../../../docs/legal/USER_AGREEMENT.zh-CN.md");
        let privacy = include_str!("../../../docs/legal/PRIVACY_POLICY.zh-CN.md");

        assert!(agreement.contains(&format!("文档版本：{version}")));
        assert!(privacy.contains(&format!("文档版本：{version}")));
    }
}
