//! Permission engine: tool categories, permission levels, and trusted folder management.
//!
//! ## Architecture
//! - `ToolCategory` classifies every tool by risk profile (Read/Write/Exec/Net).
//! - `PermissionLevel` defines the default policy (1–3).
//! - `needs_permission()` evaluates whether a tool call requires user confirmation.
//! - `TrustedFolderSet` persists cross-workspace folder trust decisions.

use std::collections::HashSet;
use std::path::{Component, Path, PathBuf};

// ──────────────────────────────────────
// Policy vocabulary
// ──────────────────────────────────────

pub use qaqh_policy::{PermissionDecision, PermissionLevel, PermissionRisk, ToolCategory};

/// Classify action impact from authoritative category and normalized resources.
pub fn classify_risk(
    category: ToolCategory,
    paths: &[PathBuf],
    workspace: &Path,
) -> PermissionRisk {
    if matches!(category, ToolCategory::Exec | ToolCategory::Net) {
        return PermissionRisk::High;
    }

    let workspace = resolve_target_path(workspace.to_path_buf());
    if paths
        .iter()
        .map(|path| resolve_target_path(path.clone()))
        .any(|path| !path.starts_with(&workspace))
    {
        return PermissionRisk::High;
    }

    match category {
        ToolCategory::Read => PermissionRisk::Low,
        ToolCategory::Write => PermissionRisk::Medium,
        ToolCategory::Exec | ToolCategory::Net => PermissionRisk::High,
    }
}

// ──────────────────────────────────────
// Path helpers
// ──────────────────────────────────────

/// Extract file/directory paths from tool arguments that the tool will read or write.
pub fn extract_target_paths(tool_name: &str, args: &serde_json::Value) -> Vec<PathBuf> {
    let mut paths = Vec::new();

    if tool_name == "read"
        && let Some(requests) = args.get("requests").and_then(|value| value.as_array())
    {
        paths.extend(requests.iter().filter_map(|request| {
            request
                .get("path")
                .and_then(|value| value.as_str())
                .map(PathBuf::from)
        }));
    }
    // Direct path argument
    if let Some(p) = args.get("path").and_then(|v| v.as_str()) {
        paths.push(PathBuf::from(p));
    }
    // Multiple paths
    if let Some(arr) = args.get("paths").and_then(|v| v.as_array()) {
        for v in arr {
            if let Some(s) = v.as_str() {
                paths.push(PathBuf::from(s));
            }
        }
    }
    // source / dest pairs (copy, move)
    if let Some(s) = args.get("source").and_then(|v| v.as_str()) {
        paths.push(PathBuf::from(s));
    }
    if let Some(d) = args.get("dest").and_then(|v| v.as_str()) {
        paths.push(PathBuf::from(d));
    }
    // copy_range: source (read) + target (write) — both workspace-bounded
    if tool_name == "copy_range" {
        if let Some(s) = args.get("source_path").and_then(|v| v.as_str()) {
            paths.push(PathBuf::from(s));
        }
        if let Some(t) = args.get("target_path").and_then(|v| v.as_str()) {
            paths.push(PathBuf::from(t));
        }
    }
    // web_fetch: `output` 是 web.rs 里唯一且无条件的 `fs::write` 目标；
    // 不提等于审批清单看不到写目标，workspace 边界/trust folder 判定失明。
    if tool_name == "web_fetch"
        && let Some(o) = args.get("output").and_then(|v| v.as_str())
    {
        paths.push(PathBuf::from(o));
    }
    // journal: replay target/out may write outside the workspace; keep it
    // authorization-bounded like other write tools.
    if tool_name == "journal" {
        if let Some(f) = args.get("file").and_then(|v| v.as_str()) {
            paths.push(PathBuf::from(f));
        }
        if let Some(o) = args.get("out").and_then(|v| v.as_str()) {
            paths.push(PathBuf::from(o));
        }
    }
    // exec 独占后唯一命令入口：取 cwd 进授权资源。
    if tool_name == "exec"
        && let Some(cwd) = args.get("cwd").and_then(|v| v.as_str())
    {
        paths.push(PathBuf::from(cwd));
    }
    // W3：apply_patch 目标在 patch 文本里，解析 Codex 格式头。
    if tool_name == "apply_patch"
        && let Some(patch) = args.get("patch").and_then(|v| v.as_str())
    {
        for target in patch_target_paths(patch) {
            paths.push(PathBuf::from(target));
        }
    }

    paths.into_iter().map(resolve_target_path).collect()
}

/// Build the bounded, user-facing operation summary for an approval dialog.
///
/// `exec` is the one built-in tool whose effect cannot be inferred from
/// [`extract_target_paths`]: the command text may write outside `cwd`. Include
/// the command/args, shell and cwd for informed approval, but deliberately
/// omit `env` and all other arbitrary tool args so the dialog cannot become a
/// secret-dumping surface.
pub fn summarize_permission_action(tool_name: &str, args: &serde_json::Value) -> Option<String> {
    if tool_name != "exec" {
        return None;
    }

    let mut parts = Vec::new();
    if let Some(command) = args
        .get("command")
        .and_then(serde_json::Value::as_str)
        .filter(|value| !value.trim().is_empty())
    {
        parts.push(format!("command: {}", json_display(command)));
    }
    for key in ["args"] {
        if let Some(values) = args
            .get(key)
            .and_then(serde_json::Value::as_array)
            .filter(|values| !values.is_empty())
        {
            let rendered = values
                .iter()
                .map(|value| serde_json::to_string(value).unwrap_or_else(|_| "\"?\"".into()))
                .collect::<Vec<_>>()
                .join(", ");
            parts.push(format!("{key}: [{rendered}]"));
        }
    }
    for key in ["shell", "cwd"] {
        if let Some(value) = args
            .get(key)
            .and_then(serde_json::Value::as_str)
            .filter(|value| !value.trim().is_empty())
        {
            parts.push(format!("{key}: {}", json_display(value)));
        }
    }

    if parts.is_empty() {
        return None;
    }
    // 审计 H1：`SandboxSpec.workspace_write` + 网络 Deny 在无强制后端的平台
    // （Windows/macOS，以及缺 landlock/bwrap 的 Linux）纯属装饰。审批对话框
    // 必须明说，不得让用户带着「命令在沙箱内」的错觉放行 exec。
    if !qaqh_sandbox::filesystem_and_network_isolation_enforced() {
        parts.push(
            "WARNING: sandbox not enforced on this platform — the command will run with full user privileges (filesystem & network)"
                .to_string(),
        );
    }
    Some(bounded_action_summary(parts.join(" · ")))
}

