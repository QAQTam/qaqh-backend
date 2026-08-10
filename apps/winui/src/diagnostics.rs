//! 壳侧性能诊断（纯本地 · 白名单 · 三档采集）。
//!
//! 隐私合规是**结构性保证**而非承诺性声明：
//!   - **白名单采集**：只允许本文件列出的结构化性能字段进入缓冲；自由文本禁止。
//!   - **仅性能侧**：对话内容、工具参数、seed、token、路径一律不采集。
//!     渲染耗时/帧间隔/事件吞吐是纯运行时指标，不涉及任何用户数据。
//!   - **无系统指纹**：唯一系统信息 = OS 版本号（`RtlGetVersion`，纯数字三元组）。
//!     不读 MachineGuid / SID / 网卡 MAC / 硬件序列号 / 显示器 EDID / 字体列表。
//!   - **纯本地**：数据只写 `%LOCALAPPDATA%\DeepX\diagnostics\`，无任何网络上报；
//!     GPT/Claude 侧合规由「导出前用户可见可审查」保证（诊断包是本地 JSON 文件）。
//!   - **默认 ZDR**：进程启动即零诊断；用户显式在设置页开启后才开始采集。
//!
//! 三档模式（对齐 Windows 诊断数据分级语义）：
//!   - `Full`   完整数据采集：渲染/帧间隔/drain/scale 事件逐条入环形缓冲（2048 条），
//!     导出含 `recent_events` 相对时间线 + 全量聚合统计。
//!   - `Minimal`最小数据采集：只累计聚合桶（计数/均值/峰值），不存事件时间线。
//!   - `ZDR`    零诊断数据记录：所有 `record_*` 入口一次原子读即返回，零分配零写入。
//!
//! 数据来源（全部 UI 线程，无锁竞争担忧，Mutex 仅为未来后台线程接入兜底）：
//!   - 渲染事件：reactor `set_render_observer`（fork engine.rs 每帧回调）；
//!   - 帧间隔 / drain 吞吐 / scale：chat_view.rs 帧回调内接线。

use std::collections::VecDeque;
use std::path::PathBuf;
use std::sync::atomic::{AtomicU8, Ordering};
use std::sync::{Mutex, OnceLock};
use std::time::Instant;

use serde_json::{json, Value};

// ── 模式（原子档位）─────────────────────────────────────
const MODE_ZERO: u8 = 0;
const MODE_MINIMAL: u8 = 1;
const MODE_FULL: u8 = 2;

static MODE: AtomicU8 = AtomicU8::new(MODE_ZERO);
static STARTED: OnceLock<Instant> = OnceLock::new();

/// 环形缓冲容量（Full 模式）。2048 × ~64B ≈ 130KB，可接受。
const RING_CAPACITY: usize = 2048;

/// 采集模式。
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum Mode {
    Full,
    Minimal,
    Zero,
}

impl Mode {
    fn from_u8(v: u8) -> Mode {
        match v {
            MODE_FULL => Mode::Full,
            MODE_MINIMAL => Mode::Minimal,
            _ => Mode::Zero,
        }
    }
    fn to_u8(self) -> u8 {
        match self {
            Mode::Full => MODE_FULL,
            Mode::Minimal => MODE_MINIMAL,
            Mode::Zero => MODE_ZERO,
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            Mode::Full => "full",
            Mode::Minimal => "minimal",
            Mode::Zero => "zero",
        }
    }
    pub fn label(self) -> &'static str {
        match self {
            Mode::Full => "完整数据采集",
            Mode::Minimal => "最小数据采集",
            Mode::Zero => "ZDR（零诊断数据记录）",
        }
    }
}

