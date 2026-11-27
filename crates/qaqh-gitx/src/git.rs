//! git CLI 适配层（ADR-0041 决策 5）：linked-worktree 生命周期与状态查询。
//!
//! 全部经 `std::process::Command` 直呼 git——无 shell、参数逐个传递，branch 名
//! 与路径不经过任何解释器，不存在注入面。v1 porcelain 解析以注册表为唯一管理
//! 事实源，`worktree list` 仅用于未注册 worktree 的发现式展示（ADR-0041 决策 6）。
//! status/head/branch 的 gix 0.90 迁移是 v2（version 精确锁纪律见 ADR-0025）。

use std::ffi::OsStr;
use std::path::{Component, Path, PathBuf};
use std::process::Command;

use crate::error::{GitxError, Result};

/// 执行 git 子命令并要求成功；返回 stdout（UTF-8 lossy）。
fn run_git(dir: &Path, args: &[&OsStr]) -> Result<String> {
    let mut cmd = Command::new("git");
    cmd.current_dir(dir).args(args);
    #[cfg(windows)]
    {
        use std::os::windows::process::CommandExt;
        const CREATE_NO_WINDOW: u32 = 0x0800_0000;
        cmd.creation_flags(CREATE_NO_WINDOW);
    }
    let output = cmd.output().map_err(|error| match error.kind() {
        std::io::ErrorKind::NotFound => GitxError::GitUnavailable,
        _ => GitxError::Io(error),
    })?;
    if !output.status.success() {
        let command = args
            .iter()
            .map(|arg| arg.to_string_lossy())
            .collect::<Vec<_>>()
            .join(" ");
        let stderr = String::from_utf8_lossy(&output.stderr).trim().to_string();
        return Err(GitxError::Git { command, stderr });
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

/// `dir` 必须在 git 仓库内，否则 [`GitxError::NotARepository`]。
pub fn verify_repository(dir: &Path) -> Result<()> {
    match run_git(
        dir,
        &[OsStr::new("rev-parse"), OsStr::new("--show-toplevel")],
    ) {
        Ok(_) => Ok(()),
        Err(GitxError::Git { stderr, .. }) if stderr.contains("not a git repository") => Err(
            GitxError::NotARepository(dir.to_string_lossy().into_owned()),
        ),
        Err(error) => Err(error),
    }
}

/// branch 名合法性交给 git 自己判定（`check-ref-format --branch`）。
pub fn validate_branch(branch: &str) -> Result<()> {
    run_git(
        std::env::temp_dir().as_path(),
        &[
            OsStr::new("check-ref-format"),
            OsStr::new("--branch"),
            OsStr::new(branch),
        ],
    )
    .map(|_| ())
    .map_err(|_| GitxError::InvalidBranch(branch.to_string()))
}

/// 解析一个 commit-ish 为完整对象 ID。
pub fn rev_parse_commit(dir: &Path, rev: &str) -> Result<String> {
    let revision = format!("{rev}^{{commit}}");
    let stdout = run_git(
        dir,
        &[
            OsStr::new("rev-parse"),
            OsStr::new("--verify"),
            OsStr::new(&revision),
        ],
    )?;
    Ok(stdout.trim().to_string())
}

/// 当前分支名；detached HEAD（输出为字面量 `HEAD`）或无提交时返回 `None`。
#[must_use]
pub fn current_branch(dir: &Path) -> Option<String> {
    let stdout = run_git(
        dir,
        &[
            OsStr::new("rev-parse"),
            OsStr::new("--abbrev-ref"),
            OsStr::new("HEAD"),
        ],
    )
    .ok()?;
    match stdout.trim() {
        "HEAD" | "" => None,
        branch => Some(branch.to_string()),
    }
}

/// 工作区是否有未提交/未跟踪变更（`status --porcelain` 非空即 dirty）。
pub fn is_dirty(dir: &Path) -> Result<bool> {
    let stdout = run_git(dir, &[OsStr::new("status"), OsStr::new("--porcelain")])?;
    Ok(!stdout.trim().is_empty())
}

/// 在 `source_root` 里创建 linked worktree，并新建分支 `branch` 指向 `base`。
pub fn worktree_add(source_root: &Path, path: &Path, branch: &str, base: &str) -> Result<()> {
    run_git(
        source_root,
        &[
            OsStr::new("worktree"),
            OsStr::new("add"),
            OsStr::new("-b"),
            OsStr::new(branch),
            path.as_os_str(),
            OsStr::new(base),
        ],
    )
    .map(|_| ())
}

/// 从 `source_root` 删除 linked worktree（`force` 追加 `--force`）。
pub fn worktree_remove(source_root: &Path, path: &Path, force: bool) -> Result<()> {
    let mut args: Vec<&OsStr> = vec![OsStr::new("worktree"), OsStr::new("remove")];
    if force {
        args.push(OsStr::new("--force"));
    }
    args.push(path.as_os_str());
    run_git(source_root, &args).map(|_| ())
}

/// `git worktree list --porcelain` 的一条记录。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RawWorktree {
    pub path: PathBuf,
    pub head: Option<String>,
    pub branch: Option<String>,
    pub detached: bool,
}

/// 发现式枚举：列出仓库的全部 linked worktree（含非本注册表管理的）。
pub fn worktree_list(repo: &Path) -> Result<Vec<RawWorktree>> {
    let stdout = run_git(
        repo,
        &[
            OsStr::new("worktree"),
            OsStr::new("list"),
            OsStr::new("--porcelain"),
        ],
    )?;
    let mut worktrees = Vec::new();
    let mut current: Option<RawWorktree> = None;
    for line in stdout.lines() {
        if line.is_empty() {
            if let Some(entry) = current.take() {
                worktrees.push(entry);
            }
            continue;
        }
        if let Some(path) = line.strip_prefix("worktree ") {
            current = Some(RawWorktree {
                path: PathBuf::from(path),
                head: None,
                branch: None,
                detached: false,
            });
        } else if let Some(head) = line.strip_prefix("HEAD ") {
            if let Some(entry) = current.as_mut() {
                entry.head = Some(head.to_string());
            }
        } else if let Some(branch) = line.strip_prefix("branch ") {
            if let Some(entry) = current.as_mut() {
                entry.branch = Some(
                    branch
                        .strip_prefix("refs/heads/")
                        .unwrap_or(branch)
                        .to_string(),
                );
            }
        } else if line == "detached"
            && let Some(entry) = current.as_mut()
        {
            entry.detached = true;
        }
    }
    if let Some(entry) = current.take() {
        worktrees.push(entry);
    }
    Ok(worktrees)
}

/// `status --porcelain=v2 --branch` 一次拿全：分支头段（upstream/ahead-behind）
/// 与逐文件 XY,不为每个字段各起一个 git 进程。
pub fn status_porcelain_v2(dir: &Path) -> Result<String> {
    run_git(
        dir,
        &[
            OsStr::new("status"),
            OsStr::new("--porcelain=v2"),
            OsStr::new("--branch"),
        ],
    )
}

/// `@{u}..HEAD` 的未推送提交（`%h%x09%s`,最新在前）。上游缺失时调用方负责不调。
pub fn unpushed_log(dir: &Path, max_count: u32) -> Result<Vec<(String, String)>> {
    let stdout = run_git(
        dir,
        &[
            OsStr::new("log"),
            OsStr::new(&format!("--max-count={max_count}")),
            OsStr::new("--format=%h%x09%s"),
            OsStr::new("@{u}..HEAD"),
        ],
    )?;
    Ok(stdout
        .lines()
        .filter(|line| !line.is_empty())
        .map(|line| match line.split_once('\t') {
            Some((hash, subject)) => (hash.to_string(), subject.to_string()),
            None => (line.to_string(), String::new()),
        })
        .collect())
}

fn invalid_path(path: &str, reason: &str) -> GitxError {
    GitxError::Io(std::io::Error::new(
        std::io::ErrorKind::InvalidInput,
        format!("unsafe gitx path {path:?}: {reason}"),
    ))
}

/// 要求操作根恰为 git worktree 顶层；子目录不能代表整个托管工作树。
fn canonical_worktree_root(dir: &Path) -> Result<PathBuf> {
    let root = std::fs::canonicalize(dir)?;
    let prefix = run_git(
        &root,
        &[OsStr::new("rev-parse"), OsStr::new("--show-prefix")],
    )?;
    let prefix = prefix.strip_suffix('\n').unwrap_or(&prefix);
    let prefix = prefix.strip_suffix('\r').unwrap_or(prefix);
    if !prefix.is_empty() {
        return Err(GitxError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "gitx operations require the managed worktree root, not a subdirectory",
        )));
    }
    Ok(root)
}

