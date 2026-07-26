// 安装逻辑：文件复制（SFX / 目录模式）、快捷方式、注册表

use std::fs;
use std::io::{self, Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};

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
}

impl InstallerConfig {
    pub fn default_path() -> String {
        let local_app_data = std::env::var("LOCALAPPDATA")
            .unwrap_or_else(|_| r"C:\Users\Default\AppData\Local".to_string());
        format!(r"{}\Programs\DeepX", local_app_data)
    }
}

// ============================================================
// Manifest
// ============================================================

#[derive(Clone)]
pub struct ManifestEntry {
    pub source: &'static str,
    pub dest: &'static str,
    pub is_dir: bool,
}

pub fn get_manifest() -> Vec<ManifestEntry> {
    vec![
        ManifestEntry { source: "desktop", dest: "", is_dir: true },
        ManifestEntry { source: "config/default.toml", dest: "config/config.toml", is_dir: false },
        ManifestEntry { source: "deepx-uninstaller.exe", dest: "uninstall.exe", is_dir: false },
    ]
}

// ============================================================
// 文件复制引擎（目录 payload 模式）
// ============================================================

pub fn run_install<F>(config: &mut InstallerConfig, on_progress: F) -> Result<(), String>
where
    F: Fn(&InstallerConfig),
{
    // 优先尝试 SFX 自解压模式（EXE 尾部带 ZIP）
    if let Ok(zip_offset) = find_zip_in_exe() {
        return run_install_sfx(config, on_progress, zip_offset);
    }

    // 回退到 payload/ 目录模式
    run_install_from_dir(config, on_progress)
}

fn run_install_from_dir<F>(config: &mut InstallerConfig, on_progress: F) -> Result<(), String>
where
    F: Fn(&InstallerConfig),
{
    let manifest = get_manifest();
    let entries: Vec<&ManifestEntry> = filter_entries(&manifest, config);

    let exe_dir = current_exe_dir()?;
    let payload_dir = exe_dir.join("payload");

    if !payload_dir.exists() {
        return Err("未找到安装数据：EXE 内无嵌入包，且 payload/ 目录不存在。".into());
    }

    config.total_files = count_files_in_dir_entries(&payload_dir, &entries);
    config.completed_files = 0;
    config.progress = 0.0;
    config.error = None;

    let target_path = config.target_path.clone();
    let target = Path::new(&target_path);

    for entry in &entries {
        let src = payload_dir.join(entry.source);
        let dst = target.join(entry.dest);

        if entry.is_dir && src.is_dir() {
            copy_dir_recursive(&src, &dst, config, &on_progress)?;
        } else if src.exists() {
            config.current_file = entry.dest.to_string();
            config.completed_files += 1;
            config.progress = config.completed_files as f32 / config.total_files as f32;
            on_progress(config);

            if let Some(parent) = dst.parent() {
                fs::create_dir_all(parent)
                    .map_err(|e| format!("创建目录失败 '{}': {}", parent.display(), e))?;
            }
            fs::copy(&src, &dst).map_err(|e| {
                format!("复制失败\n  源: {}\n  目标: {}\n  错误: {}", src.display(), dst.display(), e)
            })?;
        } else {
            eprintln!("警告: 源不存在 '{}'，跳过", src.display());
        }
    }

    config.progress = 1.0;
    on_progress(config);
    Ok(())
}

fn filter_entries<'a>(manifest: &'a [ManifestEntry], config: &InstallerConfig) -> Vec<&'a ManifestEntry> {
    manifest
        .iter()
        .filter(|e| !e.source.starts_with("desktop") || config.install_desktop_app)
        .collect()
}

fn current_exe_dir() -> Result<PathBuf, String> {
    std::env::current_exe()
        .map_err(|e| format!("无法获取安装器路径: {}", e))?
        .parent()
        .map(|p| p.to_path_buf())
        .ok_or_else(|| "无法获取安装器目录".into())
}

fn count_files_in_dir_entries(payload_dir: &Path, entries: &[&ManifestEntry]) -> usize {
    let mut total = 0;
    for entry in entries {
        let src = payload_dir.join(entry.source);
        if entry.is_dir && src.is_dir() {
            total += count_files(&src);
        } else if src.exists() {
            total += 1;
        }
    }
    total.max(1) // 避免除零
}

fn count_files(dir: &Path) -> usize {
    let mut n = 0;
    if let Ok(iter) = fs::read_dir(dir) {
        for e in iter.flatten() {
            let p = e.path();
            if p.is_dir() { n += count_files(&p); } else { n += 1; }
        }
    }
    n
}

