//! Bounded JSONL tail reads (Phase 2: 有界恢复，对标 Codex `ReverseJsonlScanner`).
//!
//! 设计契约：
//! - **只读尾部**：从文件末尾按 64 KB 块反向扫描，凑够 `max_lines` 条完整行
//!   即停——耗时/内存 O(尾部) 而非 O(文件大小)。1 GB 归档读最近 200 条
//!   依然只触碰最后几块。
//! - **跳过损坏行**：反向扫描对"行"的判定以 `\n` 为界，切点之前的残缺
//!   头部行直接丢弃（它与正向读的 torn-tail 容错语义对称：崩溃写一半的
//!   行不算完整消息）。
//! - **与正向读的语义对齐**：`read_messages_tail` 返回正向顺序的最近 N 条
//!   **可解析**消息；解析失败的行跳过（与
//!   `read_messages_without_deduplication` 的容错精神一致，但它是全量
//!   fail-closed，这里是尾部 best-effort——用于投影重建的降级路径，
//!   逐行跳过比整段拒绝更鲁棒）。
//!
//! 为什么不维护持久化字节偏移索引（如 Codex 的
//! `next_rollout_byte_offset`）：messages.jsonl 的消费端（timeline 重建）
//! 只需要"最近 N 条"，反向扫描无需任何持久化状态、无索引失效问题
//! （compact/rewrite 会改文件前部，偏移索引需要失效逻辑）。这是更小的
//! 复杂度换取同级的恢复界。

use std::io::{Read, Seek, SeekFrom};
use std::path::Path;

/// 反向扫描块大小（对标 Codex `READ_CHUNK_SIZE`）。
const CHUNK_SIZE: u64 = 64 * 1024;

/// 从 `path` 反向读取最近 `max_lines` 条完整行（正向顺序返回）。
///
/// 返回 `(lines, truncated)`：`truncated=true` 表示文件头部还有更多行
/// 未读。文件不存在 → `Ok((vec![], false))`。空行跳过（与正向读一致）。
pub fn read_last_lines(path: &Path, max_lines: usize) -> std::io::Result<(Vec<String>, bool)> {
    if max_lines == 0 {
        return Ok((Vec::new(), true));
    }
    let mut file = match std::fs::File::open(path) {
        Ok(file) => file,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {
            return Ok((Vec::new(), false));
        }
        Err(error) => return Err(error),
    };
    let file_len = file.metadata()?.len();
    if file_len == 0 {
        return Ok((Vec::new(), false));
    }

    // 反向累计的行缓冲：`pending` 是可能不完整的"块尾行"残余。
    let mut collected: Vec<Vec<u8>> = Vec::with_capacity(max_lines);
    let mut pending: Vec<u8> = Vec::new(); // 当前未闭合行的后缀（反序）
    let mut read_end = file_len;
    let mut truncated = false;

    while read_end > 0 {
        let chunk_start = read_end.saturating_sub(CHUNK_SIZE);
        let mut chunk = vec![0u8; (read_end - chunk_start) as usize];
        file.seek(SeekFrom::Start(chunk_start))?;
        file.read_exact(&mut chunk)?;

        // 从块尾向块头扫描换行符。
        let mut cursor = chunk.len();
        let mut carried = false; // else 分支已把 chunk[..cursor] 并入 pending。
        while cursor > 0 {
            let newline_at = chunk[..cursor]
                .iter()
                .rposition(|&byte| byte == b'\n')
                .map(|offset| chunk_start as usize + offset);
            let Some(absolute) = newline_at else {
                // 整块无换行：该段是未闭合行的前段，并入 pending 后继续读上一块。
                let mut extended = chunk[..cursor].to_vec();
                extended.extend_from_slice(&pending);
                pending = extended;
                carried = true;
                break;
            };
            let relative = absolute - chunk_start as usize;
            // 换行符之后、cursor 之前的段是该行的**前段**；`pending` 是跨块
            // 积累的该行**后段**（都保持正序）。拼接即得完整行。
            let mut line: Vec<u8> = chunk[relative + 1..cursor].to_vec();
            line.extend_from_slice(&pending);
            pending.clear();
            if !line.is_empty() {
                collected.push(line);
                if collected.len() >= max_lines {
                    // 已凑够；但只有当前面还有未读内容才算 truncated。
                    truncated = absolute > 0;
                    let mut lines: Vec<String> = collected
                        .into_iter()
                        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                        .collect();
                    lines.reverse();
                    return Ok((lines, truncated));
                }
            }
            cursor = relative;
            // read_end 语义收窄到当前 cursor（后续块的 pending 以此为界）。
        }
        // 本块处理完：若内层循环因“无换行”break，chunk[..cursor] 已并入
        // pending（carried）；否则 cursor==0 无残余。
        if cursor > 0 && !carried {
            let mut extended = chunk[..cursor].to_vec();
            extended.extend_from_slice(&pending);
            pending = extended;
        }
        read_end = chunk_start;
        if read_end == 0 && !pending.is_empty() {
            // 文件头：最后一段残余也是一条完整行（无前导换行，正序）。
            let line = std::mem::take(&mut pending);
            if !line.is_empty() {
                collected.push(line);
            }
        }
    }

    let mut lines: Vec<String> = collected
        .into_iter()
        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
        .collect();
    lines.reverse();
    Ok((lines, truncated))
}

