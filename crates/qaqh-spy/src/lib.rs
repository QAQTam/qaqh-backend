//! qaqh-spy：工作区文件变动审计与紧急备份。
//!
//! 用法：**进程内库**。qaqh 在工具批边界做一次全量扫描——调用点位于
//! `qaqh-runtime/src/agent/workspace_audit.rs`：`begin` 调 `Session::scan`
//! 打边界，`finish` 调 `Session::changes_after_tool_end` 取净变更、**先**做宿主
//! 回填（SMJ + file_state 账本）、再调 `Session::render_report` 渲染限额报告。
//! 拆成两步是 PR3 的决定：报告里的回滚提示要用到回填产出的 SMJ 序号，而序号
//! 只能由宿主提供（见 `changes_after_tool_end` 方法说明）。
//! 设计见 `docs/plan-workspace-diff-injection.md`。
//!
//! 核心原则（源自 codespy DESIGN.md，已随本仓调整）：全量状态扫描是事实来源；
//! 存储在工作区外（CAS blob + journal + manifests）；报告限额 + 危险启发式。
//! 保留窗口 GC 随会话打开自动触发（`QAQH_SPY_KEEP_MANIFESTS` 可调）。
//!
//! 未从 codespy 迁入的部分：独立 CLI（clap）、`watch` 常驻兜底、`procmon` 命令归因
//! ——前两者属 M2+ 路线图，后者 M3 搁置；`qaqh-workspace` 只需库 API。

pub mod diff;
pub mod report;
pub mod scan;
pub mod store;

pub use diff::asymmetric_unified_diff;
pub use report::Report;
pub use scan::{ScanOpts, ScanOutcome};
pub use store::{Change, ChangeStatus, GcOutcome};

/// 按归属切分变更：`(可归因给本会话, 外部)`。
///
/// 两侧都算"可归因"的情形：条目 `owner` 为空（加字段之前的存量、或未带归属的
/// 外部扫描），以及宿主自己没有 session id（`owner` 传入空串）——**宁可多报，
/// 也不静默吞掉别人的真实变更**。只有"明确属于另一个会话"的条目才被压成计数。
pub fn split_by_owner(changes: &[Change], owner: &str) -> (Vec<Change>, Vec<Change>) {
    changes.iter().cloned().partition(|c| {
        // 空 owner（存量/无主）与无 session 的宿主都算可归因，见函数说明。
        c.owner.is_empty() || owner.is_empty() || c.owner == owner
    })
}

use std::collections::HashSet;
use std::path::{Path, PathBuf};

use anyhow::{Context, Result, bail};

use crate::store::{Store, atomic_write_bytes};

/// 启动 GC 的保留窗口（manifest 份数）。`QAQH_SPY_KEEP_MANIFESTS` 可覆盖，
/// 合法区间 [16, 100_000]，默认 256（≈128 个有变更的工具批）。
fn gc_keep() -> usize {
    std::env::var("QAQH_SPY_KEEP_MANIFESTS")
        .ok()
        .and_then(|v| v.trim().parse::<usize>().ok())
        .filter(|n| (16..=100_000).contains(n))
        .unwrap_or(256)
}

/// GC 的字节预算（blob 目录引用总量上限，第二道闸）。`QAQH_SPY_MAX_BYTES`
/// 可覆盖；**0 = 不限**（只用份数窗口）。默认 512 MiB；显式配置低于 1 MiB
/// 时抬到 1 MiB——预算过小会把窗口挤到只剩 1 份，undo 能力名存实亡。
fn gc_max_bytes() -> Option<u64> {
    const DEFAULT: u64 = 512 * 1024 * 1024;
    const FLOOR: u64 = 1024 * 1024;
    match std::env::var("QAQH_SPY_MAX_BYTES")
        .ok()
        .and_then(|v| v.trim().parse::<u64>().ok())
    {
        Some(0) => None,
        Some(n) => Some(n.max(FLOOR)),
        None => Some(DEFAULT),
    }
}

/// 一次扫描的唯一标识（manifest id）。库调用方用它划定工具调用的边界：
/// 执行前 `scan(ToolStart)`，执行后 `report_since(&mark, budget)`。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ScanId(pub String);