fn copy_dir_recursive<F>(src: &Path, dst: &Path, config: &mut InstallerConfig, on_progress: &F) -> Result<(), String>
where
    F: Fn(&InstallerConfig),
{
    fs::create_dir_all(dst)
        .map_err(|e| format!("创建目录失败 '{}': {}", dst.display(), e))?;

    for entry in fs::read_dir(src).map_err(|e| format!("读取目录失败 '{}': {}", src.display(), e))? {
        let entry = entry.map_err(|e| format!("读取目录项失败: {}", e))?;
        let sp = entry.path();
        let dp = dst.join(entry.file_name());
        if sp.is_dir() {
            copy_dir_recursive(&sp, &dp, config, on_progress)?;
        } else {
            config.current_file = sp.file_name().unwrap_or_default().to_string_lossy().to_string();
            config.completed_files += 1;
            config.progress = config.completed_files as f32 / config.total_files as f32;
            on_progress(config);
            fs::copy(&sp, &dp).map_err(|e| {
                format!("复制失败\n  源: {}\n  目标: {}\n  错误: {}", sp.display(), dp.display(), e)
            })?;
        }
    }
    Ok(())
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
            SeekFrom::End(p) => SeekFrom::End(p),   // 尾部 EOCD 直接用文件尾
            SeekFrom::Current(p) => SeekFrom::Current(p),
        };
        let abs = self.inner.seek(real)?;
        Ok(abs.saturating_sub(self.offset))
    }

    fn stream_position(&mut self) -> io::Result<u64> {
        self.inner.stream_position().map(|p| p.saturating_sub(self.offset))
    }
}

/// 在 EXE 尾部扫描 ZIP 的 EOCD 签名，返回 ZIP 起始偏移（失败则说明非 SFX）
fn find_zip_in_exe() -> Result<u64, String> {
    let exe_path = std::env::current_exe().map_err(|e| format!("无法获取自身路径: {}", e))?;
    let mut f = fs::File::open(&exe_path).map_err(|e| format!("打开自身失败: {}", e))?;
    let file_len = f.seek(SeekFrom::End(0)).map_err(|e| format!("seek 失败: {}", e))?;

    // ZIP EOCD 最小 22 字节，最大 22 + 65535（注释）
    let scan = 65536u64.min(file_len);
    let mut buf = vec![0u8; scan as usize];
    f.seek(SeekFrom::End(-(scan as i64)))
        .map_err(|e| format!("seek 失败: {}", e))?;
    f.read_exact(&mut buf).map_err(|e| format!("读取尾部失败: {}", e))?;

    // 从后往前扫 EOCD 签名 PK\x05\x06
    let sig: [u8; 4] = [0x50, 0x4B, 0x05, 0x06];
    let pos = buf
        .windows(4)
        .rposition(|w| w == sig)
        .ok_or_else(|| "末尾未找到 ZIP 签名".to_string())?;

    // 解析 EOCD 获取 central directory 偏移和大小
    let eocd_file_pos = file_len - scan + pos as u64;
    let cd_size = u32::from_le_bytes([buf[pos + 12], buf[pos + 13], buf[pos + 14], buf[pos + 15]]) as u64;
    let cd_offset = u32::from_le_bytes([buf[pos + 16], buf[pos + 17], buf[pos + 18], buf[pos + 19]]) as u64;

    // ZIP 起始 = EOCD 文件位置 - central_dir_size - central_dir_offset
    let zip_start = eocd_file_pos
        .checked_sub(cd_size)
        .and_then(|v| v.checked_sub(cd_offset))
        .ok_or("ZIP 偏移计算溢出")?;

    // 验证：读取 central directory 签名 PK\x01\x02
    f.seek(SeekFrom::Start(zip_start + cd_offset))
        .map_err(|e| format!("seek CD 失败: {}", e))?;
    let mut cd_sig = [0u8; 4];
    f.read_exact(&mut cd_sig).map_err(|e| format!("读取 CD 签名失败: {}", e))?;
    if cd_sig != [0x50, 0x4B, 0x01, 0x02] {
        return Err("ZIP central directory 签名验证失败".into());
    }

    Ok(zip_start)
}

fn run_install_sfx<F>(config: &mut InstallerConfig, on_progress: F, zip_offset: u64) -> Result<(), String>
where
    F: Fn(&InstallerConfig),
{
    let manifest = get_manifest();
    let entries: Vec<&ManifestEntry> = filter_entries(&manifest, config);

    let exe_path = std::env::current_exe().map_err(|e| format!("无法获取自身路径: {}", e))?;
    let file = fs::File::open(&exe_path).map_err(|e| format!("打开自身失败: {}", e))?;
    let reader = OffsetReader { inner: file, offset: zip_offset };

    let mut archive = zip::ZipArchive::new(reader)
        .map_err(|e| format!("读取内嵌 ZIP 失败: {}", e))?;

    // 统计匹配的文件数
    let matching: Vec<_> = (0..archive.len())
        .filter_map(|i| {
            let zf = archive.by_index(i).ok()?;
            let name = zf.name().to_string();
            if name.ends_with('/') { return None; }
            let normalized = name.replace('\\', "/");
            manifest_match(&entries, &normalized).map(|dest| (i, dest))
        })
        .collect();

    config.total_files = matching.len().max(1);
    config.completed_files = 0;
    config.progress = 0.0;
    config.error = None;

    let target = Path::new(&config.target_path);

    for (idx, (i, dest)) in matching.iter().enumerate() {
        let mut zip_file = archive.by_index(*i)
            .map_err(|e| format!("读取 ZIP 条目 {} 失败: {}", *i, e))?;

        config.current_file = dest.clone();
        config.completed_files = idx + 1;
        config.progress = config.completed_files as f32 / config.total_files as f32;
        on_progress(config);

        let dest_path = target.join(dest);
        if let Some(parent) = dest_path.parent() {
            fs::create_dir_all(parent)
                .map_err(|e| format!("创建目录失败 '{}': {}", parent.display(), e))?;
        }

        let mut out = fs::File::create(&dest_path)
            .map_err(|e| format!("创建文件失败 '{}': {}", dest_path.display(), e))?;

        io::copy(&mut zip_file, &mut out)
            .map_err(|e| format!("解压失败 '{}': {}", dest, e))?;
    }

    config.progress = 1.0;
    on_progress(config);
    Ok(())
}

