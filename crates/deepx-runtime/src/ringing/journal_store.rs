//! 持久化 journal 存储（daemon 重启后可靠事件不丢）。
//!
//! 磁盘格式为 append-only JSONL：每条记录对应一次内存变更
//! （`Append`/`Checkpoint`/`Compact`），启动时按序重放可完整重建
//! `ReliableJournal`/`ChannelRouter`/`SnapshotProjector` 与 cutover 状态。
//! 有界语义由重放时的容量上限自然复现，与内存行为一致；磁盘增长是已知取舍
//! （RoundCompleted 压缩只追加一条 `Compact` 记录，不删除旧行）。
//! I/O 失败返回 Err，由调用方记录日志，绝不阻塞事件发布路径。

use std::collections::HashMap;
use std::fs::{File, OpenOptions};
use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};

use deepx_domain::RingingChannel;
use deepx_ringing::RingingEventEnvelope;
use serde::{Deserialize, Serialize};

type JournalKey = (RingingChannel, String);

/// 磁盘操作日志条目（按序重放）。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum JournalOp {
    /// reliable 追加或 replaceable 覆盖（按 `envelope.delivery` 重放）。
    Append { envelope: RingingEventEnvelope },
    /// replaceable 稀疏 checkpoint。
    Checkpoint { identity: String, stream_seq: u64 },
    /// RoundCompleted 到达后压缩该 round 的 delta 条目。
    Compact { turn_id: String, round_num: u32 },
}

/// 装载结果：每 (channel, seed) 的 op 序列 + cutover 状态。
#[derive(Debug, Default)]
pub struct LoadedJournal {
    pub per_seed: Vec<(RingingChannel, String, Vec<JournalOp>)>,
    pub cutover: Option<serde_json::Value>,
}

/// 持久化 journal 存储（root/journal/{channel}/{seed}.jsonl + root/cutover.json）。
#[derive(Debug)]
pub struct JournalStore {
    root: PathBuf,
    files: HashMap<JournalKey, File>,
}