fn json_display(value: &str) -> String {
    serde_json::to_string(value).unwrap_or_else(|_| "\"?\"".to_string())
}

fn bounded_action_summary(value: String) -> String {
    const MAX_CHARS: usize = 4096;
    if value.chars().count() <= MAX_CHARS {
        return value;
    }
    let mut bounded: String = value.chars().take(MAX_CHARS - 1).collect();
    bounded.push('…');
    bounded
}

/// 解析 Codex 格式 patch 文本的目标路径（M1/W3 共享助手）。
/// 支持 `*** Update File:` / `*** Add File:` / `*** Delete File:` /
/// `*** Move to:` 四种头；解析结果仅用于冲突分组与授权资源绑定。
pub fn patch_target_paths(patch: &str) -> Vec<String> {
    const TAGS: [&str; 4] = [
        "*** Update File: ",
        "*** Add File: ",
        "*** Delete File: ",
        "*** Move to: ",
    ];
    let mut paths = Vec::new();
    for line in patch.lines() {
        let line = line.trim_start();
        for tag in TAGS {
            if let Some(rest) = line.strip_prefix(tag) {
                let p = rest.trim();
                if !p.is_empty() {
                    paths.push(p.to_string());
                }
                break;
            }
        }
    }
    paths
}

/// Resolve symlinks/junctions in the nearest existing ancestor, then append
/// any missing suffix. This keeps authorization checks correct for new files.
pub fn resolve_target_path(path: PathBuf) -> PathBuf {
    let absolute = if path.is_absolute() {
        path
    } else {
        // W7：与执行侧对齐——相对路径以工作区根为基准，而非进程 cwd；
        // 否则授权/审计绑定的资源与实际写入路径错位。
        let ws = crate::current_workspace();
        if ws.is_empty() || ws == "." {
            std::env::current_dir()
                .map(|cwd| cwd.join(&path))
                .unwrap_or(path)
        } else {
            std::path::Path::new(&ws).join(&path)
        }
    };
    let normalized = normalize_lexically(&absolute);
    // 纯根路径（`/`、`\`、盘根）不做 canonicalize：Windows 上 `canonicalize("/")`
    // 解析为**当前盘**根（如 `C:\`），让分隔符垃圾信任条目逃过 `trimmed_key`
    // 的 fail-open 守卫，把整块盘当成信任子树。原样返回使 `path_within_dir`
    // 走 fail-closed（弹审批）。
    if !normalized
        .components()
        .any(|component| matches!(component, std::path::Component::Normal(_)))
    {
        return normalized;
    }
    let mut ancestor = normalized.as_path();
    let mut missing = Vec::new();

    while !ancestor.exists() {
        let Some(name) = ancestor.file_name() else {
            return normalized;
        };
        missing.push(name.to_os_string());
        let Some(parent) = ancestor.parent() else {
            return normalized;
        };
        ancestor = parent;
    }

    let Ok(mut resolved) = std::fs::canonicalize(ancestor) else {
        return normalized;
    };
    for component in missing.iter().rev() {
        resolved.push(component);
    }
    resolved
}

pub fn normalize_lexically(path: &Path) -> PathBuf {
    let mut normalized = PathBuf::new();
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir | Component::Normal(_) => {
                normalized.push(component.as_os_str());
            }
            Component::CurDir => {}
            Component::ParentDir => {
                normalized.pop();
            }
        }
    }
    normalized
}

/// 路径比较键：在 `OsStr` 的**原始字节**上比较，不做有损转换。
///
/// - Windows 文件系统大小写不敏感，仅在 `cfg!(windows)` 时折叠 ASCII 大小写；
///   其余平台（Linux/macOS、WSL `/mnt/*`）大小写敏感，原样保留字节——
///   否则 `Shared/` 与 `shared/` 这两个**不同目录**会被判为同一目录，
///   导致越权 `AutoApprove`。
/// - 不用 `to_string_lossy()`：非法字节会被统一替换为 `U+FFFD`，使
///   `sh\xFFared` 与 `sh\u{FFFD}ared` 折叠成同一键继而误判相等。
///   非 UTF-8 路径保留原字节参与比较，不折叠、不替换。
fn path_comparison_key(path: &Path) -> Vec<u8> {
    let bytes = path.as_os_str().as_encoded_bytes().to_vec();
    if cfg!(windows) {
        bytes.to_ascii_lowercase()
    } else {
        bytes
    }
}

/// 去掉路径尾部的分隔符（`/` 与 `\`），并把空串归一为「无键」。
///
/// 空信任目录（`""`、`"/"`）若参与匹配会变成空前缀，把所有绝对路径都判为
/// 子树成员（`path_within_dir("/etc/x", "")` == true），是 fail-open，必须拦掉。
fn trimmed_key(path: &Path) -> Option<Vec<u8>> {
    let mut key = path_comparison_key(path);
    while key.len() > 1 && matches!(key.last(), Some(b'/' | b'\\')) {
        key.pop();
    }
    if key.is_empty() || key == b"/" || key == b"\\" {
        return None;
    }
    Some(key)
}

/// Whether `path` lies inside `dir` (or is `dir` itself), on **path component**
/// boundaries.
///
/// Component-wise comparison matters: a raw string `starts_with` would treat
/// `D:\shared-other` as inside `D:\shared` and auto-approve a write the user
/// never trusted.
///
/// Fail-closed：任何一侧无法得到非空比较键（空/纯分隔符目录）时返回 `false`，
/// 即「不算命中信任目录」，维持弹审批，而非放行。
pub fn path_within_dir(path: &Path, dir: &Path) -> bool {
    let Some(dir_key) = trimmed_key(dir) else {
        return false;
    };
    let Some(key) = trimmed_key(path) else {
        return false;
    };
    if key == dir_key {
        return true;
    }
    key.strip_prefix(dir_key.as_slice())
        .is_some_and(|rest| rest.first().is_some_and(|b| matches!(b, b'/' | b'\\')))
}