/// 扫描触发来源（写入 journal，审计时可区分）。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Trigger {
    /// 工具调用开始（打边界点）
    ToolStart,
    /// 工具调用结束（注入 diff 前扫描）
    ToolEnd,
    /// 常驻兜底的周期扫描
    Periodic,
    /// 人工 / CLI
    Manual,
}

impl Trigger {
    pub fn as_str(self) -> &'static str {
        match self {
            Trigger::ToolStart => "tool_start",
            Trigger::ToolEnd => "tool_end",
            Trigger::Periodic => "periodic",
            Trigger::Manual => "manual",
        }
    }

    pub fn parse(s: &str) -> Option<Trigger> {
        match s {
            "tool_start" => Some(Trigger::ToolStart),
            "tool_end" => Some(Trigger::ToolEnd),
            "periodic" => Some(Trigger::Periodic),
            "manual" => Some(Trigger::Manual),
            _ => None,
        }
    }
}

#[derive(Debug, Clone)]
pub struct UndoOutcome {
    /// restored（恢复被删文件）| removed（撤销新增）| reverted（还原修改）
    pub action: &'static str,
    pub path: String,
}

#[derive(Debug, Clone)]
pub struct RestoreOutcome {
    pub written: usize,
    pub skipped: usize,
    /// 当前存在但不在目标 manifest 里的文件
    pub extras: Vec<String>,
    pub pruned: usize,
}

/// 一个 agent 会话一个 `Session`。多会话共享同一 store（内部文件锁串行化扫描），
/// 边界（`ScanId`）由各会话自己持有，互不干扰。
pub struct Session {
    root: PathBuf,
    store: Store,
    opts: ScanOpts,
    ctx_before: usize,
    ctx_after: usize,
    /// 归属标识（宿主传 agent session id）。写进每条 `Change.owner`，
    /// 供 [`split_by_owner`] 把并发会话/子代理的变更与自己的分开。
    owner: String,
}

impl Session {
    /// 打开会话。`store_dir` 传 None 时用默认存储根（`QAQH_SPY_DIR` 或
    /// `platform::data_dir()`，其下仍有 `spy/<工作区哈希>` 子目录），始终位于工作区之外。
    pub fn open(workspace: impl Into<PathBuf>, store_dir: Option<PathBuf>) -> Result<Self> {
        let ws = workspace.into();
        let root = std::fs::canonicalize(&ws)
            .with_context(|| format!("工作区不存在: {}", ws.display()))?;
        if !root.is_dir() {
            bail!("工作区不是目录: {}", root.display());
        }
        let store = Store::open(&root, store_dir.as_deref())?;
        // 启动 GC（热补丁）：manifest 超出保留窗口时回收旧 manifest / journal
        // 条目 / 孤儿 blob。先做廉价检查（manifest 计数），未超限零成本——
        // 本构造函数在工具批边界被高频调用（runtime 每批 begin() 一次）。
        // GC 失败静默跳过（本 crate 不依赖 log），下次 open 仍超限时重试。
        let keep = gc_keep();
        let over_window = store.all_manifest_ids().is_ok_and(|ids| ids.len() > keep);
        if over_window {
            let _ = store.gc(keep, gc_max_bytes(), std::time::Duration::from_millis(100));
        }
        Ok(Session {
            root,
            store,
            opts: ScanOpts::default(),
            // 零上下文：`@@ -old,cnt +new,cnt @@` 头已带精确行号锚点，上下文行
            // 只是字节放大器（一次单行编辑被 5+3 行上下文膨胀 ~8 倍），而 spy
            // diff 的用途是**审计自查**——「这批改动是否符合意图」，不是编辑素材
            // （编辑有 read/apply_patch 的精确内容与 hash）。压缩后的 diff 能在
            // 同样的 4KB 报告预算里展开更多文件。需要上下文时用 [`Self::with_context`]。
            ctx_before: 0,
            ctx_after: 0,
            owner: String::new(),
        })
    }

    /// 设置归属标识（宿主的 agent session id）。扫描落进 journal 的每条变更都会带上它，
    /// 于是共享同一 store 的并发会话能把自己的变更和别人的分开（见 [`split_by_owner`]）。
    /// 不设则为空串——所有条目一律算"可归因"，等价于本字段引入前的行为。
    pub fn with_owner(mut self, owner: impl Into<String>) -> Self {
        self.owner = owner.into();
        self
    }

