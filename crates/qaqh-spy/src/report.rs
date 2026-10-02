//! 工具边界报告：把 mark 之后的净变更渲染成限额文本——
//! 危险启发式置顶 ⚠、每文件一行 stat、最可疑的前几个文件带 5 前/3 后 diff 全文、
//! 超限折叠。文本由调用方决定去向（qaqh 投影进工具结果 / CLI 包成 hook JSON）。

use std::collections::BTreeMap;

use anyhow::Result;

use crate::diff::asymmetric_unified_diff;
use crate::store::{Change, ChangeStatus, Store};

/// 最多展开 diff 全文的文件数（其余折叠成一行 stat）
const MAX_FULL_DIFFS: usize = 3;

/// 为折叠/截断提示预留的字节数，保证提示总能出现在报告里
const NOTICE_RESERVE: usize = 160;

pub(crate) fn build_report(
    store: &Store,
    changes: &[Change],
    max_bytes: usize,
    ctx_before: usize,
    ctx_after: usize,
) -> Result<String> {
    // 1) 按 path 归并净变更：first.before → last.after
    struct Net {
        before: Option<String>,
        after: Option<String>,
        before_size: Option<u64>,
        after_size: Option<u64>,
    }
    let mut net: BTreeMap<String, Net> = BTreeMap::new();
    for c in changes {
        let e = net.entry(c.path.clone()).or_insert(Net {
            before: c.before.clone(),
            after: c.after.clone(),
            before_size: c.size_before,
            after_size: c.size_after,
        });
        e.after = c.after.clone();
        e.after_size = c.size_after;
    }

    // 2) 危险启发式 + 可疑度打分
    struct Item {
        path: String,
        status: ChangeStatus,
        net: Net,
        score: u32,
        warnings: Vec<String>,
    }
    let mut items: Vec<Item> = Vec::new();
    for (path, n) in net {
        if n.before.is_none() && n.after.is_none() {
            continue; // 净效果为零（新增后又删了）
        }
        let status = match (&n.before, &n.after) {
            (None, Some(_)) => ChangeStatus::Added,
            (Some(_), None) => ChangeStatus::Deleted,
            _ => ChangeStatus::Modified,
        };
        let mut score = match status {
            ChangeStatus::Deleted => 50,
            ChangeStatus::Added => 10,
            ChangeStatus::Modified => 20,
        };
        let mut warnings: Vec<String> = Vec::new();
        if let Some(after) = &n.after {
            let a = store.read_blob(after)?;
            let original = n
                .before_size
                .map(fmt_size)
                .unwrap_or_else(|| "空".to_string());
            if a.is_empty() {
                warnings.push(format!("已变为空文件（原 {original}）"));
                score = 100;
            } else if a.iter().all(|b| b.is_ascii_whitespace()) {
                warnings.push(format!("已变为纯空白文件（原 {original}）"));
                score = 100;
            }
            if let (Some(bs), Some(asz)) = (n.before_size, n.after_size)
                && bs >= 1024
                && asz * 5 < bs
                && score < 100
            {
                warnings.push(format!("体积骤缩 {} → {}", fmt_size(bs), fmt_size(asz)));
                score = score.max(80);
            }
            if let Some(before) = &n.before {
                let b = store.read_blob(before)?;
                let text_b = std::str::from_utf8(&b).is_ok() && !b.contains(&0u8);
                let text_a = std::str::from_utf8(&a).is_ok() && !a.contains(&0u8);
                if text_b && !text_a {
                    warnings.push("编码损坏：变更前是有效文本，变更后不是".to_string());
                    score = score.max(90);
                }
            }
        }
        items.push(Item {
            path,
            status,
            net: n,
            score,
            warnings,
        });
    }
    items.sort_by(|x, y| {
        y.score
            .cmp(&x.score)
            .then_with(|| {
                let dx = x
                    .net
                    .before_size
                    .unwrap_or(0)
                    .abs_diff(x.net.after_size.unwrap_or(0));
                let dy = y
                    .net
                    .before_size
                    .unwrap_or(0)
                    .abs_diff(y.net.after_size.unwrap_or(0));
                dy.cmp(&dx)
            })
            .then_with(|| x.path.cmp(&y.path))
    });

    // 3) 渲染
    let mut out = String::new();
    if items.is_empty() {
        return Ok(out);
    }
    out.push_str(&format!(
        "[workspace] 自标记以来 {} 个文件变更\n",
        items.len()
    ));
    let warned: Vec<&Item> = items.iter().filter(|i| !i.warnings.is_empty()).collect();
    if !warned.is_empty() {
        out.push_str(&format!(
            "[workspace] ⚠ 危险信号：{} 个文件可疑，脚本可能没按预期工作\n",
            warned.len()
        ));
        for i in &warned {
            out.push_str(&format!("  ⚠ {}: {}\n", i.path, i.warnings.join("；")));
        }
    }

    let mut full = 0usize;
    let mut collapsed = 0usize;
    for it in &items {
        let stat = format!(
            "{} ({}, {} → {})",
            it.path,
            status_zh(it.status),
            it.net
                .before_size
                .map(fmt_size)
                .unwrap_or_else(|| "-".into()),
            it.net
                .after_size
                .map(fmt_size)
                .unwrap_or_else(|| "-".into())
        );
        let want_full = !it.warnings.is_empty() || full < MAX_FULL_DIFFS;
        if !want_full {
            collapsed += 1;
            continue;
        }
        let (old, new) = match (&it.net.before, &it.net.after) {
            (Some(b), Some(a)) => (store.read_blob(b)?, store.read_blob(a)?),
            (Some(b), None) => (store.read_blob(b)?, Vec::new()),
            (None, Some(a)) => (Vec::new(), store.read_blob(a)?),
            _ => (Vec::new(), Vec::new()),
        };
        let text_old = std::str::from_utf8(&old).is_ok() && !old.contains(&0u8);
        let text_new = std::str::from_utf8(&new).is_ok() && !new.contains(&0u8);
        let section = if text_old && text_new {
            let d = asymmetric_unified_diff(
                &String::from_utf8_lossy(&old),
                &String::from_utf8_lossy(&new),
                ctx_before,
                ctx_after,
            );
            if d.is_empty() {
                format!("─── {stat}\n(内容无文本差异)\n")
            } else {
                format!("─── {stat}\n{d}")
            }
        } else {
            format!("─── {stat}\n(二进制内容，仅快照不出 diff)\n")
        };
        if out.len() + section.len() + NOTICE_RESERVE > max_bytes && it.warnings.is_empty() {
            collapsed += 1;
            continue;
        }
        out.push_str(&section);
        full += 1;
    }
    if collapsed > 0 {
        out.push_str(&format!(
            "…另有 {collapsed} 个文件未展开（journal query 查看完整流水）\n"
        ));
    }
    if out.len() > max_bytes {
        let mut cut = max_bytes;
        while cut > 0 && !out.is_char_boundary(cut) {
            cut -= 1;
        }
        out.truncate(cut);
        out.push_str("\n[workspace] 报告超限已截断\n");
    }
    Ok(out)
}