/// 归一协议相对路径，并验证每个已存在的祖先解析后仍位于工作区内。
///
/// 路径按字面 pathspec 使用；反斜杠、Windows drive 路径与 `..` 在任何平台
/// 都拒绝，避免同一请求在 Unix/Windows 上得到不同授权结果。缺失路径（例如
/// 已删除文件）仍检查其最近已存在祖先，因此不能借不存在的尾段绕过符号链接检查。
fn validate_relative_path(dir: &Path, path: &str) -> Result<PathBuf> {
    if path.is_empty() || path.contains('\0') {
        return Err(invalid_path(path, "expected a non-empty relative path"));
    }
    if path.contains('\\') {
        return Err(invalid_path(path, "use '/' as the path separator"));
    }
    let bytes = path.as_bytes();
    if bytes.len() >= 2 && bytes[0].is_ascii_alphabetic() && bytes[1] == b':' {
        return Err(invalid_path(
            path,
            "drive-relative and absolute paths are forbidden",
        ));
    }

    let mut relative = PathBuf::new();
    for component in Path::new(path).components() {
        match component {
            Component::Normal(part) => relative.push(part),
            Component::CurDir => {}
            Component::ParentDir => {
                return Err(invalid_path(path, "parent traversal is forbidden"));
            }
            Component::Prefix(_) | Component::RootDir => {
                return Err(invalid_path(path, "absolute paths are forbidden"));
            }
        }
    }
    if relative.as_os_str().is_empty() {
        return Err(invalid_path(
            path,
            "expected a path below the worktree root",
        ));
    }

    let root = std::fs::canonicalize(dir)?;
    let mut current = root.clone();
    for component in relative.components() {
        current.push(component);
        match std::fs::symlink_metadata(&current) {
            Ok(_) => {
                current = std::fs::canonicalize(&current)?;
                if !current.starts_with(&root) {
                    return Err(invalid_path(path, "symlink resolves outside the worktree"));
                }
            }
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => break,
            Err(error) => return Err(error.into()),
        }
    }
    Ok(relative)
}

