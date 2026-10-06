//! Shared helpers for file edit tools.

use std::io::Write;
use std::path::Path;

// ── Shared limits (read/edit 统一上限，单点维护) ──
pub const READ_MAX_LINES: usize = 400;
pub const READ_MAX_CHARS: usize = 24_000;
/// 单文件读取字节上限（防止大文件/特殊文件全量读入内存）。
pub const READ_MAX_BYTES: u64 = 8 * 1024 * 1024;

/// Stable content fingerprint exposed by `read` and accepted as a write precondition.
pub fn content_hash(content: &str) -> String {
    use sha2::{Digest, Sha256};
    hex::encode(Sha256::digest(content.as_bytes()))
}


/// Write through a sibling temporary file, so a failed write never leaves a partially
/// truncated destination. Rename is atomic on supported filesystems.
pub fn atomic_write(path: &str, content: &str) -> std::io::Result<()> {
    let target = Path::new(path);
    let parent = target.parent().unwrap_or_else(|| Path::new("."));
    let name = target
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("qaqh-file");
    let nonce = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let temporary = parent.join(format!(".{name}.qaqh-{}-{nonce}.tmp", std::process::id()));
    let result = (|| {
        let mut file = std::fs::OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&temporary)?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
        replace_file(&temporary, target)
    })();
    if result.is_err() {
        let _ = std::fs::remove_file(&temporary);
    }
    result
}

#[cfg(not(windows))]
fn replace_file(source: &Path, target: &Path) -> std::io::Result<()> {
    std::fs::rename(source, target)
}

#[cfg(windows)]
fn replace_file(source: &Path, target: &Path) -> std::io::Result<()> {
    use std::os::windows::ffi::OsStrExt;
    use windows::Win32::Storage::FileSystem::{
        MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH, MoveFileExW,
    };
    use windows::core::PCWSTR;

    let source = source
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    let target = target
        .as_os_str()
        .encode_wide()
        .chain(std::iter::once(0))
        .collect::<Vec<_>>();
    unsafe {
        MoveFileExW(
            PCWSTR::from_raw(source.as_ptr()),
            PCWSTR::from_raw(target.as_ptr()),
            MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
        )
    }
    .map_err(std::io::Error::other)
}

/// 行尾风格。
/// 行尾风格。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Ending {
    Lf,
    Crlf,
}

impl Ending {
    pub fn as_str(self) -> &'static str {
        match self {
            Ending::Lf => "\n",
            Ending::Crlf => "\r\n",
        }
    }
}

/// LF 规范视图的还原信息。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct LineEndings {
    /// 出现次数更多的行尾（平票取 CRLF；无换行 → LF）。
    pub preferred: Ending,
    /// 文件是否包含多种行尾。
    pub mixed: bool,
}

/// 统一归一化：`\r\n` 与孤立 `\r` 一律视为换行，返回 LF 视图 + 行尾信息。
///
/// # 换行统一契约（LF canonical view）
///
/// 所有文件工具共享同一"规范视图"（LF）：
/// - `read` 的展示/行号/hash、`edit` 的精确匹配、账本 hash 都基于该视图；
/// - `edit` 写回只把**插入文本**按命中行的行尾还原，文件其余字节原样保留；
///   copy_range 等整段写回按 `endings.preferred` 选行尾；
/// - 新文件/`write` 按模型给定内容原样落盘（不归一化）。
pub fn normalize_newlines(content: &str) -> (String, LineEndings) {
    let mut lf = String::with_capacity(content.len());
    let mut crlf = 0usize;
    let mut other_lf = 0usize;
    let mut lone_cr = 0usize;
    let mut chars = content.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\r' => {
                if chars.peek() == Some(&'\n') {
                    chars.next();
                    crlf += 1;
                } else {
                    lone_cr += 1;
                }
                lf.push('\n');
            }
            '\n' => {
                other_lf += 1;
                lf.push('\n');
            }
            c => lf.push(c),
        }
    }
    let lf_total = crlf + lone_cr + other_lf;
    let endings = if lf_total == 0 || crlf == 0 {
        LineEndings {
            preferred: Ending::Lf,
            mixed: false,
        }
    } else if crlf == lf_total {
        LineEndings {
            preferred: Ending::Crlf,
            mixed: false,
        }
    } else {
        LineEndings {
            preferred: if crlf * 2 >= lf_total {
                Ending::Crlf
            } else {
                Ending::Lf
            },
            mixed: true,
        }
    };
    (lf, endings)
}