/// Check if ALL target paths are inside the workspace root.
pub fn all_within_workspace(paths: &[PathBuf], workspace: &Path) -> bool {
    if paths.is_empty() {
        return true;
    } // tools without paths (e.g. ask) are considered safe
    paths.iter().all(|p| p.starts_with(workspace))
}

// ──────────────────────────────────────
// Permission decision
// ──────────────────────────────────────

// `PermissionDecision` is re-exported above from `qaqh-policy`.

/// Whether `path` points at the agent's own persistent state (history /
/// credentials under the platform data dir). Blocked from normal `read`
/// access even under SkipPermissions to prevent exfiltration of prior turns.
///
/// 也是远端 `fs.read`/`fs.list` 的单一事实源（T-2-1）：白名单放行的数据根下，
/// 这些敏感路径必须单独拦掉。
///
/// 判定对象始终是**本产品数据根**子树：权威路径（`sessions/**`、`config.toml`）
/// 按组件级包含比较，数据根内其余文件（`token_stats.jsonl`、`secrets.toml` …）
/// 才走文件名名单。数据根之外同名的目录/文件（第三方产品的 `…\sessions\…log`、
/// 仓库里的 `meta.json`）不在此列——否则只读工具会被无端弹审批。
pub fn is_sensitive_session_path(path: &Path) -> bool {
    // Block the agent from reading its own persistent history / credentials.
    // These live under the platform data dir (e.g. ~/.config/qaqh/sessions/…/messages.jsonl,
    // meta.json, token_stats.jsonl, secrets.toml) and are outside any workspace.
    // Under SkipPermissions they'd otherwise auto-approve, allowing the model to exfiltrate
    // prior turns via a normal `read` tool call and then replay that content
    // into the gateway (messages.jsonl → gateway leak).
    //
    // 平台会话目录本身及其全部后代：字符串名单靠 `"/sessions/"` 判定会漏掉
    // 目录本身（无尾分隔符），且数据根可被 `QAQH_DATA_DIR` 重定向到任意名字
    // ——所以这里按平台权威路径做组件级包含判定（T-2-1 让 `fs.list` 拦下
    // `sessions/` 目录本身）。
    let candidate = normalize_lexically(&resolve_target_path(path.to_path_buf()));
    let sessions_dir = qaqh_types::platform::sessions_dir();
    if sessions_dir.is_absolute() {
        let sessions_norm = normalize_lexically(&resolve_target_path(sessions_dir));
        if path_within_dir(&candidate, &sessions_norm) {
            return true;
        }
    }
    // 审计 M4：用户目录 config.toml 也在敏感名单内。skip-permissions 下模型
    // 可经普通 write 工具重写它固化 `permission_tier = 3` 或改 `base_url`
    // （后续 LLM 请求连同 api_key 导向攻击者端点），且 mtime 轮询热加载
    // **免重启生效**。
    // 配置只从平台数据根读取，按权威路径做组件级判定，不靠宽泛子串。
    let config_file = qaqh_types::platform::config_path();
    if config_file.is_absolute() {
        let config_norm = normalize_lexically(&resolve_target_path(config_file));
        if path_within_dir(&candidate, &config_norm) {
            return true;
        }
    }
    if !within_own_data_root(&candidate) {
        return false;
    }
    let s = candidate.to_string_lossy().to_ascii_lowercase();
    s.contains("messages.jsonl")
        || s.contains("meta.json")
        || s.contains("token_stats.jsonl")
        || s.contains("secrets.toml")
        || s.contains("/sessions/")
        || s.contains("\\sessions\\")
        || s.contains(".qaqh/sessions")
}

/// 名单尾巴的定界谓词：只有落在**本产品数据根**子树内的路径才允许参与文件名
/// 子串判定（2026-10-08 修，见 [`is_sensitive_session_path`]）。
///
/// 数据根不可定界（`HOME`/`USERPROFILE` 皆缺，`data_dir()` 非绝对）时返回
/// `true`，保持旧的保守姿态——宁多弹一次审批，也不在无法判定归属时放行。
fn within_own_data_root(candidate: &Path) -> bool {
    let data_dir = qaqh_types::platform::data_dir();
    if !data_dir.is_absolute() {
        return true;
    }
    path_within_dir(
        candidate,
        &normalize_lexically(&resolve_target_path(data_dir)),
    )
}

/// Whether `path` lies inside a skill discovery root (audit 2026-10-01 H2).
///
/// Files under these roots are discovered every turn and injected into model
/// context as authoritative instructions (`skills(action=activate)`), and the
/// files persist in the repo across sessions. A write there is an instruction
/// injection, so it never rides the Level-3 in-workspace auto-approve.
/// Containment is lexical (roots may not exist yet — the attack writes *new*
/// files), resolved against the same roots [`qaqh_skills::discover`] scans.
fn is_skill_instruction_path(path: &Path, workspace_root: &Path) -> bool {
    let candidate = normalize_lexically(&resolve_target_path(path.to_path_buf()));
    if candidate.as_os_str().is_empty() {
        return false;
    }
    qaqh_skills::skill_roots(workspace_root).iter().any(|root| {
        let root_norm = normalize_lexically(&resolve_target_path(root.clone()));
        path_within_dir(&candidate, &root_norm)
    })
}

