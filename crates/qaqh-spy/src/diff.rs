//! 非对称上下文的 unified diff（变更前 `before` 行、后 `after` 行）。
//!
//! similar 自带的 `UnifiedDiff::context_radius` 和 git 的 `-U` 都只能生成对称
//! 上下文，而需求是 5 前 / 3 后，所以基于 `DiffOp` 手写 hunk 切分：
//! 变更区 → 外扩上下文 → 重叠合并 → 逐 hunk 渲染。

use similar::{ChangeTag, DiffTag, TextDiff};

/// 计算带非对称上下文的 unified diff；无差异时返回空串。
/// 不含 `---/+++` 文件头（由调用方按需拼接）；输出保证每行以 `\n` 结尾。
pub fn asymmetric_unified_diff(old: &str, new: &str, before: usize, after: usize) -> String {
    let diff = TextDiff::from_lines(old, new);
    let ops = diff.ops();

    // 1) 变更区（旧文件坐标 [start, end)）；相邻（中间没有 Equal 隔开）的直接合并。
    //    Insert 的旧区间为空，保留为 (p, p)，锚点是插入点。
    let mut regions: Vec<(usize, usize)> = Vec::new();
    for op in ops {
        if op.tag() == DiffTag::Equal {
            continue;
        }
        let r = op.old_range();
        match regions.last_mut() {
            Some(last) if last.1 >= r.start => last.1 = last.1.max(r.end),
            _ => regions.push((r.start, r.end)),
        }
    }
    if regions.is_empty() {
        return String::new();
    }

    // 2) 每个变更区向外扩上下文，重叠的再合并成 hunk
    let total_old = diff.old_len();
    let mut hunks: Vec<(usize, usize)> = Vec::new();
    for &(os, oe) in &regions {
        let s = os.saturating_sub(before);
        let e = (oe + after).min(total_old);
        match hunks.last_mut() {
            Some(last) if s <= last.1 => last.1 = last.1.max(e),
            _ => hunks.push((s, e)),
        }
    }

    // 3) 逐 hunk 渲染。归队规则：Equal 与 [hs, he) 求交；Delete/Replace 必然完整
    //    落在某个变更区内（也就完整落在 hunk 内）；Insert 以插入点锚定。
    let mut out = String::new();
    for &(hs, he) in &hunks {
        let mut body = String::new();
        let (mut old_cnt, mut new_cnt) = (0usize, 0usize);
        let mut new_start = 0usize;
        let mut seen = false;

        for op in ops {
            let or = op.old_range();
            let nr = op.new_range();
            match op.tag() {
                DiffTag::Equal => {
                    if or.end <= hs || or.start >= he {
                        continue;
                    }
                    let s = or.start.max(hs);
                    let e = or.end.min(he);
                    if !seen {
                        new_start = nr.start + (s - or.start);
                        seen = true;
                    }
                    for ch in diff.iter_changes(op) {
                        let i = ch.old_index().expect("Equal 变更必有旧索引");
                        if i >= s && i < e {
                            old_cnt += 1;
                            new_cnt += 1;
                            push_line(&mut body, ' ', ch.value());
                        }
                    }
                }
                DiffTag::Delete => {
                    if or.start < hs || or.end > he {
                        continue;
                    }
                    if !seen {
                        new_start = nr.start;
                        seen = true;
                    }
                    for ch in diff.iter_changes(op) {
                        old_cnt += 1;
                        push_line(&mut body, '-', ch.value());
                    }
                }
                DiffTag::Replace => {
                    if or.start < hs || or.end > he {
                        continue;
                    }
                    if !seen {
                        new_start = nr.start;
                        seen = true;
                    }
                    for ch in diff.iter_changes(op) {
                        match ch.tag() {
                            ChangeTag::Delete => {
                                old_cnt += 1;
                                push_line(&mut body, '-', ch.value());
                            }
                            ChangeTag::Insert => {
                                new_cnt += 1;
                                push_line(&mut body, '+', ch.value());
                            }
                            ChangeTag::Equal => unreachable!("Replace op 不会产生 Equal 变更"),
                        }
                    }
                }
                DiffTag::Insert => {
                    if or.start < hs || or.start > he {
                        continue;
                    }
                    if !seen {
                        new_start = nr.start;
                        seen = true;
                    }
                    for ch in diff.iter_changes(op) {
                        new_cnt += 1;
                        push_line(&mut body, '+', ch.value());
                    }
                }
            }
        }

        // unified 约定：count 为 0 时显示的起始行是“其后”的那一行
        let old_disp = if old_cnt == 0 { hs } else { hs + 1 };
        let new_disp = if new_cnt == 0 {
            new_start
        } else {
            new_start + 1
        };
        out.push_str(&format!(
            "@@ -{old_disp},{old_cnt} +{new_disp},{new_cnt} @@\n"
        ));
        out.push_str(&body);
    }
    out
}