/// 共享行索引：按 `\n` 切行并记录每行行首字节偏移。
///
/// 语义与 `read` 完全一致：结尾换行不产生额外空行；空文件是 1 行空行。
#[derive(Debug)]
pub struct LineIndex<'a> {
    lines: Vec<&'a str>,
    byte_starts: Vec<usize>,
}

impl<'a> LineIndex<'a> {
    pub fn new(content: &'a str) -> Self {
        let mut lines: Vec<&'a str> = content.split('\n').collect();
        if content.ends_with('\n') {
            lines.pop();
        }
        let mut byte_starts = Vec::with_capacity(lines.len());
        let mut start = 0usize;
        for line in &lines {
            byte_starts.push(start);
            start += line.len() + 1; // '\n'
        }
        Self { lines, byte_starts }
    }

    pub fn lines(&self) -> &[&'a str] {
        &self.lines
    }

    pub fn line_count(&self) -> usize {
        self.lines.len()
    }

    /// `byte`（LF 视图字节偏移）所在行，1-based。
    pub fn line_of_byte(&self, byte: usize) -> usize {
        self.byte_starts.partition_point(|&s| s <= byte).max(1)
    }
}

/// LF 视图字节偏移 → 原始内容字节偏移（`\r\n`/孤立 `\r` 折叠为 `\n` 的逆映射）。
/// 目标偏移不在字符边界上时返回 None。
pub fn raw_offset_for_lf_offset(raw: &str, lf_offset: usize) -> Option<usize> {
    let mut lf_bytes = 0usize;
    let mut iter = raw.char_indices().peekable();
    while let Some((raw_i, ch)) = iter.next() {
        if lf_bytes == lf_offset {
            return Some(raw_i);
        }
        let consumed = if ch == '\r' {
            if iter.peek().is_some_and(|(_, next)| *next == '\n') {
                iter.next();
            }
            1
        } else {
            ch.len_utf8()
        };
        lf_bytes += consumed;
    }
    (lf_bytes == lf_offset).then_some(raw.len())
}

/// `raw_pos` 所在行的行尾；找不到行终止符的末行 → None。
pub fn ending_at(raw: &str, raw_pos: usize) -> Option<Ending> {
    let rest = raw.get(raw_pos..)?;
    for (i, b) in rest.bytes().enumerate() {
        match b {
            b'\r' => {
                return if rest.as_bytes().get(i + 1) == Some(&b'\n') {
                    Some(Ending::Crlf)
                } else {
                    Some(Ending::Lf)
                };
            }
            b'\n' => return Some(Ending::Lf),
            _ => {}
        }
    }
    None
}

/// 路径守卫错误（read/write/edit/apply_patch 共用）。
#[derive(Debug)]
pub enum PathGuardError {
    /// Windows 保留设备名或设备命名空间路径。
    DevicePath(String),
    /// 最终组件是符号链接（写路径按策略拒绝；读路径跟随）。
    Symlink { path: String, target: String },
    /// 存在但不是普通文件（目录/字符设备/块设备/FIFO/socket）。
    NotRegular { path: String, kind: &'static str },
}

impl PathGuardError {
    pub fn code(&self) -> &'static str {
        match self {
            Self::DevicePath(_) => "unsupported_path",
            Self::Symlink { .. } => "symlink_target",
            Self::NotRegular { .. } => "unsupported_file_type",
        }
    }

    pub fn message(&self) -> String {
        match self {
            Self::DevicePath(path) => {
                format!("'{path}' is a reserved device name or device namespace path")
            }
            Self::Symlink { path, target } => format!(
                "'{path}' is a symbolic link to '{target}'; refusing to replace or write through it — use the target path directly"
            ),
            Self::NotRegular { path, kind } => {
                format!("'{path}' is not a regular file ({kind}); only regular files are supported")
            }
        }
    }

    pub fn hint(&self) -> Option<String> {
        match self {
            Self::DevicePath(_) => None,
            Self::Symlink { .. } => {
                Some("Resolve the link and retry with the real target path.".to_string())
            }
            Self::NotRegular { .. } => {
                Some("Use exec for devices/pipes, or read a regular file.".to_string())
            }
        }
    }
}