/// Determine whether a tool call requires user permission.
///
/// - `level`: current permission level
/// - `tool_name`: registered tool name
/// - `args`: tool arguments (JSON)
/// - `workspace_root`: workspace root directory (used for boundary checks)
/// - `trusted_dirs`: set of previously trusted directories
/// - `declared_category`: capability category from the handler declaration
///   （单一事实源；`process` 等按 action 细分的工具在内部覆盖）
pub fn needs_permission(
    level: PermissionLevel,
    tool_name: &str,
    args: &serde_json::Value,
    workspace_root: &Path,
    trusted_dirs: &HashSet<PathBuf>,
    declared_category: ToolCategory,
) -> PermissionDecision {
    // Task only mutates the active session's own todo.json. It does not
    // touch workspace files, run code, or access external resources. Requiring
    // approval for each model-authored status transition creates recursive,
    // repeated prompts without protecting a user-controlled resource.
    if matches!(tool_name, "todo_list" | "todo_update" | "todo_write") {
        return PermissionDecision::AutoApprove;
    }

    // ask is itself the user-interaction boundary. Opening a permission
    // dialog for it creates a recursive prompt and prevents the Ring from
    // delivering the actual model question.
    if tool_name == "ask" {
        return PermissionDecision::AutoApprove;
    }

    // Sensitive session files are never auto-approved, even under SkipPermissions.
    // The paths are outside the workspace already, but SkipPermissions would otherwise
    // bypass the outside-workspace check. Treat them as High risk and force a
    // dialog so the user sees "read messages.jsonl" before it happens.
    let paths = extract_target_paths(tool_name, args);
    if paths.iter().any(|p| is_sensitive_session_path(p)) {
        let risk = PermissionRisk::High;
        return PermissionDecision::AskUser {
            reason: format!(
                "Sensitive session file access requires confirmation: '{}'",
                tool_name
            ),
            paths,
            category: ToolCategory::Read,
            risk,
            consequence:
                "May expose prior conversation history or credentials to the model/gateway."
                    .to_string(),
        };
    }

    // `process` 按调用形态细分（per-action 授权颗粒度的扩展点）：
    // - process: check/wait 只读、kill/write 控制进程；
    // - edit 已收敛为纯写工具（read 模式移除，行号系统归 read 工具）。
    let category = match tool_name {
        "process" => match args.get("action").and_then(|value| value.as_str()) {
            Some("check" | "wait") => ToolCategory::Read,
            Some("write" | "kill") => ToolCategory::Exec,
            _ => ToolCategory::Write,
        },
        _ => declared_category,
    };
    let workspace_root = resolve_target_path(workspace_root.to_path_buf());
    let risk = classify_risk(category, &paths, &workspace_root);
    let consequence = risk.consequence().to_string();

    // 审计 H2：skill 目录写入 = 权威指令注入面。默认档 L3 下工作区内写自动
    // 放行，被注入的模型可静默把 SKILL.md 植入 `.qaqh/skills/`，下一回合被
    // 发现目录自动收录并以系统级权威指令身份注入，且文件留在仓库跨会话
    // 持久生效。故写工具落点在任一 skill 发现根内时无条件弹审批（含 L4——
    // 这类文件一旦落盘，其影响远超单次命令）。
    if category == ToolCategory::Write
        && paths
            .iter()
            .any(|path| is_skill_instruction_path(path, &workspace_root))
    {
        return PermissionDecision::AskUser {
            reason: format!(
                "Writing a skill file requires confirmation: '{}'",
                tool_name
            ),
            paths,
            category,
            risk: PermissionRisk::High,
            consequence: "Skill files are injected into model context as authoritative \
                          instructions and persist across sessions."
                .to_string(),
        };
    }

    // SkipPermissions is the explicit bypass mode: ordinary tools auto-approve,
    // including Exec/Net. The sensitive-session-file guard above still wins.
    // Exec sandboxing is independent of the tier: bypassing approval does not
    // disable the sandbox spec.
    if level == PermissionLevel::SkipPermissions {
        return PermissionDecision::AutoApprove;
    }

    // 2026-10-05 读自由规则：Read 类**无条件放行**——读取完全不受工作区限制
    // （read/grep/glob/read_image 等在任意档位、任意路径下都不弹审批）。
    // 两道既有守卫不受影响：敏感路径守卫（会话历史/凭据外泄）已在上方先行
    // 拦截；子代理沙箱的「跨 workspace 一律拒」在 admit 层兜底（S3 防越狱），
    // 不依赖本函数的边界判定。
    if category == ToolCategory::Read {
        return PermissionDecision::AutoApprove;
    }

    // WorkspaceWrite: workspace writes auto-approve. Exec and Net still require
    // confirmation. Cross-workspace writes go through one-time folder trust.
    if level >= PermissionLevel::WorkspaceWrite && category == ToolCategory::Write {
        // If no paths or all paths are within the workspace, auto-approve the write.
        if all_within_workspace(&paths, &workspace_root) {
            return PermissionDecision::AutoApprove;
        }

        // Cross-workspace: check trusted folders.
        //
        // BUG-2026-09-13-14：旧实现取 `outside.parent()` 后与信任目录做**精确
        // 相等**比较。信任 `D:\shared` 后写 `D:\shared\sub\new.rs`（`sub` 新建）
        // 的父目录是 `D:\shared\sub` ≠ 信任目录 → 每次重新弹审批，与
        // "one-time trust" 语义相反；且两侧比较对大小写敏感（Windows 文件系统
        // 大小写不敏感，`D:\Shared` 与 `d:\shared` 是同一目录）。
        // 现在改为「信任目录的子树（含自身）」按分量级前缀比较。
        //
        // 同时收敛既有的「只看首个在外路径」fail-open：多路径调用（如 copy
        // 的 source/dest）中只要有一个路径落在信任目录之外，就必须整体弹审批，
        // 否则未信任路径会被同批次里已信任的那条连带放行。
        let trusted_keys: Vec<PathBuf> = trusted_dirs
            .iter()
            .map(|trusted| resolve_target_path(trusted.clone()))
            .collect();
        let all_trusted = paths
            .iter()
            .filter(|path| !path.starts_with(&workspace_root))
            .all(|outside| {
                trusted_keys
                    .iter()
                    .any(|trusted| path_within_dir(outside, trusted))
            });
        if all_trusted {
            return PermissionDecision::AutoApprove;
        }
    }

    // Otherwise: ask user
    // （Read 类已在上文无条件放行，不会再落到这里；此处的 Read 判定仅剩
    //   死防御。）
    let reason = if level == PermissionLevel::ReadOnly {
        format!(
            "read-only mode: '{}' (write/exec/net) requires confirmation.",
            tool_name
        )
    } else if matches!(category, ToolCategory::Exec | ToolCategory::Net) {
        format!(
            "'{}' requires execution or network confirmation.",
            tool_name
        )
    } else {
        format!(
            "'{}' writes outside the workspace (one-time folder trust).",
            tool_name
        )
    };

    PermissionDecision::AskUser {
        reason,
        paths,
        category,
        risk,
        consequence,
    }
}

