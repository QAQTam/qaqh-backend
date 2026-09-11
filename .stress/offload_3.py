import io

def patch(path, pairs):
    with io.open(path, "r", encoding="utf-8") as f:
        src = f.read()
    for old, new, label in pairs:
        count = src.count(old)
        assert count == 1, f"[{label}] expected 1, found {count}\n---\n{old[:200]}"
        src = src.replace(old, new, 1)
        print(f"[ok] {label}")
    with io.open(path, "w", encoding="utf-8", newline="") as f:
        f.write(src)

# ============ timeline_store.rs: offload 侧车读写 ============
TS = r"D:\project\QAQ-Harness\crates\qaqh-runtime\src\timeline_store.rs"
patch(TS, [
(
"""    pub fn persist(
        &self,
        seed: &str,
        snapshot: &TimelineSnapshot,
        journal: Vec<TimelineEntry>,
    ) -> std::io::Result<()> {""",
"""    /// offload 侧车路径：`offload/{seed}.jsonl`，append-only（每行一个
    /// 已 seal turn 的完整 TimelineTurn JSON）。append 语义 O(文本) 无放大；
    /// 同 turn 重 seal（reopen）时后行胜（读侧取该 turn_id 最后一条）。
    fn offload_path_for(&self, seed: &str) -> PathBuf {
        self.root
            .parent()
            .unwrap_or(&self.root)
            .join("ringing-offload")
            .join(format!("{}.jsonl", sanitize_seed(seed)))
    }

    /// turn seal 卸载：把完整 turn 文本追加进侧车。
    /// I/O 失败仅记日志——卸载是内存优化，绝不阻塞事件路径。
    pub fn append_offloaded_turn(&self, seed: &str, turn: &qaqh_domain::TimelineTurn) {
        use std::io::Write;
        let path = self.offload_path_for(seed);
        if let Some(parent) = path.parent() {
            if let Err(error) = std::fs::create_dir_all(parent) {
                log::warn!("[timeline] offload dir create failed for {seed}: {error}");
                return;
            }
        }
        let line = match serde_json::to_string(turn) {
            Ok(line) => line,
            Err(error) => {
                log::warn!("[timeline] offload serialize failed for {seed}: {error}");
                return;
            }
        };
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path);
        let mut file = match file {
            Ok(file) => file,
            Err(error) => {
                log::warn!("[timeline] offload open failed for {seed}: {error}");
                return;
            }
        };
        if let Err(error) = writeln!(file, "{line}")
            .and_then(|_| file.flush())
        {
            log::warn!("[timeline] offload append failed for {seed}: {error}");
        }
    }

    /// 读取某 turn 的最新完整文本（侧车同 turn_id 后行胜）。
    pub fn load_offloaded_turn(
        &self,
        seed: &str,
        turn_id: &str,
    ) -> Option<qaqh_domain::TimelineTurn> {
        let path = self.offload_path_for(seed);
        let body = std::fs::read_to_string(path).ok()?;
        let mut latest: Option<qaqh_domain::TimelineTurn> = None;
        for line in body.lines() {
            if line.trim().is_empty() {
                continue;
            }
            if let Ok(turn) = serde_json::from_str::<qaqh_domain::TimelineTurn>(line)
                && turn.turn_id == turn_id
            {
                latest = Some(turn);
            }
        }
        latest
    }

    pub fn persist(
        &self,
        seed: &str,
        snapshot: &TimelineSnapshot,
        journal: Vec<TimelineEntry>,
    ) -> std::io::Result<()> {""",
"timeline_store offload sidecar"),
])
print("[done] store sidecar")