/// 白名单事件（仅性能字段；无自由文本）。
#[derive(Clone, Debug)]
enum Event {
    /// reactor 单次渲染耗时（ms + 控件 diff 计数）。
    Render {
        at_ms: u32,
        tree_ms: f32,
        reconcile_ms: f32,
        effects_ms: f32,
        diffed: u32,
        skipped: u32,
        created: u32,
    },
    /// 帧回调间隔（ms；>2s 的暂停恢复间隔不入缓冲）。
    Frame { at_ms: u32, interval_ms: f32 },
    /// 单帧 drain 的事件数（chat 事件 + output 决议）。
    Drain { at_ms: u32, events: u32 },
    /// 数据规模快照（每 5s 一次）。
    Scale { at_ms: u32, turns: u32, window: u32, rev: u64 },
    /// reactor fault（仅 context 名，panic message 含敏感内容不入诊断）。
    Fault { at_ms: u32, context: &'static str },
}

/// 聚合桶（Minimal / Full 共用）。
#[derive(Default, Clone)]
struct Agg {
    renders: u64,
    tree_ms_sum: f64,
    tree_ms_max: f64,
    reconcile_ms_sum: f64,
    reconcile_ms_max: f64,
    effects_ms_sum: f64,
    effects_ms_max: f64,
    total_ms_max: f64,
    frames: u64,
    frame_ms_sum: f64,
    frame_ms_max: f64,
    drains: u64,
    drain_events: u64,
    drain_events_max: u64,
    scales: u64,
    faults: Vec<(&'static str, u64)>,
}

struct Buffer {
    events: VecDeque<Event>,
    agg: Agg,
}

static BUF: OnceLock<Mutex<Buffer>> = OnceLock::new();

fn buffer() -> &'static Mutex<Buffer> {
    BUF.get_or_init(|| {
        Mutex::new(Buffer {
            events: VecDeque::with_capacity(RING_CAPACITY),
            agg: Agg::default(),
        })
    })
}

fn uptime_ms() -> u32 {
    let started = *STARTED.get_or_init(Instant::now);
    started.elapsed().as_millis().min(u32::MAX as u128) as u32
}

// ── 模式管理 ─────────────────────────────────────────────

/// 启动时调用：从持久化文件恢复上次选择的模式（不存在 = ZDR）。
pub fn init() {
    if let Ok(text) = std::fs::read_to_string(config_path()) {
        if let Ok(v) = serde_json::from_str::<Value>(&text) {
            if let Some(m) = v.get("mode").and_then(|x| x.as_str()) {
                MODE.store(match m {
                    "full" => MODE_FULL,
                    "minimal" => MODE_MINIMAL,
                    _ => MODE_ZERO,
                }, Ordering::Relaxed);
            }
        }
    }
}

/// 设置模式并立即持久化（设置页调用）。模式切换 = 采集上下文重置
/// （清空缓冲与聚合，避免跨模式串扰/残留污染导出）。
pub fn set_mode(mode: Mode) {
    MODE.store(mode.to_u8(), Ordering::Relaxed);
    let mut buf = buffer().lock().unwrap();
    buf.events.clear();
    buf.agg = Agg::default();
    drop(buf);
    let _ = std::fs::create_dir_all(config_path().parent().unwrap_or(std::path::Path::new(".")));
    let _ = std::fs::write(config_path(), json!({ "mode": mode.name() }).to_string());
}

pub fn mode() -> Mode {
    Mode::from_u8(MODE.load(Ordering::Relaxed))
}

fn config_path() -> PathBuf {
    let base =
        std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".to_string());
    PathBuf::from(base).join("DeepX").join("diagnostics-mode.json")
}

// ── 采集入口（ZDR 下均为一次原子读）────────────────────

/// reactor 渲染完成回调（fork engine.rs set_render_observer 每帧调用）。
pub fn record_render(tree_ms: f64, reconcile_ms: f64, effects_ms: f64, diffed: u64, skipped: u64, created: u64) {
    if MODE.load(Ordering::Relaxed) == MODE_ZERO {
        return;
    }
    let at_ms = uptime_ms();
    let mut buf = buffer().lock().unwrap();
    let agg = &mut buf.agg;
    agg.renders += 1;
    agg.tree_ms_sum += tree_ms;
    agg.tree_ms_max = agg.tree_ms_max.max(tree_ms);
    agg.reconcile_ms_sum += reconcile_ms;
    agg.reconcile_ms_max = agg.reconcile_ms_max.max(reconcile_ms);
    agg.effects_ms_sum += effects_ms;
    agg.effects_ms_max = agg.effects_ms_max.max(effects_ms);
    agg.total_ms_max = agg.total_ms_max.max(tree_ms + reconcile_ms + effects_ms);
    if MODE.load(Ordering::Relaxed) == MODE_FULL {
        push_event(Event::Render {
            at_ms,
            tree_ms: tree_ms as f32,
            reconcile_ms: reconcile_ms as f32,
            effects_ms: effects_ms as f32,
            diffed: diffed.min(u32::MAX as u64) as u32,
            skipped: skipped.min(u32::MAX as u64) as u32,
            created: created.min(u32::MAX as u64) as u32,
        }, &mut buf);
    }
}

/// 帧回调间隔（chat_view on_frame；interval > 2000ms = 暂停恢复，跳过）。
pub fn record_frame_interval(interval_ms: f64) {
    if MODE.load(Ordering::Relaxed) == MODE_ZERO {
        return;
    }
    if !(0.0..=2000.0).contains(&interval_ms) {
        return;
    }
    let at_ms = uptime_ms();
    let mut buf = buffer().lock().unwrap();
    let agg = &mut buf.agg;
    agg.frames += 1;
    agg.frame_ms_sum += interval_ms;
    agg.frame_ms_max = agg.frame_ms_max.max(interval_ms);
    if MODE.load(Ordering::Relaxed) == MODE_FULL {
        push_event(Event::Frame { at_ms, interval_ms: interval_ms as f32 }, &mut buf);
    }
}

/// 单帧 drain 吞吐（chat 事件 + output 决议总数）。
pub fn record_drain(events: u32) {
    if MODE.load(Ordering::Relaxed) == MODE_ZERO {
        return;
    }
    let at_ms = uptime_ms();
    let mut buf = buffer().lock().unwrap();
    let agg = &mut buf.agg;
    agg.drains += 1;
    agg.drain_events += events as u64;
    agg.drain_events_max = agg.drain_events_max.max(events as u64);
    if MODE.load(Ordering::Relaxed) == MODE_FULL {
        push_event(Event::Drain { at_ms, events }, &mut buf);
    }
}

/// 数据规模快照（chat_view scale 日志处，每 5s 一次）。
pub fn record_scale(turns: u32, window: u32, rev: u64) {
    if MODE.load(Ordering::Relaxed) == MODE_ZERO {
        return;
    }
    let at_ms = uptime_ms();
    let mut buf = buffer().lock().unwrap();
    buf.agg.scales += 1;
    if MODE.load(Ordering::Relaxed) == MODE_FULL {
        push_event(Event::Scale { at_ms, turns, window, rev }, &mut buf);
    }
}

/// reactor fault（仅 context 名；message 不入诊断）。
pub fn record_fault(context: &'static str) {
    if MODE.load(Ordering::Relaxed) == MODE_ZERO {
        return;
    }
    let at_ms = uptime_ms();
    let mut buf = buffer().lock().unwrap();
    let agg = &mut buf.agg;
    if let Some((_, n)) = agg.faults.iter_mut().find(|(c, _)| *c == context) {
        *n += 1;
    } else {
        agg.faults.push((context, 1));
    }
    if MODE.load(Ordering::Relaxed) == MODE_FULL {
        push_event(Event::Fault { at_ms, context }, &mut buf);
    }
}

fn push_event(ev: Event, buf: &mut Buffer) {
    if buf.events.len() >= RING_CAPACITY {
        buf.events.pop_front();
    }
    buf.events.push_back(ev);
}

// ── 导出（纯本地白名单 JSON）────────────────────────────

/// 生成诊断包 JSON（白名单字段；`mode` 为当前模式）。
pub fn export_json() -> Value {
    let buf = buffer().lock().unwrap();
    let agg = buf.agg.clone();
    let events: Vec<Value> = if mode() == Mode::Full {
        buf.events.iter().map(event_to_json).collect()
    } else {
        Vec::new()
    };
    let perf = json!({
        "renders": {
            "count": agg.renders,
            "avg_tree_build_ms": r1(agg.tree_ms_sum / agg.renders.max(1) as f64),
            "max_tree_build_ms": r1(agg.tree_ms_max),
            "avg_reconcile_ms": r1(agg.reconcile_ms_sum / agg.renders.max(1) as f64),
            "max_reconcile_ms": r1(agg.reconcile_ms_max),
            "avg_effects_ms": r1(agg.effects_ms_sum / agg.renders.max(1) as f64),
            "max_effects_ms": r1(agg.effects_ms_max),
            "max_total_ms": r1(agg.total_ms_max),
        },
        "frames": {
            "count": agg.frames,
            "avg_interval_ms": r1(agg.frame_ms_sum / agg.frames.max(1) as f64),
            "max_interval_ms": r1(agg.frame_ms_max),
        },
        "drain": {
            "frames": agg.drains,
            "total_events": agg.drain_events,
            "max_events_per_frame": agg.drain_events_max,
        },
        "scales": agg.scales,
        "faults": agg.faults.iter().map(|(c, n)| (c, n)).collect::<Vec<_>>(),
    });
    json!({
        "schema": 1,
        "mode": mode().name(),
        "generated_uptime_secs": r1(uptime_ms() as f64 / 1000.0),
        "app": {
            "version": env!("CARGO_PKG_VERSION"),
            "build_id": build_id(),
        },
        "os": os_version(),
        "perf": perf,
        "recent_events": events,
    })
}

/// 写诊断包到 `%LOCALAPPDATA%\DeepX\diagnostics\`，返回完整路径。
pub fn export_to_file() -> Option<String> {
    let base = std::env::var("LOCALAPPDATA").unwrap_or_else(|_| ".".to_string());
    let dir = PathBuf::from(base).join("DeepX").join("diagnostics");
    std::fs::create_dir_all(&dir).ok()?;
    // 文件名带 build_id（含 UTC 时间戳）+ 自增序号，避免覆盖历史导出。
    let prefix = if build_id().is_empty() {
        "dev".to_string()
    } else {
        build_id()
    };
    let mut n = 1usize;
    loop {
        let name = if n == 1 {
            format!("diagnostics-{prefix}.json")
        } else {
            format!("diagnostics-{prefix}-{n}.json")
        };
        let path = dir.join(&name);
        if !path.exists() {
            let text = serde_json::to_string_pretty(&export_json()).ok()?;
            std::fs::write(&path, text).ok()?;
            return Some(path.to_string_lossy().into_owned());
        }
        n += 1;
    }
}

/// 当前缓冲的事件条数（设置页状态行显示）。
pub fn buffered_events() -> usize {
    if mode() == Mode::Zero {
        return 0;
    }
    buffer().lock().unwrap().events.len()
}

fn event_to_json(ev: &Event) -> Value {
    match ev {
        Event::Render { at_ms, tree_ms, reconcile_ms, effects_ms, diffed, skipped, created } => json!({
            "t_ms": at_ms, "k": "render",
            "tree_ms": tree_ms, "reconcile_ms": reconcile_ms, "effects_ms": effects_ms,
            "diffed": diffed, "skipped": skipped, "created": created,
        }),
        Event::Frame { at_ms, interval_ms } => json!({
            "t_ms": at_ms, "k": "frame", "interval_ms": interval_ms,
        }),
        Event::Drain { at_ms, events } => json!({
            "t_ms": at_ms, "k": "drain", "events": events,
        }),
        Event::Scale { at_ms, turns, window, rev } => json!({
            "t_ms": at_ms, "k": "scale", "turns": turns, "window": window, "rev": rev,
        }),
        Event::Fault { at_ms, context } => json!({
            "t_ms": at_ms, "k": "fault", "context": context,
        }),
    }
}

fn r1(v: f64) -> f64 {
    (v * 10.0).round() / 10.0
}

/// 构建标识：exe 同目录 `build-info.json`（installer 注入），fallback 包版本。
fn build_id() -> String {
    if let Ok(exe) = std::env::current_exe() {
        if let Some(dir) = exe.parent() {
            let info = dir.join("build-info.json");
            if let Ok(text) = std::fs::read_to_string(info) {
                if let Ok(v) = serde_json::from_str::<Value>(&text) {
                    if let Some(id) = v.get("build_id").and_then(|x| x.as_str()) {
                        return id.to_string();
                    }
                }
            }
        }
    }
    String::new()
}

/// OS 版本号（RtlGetVersion，纯数字三元组；失败 = None）。
/// 这是诊断包中**唯一**的系统信息——不包含任何指纹字段。
fn os_version() -> Option<Value> {
    use windows::Win32::wdm::RtlGetVersion;
    use windows::Win32::winnt::OSVERSIONINFOW;
    let mut info = OSVERSIONINFOW::default();
    info.dwOSVersionInfoSize = std::mem::size_of::<OSVERSIONINFOW>() as u32;
    let status = unsafe { RtlGetVersion(&mut info) };
    if status.is_ok() {
        Some(json!({
            "major": info.dwMajorVersion,
            "minor": info.dwMinorVersion,
            "build": info.dwBuildNumber,
        }))
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 全局 MODE/BUF 是共享静态；cargo test 默认并行跑，测试间必须串行。
    static TEST_LOCK: Mutex<()> = Mutex::new(());

    /// 模式切换 + 持久化 roundtrip。
    #[test]
    fn mode_roundtrip() {
        let _g = TEST_LOCK.lock().unwrap();
        set_mode(Mode::Full);
        assert_eq!(mode(), Mode::Full);
        set_mode(Mode::Zero);
        assert_eq!(mode(), Mode::Zero);
        // 恢复为默认（避免污染后续测试）。
        set_mode(Mode::Zero);
    }

    /// ZDR：record_* 全为 no-op，缓冲为空、导出 perf 计数为 0。
    #[test]
    fn zdr_is_noop() {
        let _g = TEST_LOCK.lock().unwrap();
        set_mode(Mode::Zero);
        record_render(1.0, 2.0, 0.5, 3, 4, 5);
        record_frame_interval(16.6);
        record_drain(42);
        record_fault("render");
        assert_eq!(buffered_events(), 0);
        let v = export_json();
        assert_eq!(v["mode"], "zero");
        assert_eq!(v["perf"]["renders"]["count"], 0);
        assert_eq!(v["recent_events"].as_array().unwrap().len(), 0);
    }

    /// Full：事件入环形缓冲，导出含时间线 + 聚合 + OS 版本号。
    #[test]
    fn full_collects_and_exports() {
        let _g = TEST_LOCK.lock().unwrap();
        set_mode(Mode::Full);
        record_render(1.5, 2.5, 0.5, 3, 4, 5);
        record_frame_interval(16.6);
        record_drain(7);
        assert_eq!(buffered_events(), 3);
        let v = export_json();
        assert_eq!(v["mode"], "full");
        assert_eq!(v["perf"]["renders"]["count"], 1);
        assert_eq!(v["recent_events"].as_array().unwrap().len(), 3);
        // 唯一系统信息：OS 版本号（数字三元组，无指纹字段）。
        let os = v["os"].as_object().expect("os version present");
        assert!(os.contains_key("major") && os.contains_key("minor") && os.contains_key("build"));
        assert!(!os.contains_key("machine_guid") && !os.contains_key("sid"));
        // 白名单：顶层仅允许声明字段。
        let top: Vec<&str> = v.as_object().unwrap().keys().map(|k| k.as_str()).collect();
        for k in top {
            assert!(
                ["schema", "mode", "generated_uptime_secs", "app", "os", "perf", "recent_events"]
                    .contains(&k),
                "unexpected top-level field: {k}"
            );
        }
        set_mode(Mode::Zero);
    }

    /// 环形缓冲上限：溢出丢最老。
    #[test]
    fn ring_bounded() {
        let _g = TEST_LOCK.lock().unwrap();
        set_mode(Mode::Full);
        for i in 0..(RING_CAPACITY + 10) {
            record_drain(i as u32);
        }
        assert_eq!(buffered_events(), RING_CAPACITY);
        set_mode(Mode::Zero);
    }

    /// Minimal：聚合累计但不存事件时间线。
    #[test]
    fn minimal_aggregates_only() {
        let _g = TEST_LOCK.lock().unwrap();
        set_mode(Mode::Minimal);
        record_render(2.0, 1.0, 0.0, 1, 1, 1);
        record_render(4.0, 3.0, 0.0, 2, 2, 2);
        assert_eq!(buffered_events(), 0);
        let v = export_json();
        assert_eq!(v["mode"], "minimal");
        assert_eq!(v["perf"]["renders"]["count"], 2);
        assert_eq!(v["perf"]["renders"]["avg_tree_build_ms"], 3.0);
        assert_eq!(v["recent_events"].as_array().unwrap().len(), 0);
        set_mode(Mode::Zero);
    }
}