fn file_kind(meta: &std::fs::Metadata) -> &'static str {
    let ft = meta.file_type();
    if ft.is_dir() {
        "directory"
    } else if ft.is_symlink() {
        "symlink"
    } else {
        #[cfg(unix)]
        {
            use std::os::unix::fs::FileTypeExt;
            if ft.is_char_device() {
                "character device"
            } else if ft.is_block_device() {
                "block device"
            } else if ft.is_fifo() {
                "fifo"
            } else if ft.is_socket() {
                "socket"
            } else {
                "special file"
            }
        }
        #[cfg(not(unix))]
        {
            "special file"
        }
    }
}

/// Windows 保留设备名（大小写不敏感；带扩展名/尾随点空格同样命中）。
pub fn is_windows_reserved_name(name: &str) -> bool {
    let trimmed = name.trim_end_matches([' ', '.']);
    let stem = trimmed.split('.').next().unwrap_or(trimmed);
    let upper = stem.to_ascii_uppercase();
    matches!(
        upper.as_str(),
        "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$"
    ) || (upper.len() == 4
        && (upper.starts_with("COM") || upper.starts_with("LPT"))
        && upper.as_bytes()[3].is_ascii_digit()
        && upper.as_bytes()[3] != b'0')
}

fn reject_device_path(path: &str) -> Result<(), PathGuardError> {
    if !cfg!(windows) {
        return Ok(());
    }
    if path.starts_with(r"\\.\") || path.starts_with(r"\\?\GLOBALROOT") {
        return Err(PathGuardError::DevicePath(path.to_string()));
    }
    let name = std::path::Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("");
    if is_windows_reserved_name(name) {
        return Err(PathGuardError::DevicePath(path.to_string()));
    }
    Ok(())
}

/// 读守卫：符号链接跟随后的目标必须是普通文件；设备/FIFO/目录拒绝。
/// 不存在或其它 IO 错误放行，由后续读取报具体错误。
pub fn ensure_readable_regular_file(path: &str) -> Result<(), PathGuardError> {
    reject_device_path(path)?;
    match std::fs::metadata(path) {
        Ok(meta) if meta.is_file() => Ok(()),
        Ok(meta) => Err(PathGuardError::NotRegular {
            path: path.to_string(),
            kind: file_kind(&meta),
        }),
        Err(_) => Ok(()),
    }
}

/// 写守卫：最终组件是符号链接 → 拒绝（策略：不替换链接、不穿透写）；
/// 已存在的设备/FIFO/目录 → 拒绝；目标不存在 → 允许创建。
pub fn ensure_writable_regular_target(path: &str) -> Result<(), PathGuardError> {
    reject_device_path(path)?;
    match std::fs::symlink_metadata(path) {
        Ok(meta) if meta.file_type().is_symlink() => {
            let target = std::fs::read_link(path)
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_else(|_| "<unresolved>".to_string());
            Err(PathGuardError::Symlink {
                path: path.to_string(),
                target,
            })
        }
        Ok(meta) if !meta.is_file() => Err(PathGuardError::NotRegular {
            path: path.to_string(),
            kind: file_kind(&meta),
        }),
        Ok(_) => Ok(()),
        Err(_) => Ok(()),
    }
}

/// Produce a unified diff between two file contents.
/// Shows the first diff region with context.
pub fn unified_diff(before: &str, after: &str, path: &str) -> String {
    use similar::TextDiff;

    if before == after {
        return String::new();
    }
    let (from, to) = diff_header_labels(path);
    let diff = TextDiff::from_lines(before, after);
    diff.unified_diff()
        .context_radius(3)
        .header(&from, &to)
        .to_string()
}

/// diff 头的 `a/` `b/` 前缀是 git 惯例，语义是「相对仓库根」；绝对路径再拼前缀
/// 会得到 `a//tmp/...` 这种双斜杠，所以绝对路径原样输出，只有相对路径加前缀。
fn diff_header_labels(path: &str) -> (String, String) {
    if is_absolute_path(path) {
        (path.to_string(), path.to_string())
    } else {
        (format!("a/{path}"), format!("b/{path}"))
    }
}

/// 展示用绝对路径判断：跨平台（Linux 绝对路径、Windows 盘符 / UNC 路径）。
fn is_absolute_path(path: &str) -> bool {
    Path::new(path).is_absolute()
        // POSIX 风格前导 /：daemon 可能在 Linux 侧（Windows 的 is_absolute
        // 对它返回 false，但路径语义是绝对的）。
        || path.starts_with('/')
        || path.starts_with('\\')
        || matches!(path.as_bytes(), [drive, b':', ..] if drive.is_ascii_alphabetic())
}