// ──────────────────────────────────────
// Trusted folder set
// ──────────────────────────────────────

/// Persistent set of trusted directories for cross-workspace access.
/// Stored as `{qaqh_dir}/sessions/{session_id}/trusted_folders.json`.
pub struct TrustedFolderSet {
    session_id: String,
    dirs: HashSet<PathBuf>,
}

impl TrustedFolderSet {
    /// Load the trusted folders file for a session, or create an empty set.
    pub fn load(session_id: &str) -> Self {
        let path = trusted_folders_path(session_id);
        let dirs = if path.exists() {
            std::fs::read_to_string(&path)
                .ok()
                .and_then(|s| serde_json::from_str::<Vec<String>>(&s).ok())
                .map(|v| v.into_iter().map(PathBuf::from).collect())
                .unwrap_or_default()
        } else {
            HashSet::new()
        };
        Self {
            session_id: session_id.to_string(),
            dirs,
        }
    }

    /// Add a directory to the trusted set and persist.
    pub fn trust(&mut self, dir: &Path) {
        self.dirs.insert(dir.to_path_buf());
        self.save();
    }

    /// Check if a directory is trusted.
    pub fn contains(&self, dir: &Path) -> bool {
        self.dirs.contains(dir)
    }

    /// Expose the underlying set for permission checks.
    pub fn set(&self) -> &HashSet<PathBuf> {
        &self.dirs
    }

    fn save(&self) {
        let path = trusted_folders_path(&self.session_id);
        // 由 qaqh_dir()/seed 拼接而来，必然带父目录；None 仅在路径为根时出现。
        let Some(dir) = path.parent() else {
            return;
        };
        let _ = std::fs::create_dir_all(dir);
        let list: Vec<String> = self
            .dirs
            .iter()
            .map(|p| p.to_string_lossy().to_string())
            .collect();
        let _ = std::fs::write(&path, serde_json::to_string(&list).unwrap_or_default());
    }
}

fn trusted_folders_path(session_id: &str) -> PathBuf {
    crate::workspace::qaqh_dir()
        .join("sessions")
        .join(session_id)
        .join("trusted_folders.json")
}

#[cfg(test)]
mod tests {
    use super::*;

    struct LinkedTempTree {
        root: PathBuf,
        link: PathBuf,
    }

    impl Drop for LinkedTempTree {
        fn drop(&mut self) {
            #[cfg(windows)]
            let _ = std::fs::remove_dir(&self.link);
            #[cfg(unix)]
            let _ = std::fs::remove_file(&self.link);
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    fn linked_temp_tree() -> (LinkedTempTree, PathBuf, PathBuf) {
        let unique = format!(
            "qaqh-permission-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .expect("system clock before Unix epoch")
                .as_nanos()
        );
        let root = std::env::temp_dir().join(unique);
        let workspace = root.join("workspace");
        let outside = root.join("outside");
        let link = workspace.join("external-link");
        std::fs::create_dir_all(&workspace).expect("create workspace");
        std::fs::create_dir_all(&outside).expect("create outside directory");

        #[cfg(windows)]
        {
            let status = std::process::Command::new("cmd")
                .args(["/C", "mklink", "/J"])
                .arg(&link)
                .arg(&outside)
                .status()
                .expect("create directory junction");
            assert!(status.success(), "mklink /J failed: {status}");
        }
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, &link).expect("create directory symlink");

        (
            LinkedTempTree {
                root,
                link: link.clone(),
            },
            workspace,
            link.join("new.txt"),
        )
    }

    #[test]
    fn from_u8_is_exhaustively_fail_closed() {
        // BUG-2026-09-13-15：全值域逐一断言——任何非法档位都不得解析为
        // skip-permissions(免审批),必须保守降级 read-only。
        for raw in 0u8..=255 {
            let level = PermissionLevel::from_u8(raw);
            match raw {
                1..=3 => {
                    assert_eq!(level.to_u8(), raw, "legal tier {raw} must map to itself");
                    assert!(PermissionLevel::is_valid_u8(raw));
                    assert_eq!(PermissionLevel::try_from_u8(raw), Ok(level));
                }
                invalid => {
                    assert_eq!(
                        level,
                        PermissionLevel::ReadOnly,
                        "illegal tier {invalid} must degrade to read-only, not fail open"
                    );
                    assert!(!PermissionLevel::is_valid_u8(invalid));
                    assert!(
                        PermissionLevel::try_from_u8(invalid).is_err(),
                        "illegal level {invalid} must be rejected by the strict parser"
                    );
                }
            }
        }
    }

    #[test]
    fn ask_user_does_not_open_a_second_permission_dialog() {
        let decision = needs_permission(
            PermissionLevel::ReadOnly,
            "ask",
            &serde_json::json!({"question":"Continue?"}),
            Path::new("."),
            &HashSet::new(),
            ToolCategory::Read,
        );

        assert!(matches!(decision, PermissionDecision::AutoApprove));
    }

    #[test]
    fn session_todo_operations_never_open_permission_dialogs() {
        for level in [
            PermissionLevel::ReadOnly,
            PermissionLevel::WorkspaceWrite,
            PermissionLevel::SkipPermissions,
        ] {
            let tool_name = "todo_write";
            {
                let decision = needs_permission(
                    level,
                    tool_name,
                    &serde_json::json!({"items": [{"title": "t"}]}),
                    Path::new("."),
                    &HashSet::new(),
                    ToolCategory::Write,
                );
                assert!(
                    matches!(decision, PermissionDecision::AutoApprove),
                    "{tool_name} should be auto-approved at level {}",
                    level.to_u8()
                );
            }
        }
    }