fn status_zh(s: ChangeStatus) -> &'static str {
    match s {
        ChangeStatus::Added => "新增",
        ChangeStatus::Modified => "修改",
        ChangeStatus::Deleted => "删除",
    }
}

fn fmt_size(n: u64) -> String {
    if n < 1024 {
        format!("{n}B")
    } else if n < 1024 * 1024 {
        format!("{:.1}KB", n as f64 / 1024.0)
    } else {
        format!("{:.1}MB", n as f64 / (1024.0 * 1024.0))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::store::tmp_dir;

    fn change(path: &str, before: Option<String>, after: Option<String>) -> Change {
        Change {
            id: "c".into(),
            scan: "s2".into(),
            ts: "t".into(),
            path: path.into(),
            status: match (&before, &after) {
                (None, Some(_)) => ChangeStatus::Added,
                (Some(_), None) => ChangeStatus::Deleted,
                _ => ChangeStatus::Modified,
            },
            before,
            after,
            size_before: None,
            size_after: None,
            trigger: "manual".into(),
        }
    }

    #[test]
    fn emptied_file_is_flagged_and_diffed() {
        let dir = tmp_dir("report");
        let store = Store::open(&dir, None).unwrap();
        let old = (1..=12).map(|i| format!("line{i}\n")).collect::<String>();
        let b_old = store.write_blob(old.as_bytes()).unwrap();
        let b_empty = store.write_blob(b"").unwrap();

        let changes = vec![change("app.py", Some(b_old), Some(b_empty))];
        let r = build_report(&store, &changes, 4096, 5, 3).unwrap();
        assert!(r.contains("⚠"), "应命中清空启发式:\n{r}");
        assert!(r.contains("app.py"));
        assert!(r.contains("@@"));
        assert!(r.contains("-line1"));
    }

    #[test]
    fn budget_folds_remaining_files() {
        let dir = tmp_dir("report-budget");
        let store = Store::open(&dir, None).unwrap();
        let mut changes = Vec::new();
        for i in 0..6 {
            let b = store
                .write_blob(format!("old {i}\nsecond\nthird\n").as_bytes())
                .unwrap();
            let a = store.write_blob(format!("new {i}\n").as_bytes()).unwrap();
            changes.push(change(&format!("f{i}.txt"), Some(b), Some(a)));
        }
        let r = build_report(&store, &changes, 300, 5, 3).unwrap();
        assert!(r.contains("未展开"), "应折叠剩余文件:\n{r}");
        assert!(r.len() < 900);
    }

    #[test]
    fn no_changes_yields_empty() {
        let dir = tmp_dir("report-empty");
        let store = Store::open(&dir, None).unwrap();
        let r = build_report(&store, &[], 4096, 5, 3).unwrap();
        assert!(r.is_empty());
    }
}