fn push_line(buf: &mut String, prefix: char, value: &str) {
    buf.push(prefix);
    buf.push_str(value);
    if !value.ends_with('\n') {
        buf.push('\n');
    }
}

#[cfg(test)]
mod tests {
    use super::asymmetric_unified_diff;

    fn lines(range: std::ops::Range<usize>) -> String {
        range.map(|i| format!("line{i}\n")).collect()
    }

    #[test]
    fn context_is_five_before_and_three_after() {
        let old = lines(0..30);
        let mut v: Vec<String> = (0..30).map(|i| format!("line{i}\n")).collect();
        v[10] = "CHANGED\n".into();
        let new = v.join("");

        let d = asymmetric_unified_diff(&old, &new, 5, 3);

        assert!(d.starts_with("@@ -6,9 +6,9 @@"), "got: {d}");
        for i in 5..10 {
            assert!(d.contains(&format!(" line{i}\n")), "缺前文 line{i}");
        }
        assert!(d.contains("-line10\n"));
        assert!(d.contains("+CHANGED\n"));
        for i in 11..14 {
            assert!(d.contains(&format!(" line{i}\n")), "缺后文 line{i}");
        }
        assert!(!d.contains(" line4\n"));
        assert!(!d.contains(" line14\n"));
    }

    #[test]
    fn nearby_changes_merge_far_changes_split() {
        let old = lines(0..40);

        let mut v: Vec<String> = (0..40).map(|i| format!("line{i}\n")).collect();
        v[5] = "A\n".into();
        v[10] = "B\n".into();
        let d = asymmetric_unified_diff(&old, &v.join(""), 5, 3);
        assert_eq!(d.matches("@@ -").count(), 1, "间隔 4 行应合并: {d}");

        let mut v: Vec<String> = (0..40).map(|i| format!("line{i}\n")).collect();
        v[5] = "A\n".into();
        v[20] = "B\n".into();
        let d = asymmetric_unified_diff(&old, &v.join(""), 5, 3);
        assert_eq!(d.matches("@@ -").count(), 2, "间隔 14 行应切分: {d}");
        assert!(d.contains("@@ -1,9 +1,9 @@"));
        assert!(d.contains("@@ -16,9 +16,9 @@"));
    }

    #[test]
    fn insert_into_empty_file() {
        let d = asymmetric_unified_diff("", "a\nb\nc\n", 5, 3);
        assert!(d.starts_with("@@ -0,0 +1,3 @@"), "got: {d}");
        assert!(d.contains("+a\n") && d.contains("+b\n") && d.contains("+c\n"));
    }

    #[test]
    fn context_clamped_at_eof() {
        let old = lines(0..10);
        let new = lines(0..9); // 删掉最后一行 line9，后面不足 3 行，只到 EOF
        let d = asymmetric_unified_diff(&old, &new, 5, 3);
        assert!(d.contains("@@ -5,6 +5,5 @@"), "got: {d}");
        assert!(d.contains("-line9\n"));
    }

    #[test]
    fn no_difference_yields_empty() {
        let s = lines(0..10);
        assert!(asymmetric_unified_diff(&s, &s, 5, 3).is_empty());
    }
}