/// 将 ZIP 条目路径匹配到 manifest，返回目标相对路径。不匹配则 None。
fn manifest_match(entries: &[&ManifestEntry], zip_path: &str) -> Option<String> {
    for e in entries {
        if e.is_dir {
            // 目录条目：匹配以 source/ 开头的所有文件
            if zip_path.starts_with(e.source) && zip_path.len() > e.source.len() + 1 {
                let rest = &zip_path[e.source.len() + 1..]; // 去掉 "desktop/"
                if e.dest.is_empty() {
                    return Some(rest.to_string());
                } else {
                    return Some(format!("{}/{}", e.dest, rest));
                }
            }
        } else {
            // 单文件条目：精确匹配 source 路径
            if zip_path == e.source || zip_path == e.source.replace('\\', "/") {
                return Some(e.dest.to_string());
            }
        }
    }
    None
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

    fs::create_dir_all(&start_menu)
        .map_err(|e| format!("创建开始菜单目录失败: {}", e))?;

    create_shortcut(
        target_exe,
        start_menu.join("DeepX.lnk").to_str().unwrap_or(""),
        description,
    )
}

#[cfg(windows)]
fn create_shortcut(target_exe: &str, lnk_path_str: &str, description: &str) -> Result<(), String> {
    use windows::Win32::System::Com::{CoCreateInstance, CoInitializeEx, CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED};
    use windows::Win32::UI::Shell::{IShellLinkW, ShellLink};
    use windows::Win32::System::Com::IPersistFile;
    use windows::core::Interface;

    unsafe {
        CoInitializeEx(None, COINIT_APARTMENTTHREADED)
            .ok()
            .map_err(|e| format!("COM 初始化失败: {:?}", e))?;
    }

    let result = unsafe {
        let shell_link: IShellLinkW =
            CoCreateInstance(&ShellLink, None, CLSCTX_INPROC_SERVER)
                .map_err(|e| format!("创建 ShellLink 失败: {:?}", e))?;

        let target_wide: Vec<u16> = target_exe.encode_utf16().chain(std::iter::once(0)).collect();
        shell_link
            .SetPath(windows::core::PCWSTR::from_raw(target_wide.as_ptr()))
            .map_err(|e| format!("SetPath 失败: {:?}", e))?;

        let desc_wide: Vec<u16> = description.encode_utf16().chain(std::iter::once(0)).collect();
        shell_link
            .SetDescription(windows::core::PCWSTR::from_raw(desc_wide.as_ptr()))
            .map_err(|e| format!("SetDescription 失败: {:?}", e))?;

        let persist: IPersistFile = shell_link.cast().map_err(|e| format!("cast 失败: {:?}", e))?;

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
pub fn create_desktop_shortcut(_: &str, _: &str) -> Result<(), String> { Ok(()) }
#[cfg(not(windows))]
pub fn create_start_menu_shortcut(_: &str, _: &str) -> Result<(), String> { Ok(()) }

// ============================================================
// 注册表 — 卸载信息
// ============================================================

#[cfg(windows)]
pub fn write_uninstall_registry(install_path: &str, version: &str) -> Result<(), String> {
    use windows::Win32::System::Registry::{
        RegSetValueExW, RegCreateKeyExW, RegCloseKey, REG_CREATE_KEY_DISPOSITION,
        HKEY_CURRENT_USER, KEY_SET_VALUE, KEY_CREATE_SUB_KEY, REG_SZ, REG_OPTION_NON_VOLATILE,
    };
    use windows::core::PCWSTR;

    let subkey: Vec<u16> =
        "Software\\Microsoft\\Windows\\CurrentVersion\\Uninstall\\DeepX"
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
            let data = std::slice::from_raw_parts(
                value_wide.as_ptr() as *const u8,
                value_wide.len() * 2,
            );

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

        set_value("DisplayName", "DeepX")?;
        set_value("Publisher", "DeepX Team")?;
        set_value("InstallLocation", install_path)?;
        set_value("DisplayVersion", version)?;
        set_value("UninstallString", &format!("{}\\uninstall.exe", install_path))?;
        set_value("NoModify", "1")?;
        set_value("NoRepair", "1")?;

        let _ = RegCloseKey(hkey);
    }

    Ok(())
}

#[cfg(not(windows))]
pub fn write_uninstall_registry(_: &str, _: &str) -> Result<(), String> { Ok(()) }