    /// 本会话的归属标识。
    pub fn owner(&self) -> &str {
        &self.owner
    }

    /// 覆盖报告里 diff 的上下文行数（默认 **0 前 / 0 后**——行号锚点由 `@@` 头
    /// 承担，见 `Session::open` 内注释）。仅在有特殊消费方需要原文邻接时才设。
    pub fn with_context(mut self, before: usize, after: usize) -> Self {
        self.ctx_before = before;
        self.ctx_after = after;
        self
    }

    /// 覆盖扫描参数（默认 [`ScanOpts::default`]）。宿主用它收紧 `lock_wait`
    /// 等与调用方等待预算相关的项。
    pub fn with_opts(mut self, opts: ScanOpts) -> Self {
        self.opts = opts;
        self
    }

    pub fn store_dir(&self) -> &std::path::Path {
        self.store.dir()
    }

    /// 全量扫描：落 blob + journal + manifest。稳态（无改动）只有 walk + stat 成本。
    pub fn scan(&self, trigger: Trigger) -> Result<ScanOutcome> {
        scan::run_scan(
            &self.root,
            &self.store,
            trigger.as_str(),
            &self.owner,
            &self.opts,
        )
    }

    /// 审计视图：mark 之后 journal 里的全部变更（按发生顺序，未按 path 归并）。
    pub fn changes_since(&self, from: &ScanId) -> Result<Vec<Change>> {
        Ok(self
            .store
            .read_journal()?
            .into_iter()
            .filter(|c| c.scan > from.0)
            .collect())
    }

    /// 漂移窗口（PR-D）：owner 上次边界 `from` 到边界 `to`（含 `to` 当批、不含
    /// `to` 之后）之间 journal 里的**去重文件数**，以及两个边界的扫描时间差毫秒。
    ///
    /// 时间差取两端 manifest 的 `ts`；任一 manifest 缺失（容量闸挤掉/损坏）返回
    /// `None`——调用方自行决定 fail-open 还是 fail-closed。窗口开区间语义与
    /// [`Session::changes_since`] 对齐：`from` 当批的变更不算漂移。
    pub fn drift_window(&self, from: &ScanId, to: &ScanId) -> Result<(usize, Option<u64>)> {
        let files: HashSet<String> = self
            .store
            .read_journal()?
            .into_iter()
            .filter(|c| c.scan > from.0 && c.scan <= to.0)
            .map(|c| c.path)
            .collect();
        let gap = (|| -> Option<u64> {
            let a = self.store.load_manifest(&from.0).ok()?;
            let b = self.store.load_manifest(&to.0).ok()?;
            let a = chrono::DateTime::parse_from_rfc3339(&a.ts).ok()?;
            let b = chrono::DateTime::parse_from_rfc3339(&b.ts).ok()?;
            Some((b - a).num_milliseconds().max(0) as u64)
        })();
        Ok((files.len(), gap))
    }

    /// 工具边界报告：先补一次扫描（确保边界之后的改动全部入账），
    /// 再对 mark 之后的净变更渲染限额报告。返回纯文本，如何注入由调用方决定。
    pub fn report_since(&self, from: &ScanId, max_bytes: usize) -> Result<String> {
        let changes = self.changes_after_tool_end(from)?;
        Ok(self.render_report(&changes, max_bytes)?.text)
    }

    /// 补一次 ToolEnd 扫描（确保边界之后的改动全部入账）并返回 mark 之后的全部
    /// 变更，**但不渲染**。宿主可在两步之间做自己的回填——例如把变更喂进宿主
    /// 审计链换取回滚定位符，再把定位符写进报告。
    pub fn changes_after_tool_end(&self, from: &ScanId) -> Result<Vec<Change>> {
        self.scan(Trigger::ToolEnd)?;
        self.changes_since(from)
    }

