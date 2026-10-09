//! deny-steer 反馈(spec §5.7):把内核拒绝翻译成模型可执行的导流指引。
//! 文案冻结;关键词启发式对齐 codex is_likely_sandbox_denied。

pub const DENIAL_FEEDBACK: &str = "[win-sbx] 写入受沙箱保护的路径被拒绝(内核 DACL 策略)。\n受保护的文件请改用受管控的文件工具(走权限审批),或先在 policy 的 writable_files / writable_roots 中声明目标。\n";

const KEYWORDS: &[&str] = &[
    "access is denied",
    "permission denied",
    "denied",
    "operation did not complete successfully",
    "拒绝访问",
    "权限不足",
];

/// 退出码非零且 stderr 命中拒绝特征 → 疑似沙箱拒绝(尽力而为,与 Linux 侧同构)。
pub fn is_likely_sandbox_denied(exit_code: u32, stderr_text: &str) -> bool {
    if exit_code == 0 {
        return false;
    }
    let hay = stderr_text.to_lowercase();
    KEYWORDS.iter().any(|k| hay.contains(k))
}

/// 从命令文本启发式提取疑似写目标(ADR 2026-10-09 v2a)。
///
/// 内核不回报被拒路径,这里只能从命令形态猜:重定向目标、写类 cmdlet 的
/// -Path/-OutFile 值、tee/cp/mv 的目标。结果仅供导流文案展示,可能漏报
/// 也可能误报,不作为授权依据。
pub fn candidate_write_targets(command: &str) -> Vec<String> {
    let words: Vec<String> = command.split_whitespace().map(str::to_string).collect();
    const WRITE_CMDLETS: &[&str] = &[
        "new-item",
        "set-content",
        "add-content",
        "out-file",
        "copy-item",
        "move-item",
    ];
    const COPY_BINARIES: &[&str] = &["cp", "mv", "copy", "move"];
    let mut targets: Vec<String> = Vec::new();
    let push_target = |raw: &str, targets: &mut Vec<String>| {
        let cleaned = raw.trim_matches(|ch| ch == '"' || ch == '\'').to_string();
        if cleaned.is_empty()
            || cleaned.starts_with('-')
            || cleaned.chars().all(|ch| ch.is_ascii_digit())
            || cleaned.eq_ignore_ascii_case("nul")
            || cleaned == "/dev/null"
        {
            return;
        }
        if !targets.contains(&cleaned) {
            targets.push(cleaned);
        }
    };
    for (idx, word) in words.iter().enumerate() {
        let lower = word.to_lowercase();
        let prev_lower = if idx > 0 {
            words[idx - 1].to_lowercase()
        } else {
            String::new()
        };
        // 重定向:独立算符(> >> 2> &>)取下一个词;黏连形态(>/path)取词尾
        if matches!(word.as_str(), ">" | ">>" | "2>" | "2>>" | "&>" | "&>>") {
            if let Some(next) = words.get(idx + 1) {
                push_target(next, &mut targets);
            }
            continue;
        }
        if word.starts_with('>') {
            let attached =
                word.trim_start_matches(|ch: char| ch.is_ascii_digit() || ch == '>' || ch == '&');
            if !attached.is_empty() && attached.len() < word.len() {
                push_target(attached, &mut targets);
            }
            continue;
        }
        // 写类 cmdlet 的目标 flag(-Path/-OutFile/-FilePath/-Destination)
        if lower.starts_with('-')
            && matches!(
                lower.as_str(),
                "-path" | "-outfile" | "-filepath" | "-destination" | "-literalpath"
            )
        {
            if let Some((_, value)) = lower.split_once('=') {
                push_target(value, &mut targets);
            } else if let Some(next) = words.get(idx + 1) {
                push_target(next, &mut targets);
            }
            continue;
        }
        // cmdlet 名直接跟裸目标(New-Item C:\x / Out-File out.txt / cp a b)
        let prev_prev = if idx > 1 {
            words[idx - 2].to_lowercase()
        } else {
            String::new()
        };
        if WRITE_CMDLETS.contains(&prev_lower.as_str())
            || prev_lower == "tee"
            || (COPY_BINARIES.contains(&prev_prev.as_str()) && idx + 1 == words.len())
        {
            push_target(word, &mut targets);
        }
    }
    targets.truncate(3);
    targets
}

/// 导流文案 + 疑似被拒写目标(v2a:模型在环,路径不准可自我纠正)。
pub fn denial_feedback_with_targets(command: &str) -> String {
    let targets = candidate_write_targets(command);
    if targets.is_empty() {
        return DENIAL_FEEDBACK.to_string();
    }
    format!(
        "{DENIAL_FEEDBACK}疑似被拒写目标(启发式提取,可能不准): {}\n请用 ask 工具向用户请求把目标加入可写范围,或改用受管控文件工具走权限审批。\n",
        targets.join("; ")
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keyword_hits() {
        assert!(is_likely_sandbox_denied(1, "Access is denied."));
        assert!(is_likely_sandbox_denied(1, "拒绝访问。"));
        assert!(!is_likely_sandbox_denied(0, "Access is denied."));
        assert!(!is_likely_sandbox_denied(1, "all good"));
    }

    #[test]
    fn candidates_cover_redirect_cmdlet_and_copy_forms() {
        assert_eq!(
            candidate_write_targets("echo x > C:\\outside\\f.txt"),
            vec!["C:\\outside\\f.txt"]
        );
        assert_eq!(
            candidate_write_targets("rg foo > out.txt 2> err.txt"),
            vec!["out.txt", "err.txt"]
        );
        assert_eq!(
            candidate_write_targets("New-Item -Path D:\\x -ItemType File"),
            vec!["D:\\x"]
        );
        assert_eq!(
            candidate_write_targets("Set-Content -OutFile out.log ..."),
            vec!["out.log"]
        );
        assert_eq!(
            candidate_write_targets("cat a | tee /etc/x"),
            vec!["/etc/x"]
        );
        assert_eq!(candidate_write_targets("cp a.txt b.txt"), vec!["b.txt"]);
    }

    #[test]
    fn candidates_skip_flags_devnull_and_dup_fds() {
        assert!(candidate_write_targets("rg x 2>&1").is_empty());
        assert!(candidate_write_targets("rg x > /dev/null").is_empty());
        assert!(candidate_write_targets("rg x > nul").is_empty());
        assert!(candidate_write_targets("git log --oneline").is_empty());
    }

    #[test]
    fn guidance_appends_targets_only_when_present() {
        assert_eq!(denial_feedback_with_targets("git log"), DENIAL_FEEDBACK);
        let with_targets = denial_feedback_with_targets("echo x > C:\\outside\\f.txt");
        assert!(with_targets.starts_with(DENIAL_FEEDBACK));
        assert!(with_targets.contains("C:\\outside\\f.txt"));
        assert!(with_targets.contains("ask"));
    }
}