    #[test]
    fn permission_risk_distinguishes_read_workspace_write_and_exec() {
        let workspace = resolve_target_path(PathBuf::from("C:/repo"));

        assert_eq!(
            classify_risk(ToolCategory::Read, &[], &workspace),
            PermissionRisk::Low
        );
        assert_eq!(
            classify_risk(
                ToolCategory::Write,
                &[workspace.join("src/lib.rs")],
                &workspace,
            ),
            PermissionRisk::Medium
        );
        assert_eq!(
            classify_risk(ToolCategory::Exec, &[], &workspace),
            PermissionRisk::High
        );
        assert_eq!(
            classify_risk(
                ToolCategory::Write,
                &[resolve_target_path(PathBuf::from("C:/outside/file"))],
                &workspace,
            ),
            PermissionRisk::High
        );
    }

    #[test]
    fn workspace_free_requires_approval_for_missing_file_beneath_external_link() {
        let (_temp, workspace, target) = linked_temp_tree();

        let decision = needs_permission(
            PermissionLevel::WorkspaceWrite,
            "write",
            &serde_json::json!({"path": target}),
            &workspace,
            &HashSet::new(),
            ToolCategory::Write,
        );

        assert!(
            matches!(decision, PermissionDecision::AskUser { .. }),
            "a missing file beneath an external directory link must not auto-approve"
        );
    }

    #[test]
    fn workspace_free_requires_approval_for_missing_file_after_parent_traversal() {
        let (_temp, workspace, _) = linked_temp_tree();
        let target = workspace
            .join("missing")
            .join("..")
            .join("..")
            .join("outside")
            .join("new.txt");

        let decision = needs_permission(
            PermissionLevel::WorkspaceWrite,
            "write",
            &serde_json::json!({"path": target}),
            &workspace,
            &HashSet::new(),
            ToolCategory::Write,
        );

        assert!(
            matches!(decision, PermissionDecision::AskUser { .. }),
            "parent traversal to a missing file outside the workspace must not auto-approve"
        );
    }

    #[test]
    fn sensitive_session_files_require_approval_even_at_skip_permissions() {
        // messages.jsonl / meta.json live outside any workspace but under
        // SkipPermissions they'd otherwise auto-approve. This must be forced to
        // AskUser to avoid
        // the model silently reading prior turns and feeding them into the gateway.
        //
        // 路径取自权威数据根（`QAQH_DATA_DIR` 或 `~/.qaqh`），不用手抄的
        // `/home/test/.config/qaqh/…` 字面量——那只能命中文件名启发式，
        // 换机器/换数据根就与真实判定脱钩。
        let sessions = qaqh_types::platform::sessions_dir();
        let data = qaqh_types::platform::data_dir();
        if !sessions.is_absolute() || !data.is_absolute() {
            return; // 数据根不可定界：权威判定无法构造
        }
        let ws = std::env::temp_dir().join("qaqh-ws-sensitive");
        for path in [
            sessions.join("abc").join("messages.jsonl"),
            sessions.join("abc").join("meta.json"),
            data.join("token_stats.jsonl"),
            data.join("secrets.toml"),
        ] {
            let decision = needs_permission(
                PermissionLevel::SkipPermissions,
                "read",
                &serde_json::json!({"path": path.display().to_string()}),
                &ws,
                &HashSet::new(),
                ToolCategory::Read,
            );
            assert!(
                matches!(decision, PermissionDecision::AskUser { .. }),
                "sensitive path {path:?} must require approval even under SkipPermissions, got {decision:?}"
            );
        }
        // Normal workspace file under SkipPermissions still auto-approves.
        let normal = needs_permission(
            PermissionLevel::SkipPermissions,
            "read",
            &serde_json::json!({"path": ws.join("src/main.rs")}),
            &ws,
            &HashSet::new(),
            ToolCategory::Read,
        );
        assert!(matches!(normal, PermissionDecision::AutoApprove));
    }

    #[test]
    fn foreign_sessions_dir_is_not_own_session_history() {
        // 回归 2026-10-08：文件名子串名单曾对**任意**路径生效，第三方产品里名为
        // `…\qodercli\sessions\<id>.log` 的日志目录、以及仓库里任何 `meta.json`，
        // 都被判成自家会话历史 → grep 在 skip-permissions 下仍弹审批
        // （`~/.qaqh/audit.csv` 实证 3 例 user_approved）。名单尾巴现已限定在
        // 数据根子树内，工作区外的普通读取在任何档位都放行。
        let sessions = qaqh_types::platform::sessions_dir();
        if !sessions.is_absolute() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("ws");
        std::fs::create_dir_all(ws.join("src")).unwrap();
        let ws = std::fs::canonicalize(&ws).unwrap();
        let foreign = dir
            .path()
            .join("Roaming")
            .join("qodercli")
            .join("sessions")
            .join("08fce531-de22.log");
        std::fs::create_dir_all(foreign.parent().unwrap()).unwrap();
        std::fs::write(&foreign, "x").unwrap();
        for path in [foreign, ws.join("src").join("meta.json")] {
            assert!(
                !is_sensitive_session_path(&path),
                "{path:?} is outside the data root and must not be treated as session history"
            );
            for level in [
                PermissionLevel::ReadOnly,
                PermissionLevel::WorkspaceWrite,
                PermissionLevel::SkipPermissions,
            ] {
                let decision = needs_permission(
                    level,
                    "grep",
                    &serde_json::json!({"paths": [path.display().to_string()]}),
                    &ws,
                    &HashSet::new(),
                    ToolCategory::Read,
                );
                assert!(
                    matches!(decision, PermissionDecision::AutoApprove),
                    "grep at L{} must not ask for {path:?}, got {decision:?}",
                    level.to_u8()
                );
            }
        }
    }