/// Count added/removed lines and find the first changed line between two
/// contents, using `similar`'s structured diff ops — no diff-text parsing.
///
/// `first_line` is the 1-based line of the first actual change in `before`
/// (more precise than the unified-diff hunk header, which includes context).
pub fn diff_stats_between(before: &str, after: &str) -> (u32, u32, u32) {
    use similar::DiffTag;
    let diff = similar::TextDiff::from_lines(before, after);
    let mut added = 0u32;
    let mut removed = 0u32;
    let mut first_line = 1u32;
    let mut got_change = false;
    for op in diff.ops() {
        if op.tag() == DiffTag::Equal {
            continue;
        }
        if !got_change {
            first_line = op.old_range().start as u32 + 1;
            got_change = true;
        }
        match op.tag() {
            DiffTag::Insert => added += op.new_range().len() as u32,
            DiffTag::Delete => removed += op.old_range().len() as u32,
            DiffTag::Replace => {
                added += op.new_range().len() as u32;
                removed += op.old_range().len() as u32;
            }
            DiffTag::Equal => {}
        }
    }
    (added, removed, first_line)
}

pub fn is_binary_read_error(err: &str) -> bool {
    err.contains("valid UTF-8")
        || err.contains("utf8")
        || err.contains("utf-8")
        || err.contains("UTF-8")
}

#[cfg(test)]
mod atomic_write_tests {
    use super::*;

    #[test]
    fn atomic_write_replaces_an_existing_file() {
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.txt");
        std::fs::write(&target, "before").unwrap();

        atomic_write(&target.to_string_lossy(), "after").unwrap();

        assert_eq!(std::fs::read_to_string(target).unwrap(), "after");
    }

    #[test]
    fn unified_diff_headers_avoid_double_slash_for_absolute_paths() {
        let diff = unified_diff("old\n", "new\n", "/tmp/work/a.txt");
        assert!(diff.contains("--- /tmp/work/a.txt"), "got: {diff}");
        assert!(diff.contains("+++ /tmp/work/a.txt"), "got: {diff}");
        assert!(!diff.contains("a//"), "got: {diff}");
        assert!(!diff.contains("b//"), "got: {diff}");
    }

    #[test]
    fn unified_diff_headers_keep_git_prefix_for_relative_paths() {
        let diff = unified_diff("old\n", "new\n", "src/a.rs");
        assert!(diff.contains("--- a/src/a.rs"), "got: {diff}");
        assert!(diff.contains("+++ b/src/a.rs"), "got: {diff}");
    }

    #[test]
    fn is_absolute_path_recognizes_platform_forms() {
        assert!(is_absolute_path("/tmp/x"));
        assert!(is_absolute_path("C:\\work\\x"));
        assert!(is_absolute_path("\\\\server\\share\\x"));
        assert!(!is_absolute_path("src/a.rs"));
        assert!(!is_absolute_path("./a.rs"));
    }

    #[test]
    fn diff_stats_between_counts_changes_and_first_line() {
        let before = "a\nb\nc\nd\ne\n";
        let after = "a\nb\nX\nY\ne\n";
        // 第 3 行起：替换 2 行
        let (added, removed, first_line) = diff_stats_between(before, after);
        assert_eq!((added, removed, first_line), (2, 2, 3));
    }

    #[test]
    fn diff_stats_between_handles_insert_and_delete() {
        let before = "a\nb\nc\n";
        let after = "a\nb\nB2\nc\nd\n";
        let (added, removed, first_line) = diff_stats_between(before, after);
        assert_eq!((added, removed, first_line), (2, 0, 3));
    }

    #[test]
    fn diff_stats_between_identical_content_is_zero() {
        let (added, removed, first_line) = diff_stats_between("x\ny\n", "x\ny\n");
        assert_eq!((added, removed, first_line), (0, 0, 1));
    }

    #[test]
    fn normalize_unifies_mixed_endings_to_lf() {
        let (lf, endings) = normalize_newlines("a\r\nb\rc\nd");
        assert_eq!(lf, "a\nb\nc\nd");
        assert!(endings.mixed);
        assert_eq!(endings.preferred, Ending::Lf); // crlf=1，其余换行 2
    }