/// 返回 `HEAD` 与工作区（含暂存区）之间的字面路径差异；`None` 表示全树。
///
/// 禁止外部 diff/textconv，且路径经 [`validate_relative_path`] 后以独立 argv
/// 传递，不经过 shell 或 git pathspec 魔法语法。
pub fn diff(dir: &Path, path: Option<&str>) -> Result<String> {
    let relative = path
        .map(|path| validate_relative_path(dir, path))
        .transpose()?;
    let root = canonical_worktree_root(dir)?;
    let mut args: Vec<&OsStr> = vec![
        OsStr::new("--literal-pathspecs"),
        OsStr::new("diff"),
        OsStr::new("--no-ext-diff"),
        OsStr::new("--no-textconv"),
        OsStr::new("--no-color"),
        OsStr::new("HEAD"),
        OsStr::new("--"),
    ];
    if let Some(relative) = &relative {
        args.push(relative.as_os_str());
    }
    run_git(&root, &args)
}

/// 暂存并提交工作树全部变更，返回完整提交对象 ID。
///
/// 提交信息只作为 `-m` 的单个 argv 传递；空信息与 NUL 在暂存前拒绝。固定路径
/// `.` 以 canonical worktree 根为 cwd，不存在请求方可控的提交路径。
pub fn commit(dir: &Path, message: &str) -> Result<String> {
    if message.trim().is_empty() || message.contains('\0') {
        return Err(GitxError::Io(std::io::Error::new(
            std::io::ErrorKind::InvalidInput,
            "gitx commit message must be non-empty and contain no NUL",
        )));
    }
    let root = canonical_worktree_root(dir)?;
    run_git(
        &root,
        &[
            OsStr::new("--literal-pathspecs"),
            OsStr::new("add"),
            OsStr::new("--all"),
            OsStr::new("--"),
            OsStr::new("."),
        ],
    )?;
    run_git(
        &root,
        &[
            OsStr::new("commit"),
            OsStr::new("-m"),
            OsStr::new(message),
            OsStr::new("--"),
        ],
    )?;
    rev_parse_commit(&root, "HEAD")
}