    #[test]
    fn skill_directory_writes_require_approval_at_every_level() {
        // 审计 H2：skill 发现根下的文件 = 权威指令注入面。默认档 L3 的工作区
        // 内写自动放行对它们不适用，L4 也不得静默——文件落盘即跨会话持久。
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let ws = std::fs::canonicalize(&ws).unwrap();
        let skill_file = ws.join(".qaqh/skills/helper/SKILL.md");
        for level in [
            PermissionLevel::ReadOnly,
            PermissionLevel::WorkspaceWrite,
            PermissionLevel::SkipPermissions,
        ] {
            let decision = needs_permission(
                level,
                "write",
                &serde_json::json!({ "path": skill_file.display().to_string() }),
                &ws,
                &HashSet::new(),
                ToolCategory::Write,
            );
            assert!(
                matches!(decision, PermissionDecision::AskUser { .. }),
                "skill write at L{} must ask, got {decision:?}",
                level.to_u8()
            );
        }
        // 旧式 `skills/` 根与不存在的深层目录（新建文件场景）同样命中。
        let fresh = ws.join("skills/new/helper/SKILL.md");
        let decision = needs_permission(
            PermissionLevel::WorkspaceWrite,
            "apply_patch",
            &serde_json::json!({ "patch": format!("*** Add File: {}\n+hi", fresh.display()) }),
            &ws,
            &HashSet::new(),
            ToolCategory::Write,
        );
        assert!(
            matches!(decision, PermissionDecision::AskUser { .. }),
            "new skill file in a not-yet-existing root must ask, got {decision:?}"
        );
        // 工作区普通文件不受影响（L3 仍自动放行）。
        let normal = needs_permission(
            PermissionLevel::WorkspaceWrite,
            "write",
            &serde_json::json!({ "path": ws.join("src/main.rs").display().to_string() }),
            &ws,
            &HashSet::new(),
            ToolCategory::Write,
        );
        assert!(
            matches!(normal, PermissionDecision::AutoApprove),
            "ordinary workspace write must stay auto-approved, got {normal:?}"
        );
    }

    #[test]
    fn user_config_toml_is_sensitive_but_lookalikes_are_not() {
        // 审计 M4：平台数据根下的 config.toml 在任何档位都要弹审批
        // （L4 下可固化提权 + 改 base_url 劫持 LLM 流量，且热加载免重启生效）。
        let config = qaqh_types::platform::config_path();
        if !config.is_absolute() {
            // 测试环境重定向了数据根且非绝对路径：组件级判定无法构造，跳过。
            return;
        }
        assert!(
            is_sensitive_session_path(&config),
            "user-dir config.toml must be sensitive"
        );
        // 组件级判定不外溢：别处的同名/含名文件不受牵连。
        for path in [
            std::path::Path::new("C:/elsewhere/config.toml"),
            std::path::Path::new("/opt/project/myconfig.toml"),
        ] {
            assert!(
                !is_sensitive_session_path(path),
                "{path:?} must not be sensitive"
            );
        }
    }

    #[test]
    fn workspace_free_requires_approval_for_exec_and_network() {
        for (tool, category) in [
            ("exec", ToolCategory::Exec),
            ("spawn_subagent", ToolCategory::Exec),
            ("web_fetch", ToolCategory::Net),
        ] {
            let decision = needs_permission(
                PermissionLevel::WorkspaceWrite,
                tool,
                &serde_json::json!({}),
                Path::new("."),
                &HashSet::new(),
                category,
            );
            assert!(
                matches!(decision, PermissionDecision::AskUser { .. }),
                "workspace-write must ask before {tool}"
            );
        }
    }

    #[test]
    fn skip_permissions_auto_approves_execution_and_network() {
        for (tool, category) in [
            ("exec", ToolCategory::Exec),
            ("spawn_subagent", ToolCategory::Exec),
            ("web_fetch", ToolCategory::Net),
        ] {
            let decision = needs_permission(
                PermissionLevel::SkipPermissions,
                tool,
                &serde_json::json!({}),
                Path::new("."),
                &HashSet::new(),
                category,
            );
            assert!(
                matches!(decision, PermissionDecision::AutoApprove),
                "SkipPermissions bypass must auto-approve {tool}"
            );
        }
    }

    #[test]
    fn skip_permissions_keeps_reads_and_writes_frictionless() {
        for (tool, category) in [
            ("read", ToolCategory::Read),
            ("write", ToolCategory::Write),
            ("edit", ToolCategory::Write),
        ] {
            let decision = needs_permission(
                PermissionLevel::SkipPermissions,
                tool,
                &serde_json::json!({"path": "src/lib.rs"}),
                Path::new("."),
                &HashSet::new(),
                category,
            );
            assert!(
                matches!(decision, PermissionDecision::AutoApprove),
                "SkipPermissions must keep {tool} auto-approved"
            );
        }
    }
}

#[cfg(test)]
mod w3_w7_tests {
    use super::*;

    #[test]
    fn shell_cwd_and_patch_targets_enter_authorization_resources() {
        let _serial = crate::TEST_RUNTIME_SERIAL
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        let tmp = tempfile::tempdir().unwrap();
        let ws = tmp.path().join("ws");
        std::fs::create_dir_all(ws.join("src")).unwrap();
        let old = crate::current_workspace();
        crate::set_workspace(ws.to_str().unwrap());
        let ws_canon = std::fs::canonicalize(&ws).unwrap_or_else(|_| ws.clone());
        let inside_ws = |p: &std::path::Path| p.starts_with(&ws_canon) || p.starts_with(&ws);

        // exec 独占后唯一命令入口：cwd 必须进授权资源。
        let res = extract_target_paths(
            "exec",
            &serde_json::json!({ "command": "ls", "cwd": "./src" }),
        );
        assert_eq!(res.len(), 1, "exec cwd missing from resources: {res:?}");
        assert!(inside_ws(&res[0]), "exec cwd not under ws: {:?}", res[0]);

        // W3：apply_patch 目标从 patch 文本解析进授权资源（两条目标）。
        let res2 = extract_target_paths(
            "apply_patch",
            &serde_json::json!({"patch": "*** Begin Patch\n*** Update File: src/a.rs\n@@\n-old\n+new\n*** Add File: src/new.rs\n+seed\n*** End Patch"}),
        );
        assert_eq!(res2.len(), 2, "patch targets missing: {res2:?}");

        // W7：相对路径以工作区根为基准（与执行侧一致），不再绑进程 cwd。
        for p in res2.iter().chain(res.iter()) {
            assert!(inside_ws(p), "resource not under ws: {:?}", p);
        }

        crate::set_workspace(&old);
    }