impl JournalStore {
    /// 创建存储根目录。失败时由调用方降级为非持久模式。
    pub fn new(root: impl Into<PathBuf>) -> std::io::Result<Self> {
        let root = root.into();
        for channel in [
            RingingChannel::Control,
            RingingChannel::Conversation,
            RingingChannel::Tool,
        ] {
            std::fs::create_dir_all(root.join("journal").join(channel.as_str()))?;
        }
        Ok(Self {
            root,
            files: HashMap::new(),
        })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// 追加一次事件（reliable 或 replaceable 覆盖）。
    pub fn append(
        &mut self,
        channel: RingingChannel,
        seed: &str,
        envelope: &RingingEventEnvelope,
    ) -> std::io::Result<()> {
        self.write_line(
            channel,
            seed,
            &JournalOp::Append {
                envelope: envelope.clone(),
            },
        )
    }

    /// 记录 replaceable checkpoint。
    pub fn checkpoint(
        &mut self,
        channel: RingingChannel,
        seed: &str,
        identity: &str,
        stream_seq: u64,
    ) -> std::io::Result<()> {
        self.write_line(
            channel,
            seed,
            &JournalOp::Checkpoint {
                identity: identity.to_string(),
                stream_seq,
            },
        )
    }

    /// 记录 round delta 压缩。
    pub fn compact(
        &mut self,
        channel: RingingChannel,
        seed: &str,
        turn_id: &str,
        round_num: u32,
    ) -> std::io::Result<()> {
        self.write_line(
            channel,
            seed,
            &JournalOp::Compact {
                turn_id: turn_id.to_string(),
                round_num,
            },
        )
    }

    /// 装载磁盘日志（损坏行跳过并记录，不整体失败）。
    pub fn load(root: impl AsRef<Path>) -> std::io::Result<LoadedJournal> {
        let root = root.as_ref().to_path_buf();
        let mut out = LoadedJournal::default();
        let journal_root = root.join("journal");
        for channel in [
            RingingChannel::Control,
            RingingChannel::Conversation,
            RingingChannel::Tool,
        ] {
            let dir = journal_root.join(channel.as_str());
            if !dir.is_dir() {
                continue;
            }
            for entry in std::fs::read_dir(&dir)? {
                let entry = entry?;
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("jsonl") {
                    continue;
                }
                let seed = path
                    .file_stem()
                    .and_then(|s| s.to_str())
                    .map(str::to_string)
                    .unwrap_or_default();
                if seed.is_empty() {
                    continue;
                }
                let ops = read_ops(&path);
                if !ops.is_empty() {
                    out.per_seed.push((channel, seed, ops));
                }
            }
        }
        let cutover_path = root.join("cutover.json");
        if cutover_path.is_file() {
            match std::fs::read_to_string(&cutover_path) {
                Ok(text) => out.cutover = serde_json::from_str(&text).ok(),
                Err(error) => log::warn!("[ringing] read cutover.json failed: {error}"),
            }
        }
        Ok(out)
    }

    /// 持久化 cutover 状态（临时文件 + rename，避免半写）。
    pub fn save_cutover(&mut self, value: &serde_json::Value) -> std::io::Result<()> {
        let path = self.root.join("cutover.json");
        let tmp = self.root.join("cutover.json.tmp");
        std::fs::write(&tmp, serde_json::to_vec(value).map_err(io_error)?)?;
        std::fs::rename(&tmp, &path)
    }

    fn write_line(
        &mut self,
        channel: RingingChannel,
        seed: &str,
        op: &JournalOp,
    ) -> std::io::Result<()> {
        let file = self.file(channel, seed)?;
        let mut line = serde_json::to_vec(op).map_err(io_error)?;
        line.push(b'\n');
        file.write_all(&line)?;
        file.flush()
    }

    fn file(&mut self, channel: RingingChannel, seed: &str) -> std::io::Result<&mut File> {
        let key = (channel, seed.to_string());
        if !self.files.contains_key(&key) {
            let path = self.path_for(channel, seed);
            let file = OpenOptions::new().create(true).append(true).open(&path)?;
            self.files.insert(key.clone(), file);
        }
        Ok(self.files.get_mut(&key).expect("inserted above"))
    }

    fn path_for(&self, channel: RingingChannel, seed: &str) -> PathBuf {
        self.root
            .join("journal")
            .join(channel.as_str())
            .join(format!("{}.jsonl", sanitize_seed(seed)))
    }
}

fn read_ops(path: &Path) -> Vec<JournalOp> {
    let mut ops = Vec::new();
    let Ok(file) = File::open(path) else {
        return ops;
    };
    for line in BufReader::new(file).lines() {
        let Ok(line) = line else { break };
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        match serde_json::from_str::<JournalOp>(line) {
            Ok(op) => ops.push(op),
            Err(error) => log::warn!(
                "[ringing] skip corrupt journal line in {}: {error}",
                path.display()
            ),
        }
    }
    ops
}

/// seed 是十六进制会话标识；防御性净化防止路径穿越。
fn sanitize_seed(seed: &str) -> String {
    seed.chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '-' || c == '_' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

fn io_error(error: impl std::fmt::Display) -> std::io::Error {
    std::io::Error::new(std::io::ErrorKind::InvalidData, error.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;
    use deepx_domain::{ConversationEvent, DomainEvent, RoundDeltaKind};

    fn env(seq: u64, event_id: &str) -> RingingEventEnvelope {
        RingingEventEnvelope::new(
            "epoch",
            "s",
            seq,
            seq,
            seq,
            event_id,
            DomainEvent::Conversation(ConversationEvent::RoundDelta {
                turn_id: "t1".into(),
                round_num: 0,
                kind: RoundDeltaKind::Thinking,
                delta: "x".into(),
            })
            .into(),
        )
    }

    #[test]
    fn round_trip_reload_preserves_ops_in_order() {
        let root = std::env::temp_dir().join(format!(
            "deepx-ringing-store-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        {
            let mut store = JournalStore::new(&root).expect("create");
            store
                .append(RingingChannel::Conversation, "s", &env(1, "e1"))
                .expect("append 1");
            store
                .checkpoint(RingingChannel::Conversation, "s", "tool:c1", 1)
                .expect("checkpoint");
            store
                .append(RingingChannel::Conversation, "s", &env(2, "e2"))
                .expect("append 2");
            store
                .compact(RingingChannel::Conversation, "s", "t1", 0)
                .expect("compact");
            store
                .save_cutover(&serde_json::json!({ "modes": [] }))
                .expect("cutover");
        }
        let loaded = JournalStore::load(&root).expect("load");
        assert_eq!(loaded.per_seed.len(), 1);
        let (channel, seed, ops) = &loaded.per_seed[0];
        assert_eq!(*channel, RingingChannel::Conversation);
        assert_eq!(seed, "s");
        assert_eq!(ops.len(), 4);
        assert!(matches!(ops[0], JournalOp::Append { .. }));
        assert!(matches!(ops[1], JournalOp::Checkpoint { .. }));
        assert!(matches!(ops[2], JournalOp::Append { .. }));
        assert!(matches!(ops[3], JournalOp::Compact { .. }));
        assert!(loaded.cutover.is_some());
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn corrupt_lines_are_skipped() {
        let root = std::env::temp_dir().join(format!(
            "deepx-ringing-corrupt-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .unwrap()
                .as_nanos()
        ));
        {
            let mut store = JournalStore::new(&root).expect("create");
            store
                .append(RingingChannel::Control, "s", &env(1, "e1"))
                .expect("append");
            let path = store.path_for(RingingChannel::Control, "s");
            std::fs::OpenOptions::new()
                .append(true)
                .open(&path)
                .expect("open")
                .write_all(b"{broken}\n")
                .expect("write corrupt");
        }
        let loaded = JournalStore::load(&root).expect("load");
        let (_, _, ops) = &loaded.per_seed[0];
        assert_eq!(ops.len(), 1, "corrupt line skipped");
        let _ = std::fs::remove_dir_all(&root);
    }
}