    /// 渲染限额报告（不扫描、不推进指针），返回正文**与实际展开的文件清单**。
    /// 与 [`Session::changes_after_tool_end`] 拆分的目的见该方法说明；展开清单
    /// 供宿主决定给哪些文件产回滚提示——逐文件产提示正是注入超出预算的主因。
    pub fn render_report(&self, changes: &[Change], max_bytes: usize) -> Result<Report> {
        report::build_report(
            &self.store,
            changes,
            max_bytes,
            self.ctx_before,
            self.ctx_after,
        )
    }

    /// 滚动指针模式：报告“上次报告以来”的变更并推进 `last_report` 指针。
    ///
    /// 库接入首选 [`Session::report_since`]（调用方自持 `ScanId` 边界，无共享指针
    /// 状态）；本方法留给外部 hook / 人工审计等无法持有边界的场合。
    pub fn report(&self, max_bytes: usize) -> Result<String> {
        let mark = self.report_mark()?;
        let text = self.report_since(&mark, max_bytes)?;
        let mut st = self.store.load_state()?;
        st.last_report = st.last_scan.clone();
        self.store.save_state(&st)?;
        Ok(text)
    }

    fn report_mark(&self) -> Result<ScanId> {
        let st = self.store.load_state()?;
        if let Some(r) = st.last_report {
            return Ok(ScanId(r));
        }
        let ids = self.store.all_manifest_ids()?;
        ids.first()
            .map(|id| ScanId(id.clone()))
            .with_context(|| "存储为空：先执行一次 scan 建立基线")
    }

    /// 单变更回滚：先校验当前内容仍是变更记录的 after（防盲目覆盖更新的改动），
    /// 不一致则拒绝，force 覆盖。
    pub fn undo(&self, change_id: &str, force: bool) -> Result<UndoOutcome> {
        let _guard = self.store.lock()?;
        let journal = self.store.read_journal()?;
        let c = journal
            .iter()
            .find(|c| c.id == change_id)
            .with_context(|| format!("变更不存在: {change_id}"))?;
        let target = self.root.join(&c.path);
        match c.status {
            ChangeStatus::Deleted => {
                let before = c.before.as_deref().context("被删文件缺少 before blob")?;
                if target.exists() && !force {
                    bail!("{} 当前已存在，回滚会覆盖它（--force 继续）", c.path);
                }
                let bytes = self.store.read_blob(before)?;
                atomic_write_bytes(&target, &bytes)?;
                Ok(UndoOutcome {
                    action: "restored",
                    path: c.path.clone(),
                })
            }
            ChangeStatus::Added => {
                if target.exists() {
                    self.ensure_matches(&target, c.after.as_deref(), &c.path, force)?;
                    std::fs::remove_file(&target)
                        .with_context(|| format!("删除失败: {}", c.path))?;
                }
                Ok(UndoOutcome {
                    action: "removed",
                    path: c.path.clone(),
                })
            }
            ChangeStatus::Modified => {
                self.ensure_matches(&target, c.after.as_deref(), &c.path, force)?;
                let before = c.before.as_deref().context("修改缺少 before blob")?;
                let bytes = self.store.read_blob(before)?;
                atomic_write_bytes(&target, &bytes)?;
                Ok(UndoOutcome {
                    action: "reverted",
                    path: c.path.clone(),
                })
            }
        }
    }

    fn ensure_matches(
        &self,
        target: &std::path::Path,
        after: Option<&str>,
        path: &str,
        force: bool,
    ) -> Result<()> {
        if force {
            return Ok(());
        }
        let cur = std::fs::read(target)
            .with_context(|| format!("{path} 已不存在，与变更记录不符（--force 继续）"))?;
        let sha = store::sha256_hex(&cur);
        match after {
            Some(a) if a == sha => Ok(()),
            other => bail!(
                "{path} 在变更之后又被改过（当前 sha={sha}，记录 after={other:?}），拒绝盲目回滚（--force 继续）"
            ),
        }
    }