    #[test]
    fn patch_target_paths_parses_codex_headers() {
        let patch = "*** Begin Patch\n*** Update File: a.rs\n*** Delete File: b.rs\n*** Move to: c.rs\nnot-a-header: d.rs\n*** End Patch";
        assert_eq!(patch_target_paths(patch), vec!["a.rs", "b.rs", "c.rs"]);
        assert!(patch_target_paths("no headers here").is_empty());
    }

    #[test]
    fn exec_approval_summary_flags_unenforced_sandbox() {
        // 审计 H1：无强制后端的平台上，审批摘要必须带「无沙箱」警告，
        // 用户不得带着「命令在沙箱内」的错觉放行 exec。
        let summary = summarize_permission_action(
            "exec",
            &serde_json::json!({ "command": "ls", "cwd": "/repo" }),
        )
        .expect("exec summary");
        if qaqh_sandbox::filesystem_and_network_isolation_enforced() {
            assert!(!summary.contains("WARNING"), "{summary}");
        } else {
            assert!(
                summary.contains("sandbox not enforced"),
                "summary must warn about unenforced sandbox: {summary}"
            );
        }
    }

    #[test]
    fn exec_permission_action_summary_shows_command_without_env() {
        let summary = summarize_permission_action(
            "exec",
            &serde_json::json!({
                "command": "cargo test",
                "args": ["--all", "name with spaces"],
                "shell": "bash",
                "cwd": "/repo",
                "env": {"API_KEY": "must-not-leak"}
            }),
        )
        .expect("exec summary");

        assert!(summary.contains(r#"command: "cargo test""#), "{summary}");
        assert!(
            summary.contains(r#"args: ["--all", "name with spaces"]"#),
            "{summary}"
        );
        assert!(summary.contains(r#"shell: "bash""#), "{summary}");
        assert!(summary.contains(r#"cwd: "/repo""#), "{summary}");
        assert!(!summary.contains("must-not-leak"), "{summary}");
        assert_eq!(
            summarize_permission_action("read", &serde_json::json!({})),
            None
        );
    }

    #[test]
    fn exec_permission_action_summary_uses_command_and_is_bounded() {
        let command = "echo ".repeat(3000);
        let summary = summarize_permission_action(
            "exec",
            &serde_json::json!({ "command": command, "cwd": "/repo" }),
        )
        .expect("command summary");
        assert!(summary.starts_with("command: \"echo echo"), "{summary}");
        assert!(summary.ends_with('…'), "summary must be visibly truncated");
        assert_eq!(summary.chars().count(), 4096);
    }

    #[test]
    fn read_outside_workspace_auto_approves_at_every_tier() {
        // 2026-10-05 读自由规则（取代旧 W3/W7「工作区外读弹审批」契约）：
        // 读取完全不受工作区限制——read/grep/glob 等在任意档位、任意路径下
        // 都自动放行；敏感路径守卫（会话文件/凭据）在 needs_permission 前段
        // 独立拦截，不受本规则影响。子代理沙箱的跨 workspace 拒读在 admit
        // 层兜底（见 authorization.rs 测试）。
        let dir = tempfile::tempdir().unwrap();
        let ws = dir.path().join("ws");
        std::fs::create_dir_all(&ws).unwrap();
        let inside = ws.join("inside.txt");
        std::fs::write(&inside, "x").unwrap();
        let outside = dir.path().join("outside.txt");
        std::fs::write(&outside, "x").unwrap();
        let ws = std::fs::canonicalize(&ws).unwrap();

        for level in [
            PermissionLevel::ReadOnly,
            PermissionLevel::WorkspaceWrite,
            PermissionLevel::SkipPermissions,
        ] {
            let inside_decision = needs_permission(
                level,
                "read",
                &serde_json::json!({"path": inside.clone()}),
                &ws,
                &HashSet::new(),
                ToolCategory::Read,
            );
            assert!(
                matches!(inside_decision, PermissionDecision::AutoApprove),
                "inside read must auto-approve at L{}",
                level.to_u8()
            );
            let outside_decision = needs_permission(
                level,
                "read",
                &serde_json::json!({"path": outside.clone()}),
                &ws,
                &HashSet::new(),
                ToolCategory::Read,
            );
            assert!(
                matches!(outside_decision, PermissionDecision::AutoApprove),
                "outside read must auto-approve at L{} (reads are workspace-free)",
                level.to_u8()
            );
        }
    }

    /// 回归（Windows）：纯根信任条目不得被 `canonicalize` 折成当前盘根。
    /// `canonicalize("/")` 在 Windows 得到 `C:\`（当前盘），会让 `/` 这样的
    /// 分隔符垃圾条目逃过 `trimmed_key` 的 fail-open 守卫 → 全盘 AutoApprove。
    #[test]
    fn root_only_trust_entry_is_not_resolved_to_current_drive_root() {
        let _serial = crate::TEST_RUNTIME_SERIAL
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        for bogus in ["/", "\\", "C:\\"] {
            let resolved = resolve_target_path(PathBuf::from(bogus));
            assert!(
                !resolved
                    .components()
                    .any(|c| matches!(c, std::path::Component::Normal(_))),
                "root-only entry {bogus:?} must stay root-only, got {resolved:?}"
            );
        }
        // 真实子路径仍走 canonicalize（存在性解析语义不变）。
        let tmp = tempfile::tempdir().unwrap();
        let resolved = resolve_target_path(tmp.path().to_path_buf());
        assert_eq!(
            resolved
                .components()
                .any(|c| matches!(c, std::path::Component::Normal(_))),
            true,
            "a real directory must still resolve"
        );
    }
}
