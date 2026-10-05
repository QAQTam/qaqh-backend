//! qaqh-spy：工作区文件变动审计与紧急备份。
//!
//! 用法：**进程内库**。qaqh 在工具批边界做一次全量扫描——调用点位于
//! `qaqh-runtime/src/agent/workspace_audit.rs`（`begin` 调 `Session::scan`、
//! `finish` 调 `Session::report_since`），把限额报告作为注入消息下发给
//! 模型（设计见 `docs/plan-workspace-diff-injection.md`）。
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
pub use scan::{ScanOpts, ScanOutcome};
pub use store::{Change, ChangeStatus, GcOutcome};

use std::path::PathBuf;

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
            let _ = store.gc(keep, std::time::Duration::from_millis(100));
        }
        Ok(Session {
            root,
            store,
            opts: ScanOpts::default(),
            ctx_before: 5,
            ctx_after: 3,
        })
    }

    /// 设置报告里 diff 的非对称上下文（默认 5 前 / 3 后）。
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
        scan::run_scan(&self.root, &self.store, trigger.as_str(), &self.opts)
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

    /// 工具边界报告：先补一次扫描（确保边界之后的改动全部入账），
    /// 再对 mark 之后的净变更渲染限额报告。返回纯文本，如何注入由调用方决定。
    pub fn report_since(&self, from: &ScanId, max_bytes: usize) -> Result<String> {
        let changes = self.changes_after_tool_end(from)?;
        self.render_report(&changes, max_bytes)
    }

    /// 补一次 ToolEnd 扫描（确保边界之后的改动全部入账）并返回 mark 之后的全部
    /// 变更，**但不渲染**。宿主可在两步之间做自己的回填——例如把变更喂进宿主
    /// 审计链换取回滚定位符，再把定位符写进报告。
    pub fn changes_after_tool_end(&self, from: &ScanId) -> Result<Vec<Change>> {
        self.scan(Trigger::ToolEnd)?;
        self.changes_since(from)
    }

    /// 渲染限额报告（不扫描、不推进指针）。与 [`Session::changes_after_tool_end`]
    /// 拆分的目的见该方法说明。
    pub fn render_report(&self, changes: &[Change], max_bytes: usize) -> Result<String> {
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
        self.store.gc(keep, self.opts.lock_wait)
    }
}
