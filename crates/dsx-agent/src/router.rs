use dsx_types::{DebugLevel, RouterCommand, TaskPhase};
use std::sync::atomic::{AtomicU8, Ordering};

/// Global phase + debug level set by the status tool, read by start_agent_loop().
/// Encoded as: bits [2:0] = TaskPhase (0=Plan,1=Coding,2=Debug),
///             bits [4:3] = DebugLevel (0=Low,1=Medium,2=High)
pub static CURRENT_PHASE: AtomicU8 = AtomicU8::new(0);

fn encode(phase: TaskPhase, level: DebugLevel) -> u8 {
    let p = match phase {
        TaskPhase::Plan => 0u8,
        TaskPhase::Coding => 1,
        TaskPhase::Debug => 2,
    };
    let l = match level {
        DebugLevel::Low => 0u8,
        DebugLevel::Medium => 1,
        DebugLevel::High => 2,
    };
    p | (l << 3)
}

pub fn read_phase() -> TaskPhase {
    let v = CURRENT_PHASE.load(Ordering::Relaxed) & 0x07;
    match v {
        0 => TaskPhase::Plan,
        1 => TaskPhase::Coding,
        2 => TaskPhase::Debug,
        _ => TaskPhase::Coding,
    }
}

pub fn read_debug_level() -> DebugLevel {
    let v = (CURRENT_PHASE.load(Ordering::Relaxed) >> 3) & 0x03;
    match v {
        0 => DebugLevel::Low,
        2 => DebugLevel::High,
        _ => DebugLevel::Medium,
    }
}

pub fn set_phase(phase: TaskPhase, level: DebugLevel) {
    CURRENT_PHASE.store(encode(phase, level), Ordering::Relaxed);
}

/// Process a [`RouterCommand`] from IPC, returning a human-readable ack string.
pub fn handle_router_command(cmd: RouterCommand) -> String {
    match cmd {
        RouterCommand::SetPhase { phase, level } => {
            set_phase(phase, level);
            format!("[OK] Router: phase={:?} level={:?}", phase, level)
        }
    }
}

// ── Routing table ──

pub struct PhaseConfig {
    pub model: &'static str,
    pub effort: Option<&'static str>,
    pub max_tokens: u32,
}

pub fn phase_config(phase: TaskPhase, level: DebugLevel) -> PhaseConfig {
    match phase {
        TaskPhase::Plan => PhaseConfig {
            model: "deepseek-v4-pro",
            effort: Some("max"),
            max_tokens: 300_000,
        },
        TaskPhase::Coding => PhaseConfig {
            model: "deepseek-v4-flash",
            effort: Some("high"),
            max_tokens: 96_000,
        },
        TaskPhase::Debug => match level {
            DebugLevel::Low => PhaseConfig {
                model: "deepseek-v4-pro",
                effort: Some("high"),
                max_tokens: 64_000,
            },
            DebugLevel::Medium => PhaseConfig {
                model: "deepseek-v4-pro",
                effort: Some("max"),
                max_tokens: 96_000,
            },
            DebugLevel::High => PhaseConfig {
                model: "deepseek-v4-pro",
                effort: Some("max"),
                max_tokens: 128_000,
            },
        },
    }
}

// ── Phase prompt suffixes ──

pub fn phase_prompt_suffix(phase: TaskPhase, lang: &str) -> Option<&'static str> {
    if lang == "zh" {
        match phase {
            TaskPhase::Plan => Some("\n\
                模式: PLAN · 模型: Pro(超级大脑)\n\
                分析和设计阶段，输出结构化方案后等待批准。"),
            TaskPhase::Coding => Some("\n\
                模式: CODING · 模型: Flash(快速经济)\n\
                高效实现变更。"),
            TaskPhase::Debug => Some("\n\
                模式: DEBUG · 模型: Pro(超级大脑)\n\
                专注排查和修复错误。"),
        }
    } else {
        match phase {
            TaskPhase::Plan => Some("\n\
                Mode: PLAN · Model: Pro (super brain)\n\
                Analyze and design, output a structured plan, wait for approval."),
            TaskPhase::Coding => Some("\n\
                Mode: CODING · Model: Flash (fast & economical)\n\
                Implement changes efficiently."),
            TaskPhase::Debug => Some("\n\
                Mode: DEBUG · Model: Pro (super brain)\n\
                Focus on diagnosing and fixing errors."),
        }
    }
}

/// Detect initial phase from user input (used when auto_mode is ON and
/// no status() has been called yet). Simple keyword scoring — no API call.
pub fn detect_initial_phase(input: &str) -> TaskPhase {
    let lower = input.to_lowercase();
    // Plan keywords: design, architecture, analyze, review, plan, refactor, 设计, 架构, 分析, 规划
    let plan_kws = ["design", "architect", "analyze", "review", "plan", "refactor",
        "方案", "架构", "分析", "规划", "审查", "重构"];
    // Debug keywords: debug, error, bug, crash, fail, fix, 调试, 错误, bug, 修复
    let debug_kws = ["debug", "error", "bug", "crash", "fail", "broken",
        "调试", "错误", "故障", "崩溃"];
    // Code keywords: implement, write, create, build, add, 实现, 写, 创建, 添加
    let code_kws = ["implement", "write", "create", "build", "add", "make",
        "实现", "写", "创建", "添加", "修改"];

    for kw in &plan_kws {
        if lower.contains(kw) { return TaskPhase::Plan; }
    }
    for kw in &debug_kws {
        if lower.contains(kw) { return TaskPhase::Debug; }
    }
    for kw in &code_kws {
        if lower.contains(kw) { return TaskPhase::Coding; }
    }
    TaskPhase::Coding
}

// ── Parse status tool arguments ──

pub fn parse_status_args(args: &str) -> (TaskPhase, DebugLevel) {
    let state = parse_string_arg(args, "state").unwrap_or_default();
    let difficulty = parse_string_arg(args, "difficulty");
    let phase = TaskPhase::from_str(&state);
    let level = difficulty
        .as_deref()
        .map(|d| DebugLevel::from_str(d))
        .unwrap_or_default();
    (phase, level)
}

fn parse_string_arg(args: &str, key: &str) -> Option<String> {
    if let Ok(v) = serde_json::from_str::<serde_json::Value>(args) {
        if let Some(val) = v.get(key) {
            return match val {
                serde_json::Value::String(s) => Some(s.clone()),
                other => Some(other.to_string()),
            };
        }
    }
    None
}