/// 反向读取 messages.jsonl 的最近 `max_messages` 条**可解析**消息（正向序）。
///
/// 解析失败的行跳过（尾部 best-effort，见模块文档）。空行不计入。
pub fn read_messages_tail(path: &Path, max_messages: usize) -> Vec<qaqh_types::Message> {
    if max_messages == 0 {
        return Vec::new();
    }
    // 多读一些行以吸收损坏行：预期绝大多数行可解析，超读 25% + 16 行余量。
    let overread = max_messages + max_messages / 4 + 16;
    let mut messages = Vec::with_capacity(max_messages);
    // read_last_lines 一次给足 overread；若损坏行比例高则再补一轮。
    let (mut lines, truncated) = match read_last_lines(path, overread) {
        Ok(result) => result,
        Err(error) => {
            log::warn!(
                "[bounded-read] tail read failed for {}: {error}",
                path.display()
            );
            return Vec::new();
        }
    };
    // never_loop 修正（2026-09-12）：原 `loop` 的三条出边全是 break，实际等价于
    // 「最多补读一轮」。展开为顺序结构，行为逐字不变，同时解开 `just clippy` 门禁。
    parse_tail_into(&mut lines, &mut messages, max_messages);
    if messages.len() < max_messages && truncated {
        // 损坏行比例过高：扩窗重读一次（64× 上限，防病态文件无限循环）。
        let overread = overread
            .saturating_mul(2)
            .min(max_messages.saturating_mul(64));
        if let Ok((more, _)) = read_last_lines(path, overread) {
            lines = more;
            // 二次仍不足即接受现状（前端拿到部分历史优于无历史）。
            parse_tail_into(&mut lines, &mut messages, max_messages);
        }
    }
    // parse_tail_into 从最新往回收集（倒序）；恢复正向时间序。
    messages.reverse();
    messages.truncate(max_messages);
    messages
}

fn parse_tail_into(
    lines: &mut Vec<String>,
    messages: &mut Vec<qaqh_types::Message>,
    max_messages: usize,
) {
    for line in lines.drain(..).rev() {
        if messages.len() >= max_messages {
            break;
        }
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        match serde_json::from_str::<qaqh_types::Message>(trimmed) {
            Ok(message) => messages.push(message),
            Err(_) => continue,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn temp_file(label: &str, body: &str) -> std::path::PathBuf {
        let path = std::env::temp_dir().join(format!(
            "qaqh-bounded-read-{label}-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        std::fs::write(&path, body).unwrap();
        path
    }

    #[test]
    fn reads_last_n_lines_in_order() {
        let body = "one\ntwo\nthree\nfour\nfive\n";
        let path = temp_file("order", body);
        let (lines, truncated) = read_last_lines(&path, 3).unwrap();
        assert_eq!(lines, vec!["three", "four", "five"]);
        assert!(truncated);
        let (all, truncated) = read_last_lines(&path, 10).unwrap();
        assert_eq!(all.len(), 5);
        assert!(!truncated);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn handles_file_without_trailing_newline() {
        let body = "one\ntwo\nthree"; // 无尾换行
        let path = temp_file("no-trailing", body);
        let (lines, truncated) = read_last_lines(&path, 5).unwrap();
        assert_eq!(lines, vec!["one", "two", "three"]);
        assert!(!truncated);
        let (last_two, truncated) = read_last_lines(&path, 2).unwrap();
        assert_eq!(last_two, vec!["two", "three"]);
        assert!(truncated);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn skips_torn_head_of_first_block() {
        // 模拟崩溃残留：最后一行写了一半（无尾换行 + 非 JSON）。
        let body = "valid-1\nvalid-2\n{\"partial\": tru";
        let path = temp_file("torn", body);
        let (lines, truncated) = read_last_lines(&path, 10).unwrap();
        // 残缺行以无前导换行的文件头形态出现——被当作一行返回（调用方
        // 解析层负责跳过），但本测试锁定行为：不 panic、不丢完整行。
        assert!(lines.contains(&"valid-1".to_string()));
        assert!(lines.contains(&"valid-2".to_string()));
        assert!(!truncated);
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn large_file_only_touches_tail_blocks() {
        // 20 MB、每行 ~2 KB：反向读 10 行必须只触碰尾部块。
        let line = format!("{}\n", "x".repeat(2048));
        let body = line.repeat(10_000); // ~20 MB
        let path = temp_file("large", &body);
        let started = std::time::Instant::now();
        let (lines, truncated) = read_last_lines(&path, 10).unwrap();
        let elapsed = started.elapsed();
        assert_eq!(lines.len(), 10);
        assert!(truncated);
        // 反向扫描 10 行 ≈ 1-2 块（128 KB），不应读全量 20 MB。
        assert!(
            elapsed < std::time::Duration::from_millis(500),
            "tail read must stay bounded, took {elapsed:?}"
        );
        assert!(lines[0].starts_with('x'));
        let _ = std::fs::remove_file(&path);
    }

    #[test]
    fn empty_and_missing_files() {
        let missing = std::env::temp_dir().join("qaqh-bounded-read-missing");
        assert_eq!(
            read_last_lines(&missing, 5).unwrap(),
            (Vec::<String>::new(), false)
        );
        let empty = temp_file("empty", "");
        assert_eq!(
            read_last_lines(&empty, 5).unwrap(),
            (Vec::<String>::new(), false)
        );
        let _ = std::fs::remove_file(&empty);
    }
}