    #[test]
    fn normalize_reports_pure_endings() {
        let (lf, endings) = normalize_newlines("a\r\nb\r\n");
        assert_eq!(lf, "a\nb\n");
        assert_eq!(endings.preferred, Ending::Crlf);
        assert!(!endings.mixed);

        let (lf, endings) = normalize_newlines("a\nb\n");
        assert_eq!(lf, "a\nb\n");
        assert_eq!(endings.preferred, Ending::Lf);
        assert!(!endings.mixed);

        // 孤立 CR（经典 Mac）按 LF 视图处理，首选行尾回落 LF。
        let (lf, endings) = normalize_newlines("a\rb\r");
        assert_eq!(lf, "a\nb\n");
        assert_eq!(endings.preferred, Ending::Lf);
        assert!(!endings.mixed);
    }

    #[test]
    fn line_index_matches_read_semantics() {
        let index = LineIndex::new("a\nb\n");
        assert_eq!(index.lines(), &["a", "b"]);
        assert_eq!(index.line_count(), 2);
        assert_eq!(index.line_of_byte(0), 1);
        assert_eq!(index.line_of_byte(2), 2);
        // 结尾换行不产生额外空行
        assert_eq!(LineIndex::new("a\n\nb\n").lines(), &["a", "", "b"]);
    }

    #[test]
    fn raw_offset_mapping_inverts_normalization() {
        let raw = "a\r\nb\rc\n";
        let (lf, _) = normalize_newlines(raw);
        assert_eq!(lf, "a\nb\nc\n");
        assert_eq!(raw_offset_for_lf_offset(raw, 0), Some(0));
        assert_eq!(raw_offset_for_lf_offset(raw, 2), Some(3)); // 'b'
        assert_eq!(raw_offset_for_lf_offset(raw, 4), Some(5)); // 'c'
        assert_eq!(raw_offset_for_lf_offset(raw, 6), Some(7)); // EOF
    }

    #[test]
    fn ending_at_reads_local_line_ending() {
        assert_eq!(ending_at("a\r\nb", 0), Some(Ending::Crlf));
        assert_eq!(ending_at("a\nb", 0), Some(Ending::Lf));
        assert_eq!(ending_at("a\rb", 0), Some(Ending::Lf)); // 孤立 CR → LF
        assert_eq!(ending_at("abc", 0), None); // 末行无行尾
    }

    #[test]
    fn windows_reserved_names_are_detected() {
        for name in [
            "NUL",
            "nul",
            "NUL.txt",
            "CON",
            "COM1",
            "LPT9",
            "CONIN$",
            "conout$.txt",
            "aux.txt.bak",
        ] {
            assert!(is_windows_reserved_name(name), "{name}");
        }
        for name in [
            "NULL",
            "COM0",
            "LPT0",
            "console",
            "nul_file",
            "auxiliary.txt",
        ] {
            assert!(!is_windows_reserved_name(name), "{name}");
        }
    }

    #[cfg(unix)]
    #[test]
    fn guards_reject_symlink_fifo_and_dev_null() {
        use std::os::unix::fs::symlink;
        let dir = tempfile::tempdir().unwrap();
        let target = dir.path().join("target.txt");
        std::fs::write(&target, "x").unwrap();
        let link = dir.path().join("link.txt");
        symlink(&target, &link).unwrap();
        let err = ensure_writable_regular_target(&link.to_string_lossy()).unwrap_err();
        assert_eq!(err.code(), "symlink_target");
        assert!(err.message().contains("target.txt"), "{}", err.message());

        let fifo = dir.path().join("fifo");
        let status = std::process::Command::new("mkfifo")
            .arg(&fifo)
            .status()
            .unwrap();
        assert!(status.success());
        assert_eq!(
            ensure_writable_regular_target(&fifo.to_string_lossy())
                .unwrap_err()
                .code(),
            "unsupported_file_type"
        );
        assert_eq!(
            ensure_readable_regular_file(&fifo.to_string_lossy())
                .unwrap_err()
                .code(),
            "unsupported_file_type"
        );
        assert_eq!(
            ensure_readable_regular_file("/dev/null")
                .unwrap_err()
                .code(),
            "unsupported_file_type"
        );
        assert!(ensure_writable_regular_target(&target.to_string_lossy()).is_ok());
        assert!(
            ensure_writable_regular_target(&dir.path().join("new.txt").to_string_lossy()).is_ok()
        );
    }
}