    /// 整库回到某扫描点：重建缺失/变更文件；多余文件默认只列出，prune 才删。
    pub fn restore(&self, scan: &ScanId, prune: bool) -> Result<RestoreOutcome> {
        let _guard = self.store.lock()?;
        let files = self.store.materialize_manifest(&scan.0)?;
        let mut written = 0usize;
        let mut skipped = 0usize;
        for (rel, entry) in &files {
            let target = self.root.join(rel);
            let same = std::fs::read(&target)
                .map(|b| store::sha256_hex(&b) == entry.sha)
                .unwrap_or(false);
            if same {
                skipped += 1;
                continue;
            }
            let bytes = self.store.read_blob(&entry.sha)?;
            atomic_write_bytes(&target, &bytes)?;
            written += 1;
        }
        let extra = [self.store.dir().to_path_buf()];
        let cx = scan::WalkCx {
            root: &self.root,
            opts: &self.opts,
            extra_exclude: &extra,
        };
        let mut extras: Vec<String> = scan::collect_current(&cx)?
            .into_keys()
            .filter(|rel| !files.contains_key(rel))
            .collect();
        let mut pruned = 0usize;
        if prune {
            for rel in &extras {
                let p = self.root.join(rel);
                if p.is_file() {
                    std::fs::remove_file(&p).with_context(|| format!("删除多余文件失败: {rel}"))?;
                    pruned += 1;
                }
            }
        }
        extras.sort();
        Ok(RestoreOutcome {
            written,
            skipped,
            extras,
            pruned,
        })
    }

    /// 完整变更流水（审计）。
    pub fn journal(&self) -> Result<Vec<Change>> {
        self.store.read_journal()
    }

    /// 查看任意历史版本内容。
    pub fn cat(&self, sha: &str) -> Result<Vec<u8>> {
        self.store.read_blob(sha)
    }

    /// 最近一次扫描 id。
    pub fn latest_scan(&self) -> Result<Option<ScanId>> {
        Ok(self.store.load_state()?.last_scan.map(ScanId))
    }

    /// 手动触发存储 GC（保留最近 `keep` 份 manifest，见 [`Store::gc`]）。
    /// 启动路径的自动 GC 在 [`Session::open`]；本方法留给外部 hook / 人工
    /// 审计，锁等待沿用 opts 的值。
    pub fn gc(&self, keep: usize) -> Result<GcOutcome> {
        self.store.gc(keep, gc_max_bytes(), self.opts.lock_wait)
    }

    /// spy 存储根（`<QAQH_SPY_DIR 或 data_dir>/spy`）——与 [`store::Store`] 的
    /// `default_dir` 同一拼装规则（去掉工作区哈希那一级）。启动 GC 据此枚举
    /// 所有工作区 store。
    fn stores_root() -> PathBuf {
        let base = std::env::var("QAQH_SPY_DIR")
            .ok()
            .filter(|d| !d.is_empty())
            .map(PathBuf::from)
            .unwrap_or_else(qaqh_types::platform::data_dir);
        base.join("spy")
    }

    /// daemon 启动钩子：对**所有**工作区 store 立即跑一遍窗口 + 字节预算 GC。
    ///
    /// 为什么 Session::open 里的 GC 不够：它挂在「第一个带写路径的工具批」上，
    /// daemon 重启后的空闲期旧 blob 不回收。这里在启动时就把上界收紧；单 store
    /// 失败静默跳过（GC 永不影响主流程）。返回人类可读摘要（供宿主日志）。
    pub fn startup_gc() -> String {
        let entries = match std::fs::read_dir(Self::stores_root()) {
            Ok(entries) => entries,
            Err(_) => return "spy startup gc: no stores yet".to_string(),
        };
        let (keep, max_bytes) = (gc_keep(), gc_max_bytes());
        let (mut stores, mut manifests, mut blobs) = (0usize, 0usize, 0usize);
        for entry in entries.flatten() {
            let path = entry.path();
            if !path.is_dir() {
                continue;
            }
            // workspace 实参在 override_dir 模式下不参与路径拼装，传空即可。
            let opened = store::Store::open(Path::new(""), Some(&path));
            if let Ok(out) =
                opened.and_then(|s| s.gc(keep, max_bytes, std::time::Duration::from_secs(2)))
            {
                stores += 1;
                manifests += out.dropped_manifests;
                blobs += out.dropped_blobs;
            }
        }
        if stores == 0 {
            return "spy startup gc: no stores yet".to_string();
        }
        format!("spy startup gc: {stores} stores, -{manifests} manifests, -{blobs} blobs")
    }
}
